use crate::GuardianMode;
use crate::build::find_client_jar;
use axial_minecraft::download::{
    ExpectedIntegrity, LauncherManagedArtifactReadiness, LibraryVerificationIntegrity,
    library_verification_plans_for,
};
use axial_minecraft::{
    LaunchModelError, ManagedRuntimeCache, RuntimeOverride, VersionBundleReadGuard, VersionJson,
    default_environment, parse_runtime_override, resolve_version,
    runtime_component_executable_present_without_probe,
    runtime_component_structurally_ready_without_probe, runtime_executable_ready_without_probe,
};
use serde::Serialize;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct LaunchReadinessRequest {
    pub library_dir: PathBuf,
    pub version_id: String,
    pub requested_java: String,
    pub guardian_mode: GuardianMode,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct LaunchReadiness {
    pub launchable: bool,
    pub reasons: Vec<LaunchReadinessReason>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct LaunchReadinessReason {
    pub id: LaunchReadinessReasonId,
    pub severity: LaunchReadinessSeverity,
    pub message: &'static str,
}

macro_rules! launch_readiness_reasons {
    ($($variant:ident),+ $(,)?) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
        #[serde(rename_all = "snake_case")]
        pub enum LaunchReadinessReasonId {
            $($variant),+
        }

        impl LaunchReadinessReasonId {
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];
        }
    };
}

