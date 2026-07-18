use crate::{
    application::filesystem::{
        BlockingFilesystemTaskError, FilesystemEntryKind, FilesystemScanBudget,
        FilesystemScanError, FilesystemScanLimits, admit_blocking_filesystem,
        admit_exclusive_blocking_filesystem, run_blocking_filesystem,
    },
    state::{AppState, UpdateOperationAdmissionError, UpdateOperationLease},
};
use async_stream::stream;
use axial_content::{
    ModFileDeleteOutcome, ModFileMutationError, delete_local_mod_file, toggle_mod_file,
};
use axum::{
    Json,
    body::{Body, Bytes},
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{ErrorKind, Read, SeekFrom, Write},
    path::{Path as FsPath, PathBuf},
};
use tokio::io::{AsyncReadExt, AsyncSeekExt};

mod open_folder;

pub(crate) use open_folder::{OpenFolderQuery, handle_open_instance_folder};

#[cfg(test)]
pub(super) use open_folder::{
    instance_folder_open_error_response, instance_folder_prepare_error_response,
    resolve_instance_folder,
};

pub(super) const LOG_TAIL_LIMIT: u64 = 128 * 1024;
pub(super) const SCREENSHOT_FILE_MAX_BYTES: u64 = 32 * 1024 * 1024;
const SCREENSHOT_FILE_STREAM_CHUNK_BYTES: usize = 64 * 1024;
const INSTANCE_RESOURCE_SCAN_LIMITS: FilesystemScanLimits = FilesystemScanLimits {
    max_depth: 32,
    max_entries: 50_000,
    max_bytes: 1024 * 1024 * 1024 * 1024,
};
pub(super) const WORLD_BACKUP_MAX_DEPTH: usize = 64;
pub(super) const WORLD_BACKUP_MAX_ENTRIES: usize = 100_000;
pub(super) const WORLD_BACKUP_MAX_BYTES: u64 = 50 * 1024 * 1024 * 1024;
const WORLD_BACKUP_SCAN_LIMITS: FilesystemScanLimits = FilesystemScanLimits {
    max_depth: WORLD_BACKUP_MAX_DEPTH,
    max_entries: WORLD_BACKUP_MAX_ENTRIES,
    max_bytes: WORLD_BACKUP_MAX_BYTES,
};
pub(super) const INSTANCE_LOG_READ_ERROR_MESSAGE: &str =
    "Could not read the instance log. Check instance folder permissions and try again.";

