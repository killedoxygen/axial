use crate::GuardianMode;
use crate::build::find_client_jar;
use axial_minecraft::download::{
    ExpectedIntegrity, LauncherManagedArtifactReadiness, LibraryVerificationIntegrity,
    asset_object_hash_prefix, jar_contains_signed_metadata, library_verification_plans_for,
    verify_existing_launcher_managed_artifact,
};
use axial_minecraft::paths::assets_dir;
use axial_minecraft::{
    LaunchModelError, ManagedRuntimeCache, RuntimeOverride, VersionJson, default_environment,
    load_version_json, parse_runtime_override, preferred_runtime_component, resolve_version,
    runtime_component_executable_present_without_probe, runtime_component_ready_without_probe,
    runtime_component_structurally_ready_without_probe, runtime_executable_ready_without_probe,
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

pub fn inspect_create_readiness(
    runtime_cache: &ManagedRuntimeCache,
    request: &LaunchReadinessRequest,
) -> LaunchReadiness {
    inspect_readiness(
        runtime_cache,
        request,
        LaunchReadinessInspection::CreateFull,
    )
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
    CreateFull,
    Summary,
    Structural,
}

fn inspect_readiness(
    runtime_cache: &ManagedRuntimeCache,
    request: &LaunchReadinessRequest,
    inspection: LaunchReadinessInspection,
) -> LaunchReadiness {
    let mut reasons = Vec::new();
    inspect_incomplete_install_markers(&request.library_dir, &request.version_id, &mut reasons);

    let version = match resolve_version(&request.library_dir, &request.version_id) {
        Ok(version) => {
            if inspection != LaunchReadinessInspection::Structural {
                inspect_version_files(
                    &request.library_dir,
                    &request.version_id,
                    &version,
                    inspection,
                    &mut reasons,
                );
            }
            Some(version)
        }
        Err(error) => {
            reasons.push(reason_for_version_error(&error));
            None
        }
    };

    inspect_runtime_files(
        runtime_cache,
        request,
        version.as_ref(),
        inspection,
        &mut reasons,
    );

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
            let client_signature_corrupt = inspection == LaunchReadinessInspection::CreateFull
                && legacy_forge_client_jar_has_signed_metadata(
                    library_dir,
                    version_id,
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

    let planned_libraries =
        library_verification_plans_for(library_dir, &version.libraries, &default_environment());
    let library_planning_failed = planned_libraries.is_err();
    let library_jobs: Vec<ArtifactVerificationJob> = planned_libraries
        .unwrap_or_default()
        .into_iter()
        .map(|library| ArtifactVerificationJob {
            path: library.path,
            name: Some(library.name),
            integrity: library.integrity,
        })
        .collect();
    let library_signature_corrupt = inspection == LaunchReadinessInspection::CreateFull
        && legacy_forge_libraries_have_signed_metadata(library_dir, version_id, &library_jobs);
    let library_readiness = match inspection {
        LaunchReadinessInspection::Summary => verify_artifact_jobs_metadata(library_jobs),
        LaunchReadinessInspection::CreateFull => verify_artifact_jobs(library_jobs),
        LaunchReadinessInspection::Structural => {
            unreachable!("structural readiness skips artifact inspection")
        }
    };
    let libraries_missing = readiness_contains(
        &library_readiness,
        LauncherManagedArtifactReadiness::Missing,
    );
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
                LaunchReadinessInspection::CreateFull => {
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
                LaunchReadinessInspection::Structural => {
                    unreachable!("structural readiness skips artifact inspection")
                }
            }
        }
    }
}

#[derive(Clone)]
struct ArtifactVerificationJob {
    path: PathBuf,
    name: Option<String>,
    integrity: LibraryVerificationIntegrity,
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
    match job.integrity {
        LibraryVerificationIntegrity::Sha1(expected) => {
            verify_existing_launcher_managed_artifact(&job.path, &expected)
        }
        LibraryVerificationIntegrity::MissingChecksum => {
            LauncherManagedArtifactReadiness::MetadataMissing
        }
    }
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
    client_jar: &Path,
) -> bool {
    legacy_forge_artifacts_must_be_unsigned(library_dir, version_id)
        && child_version_jar(library_dir, version_id, client_jar)
        && jar_contains_signed_metadata(client_jar)
}

