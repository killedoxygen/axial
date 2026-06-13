use crate::GuardianMode;
use crate::build::{find_client_jar, uses_module_bootstrap};
use croopor_minecraft::{
    LaunchModelError, RuntimeOverride, VersionJson, default_environment, load_version_json,
    parse_runtime_override, preferred_runtime_component, resolve_libraries, resolve_version,
    runtime_component_ready_without_probe, runtime_executable_ready_without_probe,
};
use serde::Serialize;
use std::collections::HashSet;
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
    VersionJsonMissing,
    ParentVersionMissing,
    IncompleteInstall,
    ClientJarMissing,
    LibrariesMissing,
    AssetIndexMissing,
    ManagedRuntimeMissing,
    JavaOverrideMissing,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LaunchReadinessSeverity {
    Blocking,
}

pub fn inspect_launch_readiness(request: &LaunchReadinessRequest) -> LaunchReadiness {
    let mut reasons = Vec::new();
    inspect_incomplete_install_markers(&request.library_dir, &request.version_id, &mut reasons);

    let version = match resolve_version(&request.library_dir, &request.version_id) {
        Ok(version) => {
            inspect_version_files(
                &request.library_dir,
                &request.version_id,
                &version,
                &mut reasons,
            );
            Some(version)
        }
        Err(error) => {
            reasons.push(reason_for_version_error(&error));
            None
        }
    };

    inspect_runtime_files(request, version.as_ref(), &mut reasons);

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
    reasons: &mut Vec<LaunchReadinessReason>,
) {
    if !uses_module_bootstrap(version)
        && find_client_jar(library_dir, version, version_id).is_none()
    {
        reasons.push(reason(
            LaunchReadinessReasonId::ClientJarMissing,
            "Client game files are missing. Install this version before launching.",
        ));
    }

    let libraries = resolve_libraries(version, library_dir, &default_environment());
    if libraries.iter().any(|library| !library.abs_path.is_file()) {
        reasons.push(reason(
            LaunchReadinessReasonId::LibrariesMissing,
            "Required libraries are missing. Install this version before launching.",
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
            ));
        }
    }
}

fn inspect_runtime_files(
    request: &LaunchReadinessRequest,
    version: Option<&VersionJson>,
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
                if !runtime_component_ready_without_probe(&request.library_dir, component.as_str())
                {
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
    let component = preferred_runtime_component(&version.java_version);
    if !runtime_component_ready_without_probe(&request.library_dir, &component) {
        reasons.push(reason(
            LaunchReadinessReasonId::ManagedRuntimeMissing,
            "Managed Java runtime is missing. Install or repair the runtime before launching.",
        ));
    }
}

fn reason_for_version_error(error: &LaunchModelError) -> LaunchReadinessReason {
    if is_missing_parent_version(error) {
        return reason(
            LaunchReadinessReasonId::ParentVersionMissing,
            "Parent version metadata is missing. Install the base version before launching.",
        );
    }

    reason(
        LaunchReadinessReasonId::VersionJsonMissing,
        "Installed version metadata is missing. Install this version before launching.",
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
    )
}

fn reason(id: LaunchReadinessReasonId, message: &'static str) -> LaunchReadinessReason {
    LaunchReadinessReason {
        id,
        severity: LaunchReadinessSeverity::Blocking,
        message,
    }
}