#[derive(Debug, Serialize)]
pub(crate) struct InstanceWorldInfo {
    pub name: String,
    pub size: u64,
    pub modified_at: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct InstanceModInfo {
    pub name: String,
    pub size: u64,
    pub modified_at: String,
    pub enabled: bool,
}

#[derive(Debug, Serialize)]
pub(crate) struct InstanceScreenshotInfo {
    pub name: String,
    pub size: u64,
    pub modified_at: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct InstanceLogInfo {
    pub name: String,
    pub size: u64,
    pub modified_at: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct InstanceResourcesResponse {
    pub worlds: Vec<InstanceWorldInfo>,
    pub mods: Vec<InstanceModInfo>,
    pub screenshots: Vec<InstanceScreenshotInfo>,
    pub logs: Vec<InstanceLogInfo>,
    pub worlds_count: usize,
    pub mods_count: usize,
    pub screenshots_count: usize,
    pub logs_count: usize,
}

#[derive(Debug, Serialize)]
pub(crate) struct InstanceLogTailResponse {
    pub name: String,
    pub size: u64,
    pub truncated: bool,
    pub text: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RenameWorldRequest {
    pub name: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RenameScreenshotRequest {
    pub name: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct UpdateModRequest {
    pub enabled: bool,
}

#[derive(Debug, Serialize)]
pub(crate) struct WorldBackupResponse {
    pub status: &'static str,
    pub backup: String,
    pub location: String,
}

pub(super) fn instance_log_read_error_response(
    _error: std::io::Error,
) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({
            "error": INSTANCE_LOG_READ_ERROR_MESSAGE
        })),
    )
}

pub(crate) async fn handle_instance_resources(
    state: &AppState,
    id: &str,
) -> Result<InstanceResourcesResponse, (StatusCode, Json<serde_json::Value>)> {
    let game_dir = instance_game_dir(state, id)?;
    run_resource_scan(move || scan_instance_resources(&game_dir)).await
}

pub(crate) async fn handle_instance_worlds(
    state: &AppState,
    id: &str,
) -> Result<Vec<InstanceWorldInfo>, (StatusCode, Json<serde_json::Value>)> {
    let game_dir = instance_game_dir(state, id)?;
    run_resource_scan(move || {
        let mut budget = FilesystemScanBudget::new(INSTANCE_RESOURCE_SCAN_LIMITS);
        scan_instance_worlds(&game_dir.join("saves"), &mut budget)
    })
    .await
}

pub(crate) async fn handle_rename_instance_world(
    state: &AppState,
    id: &str,
    name: &str,
    payload: RenameWorldRequest,
) -> Result<serde_json::Value, (StatusCode, Json<serde_json::Value>)> {
    validate_world_name(name)?;
    validate_world_name(&payload.name)?;

    let filesystem = admit_blocking_filesystem()
        .await
        .map_err(resource_filesystem_task_error_response)?;
    let lifecycle_guard = acquire_instance_resource_lifecycle(state, id).await?;
    reject_running_instance(state, id, "worlds").await?;
    let game_dir = instance_game_dir(state, id)?;
    let source = game_dir.join("saves").join(name);
    let target = game_dir.join("saves").join(&payload.name);
    filesystem
        .run(move || {
            let _lifecycle_guard = lifecycle_guard;
            require_world_dir(&source)?;
            if target_exists(&target) {
                return Err(json_error(StatusCode::CONFLICT, "world already exists"));
            }
            fs::rename(source, target).map_err(world_file_write_error_response)
        })
        .await
        .map_err(resource_filesystem_task_error_response)??;
    Ok(serde_json::json!({ "status": "ok", "name": payload.name }))
}

pub(crate) async fn handle_delete_instance_world(
    state: &AppState,
    id: &str,
    name: &str,
) -> Result<serde_json::Value, (StatusCode, Json<serde_json::Value>)> {
    validate_world_name(name)?;
    let filesystem = admit_exclusive_blocking_filesystem()
        .await
        .map_err(resource_filesystem_task_error_response)?;
    let lifecycle_guard = acquire_instance_resource_lifecycle(state, id).await?;
    reject_running_instance(state, id, "worlds").await?;
    let game_dir = instance_game_dir(state, id)?;
    let source = game_dir.join("saves").join(name);
    filesystem
        .run(move || {
            let _lifecycle_guard = lifecycle_guard;
            require_world_dir(&source)?;
            fs::remove_dir_all(source).map_err(world_file_write_error_response)
        })
        .await
        .map_err(resource_filesystem_task_error_response)??;
    Ok(serde_json::json!({ "status": "ok" }))
}

pub(crate) async fn handle_backup_instance_world(
    state: &AppState,
    id: &str,
    name: &str,
) -> Result<WorldBackupResponse, (StatusCode, Json<serde_json::Value>)> {
    validate_world_name(name)?;
    let filesystem = admit_exclusive_blocking_filesystem()
        .await
        .map_err(resource_filesystem_task_error_response)?;
    let lifecycle_guard = acquire_instance_resource_lifecycle(state, id).await?;
    reject_running_instance(state, id, "worlds").await?;
    let game_dir = instance_game_dir(state, id)?;
    let source = game_dir.join("saves").join(name);
    let backup_root = game_dir.join("backups").join("worlds");
    let world_name = name.to_string();
    let backup = filesystem
        .run(move || {
            let _lifecycle_guard = lifecycle_guard;
            require_world_dir(&source)?;
            fs::create_dir_all(&backup_root).map_err(world_file_write_error_response)?;
            let backup = available_world_backup_name(&backup_root, &world_name)?;
            copy_world_backup_staged(&source, &backup_root, &backup)
                .map_err(world_backup_copy_error_response)?;
            Ok::<_, (StatusCode, Json<serde_json::Value>)>(backup)
        })
        .await
        .map_err(resource_filesystem_task_error_response)??;

    Ok(WorldBackupResponse {
        status: "ok",
        location: format!("backups/worlds/{backup}"),
        backup,
    })
}

pub(crate) async fn handle_instance_mods(
    state: &AppState,
    id: &str,
) -> Result<Vec<InstanceModInfo>, (StatusCode, Json<serde_json::Value>)> {
    let game_dir = instance_game_dir(state, id)?;
    run_resource_scan(move || {
        let mut budget = FilesystemScanBudget::new(INSTANCE_RESOURCE_SCAN_LIMITS);
        scan_instance_mods(&game_dir.join("mods"), &mut budget)
    })
    .await
}

pub(crate) async fn handle_update_instance_mod(
    state: &AppState,
    id: &str,
    name: &str,
    payload: UpdateModRequest,
) -> Result<serde_json::Value, (StatusCode, Json<serde_json::Value>)> {
    validate_mod_name(name)?;
    let filesystem = admit_blocking_filesystem()
        .await
        .map_err(resource_filesystem_task_error_response)?;
    let update_admission = admit_instance_mod_mutation(state)?;
    let lifecycle_guard = acquire_instance_resource_lifecycle(state, id).await?;
    reject_running_instance(state, id, "mods").await?;

    let game_dir = instance_game_dir(state, id)?;
    let mods_dir = game_dir.join("mods");
    let source = mods_dir.join(name);
    let requested_name = requested_mod_filename(name, payload.enabled);
    let mutation = state.admit_managed_artifact_mutation().map_err(|error| {
        mod_manifest_error_response(axial_content::ContentError::Io(std::io::Error::other(
            error.to_string(),
        )))
    })?;
    let original_name = name.to_string();
    let outcome = filesystem
        .run(move || {
            let (_update_admission, _lifecycle_guard, _mutation) =
                (update_admission, lifecycle_guard, mutation);
            require_mod_file(&source)?;
            if requested_name != original_name && target_exists(&mods_dir.join(&requested_name)) {
                return Err(json_error(StatusCode::CONFLICT, "mod already exists"));
            }
            toggle_mod_file(&game_dir, &original_name, payload.enabled)
                .map_err(mod_content_mutation_error_response)
        })
        .await
        .map_err(resource_filesystem_task_error_response)??;
    Ok(serde_json::json!({ "status": "ok", "name": outcome.filename, "enabled": payload.enabled }))
}

pub(crate) async fn handle_delete_instance_mod(
    state: &AppState,
    id: &str,
    name: &str,
) -> Result<serde_json::Value, (StatusCode, Json<serde_json::Value>)> {
    validate_mod_name(name)?;
    let filesystem = admit_blocking_filesystem()
        .await
        .map_err(resource_filesystem_task_error_response)?;
    let update_admission = admit_instance_mod_mutation(state)?;
    let lifecycle_guard = acquire_instance_resource_lifecycle(state, id).await?;
    reject_running_instance(state, id, "mods").await?;

    let game_dir = instance_game_dir(state, id)?;
    let source = game_dir.join("mods").join(name);
    let mutation = state.admit_managed_artifact_mutation().map_err(|error| {
        mod_manifest_error_response(axial_content::ContentError::Io(std::io::Error::other(
            error.to_string(),
        )))
    })?;
    let original_name = name.to_string();
    let outcome = filesystem
        .run(move || {
            let (_update_admission, _lifecycle_guard, _mutation) =
                (update_admission, lifecycle_guard, mutation);
            require_mod_file(&source)?;
            delete_local_mod_file(&game_dir, &original_name)
                .map_err(mod_content_mutation_error_response)
        })
        .await
        .map_err(resource_filesystem_task_error_response)??;
    match outcome {
        ModFileDeleteOutcome::Deleted => Ok(serde_json::json!({ "status": "ok" })),
        ModFileDeleteOutcome::Managed => Err(json_error(
            StatusCode::CONFLICT,
            "managed mods must be removed through content operations",
        )),
    }
}

pub(super) fn admit_instance_mod_mutation(
    state: &AppState,
) -> Result<UpdateOperationLease, (StatusCode, Json<serde_json::Value>)> {
    state
        .try_admit_update_sensitive_operation()
        .map_err(|error| {
            let message = match error {
                UpdateOperationAdmissionError::ApplyInProgress => {
                    "Content changes are unavailable while an update is being applied."
                }
                UpdateOperationAdmissionError::RestartPending => {
                    "Restart Axial to finish the applied update before changing content."
                }
            };
            json_error(StatusCode::SERVICE_UNAVAILABLE, message)
        })
}

pub(crate) async fn handle_instance_screenshots(
    state: &AppState,
    id: &str,
) -> Result<Vec<InstanceScreenshotInfo>, (StatusCode, Json<serde_json::Value>)> {
    let game_dir = instance_game_dir(state, id)?;
    run_resource_scan(move || {
        let mut budget = FilesystemScanBudget::new(INSTANCE_RESOURCE_SCAN_LIMITS);
        scan_instance_screenshots(&game_dir.join("screenshots"), &mut budget)
    })
    .await
}

pub(crate) async fn handle_instance_screenshot_file(
    state: &AppState,
    id: &str,
    name: &str,
) -> Result<Response, (StatusCode, Json<serde_json::Value>)> {
    validate_screenshot_name(name)?;
    let content_type = screenshot_content_type(name)
        .ok_or_else(|| json_error(StatusCode::BAD_REQUEST, "invalid screenshot filename"))?;

    let game_dir = instance_game_dir(state, id)?;
    let path = game_dir.join("screenshots").join(name);
    let metadata = require_screenshot_file_async(&path).await?;
    if metadata.len() > SCREENSHOT_FILE_MAX_BYTES {
        return Err(json_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "screenshot file is too large",
        ));
    }

    let mut file = tokio::fs::File::open(&path)
        .await
        .map_err(screenshot_file_read_error_response)?;
    let stream = stream! {
        let mut buffer = vec![0_u8; SCREENSHOT_FILE_STREAM_CHUNK_BYTES];
        loop {
            match file.read(&mut buffer).await {
                Ok(0) => break,
                Ok(bytes_read) => {
                    yield Ok::<Bytes, std::io::Error>(Bytes::copy_from_slice(
                        &buffer[..bytes_read],
                    ));
                }
                Err(error) => {
                    yield Err::<Bytes, std::io::Error>(error);
                    break;
                }
            }
        }
    };
    let mut response = Body::from_stream(stream).into_response();
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    Ok(response)
}

pub(crate) async fn handle_rename_instance_screenshot(
    state: &AppState,
    id: &str,
    name: &str,
    payload: RenameScreenshotRequest,
) -> Result<serde_json::Value, (StatusCode, Json<serde_json::Value>)> {
    validate_screenshot_name(name)?;
    validate_screenshot_name(&payload.name)?;
    if screenshot_content_type(name) != screenshot_content_type(&payload.name) {
        return Err(json_error(
            StatusCode::BAD_REQUEST,
            "screenshot file type cannot change",
        ));
    }

    let game_dir = instance_game_dir(state, id)?;
    let screenshots_dir = game_dir.join("screenshots");
    let source = screenshots_dir.join(name);
    let target = screenshots_dir.join(&payload.name);
    run_blocking_filesystem(move || {
        require_screenshot_file(&source)?;
        if target_exists(&target) {
            return Err(json_error(
                StatusCode::CONFLICT,
                "screenshot already exists",
            ));
        }
        fs::rename(source, target).map_err(screenshot_file_write_error_response)
    })
    .await
    .map_err(resource_filesystem_task_error_response)??;
    Ok(serde_json::json!({ "status": "ok", "name": payload.name }))
}

pub(crate) async fn handle_delete_instance_screenshot(
    state: &AppState,
    id: &str,
    name: &str,
) -> Result<serde_json::Value, (StatusCode, Json<serde_json::Value>)> {
    validate_screenshot_name(name)?;

    let game_dir = instance_game_dir(state, id)?;
    let source = game_dir.join("screenshots").join(name);
    run_blocking_filesystem(move || {
        require_screenshot_file(&source)?;
        fs::remove_file(source).map_err(screenshot_file_write_error_response)
    })
    .await
    .map_err(resource_filesystem_task_error_response)??;
    Ok(serde_json::json!({ "status": "ok" }))
}

pub(crate) async fn handle_instance_logs(
    state: &AppState,
    id: &str,
) -> Result<Vec<InstanceLogInfo>, (StatusCode, Json<serde_json::Value>)> {
    let game_dir = instance_game_dir(state, id)?;
    run_resource_scan(move || {
        let mut budget = FilesystemScanBudget::new(INSTANCE_RESOURCE_SCAN_LIMITS);
        scan_instance_logs(&game_dir.join("logs"), &mut budget)
    })
    .await
}

pub(crate) async fn handle_instance_log_tail(
    state: &AppState,
    id: &str,
    name: &str,
) -> Result<InstanceLogTailResponse, (StatusCode, Json<serde_json::Value>)> {
    let game_dir = instance_game_dir(state, id)?;
    if !is_safe_resource_name(name) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "invalid log filename" })),
        ));
    }

    let path = game_dir.join("logs").join(name);
    let metadata = match tokio::fs::symlink_metadata(&path).await {
        Ok(metadata) if metadata.file_type().is_file() => metadata,
        Ok(_) => {
            return Err((
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": "log not found" })),
            ));
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return Err((
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": "log not found" })),
            ));
        }
        Err(error) => return Err(instance_log_read_error_response(error)),
    };
    let size = metadata.len();
    let start = size.saturating_sub(LOG_TAIL_LIMIT);
    let tail_len = (size - start) as usize;
    let mut file = tokio::fs::File::open(&path)
        .await
        .map_err(instance_log_read_error_response)?;
    file.seek(SeekFrom::Start(start))
        .await
        .map_err(instance_log_read_error_response)?;
    let mut bytes = vec![0_u8; tail_len];
    let mut bytes_read = 0;
    while bytes_read < bytes.len() {
        let read = file
            .read(&mut bytes[bytes_read..])
            .await
            .map_err(instance_log_read_error_response)?;
        if read == 0 {
            break;
        }
        bytes_read += read;
    }
    bytes.truncate(bytes_read);
    let text = String::from_utf8_lossy(&bytes).to_string();
    let text = crate::observability::sanitize_public_log_text(
        &text,
        crate::observability::RedactionAudience::UserVisible,
        LOG_TAIL_LIMIT as usize,
    );

    Ok(InstanceLogTailResponse {
        name: name.to_string(),
        size,
        truncated: start > 0,
        text,
    })
}

