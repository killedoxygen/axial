use crate::GuardianMode;
use crate::build::find_client_jar;
use croopor_minecraft::download::{
    ExpectedIntegrity, LauncherManagedArtifactReadiness, asset_object_hash_prefix,
    jar_contains_signed_metadata, library_jobs_for, verify_existing_launcher_managed_artifact,
    verify_existing_launcher_managed_artifact_allowing_missing_checksum,
};
use croopor_minecraft::paths::assets_dir;
use croopor_minecraft::{
    LaunchModelError, RuntimeOverride, VersionJson, default_environment, load_version_json,
    parse_runtime_override, preferred_runtime_component, resolve_version,
    runtime_component_executable_present_without_probe, runtime_component_ready_without_probe,
    runtime_executable_ready_without_probe,
};
use serde::Deserialize;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LaunchReadinessReasonId {
    InstalledVersionsDegraded,
    VersionJsonMissing,
    ParentVersionMissing,
    IncompleteInstall,
    ClientJarMissing,
    ClientJarCorrupt,
    LibrariesMissing,
    LibrariesCorrupt,
    LauncherManagedArtifactSignatureCorrupt,
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

pub fn inspect_launch_readiness(request: &LaunchReadinessRequest) -> LaunchReadiness {
    inspect_launch_readiness_with_depth(request, LaunchReadinessInspection::Full)
}

pub fn inspect_launch_readiness_summary(request: &LaunchReadinessRequest) -> LaunchReadiness {
    inspect_launch_readiness_with_depth(request, LaunchReadinessInspection::Summary)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LaunchReadinessInspection {
    Full,
    Summary,
}

fn inspect_launch_readiness_with_depth(
    request: &LaunchReadinessRequest,
    inspection: LaunchReadinessInspection,
) -> LaunchReadiness {
    let mut reasons = Vec::new();
    inspect_incomplete_install_markers(&request.library_dir, &request.version_id, &mut reasons);

    let version = match resolve_version(&request.library_dir, &request.version_id) {
        Ok(version) => {
            inspect_version_files(
                &request.library_dir,
                &request.version_id,
                &version,
                inspection,
                &mut reasons,
            );
            Some(version)
        }
        Err(error) => {
            reasons.push(reason_for_version_error(&error));
            None
        }
    };

    inspect_runtime_files(request, version.as_ref(), inspection, &mut reasons);

    LaunchReadiness {
        launchable: reasons
            .iter()
            .all(|reason| reason.severity != LaunchReadinessSeverity::Blocking),
        reasons,
    }
}

fn inspect_incomplete_install_markers(
    library_dir: &Path,
    version_id: &str,
    reasons: &mut Vec<LaunchReadinessReason>,
) {
    let mut current_id = version_id.trim().to_string();
    let mut seen = HashSet::new();
    let mut depth = 0;

    while !current_id.is_empty() && seen.insert(current_id.clone()) && depth <= 10 {
        let version_dir = library_dir.join("versions").join(&current_id);
        if version_dir.join(".incomplete").exists() {
            reasons.push(reason(
                LaunchReadinessReasonId::IncompleteInstall,
                "Installation is incomplete. Finish or repair this version before launching.",
                LaunchReadinessSeverity::Blocking,
            ));
            return;
        }

        let Ok(version) = load_version_json(library_dir, &current_id) else {
            return;
        };
        current_id = version.inherits_from.trim().to_string();
        depth += 1;
    }
}

fn inspect_version_files(
    library_dir: &Path,
    version_id: &str,
    version: &VersionJson,
    inspection: LaunchReadinessInspection,
    reasons: &mut Vec<LaunchReadinessReason>,
) {
    match find_client_jar(library_dir, version, version_id) {
        Some(client_jar) => {
            let client_signature_corrupt = inspection == LaunchReadinessInspection::Full
                && legacy_forge_client_jar_has_signed_metadata(
                    library_dir,
                    version_id,
                    version,
                    &client_jar,
                );
            match version.downloads.client.as_ref() {
                Some(entry) if inspection == LaunchReadinessInspection::Summary => {
                    let expected = ExpectedIntegrity::from_mojang(entry.size, &entry.sha1);
                    inspect_artifact_metadata(
                        &client_jar,
                        &expected,
                        missing_client_reason,
                        corrupt_client_reason,
                        reasons,
                    );
                }
                Some(entry) => match verify_existing_launcher_managed_artifact(
                    &client_jar,
                    &ExpectedIntegrity::from_mojang(entry.size, &entry.sha1),
                ) {
                    LauncherManagedArtifactReadiness::Verified if client_signature_corrupt => {
                        push_signature_corrupt_reason(reasons);
                    }
                    LauncherManagedArtifactReadiness::Verified => {}
                    LauncherManagedArtifactReadiness::Missing => {
                        reasons.push(missing_client_reason());
                    }
                    _ if client_signature_corrupt => {
                        push_signature_corrupt_reason(reasons);
                    }
                    _ => {
                        reasons.push(corrupt_client_reason());
                    }
                },
                None if client_signature_corrupt => {
                    push_signature_corrupt_reason(reasons);
                }
                None => {}
            }
        }
        None => {
            reasons.push(missing_client_reason());
        }
    }

    let library_jobs: Vec<ArtifactVerificationJob> =
        library_jobs_for(library_dir, &version.libraries, &default_environment())
            .into_iter()
            .map(|library| ArtifactVerificationJob {
                path: library.path,
                name: Some(library.name),
                expected: library.expected,
                allow_missing_checksum: library.allow_missing_checksum,
            })
            .collect();
    let library_signature_corrupt = inspection == LaunchReadinessInspection::Full
        && legacy_forge_libraries_have_signed_metadata(
            library_dir,
            version_id,
            version,
            &library_jobs,
        );
    let library_readiness = match inspection {
        LaunchReadinessInspection::Summary => verify_artifact_jobs_metadata(library_jobs),
        LaunchReadinessInspection::Full => verify_artifact_jobs(library_jobs),
    };
    let libraries_missing = readiness_contains(
        &library_readiness,
        LauncherManagedArtifactReadiness::Missing,
    );
    let libraries_corrupt = library_readiness
        .iter()
        .any(|status| *status != LauncherManagedArtifactReadiness::Verified);
    if libraries_missing {
        reasons.push(reason(
            LaunchReadinessReasonId::LibrariesMissing,
            "Required libraries are missing. Install this version before launching.",
            LaunchReadinessSeverity::Blocking,
        ));
    } else if library_signature_corrupt {
        push_signature_corrupt_reason(reasons);
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
            match inspection {
                LaunchReadinessInspection::Summary => {
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
                LaunchReadinessInspection::Full => {
                    match verify_existing_launcher_managed_artifact(&asset_index_path, &expected) {
                        LauncherManagedArtifactReadiness::Verified => {
                            inspect_asset_object_files(library_dir, &asset_index_path, reasons);
                        }
                        LauncherManagedArtifactReadiness::Missing => reasons.push(reason(
                            LaunchReadinessReasonId::AssetIndexMissing,
                            "Asset index is missing. Install this version before launching.",
                            LaunchReadinessSeverity::Blocking,
                        )),
                        _ => reasons.push(reason(
                            LaunchReadinessReasonId::AssetIndexCorrupt,
                            "Asset index is corrupt. Repair this version before launching.",
                            LaunchReadinessSeverity::Blocking,
                        )),
                    }
                }
            }
        }
    }
}

#[derive(Clone)]
struct ArtifactVerificationJob {
    path: PathBuf,
    name: Option<String>,
    expected: ExpectedIntegrity,
    allow_missing_checksum: bool,
}

fn verify_artifact_jobs(
    jobs: Vec<ArtifactVerificationJob>,
) -> Vec<LauncherManagedArtifactReadiness> {
    if jobs.is_empty() {
        return Vec::new();
    }

    let worker_count = std::thread::available_parallelism()
        .map(|value| value.get())
        .unwrap_or(2)
        .saturating_mul(2)
        .clamp(2, 16)
        .min(jobs.len());
    if worker_count <= 1 {
        return jobs.into_iter().map(verify_artifact_job).collect();
    }

    let chunk_size = jobs.len().div_ceil(worker_count);
    let handles = jobs
        .chunks(chunk_size)
        .map(|chunk| {
            let chunk = chunk.to_vec();
            std::thread::spawn(move || {
                chunk
                    .into_iter()
                    .map(verify_artifact_job)
                    .collect::<Vec<_>>()
            })
        })
        .collect::<Vec<_>>();

    handles
        .into_iter()
        .flat_map(|handle| handle.join().unwrap_or_default())
        .collect()
}

fn verify_artifact_job(job: ArtifactVerificationJob) -> LauncherManagedArtifactReadiness {
    if job.allow_missing_checksum {
        return verify_existing_launcher_managed_artifact_allowing_missing_checksum(
            &job.path,
            &job.expected,
        );
    }
    verify_existing_launcher_managed_artifact(&job.path, &job.expected)
}

fn verify_artifact_jobs_metadata(
    jobs: Vec<ArtifactVerificationJob>,
) -> Vec<LauncherManagedArtifactReadiness> {
    jobs.into_iter().map(verify_artifact_job_metadata).collect()
}

fn verify_artifact_job_metadata(job: ArtifactVerificationJob) -> LauncherManagedArtifactReadiness {
    if job.expected.sha1.is_none() && !job.allow_missing_checksum {
        return LauncherManagedArtifactReadiness::MetadataMissing;
    }
    if let Some(expected_sha1) = job.expected.sha1.as_deref()
        && !is_sha1_hex(expected_sha1)
    {
        return LauncherManagedArtifactReadiness::MetadataInvalid;
    }
    let Ok(metadata) = std::fs::symlink_metadata(&job.path) else {
        return LauncherManagedArtifactReadiness::Missing;
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return LauncherManagedArtifactReadiness::UnsupportedExisting;
    }
    if job.allow_missing_checksum && metadata.len() == 0 {
        return LauncherManagedArtifactReadiness::Corrupt;
    }
    if let Some(expected_size) = job.expected.size
        && metadata.len() != expected_size
    {
        return LauncherManagedArtifactReadiness::Corrupt;
    }
    LauncherManagedArtifactReadiness::Verified
}

fn readiness_contains(
    statuses: &[LauncherManagedArtifactReadiness],
    needle: LauncherManagedArtifactReadiness,
) -> bool {
    statuses.contains(&needle)
}

fn inspect_artifact_metadata(
    path: &Path,
    expected: &ExpectedIntegrity,
    missing_reason: impl FnOnce() -> LaunchReadinessReason,
    corrupt_reason: impl FnOnce() -> LaunchReadinessReason,
    reasons: &mut Vec<LaunchReadinessReason>,
) -> bool {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        reasons.push(missing_reason());
        return true;
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        reasons.push(corrupt_reason());
        return true;
    }
    if let Some(expected_size) = expected.size
        && metadata.len() != expected_size
    {
        reasons.push(corrupt_reason());
        return true;
    }
    false
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

fn signature_corrupt_reason() -> LaunchReadinessReason {
    reason(
        LaunchReadinessReasonId::LauncherManagedArtifactSignatureCorrupt,
        "Launcher-managed jar signatures are inconsistent. Repair this version before launching.",
        LaunchReadinessSeverity::Blocking,
    )
}

fn push_signature_corrupt_reason(reasons: &mut Vec<LaunchReadinessReason>) {
    if !reasons
        .iter()
        .any(|reason| reason.id == LaunchReadinessReasonId::LauncherManagedArtifactSignatureCorrupt)
    {
        reasons.push(signature_corrupt_reason());
    }
}

fn legacy_forge_client_jar_has_signed_metadata(
    library_dir: &Path,
    version_id: &str,
    version: &VersionJson,
    client_jar: &Path,
) -> bool {
    legacy_forge_artifacts_must_be_unsigned(library_dir, version_id, version)
        && child_version_jar(library_dir, version_id, client_jar)
        && jar_contains_signed_metadata(client_jar)
}

fn legacy_forge_libraries_have_signed_metadata(
    library_dir: &Path,
    version_id: &str,
    version: &VersionJson,
    jobs: &[ArtifactVerificationJob],
) -> bool {
    if !legacy_forge_artifacts_must_be_unsigned(library_dir, version_id, version) {
        return false;
    }
    jobs.iter().any(|job| {
        legacy_forge_library_job_requires_unsigned_metadata(job)
            && jar_contains_signed_metadata(&job.path)
    })
}

fn child_version_jar(library_dir: &Path, version_id: &str, jar: &Path) -> bool {
    if version_id.trim().is_empty() {
        return false;
    }
    jar.strip_prefix(library_dir.join("versions").join(version_id))
        .is_ok()
}

#[derive(Deserialize)]
struct ReadinessLoaderMetadata {
    #[serde(default)]
    component_id: String,
    #[serde(default)]
    minecraft_version: String,
}

fn legacy_forge_artifacts_must_be_unsigned(
    library_dir: &Path,
    version_id: &str,
    version: &VersionJson,
) -> bool {
    let metadata = read_loader_metadata(library_dir, version_id);
    let is_forge = metadata
        .as_ref()
        .is_some_and(|metadata| metadata.component_id == "net.minecraftforge")
        || version
            .libraries
            .iter()
            .any(|library| library.name.starts_with("net.minecraftforge:"))
        || version
            .main_class
            .to_ascii_lowercase()
            .contains("launchwrapper");
    if !is_forge {
        return false;
    }

    metadata.as_ref().is_some_and(|metadata| {
        minecraft_version_requires_unsigned_legacy_forge_artifacts(&metadata.minecraft_version)
    }) || minecraft_version_requires_unsigned_legacy_forge_artifacts(&version.inherits_from)
        || minecraft_version_requires_unsigned_legacy_forge_artifacts(&version.id)
}

fn read_loader_metadata(library_dir: &Path, version_id: &str) -> Option<ReadinessLoaderMetadata> {
    let path = library_dir
        .join("versions")
        .join(version_id)
        .join(".croopor-loader.json");
    let data = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&data).ok()
}

fn legacy_forge_library_requires_unsigned_metadata(name: &str) -> bool {
    name.starts_with("net.minecraftforge:minecraftforge:")
        || name.starts_with("net.minecraftforge:forge:")
}

fn legacy_forge_library_job_requires_unsigned_metadata(job: &ArtifactVerificationJob) -> bool {
    job.name
        .as_deref()
        .is_some_and(legacy_forge_library_requires_unsigned_metadata)
        || legacy_forge_library_path_requires_unsigned_metadata(&job.path)
}

fn legacy_forge_library_path_requires_unsigned_metadata(path: &Path) -> bool {
    let normalized = path.to_string_lossy().replace('\\', "/");
    normalized.contains("/net/minecraftforge/minecraftforge/")
        || normalized.contains("/net/minecraftforge/forge/")
}

fn minecraft_version_requires_unsigned_legacy_forge_artifacts(value: &str) -> bool {
    let value = value.trim();
    if value.is_empty() {
        return false;
    }
    if matches!(value.as_bytes().first(), Some(b'a' | b'b')) {
        return true;
    }
    let numbers = value
        .split(|ch: char| !ch.is_ascii_digit())
        .filter(|part| !part.is_empty())
        .filter_map(|part| part.parse::<u32>().ok())
        .collect::<Vec<_>>();
    matches!(numbers.as_slice(), [1, minor, ..] if *minor <= 6)
}

#[derive(Deserialize)]
struct ReadinessAssetIndex {
    objects: HashMap<String, ReadinessAssetObject>,
}

#[derive(Deserialize)]
struct ReadinessAssetObject {
    hash: String,
    #[serde(default)]
    size: i64,
}

fn inspect_asset_object_files(
    library_dir: &Path,
    asset_index_path: &Path,
    reasons: &mut Vec<LaunchReadinessReason>,
) {
    let index = match std::fs::read_to_string(asset_index_path)
        .ok()
        .and_then(|data| serde_json::from_str::<ReadinessAssetIndex>(&data).ok())
    {
        Some(index) => index,
        None => {
            reasons.push(reason(
                LaunchReadinessReasonId::AssetIndexCorrupt,
                "Asset index is corrupt. Repair this version before launching.",
                LaunchReadinessSeverity::Blocking,
            ));
            return;
        }
    };

    let objects_dir = assets_dir(library_dir).join("objects");
    let mut checked_hashes = HashSet::new();
    let mut object_jobs = Vec::new();
    for object in index.objects.values() {
        if !checked_hashes.insert(object.hash.clone()) {
            continue;
        }
        let Ok(prefix) = asset_object_hash_prefix(&object.hash) else {
            reasons.push(asset_corrupt_reason());
            return;
        };
        let path = objects_dir.join(prefix).join(&object.hash);
        let expected = ExpectedIntegrity::from_mojang(object.size, &object.hash);
        object_jobs.push(ArtifactVerificationJob {
            path,
            name: None,
            expected,
            allow_missing_checksum: false,
        });
    }
    let object_readiness = verify_artifact_jobs(object_jobs);
    if readiness_contains(&object_readiness, LauncherManagedArtifactReadiness::Missing) {
        reasons.push(asset_missing_reason());
        return;
    }
    if object_readiness
        .iter()
        .any(|status| *status != LauncherManagedArtifactReadiness::Verified)
    {
        reasons.push(asset_corrupt_reason());
    }

    // Legacy virtual assets are a derived shared view over the verified object store.
    // Different legacy versions reuse names with different content, so stale copies
    // must be repaired before launch rather than treated as authoritative readiness.
}

fn asset_missing_reason() -> LaunchReadinessReason {
    reason(
        LaunchReadinessReasonId::AssetIndexMissing,
        "Game assets are missing. Install this version before launching.",
        LaunchReadinessSeverity::Blocking,
    )
}

fn asset_corrupt_reason() -> LaunchReadinessReason {
    reason(
        LaunchReadinessReasonId::AssetIndexCorrupt,
        "Game assets are corrupt. Repair this version before launching.",
        LaunchReadinessSeverity::Blocking,
    )
}

fn inspect_runtime_files(
    request: &LaunchReadinessRequest,
    version: Option<&VersionJson>,
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
                return;
            }
            RuntimeOverride::Component(component) => {
                let ready = match inspection {
                    LaunchReadinessInspection::Summary => {
                        runtime_component_executable_present_without_probe(
                            &request.library_dir,
                            component.as_str(),
                        )
                    }
                    LaunchReadinessInspection::Full => runtime_component_ready_without_probe(
                        &request.library_dir,
                        component.as_str(),
                    ),
                };
                if !ready {
                    reasons.push(java_override_missing_reason());
                }
                return;
            }
            RuntimeOverride::None => {}
        }
    }

    let Some(version) = version else {
        return;
    };
    if inspection == LaunchReadinessInspection::Summary {
        return;
    }
    let component = preferred_runtime_component(&version.java_version);
    if !runtime_component_ready_without_probe(&request.library_dir, &component) {
        reasons.push(reason(
            LaunchReadinessReasonId::ManagedRuntimeMissing,
            "Managed Java runtime is missing and will be prepared before launch.",
            LaunchReadinessSeverity::Recoverable,
        ));
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
        LaunchReadinessReasonId, LaunchReadinessRequest, LaunchReadinessSeverity,
        inspect_launch_readiness, inspect_launch_readiness_summary,
    };
    use crate::GuardianMode;
    use sha1::{Digest as _, Sha1};
    use std::fs;
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn managed_runtime_missing_is_recoverable_in_managed_mode() {
        let library_dir = temp_library("managed-runtime-missing-recoverable");
        write_version_json(
            &library_dir,
            "1.21.1",
            r#"{
                "id": "1.21.1",
                "type": "release",
                "mainClass": "net.minecraft.client.main.Main",
                "assetIndex": {},
                "javaVersion": {
                    "component": "croopor-test-runtime-missing",
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

        let readiness = inspect_launch_readiness(&LaunchReadinessRequest {
            library_dir: library_dir.clone(),
            version_id: "1.21.1".to_string(),
            requested_java: String::new(),
            guardian_mode: GuardianMode::Managed,
        });

        assert!(readiness.launchable, "{:?}", readiness.reasons);
        let reason = readiness
            .reasons
            .iter()
            .find(|reason| reason.id == LaunchReadinessReasonId::ManagedRuntimeMissing)
            .expect("managed runtime reason");
        assert_eq!(reason.severity, LaunchReadinessSeverity::Recoverable);
        cleanup(&library_dir);
    }

    #[test]
    fn custom_component_override_missing_stays_blocking() {
        let library_dir = temp_library("custom-runtime-missing-blocking");
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

        let readiness = inspect_launch_readiness(&LaunchReadinessRequest {
            library_dir: library_dir.clone(),
            version_id: "1.21.1".to_string(),
            requested_java: "croopor-test-runtime-missing".to_string(),
            guardian_mode: GuardianMode::Custom,
        });

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
    fn corrupt_client_jar_blocks_launch_readiness() {
        let library_dir = temp_library("corrupt-client-jar");
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

        let readiness = inspect_launch_readiness(&LaunchReadinessRequest {
            library_dir: library_dir.clone(),
            version_id: "1.21.1".to_string(),
            requested_java: String::new(),
            guardian_mode: GuardianMode::Managed,
        });

        assert!(!readiness.launchable);
        assert!(readiness.reasons.iter().any(|reason| {
            reason.id == LaunchReadinessReasonId::ClientJarCorrupt
                && reason.severity == LaunchReadinessSeverity::Blocking
        }));
        cleanup(&library_dir);
    }

    #[test]
    fn corrupt_library_blocks_launch_readiness() {
        let library_dir = temp_library("corrupt-library");
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

        let readiness = inspect_launch_readiness(&LaunchReadinessRequest {
            library_dir: library_dir.clone(),
            version_id: "1.21.1".to_string(),
            requested_java: String::new(),
            guardian_mode: GuardianMode::Managed,
        });

        assert!(!readiness.launchable);
        assert!(readiness.reasons.iter().any(|reason| {
            reason.id == LaunchReadinessReasonId::LibrariesCorrupt
                && reason.severity == LaunchReadinessSeverity::Blocking
        }));
        cleanup(&library_dir);
    }

    #[test]
    fn summary_readiness_reports_missing_library_metadata() {
        let library_dir = temp_library("summary-missing-library");
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

        let readiness = inspect_launch_readiness_summary(&LaunchReadinessRequest {
            library_dir: library_dir.clone(),
            version_id: "1.21.1".to_string(),
            requested_java: String::new(),
            guardian_mode: GuardianMode::Managed,
        });

        assert!(!readiness.launchable);
        assert!(readiness.reasons.iter().any(|reason| {
            reason.id == LaunchReadinessReasonId::LibrariesMissing
                && reason.severity == LaunchReadinessSeverity::Blocking
        }));
        cleanup(&library_dir);
    }

    #[test]
    fn marked_checksumless_library_allows_readable_jar() {
        let library_dir = temp_library("marked-checksumless-library-readable");
        let client = b"client";
        write_version_json(
            &library_dir,
            "quilt-loader-test",
            &format!(
                r#"{{
                    "id": "quilt-loader-test",
                    "type": "release",
                    "mainClass": "org.quiltmc.loader.impl.launch.knot.KnotClient",
                    "assetIndex": {{}},
                    "downloads": {{
                        "client": {{ "sha1": "{}", "size": {} }}
                    }},
                    "libraries": [{{
                        "name": "org.quiltmc:quilt-loader:0.29.2",
                        "url": "https://maven.example.invalid/",
                        "crooporChecksumlessAllowed": true
                    }}]
                }}"#,
                sha1_hex(client),
                client.len()
            ),
        );
        fs::write(
            library_dir
                .join("versions")
                .join("quilt-loader-test")
                .join("quilt-loader-test.jar"),
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

        let readiness = inspect_launch_readiness(&LaunchReadinessRequest {
            library_dir: library_dir.clone(),
            version_id: "quilt-loader-test".to_string(),
            requested_java: String::new(),
            guardian_mode: GuardianMode::Managed,
        });

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
    fn marked_checksumless_library_rejects_unreadable_jar() {
        let library_dir = temp_library("marked-checksumless-library-unreadable");
        let client = b"client";
        write_version_json(
            &library_dir,
            "quilt-loader-test",
            &format!(
                r#"{{
                    "id": "quilt-loader-test",
                    "type": "release",
                    "mainClass": "org.quiltmc.loader.impl.launch.knot.KnotClient",
                    "assetIndex": {{}},
                    "downloads": {{
                        "client": {{ "sha1": "{}", "size": {} }}
                    }},
                    "libraries": [{{
                        "name": "org.quiltmc:quilt-loader:0.29.2",
                        "url": "https://maven.example.invalid/",
                        "crooporChecksumlessAllowed": true
                    }}]
                }}"#,
                sha1_hex(client),
                client.len()
            ),
        );
        fs::write(
            library_dir
                .join("versions")
                .join("quilt-loader-test")
                .join("quilt-loader-test.jar"),
            client,
        )
        .expect("write client jar");
        let library_path = library_dir
            .join("libraries")
            .join("org/quiltmc/quilt-loader/0.29.2/quilt-loader-0.29.2.jar");
        fs::create_dir_all(library_path.parent().expect("library parent")).expect("library dir");
        fs::write(&library_path, b"not a jar").expect("write unreadable jar");

        let readiness = inspect_launch_readiness(&LaunchReadinessRequest {
            library_dir: library_dir.clone(),
            version_id: "quilt-loader-test".to_string(),
            requested_java: String::new(),
            guardian_mode: GuardianMode::Managed,
        });

        assert!(!readiness.launchable);
        assert!(readiness.reasons.iter().any(|reason| {
            reason.id == LaunchReadinessReasonId::LibrariesCorrupt
                && reason.severity == LaunchReadinessSeverity::Blocking
        }));
        cleanup(&library_dir);
    }

    #[test]
    fn signed_legacy_forge_child_client_blocks_launch_readiness() {
        let library_dir = temp_library("signed-legacy-forge-child-client");
        let version_id = "1.5.2-forge-7.8.1.738";
        let signed_client = zip_bytes(&[
            ("META-INF/MANIFEST.MF", b"signed manifest"),
            ("META-INF/MOJANG_C.SF", b"signature"),
            ("net/minecraft/client/Minecraft.class", b"class"),
        ]);
        write_version_json(
            &library_dir,
            version_id,
            &format!(
                r#"{{
                    "id": "{version_id}",
                    "type": "release",
                    "mainClass": "net.minecraft.launchwrapper.Launch",
                    "assetIndex": {{}},
                    "downloads": {{
                        "client": {{ "sha1": "{}", "size": {}, "url": "" }}
                    }},
                    "libraries": []
                }}"#,
                sha1_hex(&signed_client),
                signed_client.len()
            ),
        );
        let version_dir = library_dir.join("versions").join(version_id);
        fs::write(
            version_dir.join(format!("{version_id}.jar")),
            &signed_client,
        )
        .expect("write signed child jar");

        let readiness = inspect_launch_readiness(&LaunchReadinessRequest {
            library_dir: library_dir.clone(),
            version_id: version_id.to_string(),
            requested_java: String::new(),
            guardian_mode: GuardianMode::Managed,
        });

        assert!(!readiness.launchable);
        assert!(readiness.reasons.iter().any(|reason| {
            reason.id == LaunchReadinessReasonId::LauncherManagedArtifactSignatureCorrupt
                && reason.severity == LaunchReadinessSeverity::Blocking
        }));
        assert!(
            !readiness
                .reasons
                .iter()
                .any(|reason| reason.id == LaunchReadinessReasonId::ClientJarCorrupt)
        );
        cleanup(&library_dir);
    }

    #[test]
    fn signed_legacy_forge_child_client_prefers_signature_reason_over_checksum_mismatch() {
        let library_dir = temp_library("signed-legacy-forge-child-client-mismatched-checksum");
        let version_id = "1.5.2-forge-7.8.1.738";
        let expected_client = b"fresh";
        let signed_client = zip_bytes(&[
            ("META-INF/MANIFEST.MF", b"signed manifest"),
            ("META-INF/MOJANG_C.SF", b"signature"),
            ("net/minecraft/client/Minecraft.class", b"class"),
        ]);
        write_version_json(
            &library_dir,
            version_id,
            &format!(
                r#"{{
                    "id": "{version_id}",
                    "type": "release",
                    "mainClass": "net.minecraft.launchwrapper.Launch",
                    "assetIndex": {{}},
                    "downloads": {{
                        "client": {{ "sha1": "{}", "size": {}, "url": "" }}
                    }},
                    "libraries": []
                }}"#,
                sha1_hex(expected_client),
                expected_client.len()
            ),
        );
        let version_dir = library_dir.join("versions").join(version_id);
        fs::write(
            version_dir.join(format!("{version_id}.jar")),
            &signed_client,
        )
        .expect("write signed child jar");

        let readiness = inspect_launch_readiness(&LaunchReadinessRequest {
            library_dir: library_dir.clone(),
            version_id: version_id.to_string(),
            requested_java: String::new(),
            guardian_mode: GuardianMode::Managed,
        });

        assert!(!readiness.launchable);
        assert!(readiness.reasons.iter().any(|reason| {
            reason.id == LaunchReadinessReasonId::LauncherManagedArtifactSignatureCorrupt
                && reason.severity == LaunchReadinessSeverity::Blocking
        }));
        assert!(
            !readiness
                .reasons
                .iter()
                .any(|reason| reason.id == LaunchReadinessReasonId::ClientJarCorrupt)
        );
        cleanup(&library_dir);
    }

    #[test]
    fn signed_legacy_forge_library_blocks_launch_readiness() {
        let library_dir = temp_library("signed-legacy-forge-library");
        let version_id = "1.5.2-forge-7.8.1.738";
        let client = b"client";
        let signed_forge = zip_bytes(&[
            ("META-INF/MANIFEST.MF", b"signed manifest"),
            ("META-INF/FORGE.SF", b"signature"),
            ("net/minecraftforge/Forge.class", b"forge"),
        ]);
        write_version_json(
            &library_dir,
            "1.5.2",
            r#"{
                "id": "1.5.2",
                "type": "release",
                "mainClass": "net.minecraft.client.Minecraft",
                "assetIndex": {},
                "libraries": []
            }"#,
        );
        write_version_json(
            &library_dir,
            version_id,
            &format!(
                r#"{{
                    "id": "{version_id}",
                    "inheritsFrom": "1.5.2",
                    "type": "release",
                    "mainClass": "net.minecraft.launchwrapper.Launch",
                    "assetIndex": {{}},
                    "downloads": {{
                        "client": {{ "sha1": "{}", "size": {}, "url": "" }}
                    }},
                    "libraries": [{{
                        "name": "net.minecraftforge:minecraftforge:7.8.1.738",
                        "url": "https://libraries.example.invalid/",
                        "sha1": "{}",
                        "size": {}
                    }}]
                }}"#,
                sha1_hex(client),
                client.len(),
                sha1_hex(&signed_forge),
                signed_forge.len()
            ),
        );
        let version_dir = library_dir.join("versions").join(version_id);
        fs::write(version_dir.join(format!("{version_id}.jar")), client).expect("write child jar");
        let forge_path = library_dir
            .join("libraries")
            .join("net/minecraftforge/minecraftforge/7.8.1.738/minecraftforge-7.8.1.738.jar");
        fs::create_dir_all(forge_path.parent().expect("forge parent")).expect("forge dir");
        fs::write(&forge_path, signed_forge).expect("write signed forge library");

        let readiness = inspect_launch_readiness(&LaunchReadinessRequest {
            library_dir: library_dir.clone(),
            version_id: version_id.to_string(),
            requested_java: String::new(),
            guardian_mode: GuardianMode::Managed,
        });

        assert!(!readiness.launchable);
        assert!(readiness.reasons.iter().any(|reason| {
            reason.id == LaunchReadinessReasonId::LauncherManagedArtifactSignatureCorrupt
                && reason.severity == LaunchReadinessSeverity::Blocking
        }));
        assert!(
            !readiness
                .reasons
                .iter()
                .any(|reason| reason.id == LaunchReadinessReasonId::LibrariesCorrupt)
        );
        cleanup(&library_dir);
    }

    #[test]
    fn signed_legacy_forge_library_prefers_signature_reason_over_checksum_mismatch() {
        let library_dir = temp_library("signed-legacy-forge-library-mismatched-checksum");
        let version_id = "1.5.2-forge-7.8.1.738";
        let client = b"client";
        let expected_library = b"fresh";
        let signed_forge = zip_bytes(&[
            ("META-INF/MANIFEST.MF", b"signed manifest"),
            ("META-INF/FORGE.SF", b"signature"),
            ("net/minecraftforge/Forge.class", b"forge"),
        ]);
        write_version_json(
            &library_dir,
            "1.5.2",
            r#"{
                "id": "1.5.2",
                "type": "release",
                "mainClass": "net.minecraft.client.Minecraft",
                "assetIndex": {},
                "libraries": []
            }"#,
        );
        write_version_json(
            &library_dir,
            version_id,
            &format!(
                r#"{{
                    "id": "{version_id}",
                    "inheritsFrom": "1.5.2",
                    "type": "release",
                    "mainClass": "net.minecraft.launchwrapper.Launch",
                    "assetIndex": {{}},
                    "downloads": {{
                        "client": {{ "sha1": "{}", "size": {}, "url": "" }}
                    }},
                    "libraries": [{{
                        "name": "net.minecraftforge:minecraftforge:7.8.1.738",
                        "url": "https://libraries.example.invalid/",
                        "sha1": "{}",
                        "size": {}
                    }}]
                }}"#,
                sha1_hex(client),
                client.len(),
                sha1_hex(expected_library),
                expected_library.len()
            ),
        );
        let version_dir = library_dir.join("versions").join(version_id);
        fs::write(version_dir.join(format!("{version_id}.jar")), client).expect("write child jar");
        let forge_path = library_dir
            .join("libraries")
            .join("net/minecraftforge/minecraftforge/7.8.1.738/minecraftforge-7.8.1.738.jar");
        fs::create_dir_all(forge_path.parent().expect("forge parent")).expect("forge dir");
        fs::write(&forge_path, signed_forge).expect("write signed forge library");

        let readiness = inspect_launch_readiness(&LaunchReadinessRequest {
            library_dir: library_dir.clone(),
            version_id: version_id.to_string(),
            requested_java: String::new(),
            guardian_mode: GuardianMode::Managed,
        });

        assert!(!readiness.launchable);
        assert!(readiness.reasons.iter().any(|reason| {
            reason.id == LaunchReadinessReasonId::LauncherManagedArtifactSignatureCorrupt
                && reason.severity == LaunchReadinessSeverity::Blocking
        }));
        assert!(
            !readiness
                .reasons
                .iter()
                .any(|reason| reason.id == LaunchReadinessReasonId::LibrariesCorrupt)
        );
        cleanup(&library_dir);
    }

    #[test]
    fn corrupt_legacy_top_level_library_blocks_launch_readiness() {
        let library_dir = temp_library("corrupt-legacy-library");
        let client = b"client";
        let expected_library = b"fresh";
        write_version_json(
            &library_dir,
            "1.5.2",
            &format!(
                r#"{{
                    "id": "1.5.2",
                    "type": "release",
                    "mainClass": "net.minecraft.client.main.Main",
                    "assetIndex": {{}},
                    "downloads": {{
                        "client": {{ "sha1": "{}", "size": {} }}
                    }},
                    "libraries": [{{
                        "name": "com.example:legacy:1.0.0",
                        "url": "https://libraries.example.invalid/",
                        "sha1": "{}",
                        "size": {}
                    }}]
                }}"#,
                sha1_hex(client),
                client.len(),
                sha1_hex(expected_library),
                expected_library.len()
            ),
        );
        fs::write(
            library_dir.join("versions").join("1.5.2").join("1.5.2.jar"),
            client,
        )
        .expect("write client jar");
        let library_path = library_dir
            .join("libraries")
            .join("com/example/legacy/1.0.0/legacy-1.0.0.jar");
        fs::create_dir_all(library_path.parent().expect("library parent")).expect("library dir");
        fs::write(&library_path, b"wrong").expect("write corrupt library");

        let readiness = inspect_launch_readiness(&LaunchReadinessRequest {
            library_dir: library_dir.clone(),
            version_id: "1.5.2".to_string(),
            requested_java: String::new(),
            guardian_mode: GuardianMode::Managed,
        });

        assert!(!readiness.launchable);
        assert!(readiness.reasons.iter().any(|reason| {
            reason.id == LaunchReadinessReasonId::LibrariesCorrupt
                && reason.severity == LaunchReadinessSeverity::Blocking
        }));
        cleanup(&library_dir);
    }

    #[test]
    fn corrupt_asset_index_blocks_launch_readiness() {
        let library_dir = temp_library("corrupt-asset-index");
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

        let readiness = inspect_launch_readiness(&LaunchReadinessRequest {
            library_dir: library_dir.clone(),
            version_id: "1.21.1".to_string(),
            requested_java: String::new(),
            guardian_mode: GuardianMode::Managed,
        });

        assert!(!readiness.launchable);
        assert!(readiness.reasons.iter().any(|reason| {
            reason.id == LaunchReadinessReasonId::AssetIndexCorrupt
                && reason.severity == LaunchReadinessSeverity::Blocking
        }));
        cleanup(&library_dir);
    }

    #[test]
    fn missing_asset_object_blocks_launch_readiness() {
        let library_dir = temp_library("missing-asset-object");
        let asset = b"asset";
        write_asset_version_fixture(&library_dir, asset, false);
        let request = LaunchReadinessRequest {
            library_dir: library_dir.clone(),
            version_id: "asset-version".to_string(),
            requested_java: String::new(),
            guardian_mode: GuardianMode::Managed,
        };

        let summary = inspect_launch_readiness_summary(&request);
        let readiness = inspect_launch_readiness(&request);

        assert!(
            summary.launchable,
            "summary readiness must not walk every asset object: {:?}",
            summary.reasons
        );
        assert!(!readiness.launchable);
        assert!(readiness.reasons.iter().any(|reason| {
            reason.id == LaunchReadinessReasonId::AssetIndexMissing
                && reason.severity == LaunchReadinessSeverity::Blocking
        }));
        cleanup(&library_dir);
    }

    #[test]
    fn corrupt_asset_object_blocks_launch_readiness() {
        let library_dir = temp_library("corrupt-asset-object");
        let asset = b"asset";
        let hash = sha1_hex(asset);
        write_asset_version_fixture(&library_dir, asset, false);
        let object_path = library_dir
            .join("assets")
            .join("objects")
            .join(&hash[..2])
            .join(&hash);
        fs::create_dir_all(object_path.parent().expect("object parent")).expect("object dir");
        fs::write(object_path, b"wrong").expect("write corrupt object");

        let readiness = inspect_launch_readiness(&LaunchReadinessRequest {
            library_dir: library_dir.clone(),
            version_id: "asset-version".to_string(),
            requested_java: String::new(),
            guardian_mode: GuardianMode::Managed,
        });

        assert!(!readiness.launchable);
        assert!(readiness.reasons.iter().any(|reason| {
            reason.id == LaunchReadinessReasonId::AssetIndexCorrupt
                && reason.severity == LaunchReadinessSeverity::Blocking
        }));
        cleanup(&library_dir);
    }

    #[test]
    fn missing_legacy_virtual_asset_copy_does_not_block_launch_readiness() {
        let library_dir = temp_library("missing-legacy-virtual-asset");
        let asset = b"asset";
        let hash = sha1_hex(asset);
        write_asset_version_fixture(&library_dir, asset, true);
        let object_path = library_dir
            .join("assets")
            .join("objects")
            .join(&hash[..2])
            .join(&hash);
        fs::create_dir_all(object_path.parent().expect("object parent")).expect("object dir");
        fs::write(object_path, asset).expect("write object");

        let readiness = inspect_launch_readiness(&LaunchReadinessRequest {
            library_dir: library_dir.clone(),
            version_id: "asset-version".to_string(),
            requested_java: String::new(),
            guardian_mode: GuardianMode::Managed,
        });

        assert!(readiness.launchable, "{:?}", readiness.reasons);
        cleanup(&library_dir);
    }

    fn temp_library(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "croopor-launcher-readiness-{name}-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&root).expect("create temp library");
        root
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
