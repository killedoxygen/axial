use crate::state::{AppState, UpdateOperationAdmissionError, UpdateOperationLease};
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
    io::{ErrorKind, SeekFrom},
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
pub(super) const WORLD_BACKUP_MAX_DEPTH: usize = 64;
const WORLD_BACKUP_MAX_ENTRIES: usize = 100_000;
const WORLD_BACKUP_MAX_BYTES: u64 = 50 * 1024 * 1024 * 1024;
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
    let worlds = scan_instance_worlds(&game_dir.join("saves"));
    let mods = scan_instance_mods(&game_dir.join("mods"));
    let screenshots = scan_instance_screenshots(&game_dir.join("screenshots"));
    let logs = scan_instance_logs(&game_dir.join("logs"));

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

pub(crate) async fn handle_instance_worlds(
    state: &AppState,
    id: &str,
) -> Result<Vec<InstanceWorldInfo>, (StatusCode, Json<serde_json::Value>)> {
    let game_dir = instance_game_dir(state, id)?;
    Ok(scan_instance_worlds(&game_dir.join("saves")))
}

pub(crate) async fn handle_rename_instance_world(
    state: &AppState,
    id: &str,
    name: &str,
    payload: RenameWorldRequest,
) -> Result<serde_json::Value, (StatusCode, Json<serde_json::Value>)> {
    reject_running_instance(state, id, "worlds").await?;
    validate_world_name(name)?;
    validate_world_name(&payload.name)?;

    let game_dir = instance_game_dir(state, id)?;
    let saves_dir = game_dir.join("saves");
    let source = saves_dir.join(name);
    let target = saves_dir.join(&payload.name);
    require_world_dir(&source)?;
    if target_exists(&target) {
        return Err(json_error(StatusCode::CONFLICT, "world already exists"));
    }

    fs::rename(source, target).map_err(world_file_write_error_response)?;
    Ok(serde_json::json!({ "status": "ok", "name": payload.name }))
}

pub(crate) async fn handle_delete_instance_world(
    state: &AppState,
    id: &str,
    name: &str,
) -> Result<serde_json::Value, (StatusCode, Json<serde_json::Value>)> {
    reject_running_instance(state, id, "worlds").await?;
    validate_world_name(name)?;

    let game_dir = instance_game_dir(state, id)?;
    let source = game_dir.join("saves").join(name);
    require_world_dir(&source)?;
    fs::remove_dir_all(source).map_err(world_file_write_error_response)?;
    Ok(serde_json::json!({ "status": "ok" }))
}

pub(crate) async fn handle_backup_instance_world(
    state: &AppState,
    id: &str,
    name: &str,
) -> Result<WorldBackupResponse, (StatusCode, Json<serde_json::Value>)> {
    reject_running_instance(state, id, "worlds").await?;
    validate_world_name(name)?;

    let game_dir = instance_game_dir(state, id)?;
    let source = game_dir.join("saves").join(name);
    require_world_dir(&source)?;

    let backup_root = game_dir.join("backups").join("worlds");
    fs::create_dir_all(&backup_root).map_err(world_file_write_error_response)?;
    let backup = available_world_backup_name(&backup_root, name)?;
    copy_world_backup_staged(&source, &backup_root, &backup)
        .map_err(world_file_write_error_response)?;

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
    Ok(scan_instance_mods(&game_dir.join("mods")))
}

pub(crate) async fn handle_update_instance_mod(
    state: &AppState,
    id: &str,
    name: &str,
    payload: UpdateModRequest,
) -> Result<serde_json::Value, (StatusCode, Json<serde_json::Value>)> {
    let _update_admission = admit_instance_mod_mutation(state)?;
    let _lifecycle_guard = state
        .try_acquire_instance_lifecycle(id)
        .await
        .ok_or_else(|| {
            json_error(
                StatusCode::CONFLICT,
                "another launch or content operation is already using this instance",
            )
        })?;
    reject_running_instance(state, id, "mods").await?;
    validate_mod_name(name)?;

    let game_dir = instance_game_dir(state, id)?;
    let mods_dir = game_dir.join("mods");
    let source = mods_dir.join(name);
    require_mod_file(&source)?;
    let requested_name = requested_mod_filename(name, payload.enabled);
    if requested_name != name && target_exists(&mods_dir.join(&requested_name)) {
        return Err(json_error(StatusCode::CONFLICT, "mod already exists"));
    }
    let _mutation = state.admit_managed_artifact_mutation().map_err(|error| {
        mod_manifest_error_response(axial_content::ContentError::Io(std::io::Error::other(
            error.to_string(),
        )))
    })?;
    let outcome = toggle_mod_file(&game_dir, name, payload.enabled)
        .map_err(mod_content_mutation_error_response)?;
    Ok(serde_json::json!({ "status": "ok", "name": outcome.filename, "enabled": payload.enabled }))
}