fn instance_game_dir(
    state: &AppState,
    id: &str,
) -> Result<PathBuf, (StatusCode, Json<serde_json::Value>)> {
    let instance = state.instances().get(id).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "instance not found" })),
        )
    })?;
    Ok(state.instances().game_dir(&instance.id))
}

async fn acquire_instance_resource_lifecycle(
    state: &AppState,
    id: &str,
) -> Result<crate::state::InstanceLifecycleLease, (StatusCode, Json<serde_json::Value>)> {
    state
        .try_acquire_instance_lifecycle(id)
        .await
        .ok_or_else(|| {
            json_error(
                StatusCode::CONFLICT,
                "another launch or content operation is already using this instance",
            )
        })
}

async fn reject_running_instance(
    state: &AppState,
    id: &str,
    resource_label: &'static str,
) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    if state.instances().get(id).is_none() {
        return Err(json_error(StatusCode::NOT_FOUND, "instance not found"));
    }
    if state.sessions().has_active_instance(id).await {
        return Err(json_error(
            StatusCode::CONFLICT,
            match resource_label {
                "mods" => "cannot change mods while the instance is running; stop the game first",
                _ => "cannot change worlds while the instance is running; stop the game first",
            },
        ));
    }
    Ok(())
}

pub(super) fn validate_world_name(name: &str) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    if name.trim() == name && is_safe_resource_name(name) {
        Ok(())
    } else {
        Err(json_error(StatusCode::BAD_REQUEST, "invalid world name"))
    }
}

