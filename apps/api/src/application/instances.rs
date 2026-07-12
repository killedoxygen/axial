mod create;
mod create_cache;
mod create_policy;
mod resources;

#[cfg(test)]
pub(crate) use create::handle_create_instance;
pub(crate) use create::{
    CreateInstanceRequest, CreateInstanceResponse, CreateInstanceViewResponse,
    CreateLoaderBuildsViewResponse, handle_create_instance_owned, handle_create_instance_view,
    handle_create_loader_builds_view,
};
pub(crate) use create_cache::{invalidate_create_view_cache, invalidate_create_view_source};

#[cfg(test)]
use create::{CreateSelection, resolve_loader_create_selection_from_build_catalog};

pub(crate) use resources::{
    InstanceLogInfo, InstanceLogTailResponse, InstanceModInfo, InstanceResourcesResponse,
    InstanceScreenshotInfo, InstanceWorldInfo, OpenFolderQuery, RenameScreenshotRequest,
    RenameWorldRequest, UpdateModRequest, WorldBackupResponse, handle_backup_instance_world,
    handle_delete_instance_mod, handle_delete_instance_screenshot, handle_delete_instance_world,
    handle_instance_log_tail, handle_instance_logs, handle_instance_mods,
    handle_instance_resources, handle_instance_screenshot_file, handle_instance_screenshots,
    handle_instance_worlds, handle_open_instance_folder, handle_rename_instance_screenshot,
    handle_rename_instance_world, handle_update_instance_mod,
};

#[cfg(test)]
use resources::{
    INSTANCE_LOG_READ_ERROR_MESSAGE, LOG_TAIL_LIMIT, SCREENSHOT_FILE_MAX_BYTES,
    WORLD_BACKUP_MAX_DEPTH, copy_world_backup_staged, instance_folder_open_error_response,
    instance_folder_prepare_error_response, instance_log_read_error_response,
    is_safe_resource_name, resolve_instance_folder, scan_instance_logs, screenshot_content_type,
    screenshot_file_read_error_response, screenshot_file_write_error_response, validate_mod_name,
    validate_screenshot_name, validate_world_name,
};

use crate::application::timing::{
    InstancesListTiming, trace_instances_list, trace_slow_instance_readiness,
};
use crate::application::version::{
    InstalledVersionsScan, VERSION_SCAN_DEGRADED_MESSAGE, VersionScanViewModel,
    installed_versions_scan,
};
use crate::guardian::normalize_create_jvm_preset;
use crate::state::{AppState, ProducerLease};
use axial_config::{EnrichedInstance, InstanceStoreError, LaunchActionState};
use axial_launcher::{
    GuardianMode, LaunchReadiness, LaunchReadinessReasonId, LaunchReadinessRequest,
    LaunchReadinessSeverity, inspect_launch_readiness_summary,
};
use axum::{Json, http::StatusCode};
use serde::{Deserialize, Serialize};
use std::{
    io::ErrorKind,
    path::PathBuf,
    time::{Duration, Instant},
};

const INSTANCE_READINESS_SLOW_SPAN: Duration = Duration::from_millis(25);

#[derive(Clone, Copy)]
enum InstanceWriteOperation {
    Create,
    Duplicate,
    Update,
    Delete,
}

impl InstanceWriteOperation {
    fn internal_error_message(self) -> &'static str {
        match self {
            Self::Create => {
                "Could not create the instance. Check app data permissions and try again."
            }
            Self::Duplicate => {
                "Could not duplicate the instance. Check app data permissions and try again."
            }
            Self::Update => {
                "Could not save the instance. Check app data permissions and try again."
            }
            Self::Delete => {
                "Could not delete the instance. Check app data permissions and try again."
            }
        }
    }
}

fn instance_write_error_response(
    operation: InstanceWriteOperation,
    error: InstanceStoreError,
) -> (StatusCode, Json<serde_json::Value>) {
    let (status, message) = match error {
        InstanceStoreError::Read(error) | InstanceStoreError::Persistence(error) => {
            match error.kind() {
                ErrorKind::NotFound => (StatusCode::NOT_FOUND, "instance not found".to_string()),
                ErrorKind::AlreadyExists => (
                    StatusCode::CONFLICT,
                    "an instance with this name already exists".to_string(),
                ),
                ErrorKind::InvalidInput => (StatusCode::BAD_REQUEST, error.to_string()),
                ErrorKind::WouldBlock => (StatusCode::CONFLICT, error.to_string()),
                _ => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    operation.internal_error_message().to_string(),
                ),
            }
        }
        InstanceStoreError::Validation(message) => (StatusCode::BAD_REQUEST, message.to_string()),
        InstanceStoreError::Parse(_) | InstanceStoreError::TooLarge { .. } => (
            StatusCode::INTERNAL_SERVER_ERROR,
            operation.internal_error_message().to_string(),
        ),
    };

    (status, Json(serde_json::json!({ "error": message })))
}

