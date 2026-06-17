mod create;
mod resources;

pub(crate) use create::{
    CreateInstanceRequest, CreateInstanceResponse, CreateInstanceViewResponse,
    handle_create_instance, handle_create_instance_view,
};

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

use crate::guardian::normalize_create_jvm_preset;
use crate::state::AppState;
use axum::{Json, http::StatusCode};
use croopor_config::{EnrichedInstance, InstanceStoreError};
use croopor_minecraft::{VersionEntry, scan_versions};
use serde::{Deserialize, Serialize};
use std::{io::ErrorKind, path::PathBuf};

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
        InstanceStoreError::Read(error) => match error.kind() {
            ErrorKind::NotFound => (StatusCode::NOT_FOUND, "instance not found".to_string()),
            ErrorKind::AlreadyExists => (
                StatusCode::CONFLICT,
                "an instance with this name already exists".to_string(),
            ),
            ErrorKind::InvalidInput => (StatusCode::BAD_REQUEST, error.to_string()),
            _ => (
                StatusCode::INTERNAL_SERVER_ERROR,
                operation.internal_error_message().to_string(),
            ),
        },
        InstanceStoreError::Parse(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            operation.internal_error_message().to_string(),
        ),
    };

    (status, Json(serde_json::json!({ "error": message })))
}

fn instance_error_kind(error: &InstanceStoreError) -> Option<ErrorKind> {
    match error {
        InstanceStoreError::Read(error) => Some(error.kind()),
        InstanceStoreError::Parse(_) => None,
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct InstancesResponse {
    pub instances: Vec<EnrichedInstance>,
    pub last_instance_id: Option<String>,
}

pub(crate) async fn handle_list_instances(state: &AppState) -> InstancesResponse {
    let versions = scan_current_versions(state);

    InstancesResponse {
        instances: state.instances().enrich(&versions),
        last_instance_id: state.instances().last_instance_id(),
    }
}

fn scan_current_versions(state: &AppState) -> Vec<VersionEntry> {
    state
        .library_dir()
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .and_then(|path| scan_versions(&path).ok())
        .unwrap_or_default()
}

fn enrich_instance_for_state(
    state: &AppState,
    instance: croopor_config::Instance,
) -> EnrichedInstance {
    let versions = scan_current_versions(state);
    let version = versions
        .iter()
        .find(|version| version.id == instance.version_id);
    let game_dir = state.instances().game_dir(&instance.id);
    EnrichedInstance::from_instance(instance, version, &game_dir)
}

pub(crate) async fn handle_get_instance(
    state: &AppState,
    id: &str,
) -> Result<EnrichedInstance, (StatusCode, Json<serde_json::Value>)> {
    let instance = state.instances().get(id);

    match instance {
        Some(instance) => Ok(enrich_instance_for_state(state, instance)),
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
    id: &str,
    payload: Option<DuplicateInstanceRequest>,
) -> Result<EnrichedInstance, (StatusCode, Json<serde_json::Value>)> {
    let payload = payload.unwrap_or_default();
    let mc_dir = state.library_dir().map(PathBuf::from);
    state
        .instances()
        .duplicate(id, payload.name, mc_dir.as_deref())
        .map(|instance| enrich_instance_for_state(state, instance))
        .map_err(|error| instance_write_error_response(InstanceWriteOperation::Duplicate, error))
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
    id: &str,
    patch: InstancePatch,
) -> Result<EnrichedInstance, (StatusCode, Json<serde_json::Value>)> {
    let mut instance = state.instances().get(id).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "instance not found" })),
        )
    })?;

    if let Some(name) = patch.name.filter(|value| !value.trim().is_empty()) {
        instance.name = name;
    }
    if let Some(version_id) = patch.version_id.filter(|value| !value.trim().is_empty()) {
        instance.version_id = version_id;
    }
    if let Some(art_seed) = patch.art_seed {
        instance.art_seed = art_seed;
    }
    if let Some(max_memory_mb) = patch.max_memory_mb {
        instance.max_memory_mb = max_memory_mb.max(0);
    }
    if let Some(min_memory_mb) = patch.min_memory_mb {
        instance.min_memory_mb = min_memory_mb.max(0);
    }
    if let Some(java_path) = patch.java_path {
        instance.java_path = java_path;
    }
    if let Some(window_width) = patch.window_width {
        instance.window_width = window_width.max(0);
    }
    if let Some(window_height) = patch.window_height {
        instance.window_height = window_height.max(0);
    }
    if let Some(jvm_preset) = patch.jvm_preset {
        instance.jvm_preset = normalize_create_jvm_preset(Some(&jvm_preset)).stored_preset;
    }
    if let Some(performance_mode) = patch.performance_mode {
        instance.performance_mode = performance_mode;
    }
    if let Some(extra_jvm_args) = patch.extra_jvm_args {
        instance.extra_jvm_args = extra_jvm_args;
    }
    if let Some(icon) = patch.icon {
        instance.icon = icon;
    }
    if let Some(accent) = patch.accent {
        instance.accent = accent;
    }
    state
        .instances()
        .update(instance)
        .map(|instance| redact_runtime_overrides(enrich_instance_for_state(state, instance)))
        .map_err(|error| instance_write_error_response(InstanceWriteOperation::Update, error))
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
    if state.instances().get(id).is_none() {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "instance not found" })),
        ));
    }

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
        .instances()
        .remove(id, !keep_files)
        .map_err(|error| instance_write_error_response(InstanceWriteOperation::Delete, error))?;

    Ok(serde_json::json!({ "status": "ok" }))
}

#[cfg(test)]
mod tests;