fn require_world_dir(path: &FsPath) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    let metadata = fs::symlink_metadata(path).map_err(|error| match error.kind() {
        ErrorKind::NotFound => json_error(StatusCode::NOT_FOUND, "world not found"),
        _ => world_file_read_error_response(error),
    })?;
    if metadata.file_type().is_dir() {
        Ok(())
    } else {
        Err(json_error(StatusCode::NOT_FOUND, "world not found"))
    }
}

pub(super) fn validate_mod_name(name: &str) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    if name.trim() == name && is_mod_name(name) {
        Ok(())
    } else {
        Err(json_error(StatusCode::BAD_REQUEST, "invalid mod filename"))
    }
}

fn require_mod_file(path: &FsPath) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    let metadata = fs::symlink_metadata(path).map_err(|error| match error.kind() {
        ErrorKind::NotFound => json_error(StatusCode::NOT_FOUND, "mod not found"),
        _ => mod_file_read_error_response(error),
    })?;
    if metadata.file_type().is_file() {
        Ok(())
    } else {
        Err(json_error(StatusCode::NOT_FOUND, "mod not found"))
    }
}

pub(super) fn validate_screenshot_name(
    name: &str,
) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    if name.trim() == name && is_screenshot_name(name) {
        Ok(())
    } else {
        Err(json_error(
            StatusCode::BAD_REQUEST,
            "invalid screenshot filename",
        ))
    }
}