#[derive(Debug, Serialize)]
pub(crate) struct InstancesResponse {
    pub instances: Vec<EnrichedInstance>,
    pub last_instance_id: Option<String>,
    pub scan_state: VersionScanViewModel,
}

pub(crate) async fn handle_list_instances(
    state: &AppState,
    producer: &ProducerLease,
) -> InstancesResponse {
    let started_at = Instant::now();
    let scan_started_at = Instant::now();
    let (scan, scan_source, refresh_count, library_dir) =
        indexed_current_versions(state, producer).await;
    let scan_elapsed = scan_started_at.elapsed();

    let enrich_started_at = Instant::now();
    let instances = enrich_instances_for_state(state, &scan, library_dir.as_deref());
    let enrich_elapsed = enrich_started_at.elapsed();

    trace_instances_list(InstancesListTiming {
        total: started_at.elapsed(),
        scan: scan_elapsed,
        enrich: enrich_elapsed,
        version_count: scan.versions.len(),
        instance_count: instances.len(),
        degraded: scan.is_degraded(),
        scan_source,
        refresh_count,
    });

    InstancesResponse {
        instances,
        last_instance_id: state.instances().last_instance_id(),
        scan_state: scan.view_model,
    }
}

async fn indexed_current_versions(
    state: &AppState,
    producer: &ProducerLease,
) -> (InstalledVersionsScan, &'static str, u32, Option<PathBuf>) {
    let Some(lookup) = state.installed_versions_snapshot(producer).await else {
        return (unconfigured_versions_scan(), "unconfigured", 0, None);
    };
    let source = lookup.source.as_str();
    let refresh_count = lookup.refresh_count;
    let library_dir = lookup.library_dir().to_path_buf();
    (
        installed_versions_scan(&lookup.snapshot),
        source,
        refresh_count,
        Some(library_dir),
    )
}

fn unconfigured_versions_scan() -> InstalledVersionsScan {
    InstalledVersionsScan {
        versions: Vec::new(),
        view_model: VersionScanViewModel {
            state_id: "library_unconfigured".to_string(),
            label: "Library is not configured".to_string(),
            degraded: false,
            detail: None,
        },
    }
}

async fn enrich_instance_for_state_indexed(
    state: &AppState,
    producer: &ProducerLease,
    instance: axial_config::Instance,
) -> EnrichedInstance {
    let (scan, _, _, library_dir) = indexed_current_versions(state, producer).await;
    enrich_instance_for_scan(state, instance, &scan, library_dir.as_deref())
}

fn enrich_instances_for_state(
    state: &AppState,
    scan: &InstalledVersionsScan,
    library_dir: Option<&std::path::Path>,
) -> Vec<EnrichedInstance> {
    state
        .instances()
        .list()
        .into_iter()
        .map(|instance| enrich_instance_for_scan(state, instance, scan, library_dir))
        .collect()
}

pub(super) fn enrich_instance_for_scan(
    state: &AppState,
    instance: axial_config::Instance,
    scan: &InstalledVersionsScan,
    library_dir: Option<&std::path::Path>,
) -> EnrichedInstance {
    let version = scan
        .versions
        .iter()
        .find(|version| version.id == instance.version_id);
    let mut enriched = redact_runtime_overrides(
        EnrichedInstance::from_instance_without_resource_counts(instance.clone(), version),
    );
    if scan.is_degraded() {
        apply_blocked_launch_action(&mut enriched, VERSION_SCAN_DEGRADED_MESSAGE);
        return enriched;
    }

    let Some(library_dir) = library_dir else {
        return enriched;
    };
    let config = state.config().current();
    let readiness_started_at = Instant::now();
    let readiness = inspect_launch_readiness_summary(&LaunchReadinessRequest {
        library_dir: library_dir.to_path_buf(),
        requested_java: selected_java_override(&instance, &config),
        version_id: instance.version_id.clone(),
        guardian_mode: GuardianMode::from_config(&config.guardian_mode),
    });
    let readiness_elapsed = readiness_started_at.elapsed();
    if readiness_elapsed >= INSTANCE_READINESS_SLOW_SPAN {
        trace_slow_instance_readiness(
            &instance.id,
            &instance.version_id,
            readiness_elapsed,
            readiness.launchable,
            readiness.reasons.len(),
        );
    }
    apply_launch_readiness(&mut enriched, &readiness);
    enriched
}