pub(crate) async fn handle_delete_instance_mod(
    state: &AppState,
    id: &str,
    name: &str,
) -> Result<serde_json::Value, (StatusCode, Json<serde_json::Value>)> {
    let _update_admission = admit_instance_mod_mutation(state)?;
    let _lifecycle_guard = state
        .try_acquire_instance_lifecycle(id)
        .await
        .ok_or_else(|| {
            json_error(
                StatusCode::CONFLICT,
                "another launch or content operation is already using this instance",
            )
        })?;
    reject_running_instance(state, id, "mods").await?;
    validate_mod_name(name)?;

    let game_dir = instance_game_dir(state, id)?;
    let source = game_dir.join("mods").join(name);
    require_mod_file(&source)?;
    match delete_local_mod_file(&game_dir, name).map_err(mod_content_mutation_error_response)? {
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
    Ok(scan_instance_screenshots(&game_dir.join("screenshots")))
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
    require_screenshot_file(&source)?;
    if target_exists(&target) {
        return Err(json_error(
            StatusCode::CONFLICT,
            "screenshot already exists",
        ));
    }

    fs::rename(source, target).map_err(screenshot_file_write_error_response)?;
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
    require_screenshot_file(&source)?;
    fs::remove_file(source).map_err(screenshot_file_write_error_response)?;
    Ok(serde_json::json!({ "status": "ok" }))
}

pub(crate) async fn handle_instance_logs(
    state: &AppState,
    id: &str,
) -> Result<Vec<InstanceLogInfo>, (StatusCode, Json<serde_json::Value>)> {
    let game_dir = instance_game_dir(state, id)?;
    Ok(scan_instance_logs(&game_dir.join("logs")))
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
    let metadata = match tokio::fs::metadata(&path).await {
        Ok(metadata) if metadata.is_file() => metadata,
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
) -> std::io::Result<()> {
    if !is_safe_resource_name(backup) {
        return Err(std::io::Error::new(
            ErrorKind::InvalidInput,
            "invalid world backup name",
        ));
    }

    let target = backup_root.join(backup);
    if target_exists(&target) {
        return Err(std::io::Error::new(
            ErrorKind::AlreadyExists,
            "world backup already exists",
        ));
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
        return Err(std::io::Error::new(
            ErrorKind::AlreadyExists,
            "world backup already exists",
        ));
    }

    match fs::rename(&temp, &target) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = fs::remove_dir_all(&temp);
            Err(error)
        }
    }
}

#[derive(Debug)]
struct CopyBudget {
    entries: usize,
    bytes: u64,
}

fn copy_world_dir_bounded(source: &FsPath, target: &FsPath) -> std::io::Result<()> {
    let mut budget = CopyBudget {
        entries: 0,
        bytes: 0,
    };
    copy_world_dir_bounded_inner(source, target, 0, &mut budget)
}

fn copy_world_dir_bounded_inner(
    source: &FsPath,
    target: &FsPath,
    depth: usize,
    budget: &mut CopyBudget,
) -> std::io::Result<()> {
    if depth > WORLD_BACKUP_MAX_DEPTH {
        return Err(std::io::Error::new(
            ErrorKind::InvalidInput,
            "world backup is too deeply nested",
        ));
    }

    let metadata = fs::symlink_metadata(source)?;
    if !metadata.file_type().is_dir() {
        return Err(std::io::Error::new(
            ErrorKind::InvalidInput,
            "world backup source is not a directory",
        ));
    }

    fs::create_dir_all(target)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            return Err(std::io::Error::new(
                ErrorKind::InvalidInput,
                "world backup cannot include links",
            ));
        }

        budget.entries = budget.entries.saturating_add(1);
        if budget.entries > WORLD_BACKUP_MAX_ENTRIES {
            return Err(std::io::Error::new(
                ErrorKind::InvalidInput,
                "world backup has too many files",
            ));
        }

        let entry_path = entry.path();
        let target_path = target.join(entry.file_name());
        if file_type.is_dir() {
            copy_world_dir_bounded_inner(&entry_path, &target_path, depth + 1, budget)?;
        } else if file_type.is_file() {
            let len = entry.metadata()?.len();
            budget.bytes = budget.bytes.saturating_add(len);
            if budget.bytes > WORLD_BACKUP_MAX_BYTES {
                return Err(std::io::Error::new(
                    ErrorKind::InvalidInput,
                    "world backup is too large",
                ));
            }
            fs::copy(entry_path, target_path)?;
        }
    }
    Ok(())
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