fn legacy_forge_libraries_have_signed_metadata(
    library_dir: &Path,
    version_id: &str,
    jobs: &[ArtifactVerificationJob],
) -> bool {
    if !legacy_forge_artifacts_must_be_unsigned(library_dir, version_id) {
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

fn legacy_forge_artifacts_must_be_unsigned(library_dir: &Path, version_id: &str) -> bool {
    let Ok(profile) = axial_minecraft::load_version_json(library_dir, version_id) else {
        return false;
    };
    let Ok(identity) = axial_minecraft::validate_materialized_loader_profile(
        version_id,
        &profile.id,
        &profile.inherits_from,
        profile.materialized,
    ) else {
        return false;
    };
    if identity.component_id() != axial_minecraft::LoaderComponentId::Forge {
        return false;
    }

    minecraft_version_requires_unsigned_legacy_forge_artifacts(identity.minecraft_version())
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
            integrity: LibraryVerificationIntegrity::Sha1(expected),
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
    runtime_cache: &ManagedRuntimeCache,
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
                    LaunchReadinessInspection::CreateFull => {
                        runtime_component_ready_without_probe(runtime_cache, component.as_str())
                    }
                };
                if !ready {
                    reasons.push(java_override_missing_reason());
                }
                return;
            }
            RuntimeOverride::None => {}
        }
    }

    if inspection != LaunchReadinessInspection::CreateFull {
        return;
    }

    let Some(version) = version else {
        return;
    };
    let component = preferred_runtime_component(&version.java_version);
    if !runtime_component_ready_without_probe(runtime_cache, &component) {
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
        ArtifactVerificationJob, LaunchReadinessReasonId, LaunchReadinessRequest,
        LaunchReadinessSeverity, inspect_create_readiness, inspect_launch_readiness_structural,
        inspect_launch_readiness_summary, verify_artifact_job, verify_artifact_job_metadata,
    };
    use crate::GuardianMode;
    use axial_minecraft::download::{
        LauncherManagedArtifactReadiness, LibraryVerificationIntegrity,
    };
    use sha1::{Digest as _, Sha1};
    use std::fs;
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn managed_runtime_missing_is_recoverable_in_managed_mode() {
        let library_dir = temp_library("managed-runtime-missing-recoverable");
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
        fs::write(
            library_dir
                .join("versions")
                .join("1.21.1")
                .join("1.21.1.jar"),
            b"client jar",
        )
        .expect("write client jar");

        let readiness = inspect_create_readiness(
            &runtime_cache,
            &LaunchReadinessRequest {
                library_dir: library_dir.clone(),
                version_id: "1.21.1".to_string(),
                requested_java: String::new(),
                guardian_mode: GuardianMode::Managed,
            },
        );

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

        let readiness = inspect_create_readiness(
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
    fn corrupt_client_jar_blocks_launch_readiness() {
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

        let readiness = inspect_create_readiness(
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
            reason.id == LaunchReadinessReasonId::ClientJarCorrupt
                && reason.severity == LaunchReadinessSeverity::Blocking
        }));
        cleanup(&library_dir);
    }

    #[test]
    fn exact_library_hash_and_size_drift_block_readiness() {
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

        let readiness = inspect_create_readiness(
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
            reason.id == LaunchReadinessReasonId::LibrariesCorrupt
                && reason.severity == LaunchReadinessSeverity::Blocking
        }));

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

        let readiness = inspect_create_readiness(
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

        let readiness = inspect_create_readiness(
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
            name: None,
            integrity: LibraryVerificationIntegrity::MissingChecksum,
        };
        assert_eq!(
            verify_artifact_job_metadata(unauthoritative.clone()),
            LauncherManagedArtifactReadiness::MetadataMissing
        );
        assert_eq!(
            verify_artifact_job(unauthoritative),
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

        let readiness = inspect_create_readiness(
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
    fn signed_legacy_forge_child_client_blocks_launch_readiness() {
        let library_dir = temp_library("signed-legacy-forge-child-client");
        let runtime_cache = isolated_runtime_cache();
        let version_id = legacy_forge_version_id();
        let signed_client = zip_bytes(&[
            ("META-INF/MANIFEST.MF", b"signed manifest"),
            ("META-INF/MOJANG_C.SF", b"signature"),
            ("net/minecraft/client/Minecraft.class", b"class"),
        ]);
        write_version_json(
            &library_dir,
            &version_id,
            &format!(
                r#"{{
                    "id": "{version_id}",
                    "inheritsFrom": "1.5.2",
                    "axialMaterialized": true,
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
        let version_dir = library_dir.join("versions").join(&version_id);
        fs::write(
            version_dir.join(format!("{version_id}.jar")),
            &signed_client,
        )
        .expect("write signed child jar");

        let readiness = inspect_create_readiness(
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
        let runtime_cache = isolated_runtime_cache();
        let version_id = legacy_forge_version_id();
        let expected_client = b"fresh";
        let signed_client = zip_bytes(&[
            ("META-INF/MANIFEST.MF", b"signed manifest"),
            ("META-INF/MOJANG_C.SF", b"signature"),
            ("net/minecraft/client/Minecraft.class", b"class"),
        ]);
        write_version_json(
            &library_dir,
            &version_id,
            &format!(
                r#"{{
                    "id": "{version_id}",
                    "inheritsFrom": "1.5.2",
                    "axialMaterialized": true,
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
        let version_dir = library_dir.join("versions").join(&version_id);
        fs::write(
            version_dir.join(format!("{version_id}.jar")),
            &signed_client,
        )
        .expect("write signed child jar");

        let readiness = inspect_create_readiness(
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
        let runtime_cache = isolated_runtime_cache();
        let version_id = legacy_forge_version_id();
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
            &version_id,
            &format!(
                r#"{{
                    "id": "{version_id}",
                    "inheritsFrom": "1.5.2",
                    "axialMaterialized": true,
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
        let version_dir = library_dir.join("versions").join(&version_id);
        fs::write(version_dir.join(format!("{version_id}.jar")), client).expect("write child jar");
        let forge_path = library_dir
            .join("libraries")
            .join("net/minecraftforge/minecraftforge/7.8.1.738/minecraftforge-7.8.1.738.jar");
        fs::create_dir_all(forge_path.parent().expect("forge parent")).expect("forge dir");
        fs::write(&forge_path, signed_forge).expect("write signed forge library");

        let readiness = inspect_create_readiness(
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
        let runtime_cache = isolated_runtime_cache();
        let version_id = legacy_forge_version_id();
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
            &version_id,
            &format!(
                r#"{{
                    "id": "{version_id}",
                    "inheritsFrom": "1.5.2",
                    "axialMaterialized": true,
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
        let version_dir = library_dir.join("versions").join(&version_id);
        fs::write(version_dir.join(format!("{version_id}.jar")), client).expect("write child jar");
        let forge_path = library_dir
            .join("libraries")
            .join("net/minecraftforge/minecraftforge/7.8.1.738/minecraftforge-7.8.1.738.jar");
        fs::create_dir_all(forge_path.parent().expect("forge parent")).expect("forge dir");
        fs::write(&forge_path, signed_forge).expect("write signed forge library");

        let readiness = inspect_create_readiness(
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
        let runtime_cache = isolated_runtime_cache();
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

        let readiness = inspect_create_readiness(
            &runtime_cache,
            &LaunchReadinessRequest {
                library_dir: library_dir.clone(),
                version_id: "1.5.2".to_string(),
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
    fn corrupt_asset_index_blocks_launch_readiness() {
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

        let readiness = inspect_create_readiness(
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
    fn missing_asset_object_blocks_launch_readiness() {
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
        let readiness = inspect_create_readiness(&runtime_cache, &request);

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
        let runtime_cache = isolated_runtime_cache();
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

        let readiness = inspect_create_readiness(
            &runtime_cache,
            &LaunchReadinessRequest {
                library_dir: library_dir.clone(),
                version_id: "asset-version".to_string(),
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
    fn missing_legacy_virtual_asset_copy_does_not_block_launch_readiness() {
        let library_dir = temp_library("missing-legacy-virtual-asset");
        let runtime_cache = isolated_runtime_cache();
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

        let readiness = inspect_create_readiness(
            &runtime_cache,
            &LaunchReadinessRequest {
                library_dir: library_dir.clone(),
                version_id: "asset-version".to_string(),
                requested_java: String::new(),
                guardian_mode: GuardianMode::Managed,
            },
        );

        assert!(readiness.launchable, "{:?}", readiness.reasons);
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

    fn legacy_forge_version_id() -> String {
        axial_minecraft::installed_version_id_for(
            axial_minecraft::LoaderComponentId::Forge,
            "1.5.2",
            "7.8.1.738",
        )
        .expect("valid Forge identity")
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