fn selected_java_override(
    instance: &axial_config::Instance,
    config: &axial_config::AppConfig,
) -> String {
    if !instance.java_path.trim().is_empty() {
        instance.java_path.trim().to_string()
    } else {
        config.java_path_override.trim().to_string()
    }
}

fn apply_launch_readiness(instance: &mut EnrichedInstance, readiness: &LaunchReadiness) {
    if readiness.launchable {
        instance.launchable = true;
        instance.launch_action = LaunchActionState::launch_ready();
        return;
    }

    let detail = readiness_blocking_message(readiness);
    instance.launchable = false;
    instance.status_detail = detail.clone();
    if instance.needs_install.trim().is_empty() {
        instance.needs_install = instance.version_id.clone();
    }
    instance.launch_action = if readiness_has_corrupt_managed_artifact(readiness) {
        LaunchActionState::repair_required(detail)
    } else if readiness_requires_launcher_install(readiness) {
        LaunchActionState::install_required(detail)
    } else if readiness_is_user_blocked(readiness) {
        instance.needs_install.clear();
        LaunchActionState::blocked(detail)
    } else {
        LaunchActionState::install_required(detail)
    };
}

fn apply_blocked_launch_action(instance: &mut EnrichedInstance, detail: &str) {
    instance.launchable = false;
    instance.status_detail = detail.to_string();
    instance.needs_install.clear();
    instance.launch_action = LaunchActionState::blocked(detail.to_string());
}

fn readiness_blocking_message(readiness: &LaunchReadiness) -> String {
    readiness
        .reasons
        .iter()
        .find(|reason| reason.severity == LaunchReadinessSeverity::Blocking)
        .or_else(|| readiness.reasons.first())
        .map(|reason| reason.message.to_string())
        .unwrap_or_else(|| "Version files are not ready.".to_string())
}

fn readiness_has_corrupt_managed_artifact(readiness: &LaunchReadiness) -> bool {
    readiness.reasons.iter().any(|reason| {
        matches!(
            reason.id,
            LaunchReadinessReasonId::ClientJarCorrupt
                | LaunchReadinessReasonId::LibrariesCorrupt
                | LaunchReadinessReasonId::AssetIndexCorrupt
        )
    })
}

fn readiness_requires_launcher_install(readiness: &LaunchReadiness) -> bool {
    readiness.reasons.iter().any(|reason| {
        matches!(
            reason.id,
            LaunchReadinessReasonId::VersionJsonMissing
                | LaunchReadinessReasonId::ParentVersionMissing
                | LaunchReadinessReasonId::IncompleteInstall
                | LaunchReadinessReasonId::ClientJarMissing
                | LaunchReadinessReasonId::LibrariesMissing
                | LaunchReadinessReasonId::AssetIndexMissing
                | LaunchReadinessReasonId::ManagedRuntimeMissing
        )
    })
}

fn readiness_is_user_blocked(readiness: &LaunchReadiness) -> bool {
    readiness
        .reasons
        .iter()
        .any(|reason| reason.id == LaunchReadinessReasonId::JavaOverrideMissing)
}

pub(crate) async fn handle_get_instance(
    state: &AppState,
    producer: &ProducerLease,
    id: &str,
) -> Result<EnrichedInstance, (StatusCode, Json<serde_json::Value>)> {
    let instance = state.instances().get(id);

    match instance {
        Some(instance) => Ok(enrich_instance_for_state_indexed(state, producer, instance).await),
        None => Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "instance not found" })),
        )),
    }
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct DuplicateInstanceRequest {
    pub name: Option<String>,
}