fn require_screenshot_file(path: &FsPath) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    let metadata = fs::symlink_metadata(path).map_err(|error| match error.kind() {
        ErrorKind::NotFound => json_error(StatusCode::NOT_FOUND, "screenshot not found"),
        _ => screenshot_file_read_error_response(error),
    })?;
    if metadata.file_type().is_file() {
        Ok(())
    } else {
        Err(json_error(StatusCode::NOT_FOUND, "screenshot not found"))
    }
}

async fn require_screenshot_file_async(
    path: &FsPath,
) -> Result<fs::Metadata, (StatusCode, Json<serde_json::Value>)> {
    let metadata = tokio::fs::symlink_metadata(path)
        .await
        .map_err(|error| match error.kind() {
            ErrorKind::NotFound => json_error(StatusCode::NOT_FOUND, "screenshot not found"),
            _ => screenshot_file_read_error_response(error),
        })?;
    if metadata.file_type().is_file() {
        Ok(metadata)
    } else {
        Err(json_error(StatusCode::NOT_FOUND, "screenshot not found"))
    }
}

fn target_exists(path: &FsPath) -> bool {
    fs::symlink_metadata(path).is_ok()
}

fn available_world_backup_name(
    backup_root: &FsPath,
    world_name: &str,
) -> Result<String, (StatusCode, Json<serde_json::Value>)> {
    let timestamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
    let base = format!("{world_name}-{timestamp}");
    if !target_exists(&backup_root.join(&base)) {
        return Ok(base);
    }
    for index in 2..=100 {
        let candidate = format!("{base}-{index}");
        if !target_exists(&backup_root.join(&candidate)) {
            return Ok(candidate);
        }
    }
    Err(json_error(
        StatusCode::CONFLICT,
        "world backup already exists; try again in a moment",
    ))
}