fn scan_instance_worlds(saves_dir: &FsPath) -> Vec<InstanceWorldInfo> {
    let mut worlds = fs::read_dir(saves_dir)
        .into_iter()
        .flat_map(|entries| entries.filter_map(Result::ok))
        .filter(|entry| entry.path().is_dir())
        .map(|entry| {
            let path = entry.path();
            let metadata = entry.metadata().ok();
            InstanceWorldInfo {
                name: entry.file_name().to_string_lossy().to_string(),
                size: dir_size(&path),
                modified_at: modified_at(metadata.as_ref()),
            }
        })
        .collect::<Vec<_>>();
    worlds.sort_by(|a, b| {
        b.modified_at
            .cmp(&a.modified_at)
            .then_with(|| a.name.cmp(&b.name))
    });
    worlds
}

fn scan_instance_mods(mods_dir: &FsPath) -> Vec<InstanceModInfo> {
    let mut mods = fs::read_dir(mods_dir)
        .into_iter()
        .flat_map(|entries| entries.filter_map(Result::ok))
        .filter(|entry| entry.path().is_file())
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();
            let lower = name.to_ascii_lowercase();
            let enabled = lower.ends_with(".jar");
            if !enabled && !lower.ends_with(".jar.disabled") {
                return None;
            }
            let metadata = entry.metadata().ok();
            Some(InstanceModInfo {
                name,
                size: metadata.as_ref().map_or(0, fs::Metadata::len),
                modified_at: modified_at(metadata.as_ref()),
                enabled,
            })
        })
        .collect::<Vec<_>>();
    mods.sort_by(|a, b| {
        a.name
            .to_ascii_lowercase()
            .cmp(&b.name.to_ascii_lowercase())
    });
    mods
}

fn scan_instance_screenshots(screenshots_dir: &FsPath) -> Vec<InstanceScreenshotInfo> {
    let mut screenshots = fs::read_dir(screenshots_dir)
        .into_iter()
        .flat_map(|entries| entries.filter_map(Result::ok))
        .filter(|entry| entry.path().is_file())
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();
            if !is_screenshot_name(&name) {
                return None;
            }
            let metadata = entry.metadata().ok();
            Some(InstanceScreenshotInfo {
                name,
                size: metadata.as_ref().map_or(0, fs::Metadata::len),
                modified_at: modified_at(metadata.as_ref()),
            })
        })
        .collect::<Vec<_>>();
    screenshots.sort_by(|a, b| {
        b.modified_at
            .cmp(&a.modified_at)
            .then_with(|| a.name.cmp(&b.name))
    });
    screenshots
}

pub(super) fn scan_instance_logs(logs_dir: &FsPath) -> Vec<InstanceLogInfo> {
    let mut logs = fs::read_dir(logs_dir)
        .into_iter()
        .flat_map(|entries| entries.filter_map(Result::ok))
        .filter(|entry| entry.path().is_file())
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();
            if !is_safe_resource_name(&name) {
                return None;
            }
            let metadata = entry.metadata().ok();
            Some(InstanceLogInfo {
                name,
                size: metadata.as_ref().map_or(0, fs::Metadata::len),
                modified_at: modified_at(metadata.as_ref()),
            })
        })
        .collect::<Vec<_>>();
    logs.sort_by(|a, b| {
        latest_log_rank(&a.name)
            .cmp(&latest_log_rank(&b.name))
            .then_with(|| b.modified_at.cmp(&a.modified_at))
            .then_with(|| a.name.cmp(&b.name))
    });
    logs
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

fn dir_size(path: &FsPath) -> u64 {
    let mut total = 0_u64;
    if let Ok(entries) = fs::read_dir(path) {
        for entry in entries.filter_map(Result::ok) {
            if let Ok(metadata) = entry.metadata() {
                if metadata.is_dir() {
                    total += dir_size(&entry.path());
                } else {
                    total += metadata.len();
                }
            }
        }
    }
    total
}