pub(crate) async fn handle_duplicate_instance(
    state: &AppState,
    producer: &ProducerLease,
    id: &str,
    payload: Option<DuplicateInstanceRequest>,
) -> Result<EnrichedInstance, (StatusCode, Json<serde_json::Value>)> {
    let payload = payload.unwrap_or_default();
    let instance = state
        .duplicate_instance(
            id.to_string(),
            payload.name,
            state.library_dir().map(PathBuf::from),
        )
        .await
        .map_err(|error| instance_write_error_response(InstanceWriteOperation::Duplicate, error))?;
    Ok(enrich_instance_for_state_indexed(state, producer, instance).await)
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct InstancePatch {
    pub name: Option<String>,
    pub version_id: Option<String>,
    pub art_seed: Option<u32>,
    pub max_memory_mb: Option<i32>,
    pub min_memory_mb: Option<i32>,
    pub java_path: Option<String>,
    pub window_width: Option<i32>,
    pub window_height: Option<i32>,
    pub jvm_preset: Option<String>,
    pub performance_mode: Option<String>,
    pub extra_jvm_args: Option<String>,
    pub icon: Option<String>,
    pub accent: Option<String>,
}

pub(crate) async fn handle_update_instance(
    state: &AppState,
    producer: &ProducerLease,
    id: &str,
    patch: InstancePatch,
) -> Result<EnrichedInstance, (StatusCode, Json<serde_json::Value>)> {
    let id = id.to_string();
    let instance = state
        .mutate_instances(move |registry| {
            let Some(index) = registry.instances.iter().position(|stored| stored.id == id) else {
                return Err(InstanceStoreError::Persistence(std::io::Error::new(
                    ErrorKind::NotFound,
                    "instance not found",
                )));
            };
            let mut instance = registry.instances[index].clone();
            if let Some(name) = patch.name.filter(|value| !value.trim().is_empty()) {
                if registry
                    .instances
                    .iter()
                    .enumerate()
                    .any(|(stored_index, stored)| stored_index != index && stored.name == name)
                {
                    return Err(InstanceStoreError::Persistence(std::io::Error::new(
                        ErrorKind::AlreadyExists,
                        "an instance with this name already exists",
                    )));
                }
                instance.name = name;
            }
            if let Some(version_id) = patch.version_id.filter(|value| !value.trim().is_empty())
                && version_id != instance.version_id
            {
                return Err(InstanceStoreError::Persistence(std::io::Error::new(
                    ErrorKind::InvalidInput,
                    "direct version changes are not supported",
                )));
            }
            if let Some(value) = patch.art_seed {
                instance.art_seed = value;
            }
            if let Some(value) = patch.max_memory_mb {
                instance.max_memory_mb = value.max(0);
            }
            if let Some(value) = patch.min_memory_mb {
                instance.min_memory_mb = value.max(0);
            }
            if let Some(value) = patch.java_path {
                instance.java_path = value;
            }
            if let Some(value) = patch.window_width {
                instance.window_width = value.max(0);
            }
            if let Some(value) = patch.window_height {
                instance.window_height = value.max(0);
            }
            if let Some(value) = patch.jvm_preset {
                instance.jvm_preset = normalize_create_jvm_preset(Some(&value))
                    .stored_preset()
                    .to_string();
            }
            if let Some(value) = patch.performance_mode {
                instance.performance_mode = value;
            }
            if let Some(value) = patch.extra_jvm_args {
                instance.extra_jvm_args = value;
            }
            if let Some(value) = patch.icon {
                instance.icon = value;
            }
            if let Some(value) = patch.accent {
                instance.accent = value;
            }
            registry.instances[index] = instance.clone();
            Ok(instance)
        })
        .await
        .map_err(|error| instance_write_error_response(InstanceWriteOperation::Update, error))?;
    Ok(enrich_instance_for_state_indexed(state, producer, instance).await)
}

fn redact_runtime_overrides(mut instance: EnrichedInstance) -> EnrichedInstance {
    instance.instance.java_path.clear();
    instance.instance.extra_jvm_args.clear();
    instance
}

pub(crate) async fn handle_delete_instance(
    state: &AppState,
    id: &str,
    query: std::collections::HashMap<String, String>,
) -> Result<serde_json::Value, (StatusCode, Json<serde_json::Value>)> {
    if state.sessions().has_active_instance(id).await {
        return Err((
            StatusCode::CONFLICT,
            Json(
                serde_json::json!({ "error": "cannot delete a running instance; stop the game first" }),
            ),
        ));
    }

    let keep_files = query.get("keep_files").is_some_and(|value| value == "true");
    state
        .delete_instance(id.to_string(), !keep_files)
        .await
        .map_err(|error| instance_write_error_response(InstanceWriteOperation::Delete, error))?;

    Ok(serde_json::json!({ "status": "ok" }))
}

#[cfg(test)]
mod tests;