fn available_temp_world_backup_name(
    backup_root: &FsPath,
    backup: &str,
) -> std::io::Result<PathBuf> {
    let suffix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_nanos())
        .unwrap_or_default();
    let base = format!("{backup}.tmp-{suffix}");
    for index in 0..100 {
        let candidate = if index == 0 {
            base.clone()
        } else {
            format!("{base}-{index}")
        };
        if !is_safe_resource_name(&candidate) {
            return Err(std::io::Error::new(
                ErrorKind::InvalidInput,
                "invalid world backup temp name",
            ));
        }
        let path = backup_root.join(candidate);
        if !target_exists(&path) {
            return Ok(path);
        }
    }
    Err(std::io::Error::new(
        ErrorKind::AlreadyExists,
        "world backup temp path already exists",
    ))
}

pub(super) fn copy_world_backup_staged(
    source: &FsPath,
    backup_root: &FsPath,
    backup: &str,
) -> Result<(), FilesystemScanError> {
    if !is_safe_resource_name(backup) {
        return Err(
            std::io::Error::new(ErrorKind::InvalidInput, "invalid world backup name").into(),
        );
    }

    let target = backup_root.join(backup);
    if target_exists(&target) {
        return Err(
            std::io::Error::new(ErrorKind::AlreadyExists, "world backup already exists").into(),
        );
    }

    let temp = available_temp_world_backup_name(backup_root, backup)?;
    match copy_world_dir_bounded(source, &temp) {
        Ok(()) => {}
        Err(error) => {
            let _ = fs::remove_dir_all(&temp);
            return Err(error);
        }
    }

    if target_exists(&target) {
        let _ = fs::remove_dir_all(&temp);
        return Err(
            std::io::Error::new(ErrorKind::AlreadyExists, "world backup already exists").into(),
        );
    }

    match fs::rename(&temp, &target) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = fs::remove_dir_all(&temp);
            Err(error.into())
        }
    }
}

fn copy_world_dir_bounded(source: &FsPath, target: &FsPath) -> Result<(), FilesystemScanError> {
    let mut budget = FilesystemScanBudget::new(WORLD_BACKUP_SCAN_LIMITS);
    copy_world_dir_bounded_inner(source, target, 0, &mut budget)
}

fn copy_world_dir_bounded_inner(
    source: &FsPath,
    target: &FsPath,
    depth: usize,
    budget: &mut FilesystemScanBudget,
) -> Result<(), FilesystemScanError> {
    if depth > WORLD_BACKUP_MAX_DEPTH {
        return Err(FilesystemScanError::DepthLimit);
    }

    fs::create_dir_all(target)?;
    for entry in budget.read_directory(source)? {
        let target_path = target.join(&entry.name);
        match entry.kind {
            FilesystemEntryKind::Directory => {
                copy_world_dir_bounded_inner(&entry.path, &target_path, depth + 1, budget)?;
            }
            FilesystemEntryKind::File => {
                budget.account_file_bytes(entry.metadata.len())?;
                copy_regular_file_exact(&entry.path, &target_path, entry.metadata.len())?;
            }
        }
    }
    Ok(())
}

fn copy_regular_file_exact(source: &FsPath, target: &FsPath, expected: u64) -> std::io::Result<()> {
    let source_metadata = fs::symlink_metadata(source)?;
    if !source_metadata.file_type().is_file() || source_metadata.len() != expected {
        return Err(std::io::Error::new(
            ErrorKind::WouldBlock,
            "world backup source changed before copy",
        ));
    }
    let input = open_regular_file_no_follow(source)?;
    let opened_metadata = input.metadata()?;
    if opened_metadata.file_type().is_symlink()
        || !opened_metadata.is_file()
        || opened_metadata.len() != expected
    {
        return Err(std::io::Error::new(
            ErrorKind::WouldBlock,
            "world backup source changed while opening",
        ));
    }
    let mut output = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(target)?;
    let copied = std::io::copy(&mut input.take(expected.saturating_add(1)), &mut output)?;
    if copied != expected {
        return Err(std::io::Error::new(
            ErrorKind::WouldBlock,
            "world backup source changed during copy",
        ));
    }
    output.flush()?;
    Ok(())
}

#[cfg(unix)]
fn open_regular_file_no_follow(path: &FsPath) -> std::io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;

    fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

#[cfg(windows)]
fn open_regular_file_no_follow(path: &FsPath) -> std::io::Result<fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;

    fs::OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
}

#[cfg(not(any(unix, windows)))]
fn open_regular_file_no_follow(path: &FsPath) -> std::io::Result<fs::File> {
    fs::File::open(path)
}

fn json_error(status: StatusCode, message: &'static str) -> (StatusCode, Json<serde_json::Value>) {
    (status, Json(serde_json::json!({ "error": message })))
}

fn world_file_read_error_response(_error: std::io::Error) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({
            "error": "Could not read world files. Check instance folder permissions and try again."
        })),
    )
}

fn world_file_write_error_response(
    _error: std::io::Error,
) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({
            "error": "Could not update world files. Check instance folder permissions and try again."
        })),
    )
}