launch_readiness_reasons! {
    InstalledVersionsDegraded,
    VersionJsonMissing,
    ParentVersionMissing,
    IncompleteInstall,
    ClientJarMissing,
    ClientJarCorrupt,
    LibrariesMissing,
    LibrariesCorrupt,
    AssetIndexMissing,
    AssetIndexCorrupt,
    ManagedRuntimeMissing,
    JavaOverrideMissing,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LaunchReadinessSeverity {
    Blocking,
    Recoverable,
}

pub fn inspect_launch_readiness_summary(
    runtime_cache: &ManagedRuntimeCache,
    request: &LaunchReadinessRequest,
) -> LaunchReadiness {
    inspect_readiness(runtime_cache, request, LaunchReadinessInspection::Summary)
}

pub fn inspect_launch_readiness_structural(
    runtime_cache: &ManagedRuntimeCache,
    request: &LaunchReadinessRequest,
) -> LaunchReadiness {
    inspect_readiness(
        runtime_cache,
        request,
        LaunchReadinessInspection::Structural,
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LaunchReadinessInspection {
    Summary,
    Structural,
}

fn inspect_readiness(
    runtime_cache: &ManagedRuntimeCache,
    request: &LaunchReadinessRequest,
    inspection: LaunchReadinessInspection,
) -> LaunchReadiness {
    let publication_read = match VersionBundleReadGuard::acquire(&request.library_dir) {
        Ok(publication_read) => publication_read,
        Err(error) => return publication_readiness_error(&error),
    };
    let mut reasons = Vec::new();

    match resolve_version(&request.library_dir, &request.version_id) {
        Ok(version) => {
            if inspection != LaunchReadinessInspection::Structural {
                inspect_version_files(
                    &request.library_dir,
                    &request.version_id,
                    &version,
                    &mut reasons,
                );
            }
        }
        Err(error) => {
            reasons.push(reason_for_version_error(&error));
        }
    }

    inspect_runtime_files(runtime_cache, request, inspection, &mut reasons);

    if let Err(error) = publication_read.revalidate() {
        return publication_readiness_error(&error);
    }

    LaunchReadiness {
        launchable: reasons
            .iter()
            .all(|reason| reason.severity != LaunchReadinessSeverity::Blocking),
        reasons,
    }
}

fn publication_readiness_error(error: &std::io::Error) -> LaunchReadiness {
    let reason = if error.kind() == ErrorKind::WouldBlock {
        reason(
            LaunchReadinessReasonId::IncompleteInstall,
            "Installation is changing. Wait for it to finish before launching.",
            LaunchReadinessSeverity::Blocking,
        )
    } else {
        reason(
            LaunchReadinessReasonId::InstalledVersionsDegraded,
            "Installed versions could not be inspected safely.",
            LaunchReadinessSeverity::Blocking,
        )
    };
    LaunchReadiness {
        launchable: false,
        reasons: vec![reason],
    }
}

fn inspect_version_files(
    library_dir: &Path,
    version_id: &str,
    version: &VersionJson,
    reasons: &mut Vec<LaunchReadinessReason>,
) {
    match find_client_jar(library_dir, version, version_id) {
        Some(client_jar) => {
            if let Some(entry) = version.downloads.client.as_ref() {
                let expected = ExpectedIntegrity::from_mojang(entry.size, &entry.sha1);
                inspect_artifact_metadata(
                    &client_jar,
                    &expected,
                    missing_client_reason,
                    corrupt_client_reason,
                    reasons,
                );
            }
        }
        None => {
            reasons.push(missing_client_reason());
        }
    }

    let planned_libraries =
        library_verification_plans_for(library_dir, &version.libraries, &default_environment());
    let library_planning_failed = planned_libraries.is_err();
    let library_jobs: Vec<ArtifactVerificationJob> = planned_libraries
        .unwrap_or_default()
        .into_iter()
        .map(|library| ArtifactVerificationJob {
            path: library.path,
            integrity: library.integrity,
        })
        .collect();
    let library_readiness = verify_artifact_jobs_metadata(library_jobs);
    let libraries_missing = library_readiness.contains(&LauncherManagedArtifactReadiness::Missing);
    let libraries_corrupt = library_readiness
        .iter()
        .any(|status| *status != LauncherManagedArtifactReadiness::Verified);
    if library_planning_failed {
        reasons.push(reason(
            LaunchReadinessReasonId::LibrariesCorrupt,
            "Library metadata is invalid. Repair this version before launching.",
            LaunchReadinessSeverity::Blocking,
        ));
    } else if libraries_missing {
        reasons.push(reason(
            LaunchReadinessReasonId::LibrariesMissing,
            "Required libraries are missing. Install this version before launching.",
            LaunchReadinessSeverity::Blocking,
        ));
    } else if libraries_corrupt {
        reasons.push(reason(
            LaunchReadinessReasonId::LibrariesCorrupt,
            "Required libraries are corrupt. Repair this version before launching.",
            LaunchReadinessSeverity::Blocking,
        ));
    }

    if !version.asset_index.id.trim().is_empty() {
        let asset_index_path = library_dir
            .join("assets")
            .join("indexes")
            .join(format!("{}.json", version.asset_index.id));
        if !asset_index_path.is_file() {
            reasons.push(reason(
                LaunchReadinessReasonId::AssetIndexMissing,
                "Asset index is missing. Install this version before launching.",
                LaunchReadinessSeverity::Blocking,
            ));
        } else {
            let expected =
                ExpectedIntegrity::from_mojang(version.asset_index.size, &version.asset_index.sha1);
            inspect_artifact_metadata(
                &asset_index_path,
                &expected,
                || {
                    reason(
                        LaunchReadinessReasonId::AssetIndexMissing,
                        "Asset index is missing. Install this version before launching.",
                        LaunchReadinessSeverity::Blocking,
                    )
                },
                || {
                    reason(
                        LaunchReadinessReasonId::AssetIndexCorrupt,
                        "Asset index is corrupt. Repair this version before launching.",
                        LaunchReadinessSeverity::Blocking,
                    )
                },
                reasons,
            );
        }
    }
}

struct ArtifactVerificationJob {
    path: PathBuf,
    integrity: LibraryVerificationIntegrity,
}

fn verify_artifact_jobs_metadata(
    jobs: Vec<ArtifactVerificationJob>,
) -> Vec<LauncherManagedArtifactReadiness> {
    jobs.into_iter().map(verify_artifact_job_metadata).collect()
}

fn verify_artifact_job_metadata(job: ArtifactVerificationJob) -> LauncherManagedArtifactReadiness {
    let expected_size = match &job.integrity {
        LibraryVerificationIntegrity::Sha1(expected) => {
            if expected
                .sha1
                .as_deref()
                .is_none_or(|sha1| !is_sha1_hex(sha1))
            {
                return LauncherManagedArtifactReadiness::MetadataInvalid;
            }
            expected.size
        }
        LibraryVerificationIntegrity::MissingChecksum => {
            return LauncherManagedArtifactReadiness::MetadataMissing;
        }
    };
    let Ok(metadata) = std::fs::symlink_metadata(&job.path) else {
        return LauncherManagedArtifactReadiness::Missing;
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return LauncherManagedArtifactReadiness::UnsupportedExisting;
    }
    if let Some(expected_size) = expected_size
        && metadata.len() != expected_size
    {
        return LauncherManagedArtifactReadiness::Corrupt;
    }
    LauncherManagedArtifactReadiness::Verified
}

fn inspect_artifact_metadata(
    path: &Path,
    expected: &ExpectedIntegrity,
    missing_reason: impl FnOnce() -> LaunchReadinessReason,
    corrupt_reason: impl FnOnce() -> LaunchReadinessReason,
    reasons: &mut Vec<LaunchReadinessReason>,
) {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        reasons.push(missing_reason());
        return;
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        reasons.push(corrupt_reason());
        return;
    }
    if let Some(expected_size) = expected.size
        && metadata.len() != expected_size
    {
        reasons.push(corrupt_reason());
    }
}

fn is_sha1_hex(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn missing_client_reason() -> LaunchReadinessReason {
    reason(
        LaunchReadinessReasonId::ClientJarMissing,
        "Client game files are missing. Install this version before launching.",
        LaunchReadinessSeverity::Blocking,
    )
}

fn corrupt_client_reason() -> LaunchReadinessReason {
    reason(
        LaunchReadinessReasonId::ClientJarCorrupt,
        "Client game files are corrupt. Repair this version before launching.",
        LaunchReadinessSeverity::Blocking,
    )
}

fn inspect_runtime_files(
    runtime_cache: &ManagedRuntimeCache,
    request: &LaunchReadinessRequest,
    inspection: LaunchReadinessInspection,
    reasons: &mut Vec<LaunchReadinessReason>,
) {
    let selected_override = parse_runtime_override(&request.requested_java);
    if matches!(request.guardian_mode, GuardianMode::Custom) {
        match selected_override {
            RuntimeOverride::ExecutablePath(path) => {
                if !runtime_executable_ready_without_probe(&path) {
                    reasons.push(java_override_missing_reason());
                }
            }
            RuntimeOverride::Component(component) => {
                let ready = match inspection {
                    LaunchReadinessInspection::Summary => {
                        runtime_component_executable_present_without_probe(
                            runtime_cache,
                            component.as_str(),
                        )
                    }
                    LaunchReadinessInspection::Structural => {
                        runtime_component_structurally_ready_without_probe(
                            runtime_cache,
                            component.as_str(),
                        )
                    }
                };
                if !ready {
                    reasons.push(java_override_missing_reason());
                }
            }
            RuntimeOverride::None => {}
        }
    }
}

fn reason_for_version_error(error: &LaunchModelError) -> LaunchReadinessReason {
    if is_missing_parent_version(error) {
        return reason(
            LaunchReadinessReasonId::ParentVersionMissing,
            "Parent version metadata is missing. Install the base version before launching.",
            LaunchReadinessSeverity::Blocking,
        );
    }

    reason(
        LaunchReadinessReasonId::VersionJsonMissing,
        "Installed version metadata is missing. Install this version before launching.",
        LaunchReadinessSeverity::Blocking,
    )
}

fn is_missing_parent_version(error: &LaunchModelError) -> bool {
    match error {
        LaunchModelError::LoadParent { source, .. } => is_missing_version_json(source),
        _ => false,
    }
}

fn is_missing_version_json(error: &LaunchModelError) -> bool {
    match error {
        LaunchModelError::ReadVersion { source, .. } => source.kind() == ErrorKind::NotFound,
        LaunchModelError::LoadParent { source, .. } => is_missing_version_json(source),
        _ => false,
    }
}

fn java_override_missing_reason() -> LaunchReadinessReason {
    reason(
        LaunchReadinessReasonId::JavaOverrideMissing,
        "Selected Java override is unavailable. Choose another Java runtime or use Managed mode.",
        LaunchReadinessSeverity::Blocking,
    )
}

fn reason(
    id: LaunchReadinessReasonId,
    message: &'static str,
    severity: LaunchReadinessSeverity,
) -> LaunchReadinessReason {
    LaunchReadinessReason {
        id,
        severity,
        message,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ArtifactVerificationJob, LaunchReadinessReasonId, LaunchReadinessRequest,
        LaunchReadinessSeverity, inspect_launch_readiness_structural,
        inspect_launch_readiness_summary, verify_artifact_job_metadata,
    };
    use crate::GuardianMode;
    use axial_minecraft::download::{
        LauncherManagedArtifactReadiness, LibraryVerificationIntegrity,
    };
    use sha1::{Digest as _, Sha1};
    use std::fs;
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    #[test]
    fn readiness_rejects_active_publication_promptly() {
        let library_dir = temp_library("active-version-publication");
        let runtime_cache = isolated_runtime_cache();
        let publication_dir = library_dir.join(".axial-publication");
        fs::create_dir_all(&publication_dir).expect("publication directory");
        let lock_file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(publication_dir.join("publication.lock"))
            .expect("publication lock file");
        lock_file.try_lock().expect("exclusive publication lock");

        let started = Instant::now();
        let readiness = inspect_launch_readiness_summary(
            &runtime_cache,
            &LaunchReadinessRequest {
                library_dir: library_dir.clone(),
                version_id: "1.21.1".to_string(),
                requested_java: String::new(),
                guardian_mode: GuardianMode::Managed,
            },
        );
        let elapsed = started.elapsed();

        assert!(
            elapsed < Duration::from_millis(400),
            "active publication admission blocked for {elapsed:?}"
        );
        assert!(!readiness.launchable);
        assert_eq!(readiness.reasons.len(), 1);
        assert_eq!(
            readiness.reasons[0].id,
            LaunchReadinessReasonId::IncompleteInstall
        );
        lock_file.unlock().expect("unlock publication");
        cleanup(&library_dir);
    }

    #[test]
    fn structural_readiness_defers_preferred_managed_runtime_to_tier_zero() {
        let library_dir = temp_library("structural-runtime-tier-zero-authority");
        let runtime_cache = isolated_runtime_cache();
        write_version_json(
            &library_dir,
            "1.21.1",
            r#"{
                "id": "1.21.1",
                "type": "release",
                "mainClass": "net.minecraft.client.main.Main",
                "assetIndex": {},
                "javaVersion": {
                    "component": "axial-test-runtime-missing",
                    "majorVersion": 21
                },
                "libraries": []
            }"#,
        );

        let readiness = inspect_launch_readiness_structural(
            &runtime_cache,
            &LaunchReadinessRequest {
                library_dir: library_dir.clone(),
                version_id: "1.21.1".to_string(),
                requested_java: String::new(),
                guardian_mode: GuardianMode::Managed,
            },
        );

        assert!(readiness.launchable, "{:?}", readiness.reasons);
        assert!(
            readiness
                .reasons
                .iter()
                .all(|reason| reason.id != LaunchReadinessReasonId::ManagedRuntimeMissing)
        );
        cleanup(&library_dir);
    }

    #[test]
    fn custom_component_override_missing_stays_blocking() {
        let library_dir = temp_library("custom-runtime-missing-blocking");
        let runtime_cache = isolated_runtime_cache();
        write_version_json(
            &library_dir,
            "1.21.1",
            r#"{
                "id": "1.21.1",
                "type": "release",
                "mainClass": "net.minecraft.client.main.Main",
                "assetIndex": {},
                "javaVersion": {
                    "component": "java-runtime-delta",
                    "majorVersion": 21
                },
                "libraries": []
            }"#,
        );
        fs::write(
            library_dir
                .join("versions")
                .join("1.21.1")
                .join("1.21.1.jar"),
            b"client jar",
        )
        .expect("write client jar");

        let readiness = inspect_launch_readiness_summary(
            &runtime_cache,
            &LaunchReadinessRequest {
                library_dir: library_dir.clone(),
                version_id: "1.21.1".to_string(),
                requested_java: "axial-test-runtime-missing".to_string(),
                guardian_mode: GuardianMode::Custom,
            },
        );

        assert!(!readiness.launchable);
        let reason = readiness
            .reasons
            .iter()
            .find(|reason| reason.id == LaunchReadinessReasonId::JavaOverrideMissing)
            .expect("custom override reason");
        assert_eq!(reason.severity, LaunchReadinessSeverity::Blocking);
        cleanup(&library_dir);
    }

    #[test]
    fn summary_readiness_does_not_hash_same_size_client_jar() {
        let library_dir = temp_library("corrupt-client-jar");
        let runtime_cache = isolated_runtime_cache();
        let expected_client = b"fresh";
        write_version_json(
            &library_dir,
            "1.21.1",
            &format!(
                r#"{{
                    "id": "1.21.1",
                    "type": "release",
                    "mainClass": "net.minecraft.client.main.Main",
                    "assetIndex": {{}},
                    "downloads": {{
                        "client": {{ "sha1": "{}", "size": {} }}
                    }},
                    "javaVersion": {{
                        "component": "java-runtime-delta",
                        "majorVersion": 21
                    }},
                    "libraries": []
                }}"#,
                sha1_hex(expected_client),
                expected_client.len()
            ),
        );
        fs::write(
            library_dir
                .join("versions")
                .join("1.21.1")
                .join("1.21.1.jar"),
            b"wrong",
        )
        .expect("write corrupt client jar");

        let readiness = inspect_launch_readiness_summary(
            &runtime_cache,
            &LaunchReadinessRequest {
                library_dir: library_dir.clone(),
                version_id: "1.21.1".to_string(),
                requested_java: String::new(),
                guardian_mode: GuardianMode::Managed,
            },
        );

        assert!(readiness.launchable, "{:?}", readiness.reasons);
        assert!(
            readiness
                .reasons
                .iter()
                .all(|reason| reason.id != LaunchReadinessReasonId::ClientJarCorrupt)
        );
        cleanup(&library_dir);
    }

    #[test]
    fn summary_readiness_ignores_same_size_library_content_but_blocks_size_drift() {
        let library_dir = temp_library("corrupt-library");
        let runtime_cache = isolated_runtime_cache();
        let client = b"client";
        let expected_library = b"fresh";
        write_version_json(
            &library_dir,
            "1.21.1",
            &format!(
                r#"{{
                    "id": "1.21.1",
                    "type": "release",
                    "mainClass": "net.minecraft.client.main.Main",
                    "assetIndex": {{}},
                    "downloads": {{
                        "client": {{ "sha1": "{}", "size": {} }}
                    }},
                    "javaVersion": {{
                        "component": "java-runtime-delta",
                        "majorVersion": 21
                    }},
                    "libraries": [{{
                        "name": "com.example:demo:1.0.0",
                        "downloads": {{
                            "artifact": {{
                                "path": "com/example/demo/1.0.0/demo-1.0.0.jar",
                                "url": "https://example.invalid/demo-1.0.0.jar",
                                "sha1": "{}",
                                "size": {}
                            }}
                        }}
                    }}]
                }}"#,
                sha1_hex(client),
                client.len(),
                sha1_hex(expected_library),
                expected_library.len()
            ),
        );
        fs::write(
            library_dir
                .join("versions")
                .join("1.21.1")
                .join("1.21.1.jar"),
            client,
        )
        .expect("write client jar");
        let library_path = library_dir
            .join("libraries")
            .join("com/example/demo/1.0.0/demo-1.0.0.jar");
        fs::create_dir_all(library_path.parent().expect("library parent")).expect("library dir");
        fs::write(&library_path, b"wrong").expect("write corrupt library");

        let readiness = inspect_launch_readiness_summary(
            &runtime_cache,
            &LaunchReadinessRequest {
                library_dir: library_dir.clone(),
                version_id: "1.21.1".to_string(),
                requested_java: String::new(),
                guardian_mode: GuardianMode::Managed,
            },
        );

        assert!(readiness.launchable, "{:?}", readiness.reasons);

        fs::write(&library_path, b"wrong-size").expect("write size-drifted library");
        let summary = inspect_launch_readiness_summary(
            &runtime_cache,
            &LaunchReadinessRequest {
                library_dir: library_dir.clone(),
                version_id: "1.21.1".to_string(),
                requested_java: String::new(),
                guardian_mode: GuardianMode::Managed,
            },
        );
        assert!(!summary.launchable);
        assert!(summary.reasons.iter().any(|reason| {
            reason.id == LaunchReadinessReasonId::LibrariesCorrupt
                && reason.severity == LaunchReadinessSeverity::Blocking
        }));
        cleanup(&library_dir);
    }

    #[test]
    fn url_less_library_is_verified_for_launch_readiness() {
        let library_dir = temp_library("url-less-library-ready");
        let runtime_cache = isolated_runtime_cache();
        let client = b"client";
        let library = b"library";
        write_version_json(
            &library_dir,
            "1.21.1",
            &format!(
                r#"{{
                    "id": "1.21.1",
                    "type": "release",
                    "mainClass": "net.minecraft.client.main.Main",
                    "assetIndex": {{}},
                    "downloads": {{
                        "client": {{ "sha1": "{}", "size": {} }}
                    }},
                    "javaVersion": {{
                        "component": "java-runtime-delta",
                        "majorVersion": 21
                    }},
                    "libraries": [{{
                        "name": "com.example:offline:1.0.0",
                        "downloads": {{
                            "artifact": {{
                                "path": "com/example/offline/1.0.0/offline-1.0.0.jar",
                                "url": "",
                                "sha1": "{}",
                                "size": {}
                            }}
                        }}
                    }}]
                }}"#,
                sha1_hex(client),
                client.len(),
                sha1_hex(library),
                library.len()
            ),
        );
        fs::write(
            library_dir
                .join("versions")
                .join("1.21.1")
                .join("1.21.1.jar"),
            client,
        )
        .expect("write client jar");
        let library_path = library_dir
            .join("libraries")
            .join("com/example/offline/1.0.0/offline-1.0.0.jar");
        fs::create_dir_all(library_path.parent().expect("library parent")).expect("library dir");
        fs::write(&library_path, library).expect("write library");

        let readiness = inspect_launch_readiness_summary(
            &runtime_cache,
            &LaunchReadinessRequest {
                library_dir: library_dir.clone(),
                version_id: "1.21.1".to_string(),
                requested_java: String::new(),
                guardian_mode: GuardianMode::Managed,
            },
        );

        assert!(readiness.launchable, "{:?}", readiness.reasons);
        assert!(!readiness.reasons.iter().any(|reason| {
            matches!(
                reason.id,
                LaunchReadinessReasonId::LibrariesMissing
                    | LaunchReadinessReasonId::LibrariesCorrupt
            )
        }));
        cleanup(&library_dir);
    }

    #[test]
    fn summary_readiness_reports_missing_library_metadata() {
        let library_dir = temp_library("summary-missing-library");
        let runtime_cache = isolated_runtime_cache();
        let client = b"client";
        let expected_library = b"fresh";
        write_version_json(
            &library_dir,
            "1.21.1",
            &format!(
                r#"{{
                    "id": "1.21.1",
                    "type": "release",
                    "mainClass": "net.minecraft.client.main.Main",
                    "assetIndex": {{}},
                    "downloads": {{
                        "client": {{ "sha1": "{}", "size": {} }}
                    }},
                    "libraries": [{{
                        "name": "com.example:demo:1.0.0",
                        "downloads": {{
                            "artifact": {{
                                "path": "com/example/demo/1.0.0/demo-1.0.0.jar",
                                "url": "https://example.invalid/demo-1.0.0.jar",
                                "sha1": "{}",
                                "size": {}
                            }}
                        }}
                    }}]
                }}"#,
                sha1_hex(client),
                client.len(),
                sha1_hex(expected_library),
                expected_library.len()
            ),
        );
        fs::write(
            library_dir
                .join("versions")
                .join("1.21.1")
                .join("1.21.1.jar"),
            client,
        )
        .expect("write client jar");

        let readiness = inspect_launch_readiness_summary(
            &runtime_cache,
            &LaunchReadinessRequest {
                library_dir: library_dir.clone(),
                version_id: "1.21.1".to_string(),
                requested_java: String::new(),
                guardian_mode: GuardianMode::Managed,
            },
        );

        assert!(!readiness.launchable);
        assert!(readiness.reasons.iter().any(|reason| {
            reason.id == LaunchReadinessReasonId::LibrariesMissing
                && reason.severity == LaunchReadinessSeverity::Blocking
        }));
        cleanup(&library_dir);
    }

    #[test]
    fn canonical_loader_profile_cannot_authorize_checksumless_library() {
        let library_dir = temp_library("marked-checksumless-library-readable");
        let runtime_cache = isolated_runtime_cache();
        let client = b"client";
        let version_id = axial_minecraft::installed_version_id_for(
            axial_minecraft::LoaderComponentId::Quilt,
            "1.21.1",
            "0.29.2",
        )
        .expect("valid loader identity");
        write_version_json(
            &library_dir,
            &version_id,
            &format!(
                r#"{{
                    "id": "{version_id}",
                    "inheritsFrom": "1.21.1",
                    "axialMaterialized": true,
                    "type": "release",
                    "mainClass": "org.quiltmc.loader.impl.launch.knot.KnotClient",
                    "assetIndex": {{}},
                    "downloads": {{
                        "client": {{ "sha1": "{}", "size": {} }}
                    }},
                    "libraries": [{{
                        "name": "org.quiltmc:quilt-loader:0.29.2",
                        "url": "https://maven.example.invalid/"
                    }}]
                }}"#,
                sha1_hex(client),
                client.len()
            ),
        );
        fs::write(
            library_dir
                .join("versions")
                .join(&version_id)
                .join(format!("{version_id}.jar")),
            client,
        )
        .expect("write client jar");
        let library_path = library_dir
            .join("libraries")
            .join("org/quiltmc/quilt-loader/0.29.2/quilt-loader-0.29.2.jar");
        fs::create_dir_all(library_path.parent().expect("library parent")).expect("library dir");
        fs::write(
            &library_path,
            zip_bytes(&[("org/quiltmc/loader/impl/QuiltLoader.class", b"loader")]),
        )
        .expect("write readable jar");

        let readiness = inspect_launch_readiness_summary(
            &runtime_cache,
            &LaunchReadinessRequest {
                library_dir: library_dir.clone(),
                version_id,
                requested_java: String::new(),
                guardian_mode: GuardianMode::Managed,
            },
        );

        assert!(!readiness.launchable);
        assert!(readiness.reasons.iter().any(|reason| {
            reason.id == LaunchReadinessReasonId::LibrariesCorrupt
                && reason.severity == LaunchReadinessSeverity::Blocking
        }));
        cleanup(&library_dir);
    }

    #[test]
    fn missing_checksum_is_always_fail_closed() {
        let library_dir = temp_library("missing-checksum-library");
        let _runtime_cache = isolated_runtime_cache();
        let path = library_dir.join("libraries/example.jar");
        fs::create_dir_all(path.parent().expect("library parent")).expect("library directory");
        let bytes = zip_bytes(&[("example/Entry.class", b"entry")]);
        fs::write(&path, &bytes).expect("write readable jar");

        let unauthoritative = ArtifactVerificationJob {
            path,
            integrity: LibraryVerificationIntegrity::MissingChecksum,
        };
        assert_eq!(
            verify_artifact_job_metadata(unauthoritative),
            LauncherManagedArtifactReadiness::MetadataMissing
        );
        cleanup(&library_dir);
    }

    #[test]
    fn non_materialized_loader_profile_is_rejected_before_library_authority() {
        let library_dir = temp_library("non-materialized-checksumless-library");
        let runtime_cache = isolated_runtime_cache();
        let client = b"client";
        let version_id = axial_minecraft::installed_version_id_for(
            axial_minecraft::LoaderComponentId::Quilt,
            "1.21.1",
            "0.29.2",
        )
        .expect("valid loader identity");
        write_version_json(
            &library_dir,
            &version_id,
            &format!(
                r#"{{
                    "id": "{version_id}",
                    "type": "release",
                    "mainClass": "org.quiltmc.loader.impl.launch.knot.KnotClient",
                    "assetIndex": {{}},
                    "downloads": {{
                        "client": {{ "sha1": "{}", "size": {} }}
                    }},
                    "libraries": [{{
                        "name": "org.quiltmc:quilt-loader:0.29.2",
                        "url": "https://maven.example.invalid/"
                    }}]
                }}"#,
                sha1_hex(client),
                client.len()
            ),
        );
        fs::write(
            library_dir
                .join("versions")
                .join(&version_id)
                .join(format!("{version_id}.jar")),
            client,
        )
        .expect("write client jar");
        let library_path = library_dir
            .join("libraries")
            .join("org/quiltmc/quilt-loader/0.29.2/quilt-loader-0.29.2.jar");
        fs::create_dir_all(library_path.parent().expect("library parent")).expect("library dir");
        fs::write(
            &library_path,
            zip_bytes(&[("org/quiltmc/loader/impl/QuiltLoader.class", b"loader")]),
        )
        .expect("write readable jar");

        let readiness = inspect_launch_readiness_summary(
            &runtime_cache,
            &LaunchReadinessRequest {
                library_dir: library_dir.clone(),
                version_id,
                requested_java: String::new(),
                guardian_mode: GuardianMode::Managed,
            },
        );

        assert!(!readiness.launchable);
        assert_eq!(readiness.reasons.len(), 1);
        assert_eq!(
            readiness.reasons[0].id,
            LaunchReadinessReasonId::VersionJsonMissing
        );
        assert_eq!(
            readiness.reasons[0].severity,
            LaunchReadinessSeverity::Blocking
        );
        cleanup(&library_dir);
    }

    #[test]
    fn summary_readiness_ignores_same_size_asset_index_content_but_blocks_size_drift() {
        let library_dir = temp_library("corrupt-asset-index");
        let runtime_cache = isolated_runtime_cache();
        let client = b"client";
        let expected_asset_index = b"fresh";
        write_version_json(
            &library_dir,
            "1.21.1",
            &format!(
                r#"{{
                    "id": "1.21.1",
                    "type": "release",
                    "mainClass": "net.minecraft.client.main.Main",
                    "assetIndex": {{
                        "id": "test-assets",
                        "sha1": "{}",
                        "size": {}
                    }},
                    "downloads": {{
                        "client": {{ "sha1": "{}", "size": {} }}
                    }},
                    "javaVersion": {{
                        "component": "java-runtime-delta",
                        "majorVersion": 21
                    }},
                    "libraries": []
                }}"#,
                sha1_hex(expected_asset_index),
                expected_asset_index.len(),
                sha1_hex(client),
                client.len()
            ),
        );
        fs::write(
            library_dir
                .join("versions")
                .join("1.21.1")
                .join("1.21.1.jar"),
            client,
        )
        .expect("write client jar");
        let asset_index_path = library_dir
            .join("assets")
            .join("indexes")
            .join("test-assets.json");
        fs::create_dir_all(asset_index_path.parent().expect("asset parent"))
            .expect("asset index dir");
        fs::write(&asset_index_path, b"wrong").expect("write corrupt asset index");

        let readiness = inspect_launch_readiness_summary(
            &runtime_cache,
            &LaunchReadinessRequest {
                library_dir: library_dir.clone(),
                version_id: "1.21.1".to_string(),
                requested_java: String::new(),
                guardian_mode: GuardianMode::Managed,
            },
        );

        assert!(readiness.launchable, "{:?}", readiness.reasons);

        fs::write(&asset_index_path, b"wrong-size").expect("write size-drifted asset index");
        let readiness = inspect_launch_readiness_summary(
            &runtime_cache,
            &LaunchReadinessRequest {
                library_dir: library_dir.clone(),
                version_id: "1.21.1".to_string(),
                requested_java: String::new(),
                guardian_mode: GuardianMode::Managed,
            },
        );
        assert!(!readiness.launchable);
        assert!(readiness.reasons.iter().any(|reason| {
            reason.id == LaunchReadinessReasonId::AssetIndexCorrupt
                && reason.severity == LaunchReadinessSeverity::Blocking
        }));
        cleanup(&library_dir);
    }

    #[test]
    fn summary_readiness_does_not_walk_asset_objects() {
        let library_dir = temp_library("missing-asset-object");
        let runtime_cache = isolated_runtime_cache();
        let asset = b"asset";
        write_asset_version_fixture(&library_dir, asset, false);
        let request = LaunchReadinessRequest {
            library_dir: library_dir.clone(),
            version_id: "asset-version".to_string(),
            requested_java: String::new(),
            guardian_mode: GuardianMode::Managed,
        };

        let summary = inspect_launch_readiness_summary(&runtime_cache, &request);

        assert!(
            summary.launchable,
            "summary readiness must not walk every asset object: {:?}",
            summary.reasons
        );
        assert!(summary.reasons.iter().all(|reason| !matches!(
            reason.id,
            LaunchReadinessReasonId::AssetIndexMissing | LaunchReadinessReasonId::AssetIndexCorrupt
        )));
        cleanup(&library_dir);
    }

    fn temp_library(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "axial-launcher-readiness-{name}-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&root).expect("create temp library");
        root
    }

    fn isolated_runtime_cache() -> axial_minecraft::ManagedRuntimeCache {
        axial_minecraft::ManagedRuntimeCache::isolated_for_test()
            .expect("isolated managed runtime cache")
    }

    fn write_version_json(library_dir: &Path, version_id: &str, json: &str) {
        let version_dir = library_dir.join("versions").join(version_id);
        fs::create_dir_all(&version_dir).expect("version dir");
        fs::write(version_dir.join(format!("{version_id}.json")), json).expect("version json");
    }

    fn write_asset_version_fixture(library_dir: &Path, asset: &[u8], legacy: bool) {
        let client = b"client";
        let hash = sha1_hex(asset);
        let asset_index = format!(
            r#"{{
                "objects": {{
                    "sounds/step.ogg": {{ "hash": "{hash}", "size": {} }}
                }},
                "virtual": {legacy}
            }}"#,
            asset.len()
        );
        let asset_index_path = library_dir
            .join("assets")
            .join("indexes")
            .join("test-assets.json");
        fs::create_dir_all(asset_index_path.parent().expect("asset parent"))
            .expect("asset index dir");
        fs::write(&asset_index_path, &asset_index).expect("write asset index");
        write_version_json(
            library_dir,
            "asset-version",
            &format!(
                r#"{{
                    "id": "asset-version",
                    "type": "release",
                    "mainClass": "net.minecraft.client.main.Main",
                    "assetIndex": {{
                        "id": "test-assets",
                        "sha1": "{}",
                        "size": {}
                    }},
                    "downloads": {{
                        "client": {{ "sha1": "{}", "size": {} }}
                    }},
                    "javaVersion": {{
                        "component": "java-runtime-delta",
                        "majorVersion": 21
                    }},
                    "libraries": []
                }}"#,
                sha1_hex(asset_index.as_bytes()),
                asset_index.len(),
                sha1_hex(client),
                client.len()
            ),
        );
        fs::write(
            library_dir
                .join("versions")
                .join("asset-version")
                .join("asset-version.jar"),
            client,
        )
        .expect("write client jar");
    }

    fn sha1_hex(bytes: &[u8]) -> String {
        let mut hasher = Sha1::new();
        hasher.update(bytes);
        format!("{:x}", hasher.finalize())
    }

    fn zip_bytes(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut cursor = std::io::Cursor::new(Vec::new());
        {
            let mut archive = zip::ZipWriter::new(&mut cursor);
            for (name, bytes) in entries {
                archive
                    .start_file(name, zip::write::SimpleFileOptions::default())
                    .expect("start zip entry");
                archive.write_all(bytes).expect("write zip entry");
            }
            archive.finish().expect("finish zip");
        }
        cursor.into_inner()
    }

    fn cleanup(path: &Path) {
        let _ = fs::remove_dir_all(path);
    }
}