fn world_backup_copy_error_response(
    error: FilesystemScanError,
) -> (StatusCode, Json<serde_json::Value>) {
    if error.is_capacity_limit() {
        return json_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "world backup exceeds safe size or file-count limits",
        );
    }
    if error.is_unsupported_layout() {
        return json_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "world backup contains unsupported filesystem entries",
        );
    }
    if matches!(
        &error,
        FilesystemScanError::Io(error) if error.kind() == ErrorKind::AlreadyExists
    ) {
        return json_error(
            StatusCode::CONFLICT,
            "world backup already exists; try again in a moment",
        );
    }
    if matches!(
        &error,
        FilesystemScanError::Io(error) if error.kind() == ErrorKind::WouldBlock
    ) {
        return json_error(
            StatusCode::CONFLICT,
            "world files changed during backup; refresh and try again",
        );
    }
    world_file_write_error_response(std::io::Error::other("world backup filesystem task failed"))
}

fn mod_file_read_error_response(_error: std::io::Error) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({
            "error": "Could not read mod files. Check instance folder permissions and try again."
        })),
    )
}

fn mod_manifest_error_response(
    _error: axial_content::ContentError,
) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({
            "error": "Could not update mod files. Check instance folder permissions and try again."
        })),
    )
}

fn mod_content_mutation_error_response(
    error: ModFileMutationError,
) -> (StatusCode, Json<serde_json::Value>) {
    match error {
        ModFileMutationError::NotFound => json_error(StatusCode::NOT_FOUND, "mod not found"),
        ModFileMutationError::Conflict => json_error(
            StatusCode::CONFLICT,
            "mod files changed while they were being updated; refresh and try again",
        ),
        ModFileMutationError::Failed(error) => mod_manifest_error_response(error),
    }
}

pub(super) fn screenshot_file_read_error_response(
    _error: std::io::Error,
) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({
            "error": "Could not read screenshot files. Check instance folder permissions and try again."
        })),
    )
}

pub(super) fn screenshot_file_write_error_response(
    _error: std::io::Error,
) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({
            "error": "Could not update screenshot files. Check instance folder permissions and try again."
        })),
    )
}

fn scan_instance_resources(
    game_dir: &FsPath,
) -> Result<InstanceResourcesResponse, FilesystemScanError> {
    let mut budget = FilesystemScanBudget::new(INSTANCE_RESOURCE_SCAN_LIMITS);
    let worlds = scan_instance_worlds(&game_dir.join("saves"), &mut budget)?;
    let mods = scan_instance_mods(&game_dir.join("mods"), &mut budget)?;
    let screenshots = scan_instance_screenshots(&game_dir.join("screenshots"), &mut budget)?;
    let logs = scan_instance_logs(&game_dir.join("logs"), &mut budget)?;
    Ok(InstanceResourcesResponse {
        worlds_count: worlds.len(),
        mods_count: mods.len(),
        screenshots_count: screenshots.len(),
        logs_count: logs.len(),
        worlds,
        mods,
        screenshots,
        logs,
    })
}

fn scan_instance_worlds(
    saves_dir: &FsPath,
    budget: &mut FilesystemScanBudget,
) -> Result<Vec<InstanceWorldInfo>, FilesystemScanError> {
    let mut worlds = Vec::new();
    for entry in budget.read_optional_directory(saves_dir)? {
        if entry.kind != FilesystemEntryKind::Directory {
            continue;
        }
        worlds.push(InstanceWorldInfo {
            name: entry.name.to_string_lossy().into_owned(),
            size: budget.directory_size(&entry.path)?,
            modified_at: modified_at(Some(&entry.metadata)),
        });
    }
    worlds.sort_by(|a, b| {
        b.modified_at
            .cmp(&a.modified_at)
            .then_with(|| a.name.cmp(&b.name))
    });
    Ok(worlds)
}

fn scan_instance_mods(
    mods_dir: &FsPath,
    budget: &mut FilesystemScanBudget,
) -> Result<Vec<InstanceModInfo>, FilesystemScanError> {
    let mut mods = Vec::new();
    for entry in budget.read_optional_directory(mods_dir)? {
        if entry.kind != FilesystemEntryKind::File {
            continue;
        }
        let name = entry.name.to_string_lossy().into_owned();
        let lower = name.to_ascii_lowercase();
        let enabled = lower.ends_with(".jar");
        if !enabled && !lower.ends_with(".jar.disabled") {
            continue;
        }
        budget.account_file_bytes(entry.metadata.len())?;
        mods.push(InstanceModInfo {
            name,
            size: entry.metadata.len(),
            modified_at: modified_at(Some(&entry.metadata)),
            enabled,
        });
    }
    mods.sort_by(|a, b| {
        a.name
            .to_ascii_lowercase()
            .cmp(&b.name.to_ascii_lowercase())
    });
    Ok(mods)
}

fn scan_instance_screenshots(
    screenshots_dir: &FsPath,
    budget: &mut FilesystemScanBudget,
) -> Result<Vec<InstanceScreenshotInfo>, FilesystemScanError> {
    let mut screenshots = Vec::new();
    for entry in budget.read_optional_directory(screenshots_dir)? {
        if entry.kind != FilesystemEntryKind::File {
            continue;
        }
        let name = entry.name.to_string_lossy().into_owned();
        if !is_screenshot_name(&name) {
            continue;
        }
        budget.account_file_bytes(entry.metadata.len())?;
        screenshots.push(InstanceScreenshotInfo {
            name,
            size: entry.metadata.len(),
            modified_at: modified_at(Some(&entry.metadata)),
        });
    }
    screenshots.sort_by(|a, b| {
        b.modified_at
            .cmp(&a.modified_at)
            .then_with(|| a.name.cmp(&b.name))
    });
    Ok(screenshots)
}

pub(super) fn scan_instance_logs(
    logs_dir: &FsPath,
    budget: &mut FilesystemScanBudget,
) -> Result<Vec<InstanceLogInfo>, FilesystemScanError> {
    let mut logs = Vec::new();
    for entry in budget.read_optional_directory(logs_dir)? {
        if entry.kind != FilesystemEntryKind::File {
            continue;
        }
        let name = entry.name.to_string_lossy().into_owned();
        if !is_safe_resource_name(&name) {
            continue;
        }
        budget.account_file_bytes(entry.metadata.len())?;
        logs.push(InstanceLogInfo {
            name,
            size: entry.metadata.len(),
            modified_at: modified_at(Some(&entry.metadata)),
        });
    }
    logs.sort_by(|a, b| {
        latest_log_rank(&a.name)
            .cmp(&latest_log_rank(&b.name))
            .then_with(|| b.modified_at.cmp(&a.modified_at))
            .then_with(|| a.name.cmp(&b.name))
    });
    Ok(logs)
}

fn latest_log_rank(name: &str) -> u8 {
    if name.eq_ignore_ascii_case("latest.log") {
        0
    } else {
        1
    }
}

fn is_screenshot_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    [".png", ".jpg", ".jpeg", ".webp"]
        .iter()
        .any(|suffix| lower.ends_with(suffix))
        && is_safe_resource_name(name)
}

fn is_mod_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    (lower.ends_with(".jar") || lower.ends_with(".jar.disabled")) && is_safe_resource_name(name)
}

fn requested_mod_filename(name: &str, enabled: bool) -> String {
    let lower = name.to_ascii_lowercase();
    if enabled && lower.ends_with(".disabled") {
        name[..name.len() - ".disabled".len()].to_string()
    } else if !enabled && !lower.ends_with(".disabled") {
        format!("{name}.disabled")
    } else {
        name.to_string()
    }
}

pub(super) fn screenshot_content_type(name: &str) -> Option<&'static str> {
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".png") {
        Some("image/png")
    } else if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
        Some("image/jpeg")
    } else if lower.ends_with(".webp") {
        Some("image/webp")
    } else {
        None
    }
}

pub(super) fn is_safe_resource_name(name: &str) -> bool {
    !name.is_empty()
        && !name.trim().is_empty()
        && name.trim() == name
        && name != "."
        && name != ".."
        && !name.starts_with('.')
        && !name.contains('/')
        && !name.contains('\\')
        && !name.chars().any(char::is_control)
        && FsPath::new(name) == FsPath::new(name).components().as_path()
}

fn modified_at(metadata: Option<&fs::Metadata>) -> String {
    metadata
        .and_then(|metadata| metadata.modified().ok())
        .map(|time| chrono::DateTime::<chrono::Utc>::from(time).to_rfc3339())
        .unwrap_or_default()
}

async fn run_resource_scan<T, Work>(work: Work) -> Result<T, (StatusCode, Json<serde_json::Value>)>
where
    T: Send + 'static,
    Work: FnOnce() -> Result<T, FilesystemScanError> + Send + 'static,
{
    run_blocking_filesystem(work)
        .await
        .map_err(resource_filesystem_task_error_response)?
        .map_err(resource_scan_error_response)
}

fn resource_scan_error_response(
    error: FilesystemScanError,
) -> (StatusCode, Json<serde_json::Value>) {
    if error.is_capacity_limit() {
        return json_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "instance resources exceed safe scan limits",
        );
    }
    if error.is_unsupported_layout() {
        return json_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "instance resources contain unsupported filesystem entries",
        );
    }
    json_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "Could not read instance resources. Check instance folder permissions and try again.",
    )
}

fn resource_filesystem_task_error_response(
    _error: BlockingFilesystemTaskError,
) -> (StatusCode, Json<serde_json::Value>) {
    json_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "Could not complete the instance filesystem operation. Try again.",
    )
}
