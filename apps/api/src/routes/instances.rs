use crate::state::AppState;
use async_stream::stream;
use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::{Path, Query, State},
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post, put},
};
use croopor_config::{EnrichedInstance, InstanceStoreError};
use croopor_minecraft::scan_versions;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{ErrorKind, SeekFrom},
    path::{Path as FsPath, PathBuf},
};
use tokio::io::{AsyncReadExt, AsyncSeekExt};

const LOG_TAIL_LIMIT: u64 = 128 * 1024;
const SCREENSHOT_FILE_MAX_BYTES: u64 = 32 * 1024 * 1024;
const SCREENSHOT_FILE_STREAM_CHUNK_BYTES: usize = 64 * 1024;
const WORLD_BACKUP_MAX_DEPTH: usize = 64;
const WORLD_BACKUP_MAX_ENTRIES: usize = 100_000;
const WORLD_BACKUP_MAX_BYTES: u64 = 50 * 1024 * 1024 * 1024;
const INSTANCE_LOG_READ_ERROR_MESSAGE: &str =
    "Could not read the instance log. Check instance folder permissions and try again.";

const INSTANCE_SUBFOLDERS: [&str; 7] = [
    "mods",
    "saves",
    "resourcepacks",
    "shaderpacks",
    "config",
    "screenshots",
    "logs",
];

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

#[derive(Debug, Serialize)]
struct InstancesResponse {
    instances: Vec<EnrichedInstance>,
    last_instance_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct InstanceWorldInfo {
    name: String,
    size: u64,
    modified_at: String,
}

#[derive(Debug, Serialize)]
struct InstanceModInfo {
    name: String,
    size: u64,
    modified_at: String,
    enabled: bool,
}

#[derive(Debug, Serialize)]
struct InstanceScreenshotInfo {
    name: String,
    size: u64,
    modified_at: String,
}

#[derive(Debug, Serialize)]
struct InstanceLogInfo {
    name: String,
    size: u64,
    modified_at: String,
}

#[derive(Debug, Serialize)]
struct InstanceResourcesResponse {
    worlds: Vec<InstanceWorldInfo>,
    mods: Vec<InstanceModInfo>,
    screenshots: Vec<InstanceScreenshotInfo>,
    logs: Vec<InstanceLogInfo>,
    worlds_count: usize,
    mods_count: usize,
    screenshots_count: usize,
    logs_count: usize,
}

#[derive(Debug, Serialize)]
struct InstanceLogTailResponse {
    name: String,
    size: u64,
    truncated: bool,
    text: String,
}

#[derive(Debug, Deserialize)]
struct RenameWorldRequest {
    name: String,
}

#[derive(Debug, Deserialize)]
struct RenameScreenshotRequest {
    name: String,
}

#[derive(Debug, Deserialize)]
struct UpdateModRequest {
    enabled: bool,
}

#[derive(Debug, Serialize)]
struct WorldBackupResponse {
    status: &'static str,
    backup: String,
    location: String,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/api/v1/instances",
            get(handle_list_instances).post(handle_create_instance),
        )
        .route(
            "/api/v1/instances/{id}",
            get(handle_get_instance)
                .put(handle_update_instance)
                .delete(handle_delete_instance),
        )
        .route(
            "/api/v1/instances/{id}/duplicate",
            post(handle_duplicate_instance),
        )
        .route(
            "/api/v1/instances/{id}/resources",
            get(handle_instance_resources),
        )
        .route("/api/v1/instances/{id}/worlds", get(handle_instance_worlds))
        .route(
            "/api/v1/instances/{id}/worlds/{name}",
            put(handle_rename_instance_world).delete(handle_delete_instance_world),
        )
        .route(
            "/api/v1/instances/{id}/worlds/{name}/backup",
            post(handle_backup_instance_world),
        )
        .route("/api/v1/instances/{id}/mods", get(handle_instance_mods))
        .route(
            "/api/v1/instances/{id}/mods/{name}",
            put(handle_update_instance_mod).delete(handle_delete_instance_mod),
        )
        .route(
            "/api/v1/instances/{id}/screenshots",
            get(handle_instance_screenshots),
        )
        .route(
            "/api/v1/instances/{id}/screenshots/{name}",
            put(handle_rename_instance_screenshot).delete(handle_delete_instance_screenshot),
        )
        .route(
            "/api/v1/instances/{id}/screenshots/{name}/file",
            get(handle_instance_screenshot_file),
        )
        .route("/api/v1/instances/{id}/logs", get(handle_instance_logs))
        .route(
            "/api/v1/instances/{id}/logs/{name}",
            get(handle_instance_log_tail),
        )
        .route(
            "/api/v1/instances/{id}/open-folder",
            post(handle_open_instance_folder),
        )
}

async fn handle_list_instances(State(state): State<AppState>) -> Json<InstancesResponse> {
    let versions = state
        .library_dir()
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .and_then(|path| scan_versions(&path).ok())
        .unwrap_or_default();

    Json(InstancesResponse {
        instances: state.instances().enrich(&versions),
        last_instance_id: state.instances().last_instance_id(),
    })
}

async fn handle_get_instance(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<croopor_config::Instance>, (axum::http::StatusCode, Json<serde_json::Value>)> {
    let instance = state.instances().get(&id);

    match instance {
        Some(instance) => Ok(Json(instance)),
        None => Err((
            axum::http::StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "instance not found" })),
        )),
    }
}

#[derive(Debug, Deserialize)]
struct CreateInstanceRequest {
    name: String,
    version_id: String,
    #[serde(default)]
    icon: String,
    #[serde(default)]
    accent: String,
}

async fn handle_create_instance(
    State(state): State<AppState>,
    Json(payload): Json<CreateInstanceRequest>,
) -> Result<Json<croopor_config::Instance>, (StatusCode, Json<serde_json::Value>)> {
    let mc_dir = state.library_dir().map(PathBuf::from);
    state
        .instances()
        .add(
            payload.name,
            payload.version_id,
            payload.icon,
            payload.accent,
            mc_dir.as_deref(),
        )
        .map(Json)
        .map_err(|error| instance_write_error_response(InstanceWriteOperation::Create, error))
}

#[derive(Debug, Default, Deserialize)]
struct DuplicateInstanceRequest {
    name: Option<String>,
}

async fn handle_duplicate_instance(
    State(state): State<AppState>,
    Path(id): Path<String>,
    payload: Option<Json<DuplicateInstanceRequest>>,
) -> Result<Json<croopor_config::Instance>, (StatusCode, Json<serde_json::Value>)> {
    let payload = payload.map(|Json(payload)| payload).unwrap_or_default();
    let mc_dir = state.library_dir().map(PathBuf::from);
    state
        .instances()
        .duplicate(&id, payload.name, mc_dir.as_deref())
        .map(Json)
        .map_err(|error| instance_write_error_response(InstanceWriteOperation::Duplicate, error))
}

#[derive(Debug, Default, Deserialize)]
struct InstancePatch {
    name: Option<String>,
    version_id: Option<String>,
    art_seed: Option<u32>,
    max_memory_mb: Option<i32>,
    min_memory_mb: Option<i32>,
    java_path: Option<String>,
    window_width: Option<i32>,
    window_height: Option<i32>,
    jvm_preset: Option<String>,
    performance_mode: Option<String>,
    extra_jvm_args: Option<String>,
    icon: Option<String>,
    accent: Option<String>,
}

async fn handle_update_instance(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(patch): Json<InstancePatch>,
) -> Result<Json<croopor_config::Instance>, (StatusCode, Json<serde_json::Value>)> {
    let mut instance = state.instances().get(&id).ok_or_else(|| {
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
        instance.jvm_preset = jvm_preset;
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
        .map(Json)
        .map_err(|error| instance_write_error_response(InstanceWriteOperation::Update, error))
}

#[derive(Debug, Deserialize)]
struct OpenFolderQuery {
    sub: Option<String>,
}

async fn handle_open_instance_folder(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<OpenFolderQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let instance = state.instances().get(&id).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "instance not found" })),
        )
    })?;

    let game_dir = state.instances().game_dir(&instance.id);
    let dir = resolve_instance_folder(&game_dir, query.sub.as_deref()).map_err(|message| {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": message })),
        )
    })?;

    std::fs::create_dir_all(&dir).map_err(instance_folder_prepare_error_response)?;
    open_path(&dir).map_err(instance_folder_open_error_response)?;

    Ok(Json(serde_json::json!({ "status": "ok" })))
}

fn instance_folder_prepare_error_response(
    _error: std::io::Error,
) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({
            "error": "Could not prepare the instance folder. Check app data permissions and try again."
        })),
    )
}

fn instance_folder_open_error_response(
    _error: std::io::Error,
) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({
            "error": "Could not open the instance folder. Check desktop permissions and try again."
        })),
    )
}

fn instance_log_read_error_response(
    _error: std::io::Error,
) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({
            "error": INSTANCE_LOG_READ_ERROR_MESSAGE
        })),
    )
}

fn resolve_instance_folder(game_dir: &FsPath, sub: Option<&str>) -> Result<PathBuf, &'static str> {
    match sub {
        None => Ok(game_dir.to_path_buf()),
        Some(subfolder) if INSTANCE_SUBFOLDERS.contains(&subfolder) => Ok(game_dir.join(subfolder)),
        Some(_) => Err("invalid instance folder"),
    }
}

async fn handle_instance_resources(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<InstanceResourcesResponse>, (StatusCode, Json<serde_json::Value>)> {
    let game_dir = instance_game_dir(&state, &id)?;
    let worlds = scan_instance_worlds(&game_dir.join("saves"));
    let mods = scan_instance_mods(&game_dir.join("mods"));
    let screenshots = scan_instance_screenshots(&game_dir.join("screenshots"));
    let logs = scan_instance_logs(&game_dir.join("logs"));

    Ok(Json(InstanceResourcesResponse {
        worlds_count: worlds.len(),
        mods_count: mods.len(),
        screenshots_count: screenshots.len(),
        logs_count: logs.len(),
        worlds,
        mods,
        screenshots,
        logs,
    }))
}

async fn handle_instance_worlds(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Vec<InstanceWorldInfo>>, (StatusCode, Json<serde_json::Value>)> {
    let game_dir = instance_game_dir(&state, &id)?;
    Ok(Json(scan_instance_worlds(&game_dir.join("saves"))))
}

async fn handle_rename_instance_world(
    State(state): State<AppState>,
    Path((id, name)): Path<(String, String)>,
    Json(payload): Json<RenameWorldRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    reject_running_instance(&state, &id, "worlds").await?;
    validate_world_name(&name)?;
    validate_world_name(&payload.name)?;

    let game_dir = instance_game_dir(&state, &id)?;
    let saves_dir = game_dir.join("saves");
    let source = saves_dir.join(&name);
    let target = saves_dir.join(&payload.name);
    require_world_dir(&source)?;
    if target_exists(&target) {
        return Err(json_error(StatusCode::CONFLICT, "world already exists"));
    }

    fs::rename(source, target).map_err(world_file_write_error_response)?;
    Ok(Json(
        serde_json::json!({ "status": "ok", "name": payload.name }),
    ))
}

async fn handle_delete_instance_world(
    State(state): State<AppState>,
    Path((id, name)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    reject_running_instance(&state, &id, "worlds").await?;
    validate_world_name(&name)?;

    let game_dir = instance_game_dir(&state, &id)?;
    let source = game_dir.join("saves").join(&name);
    require_world_dir(&source)?;
    fs::remove_dir_all(source).map_err(world_file_write_error_response)?;
    Ok(Json(serde_json::json!({ "status": "ok" })))
}

async fn handle_backup_instance_world(
    State(state): State<AppState>,
    Path((id, name)): Path<(String, String)>,
) -> Result<Json<WorldBackupResponse>, (StatusCode, Json<serde_json::Value>)> {
    reject_running_instance(&state, &id, "worlds").await?;
    validate_world_name(&name)?;

    let game_dir = instance_game_dir(&state, &id)?;
    let source = game_dir.join("saves").join(&name);
    require_world_dir(&source)?;

    let backup_root = game_dir.join("backups").join("worlds");
    fs::create_dir_all(&backup_root).map_err(world_file_write_error_response)?;
    let backup = available_world_backup_name(&backup_root, &name)?;
    copy_world_backup_staged(&source, &backup_root, &backup)
        .map_err(world_file_write_error_response)?;

    Ok(Json(WorldBackupResponse {
        status: "ok",
        location: format!("backups/worlds/{backup}"),
        backup,
    }))
}

async fn handle_instance_mods(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Vec<InstanceModInfo>>, (StatusCode, Json<serde_json::Value>)> {
    let game_dir = instance_game_dir(&state, &id)?;
    Ok(Json(scan_instance_mods(&game_dir.join("mods"))))
}

async fn handle_update_instance_mod(
    State(state): State<AppState>,
    Path((id, name)): Path<(String, String)>,
    Json(payload): Json<UpdateModRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    reject_running_instance(&state, &id, "mods").await?;
    validate_mod_name(&name)?;

    let game_dir = instance_game_dir(&state, &id)?;
    let mods_dir = game_dir.join("mods");
    let source = mods_dir.join(&name);
    require_mod_file(&source)?;
    let target_name = mod_enabled_name(&name, payload.enabled)?;
    if target_name == name {
        return Ok(Json(
            serde_json::json!({ "status": "ok", "name": name, "enabled": payload.enabled }),
        ));
    }

    let target = mods_dir.join(&target_name);
    if target_exists(&target) {
        return Err(json_error(StatusCode::CONFLICT, "mod already exists"));
    }

    fs::rename(source, target).map_err(mod_file_write_error_response)?;
    Ok(Json(
        serde_json::json!({ "status": "ok", "name": target_name, "enabled": payload.enabled }),
    ))
}

async fn handle_delete_instance_mod(
    State(state): State<AppState>,
    Path((id, name)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    reject_running_instance(&state, &id, "mods").await?;
    validate_mod_name(&name)?;

    let game_dir = instance_game_dir(&state, &id)?;
    let source = game_dir.join("mods").join(&name);
    require_mod_file(&source)?;
    fs::remove_file(source).map_err(mod_file_write_error_response)?;
    Ok(Json(serde_json::json!({ "status": "ok" })))
}

async fn handle_instance_screenshots(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Vec<InstanceScreenshotInfo>>, (StatusCode, Json<serde_json::Value>)> {
    let game_dir = instance_game_dir(&state, &id)?;
    Ok(Json(scan_instance_screenshots(
        &game_dir.join("screenshots"),
    )))
}

async fn handle_instance_screenshot_file(
    State(state): State<AppState>,
    Path((id, name)): Path<(String, String)>,
) -> Result<Response, (StatusCode, Json<serde_json::Value>)> {
    validate_screenshot_name(&name)?;
    let content_type = screenshot_content_type(&name)
        .ok_or_else(|| json_error(StatusCode::BAD_REQUEST, "invalid screenshot filename"))?;

    let game_dir = instance_game_dir(&state, &id)?;
    let path = game_dir.join("screenshots").join(&name);
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

async fn handle_rename_instance_screenshot(
    State(state): State<AppState>,
    Path((id, name)): Path<(String, String)>,
    Json(payload): Json<RenameScreenshotRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    validate_screenshot_name(&name)?;
    validate_screenshot_name(&payload.name)?;
    if screenshot_content_type(&name) != screenshot_content_type(&payload.name) {
        return Err(json_error(
            StatusCode::BAD_REQUEST,
            "screenshot file type cannot change",
        ));
    }

    let game_dir = instance_game_dir(&state, &id)?;
    let screenshots_dir = game_dir.join("screenshots");
    let source = screenshots_dir.join(&name);
    let target = screenshots_dir.join(&payload.name);
    require_screenshot_file(&source)?;
    if target_exists(&target) {
        return Err(json_error(
            StatusCode::CONFLICT,
            "screenshot already exists",
        ));
    }

    fs::rename(source, target).map_err(screenshot_file_write_error_response)?;
    Ok(Json(
        serde_json::json!({ "status": "ok", "name": payload.name }),
    ))
}

async fn handle_delete_instance_screenshot(
    State(state): State<AppState>,
    Path((id, name)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    validate_screenshot_name(&name)?;

    let game_dir = instance_game_dir(&state, &id)?;
    let source = game_dir.join("screenshots").join(&name);
    require_screenshot_file(&source)?;
    fs::remove_file(source).map_err(screenshot_file_write_error_response)?;
    Ok(Json(serde_json::json!({ "status": "ok" })))
}

async fn handle_instance_logs(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Vec<InstanceLogInfo>>, (StatusCode, Json<serde_json::Value>)> {
    let game_dir = instance_game_dir(&state, &id)?;
    Ok(Json(scan_instance_logs(&game_dir.join("logs"))))
}

async fn handle_instance_log_tail(
    State(state): State<AppState>,
    Path((id, name)): Path<(String, String)>,
) -> Result<Json<InstanceLogTailResponse>, (StatusCode, Json<serde_json::Value>)> {
    let game_dir = instance_game_dir(&state, &id)?;
    if !is_safe_resource_name(&name) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "invalid log filename" })),
        ));
    }

    let path = game_dir.join("logs").join(&name);
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

    Ok(Json(InstanceLogTailResponse {
        name,
        size,
        truncated: start > 0,
        text: String::from_utf8_lossy(&bytes).to_string(),
    }))
}

async fn handle_delete_instance(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<std::collections::HashMap<String, String>>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    if state.instances().get(&id).is_none() {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "instance not found" })),
        ));
    }

    if state.sessions().has_active_instance(&id).await {
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
        .remove(&id, !keep_files)
        .map_err(|error| instance_write_error_response(InstanceWriteOperation::Delete, error))?;

    Ok(Json(serde_json::json!({ "status": "ok" })))
}

fn open_path(path: &std::path::Path) -> std::io::Result<()> {
    let mut command = if cfg!(target_os = "windows") {
        let mut cmd = std::process::Command::new("explorer");
        cmd.arg(path);
        cmd
    } else if cfg!(target_os = "macos") {
        let mut cmd = std::process::Command::new("open");
        cmd.arg(path);
        cmd
    } else {
        let mut cmd = std::process::Command::new("xdg-open");
        cmd.arg(path);
        cmd
    };

    let _child = command.spawn()?;
    Ok(())
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

fn validate_world_name(name: &str) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
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

fn validate_mod_name(name: &str) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
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

fn validate_screenshot_name(name: &str) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
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

fn copy_world_backup_staged(
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

fn mod_file_write_error_response(_error: std::io::Error) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({
            "error": "Could not update mod files. Check instance folder permissions and try again."
        })),
    )
}

fn screenshot_file_read_error_response(
    _error: std::io::Error,
) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({
            "error": "Could not read screenshot files. Check instance folder permissions and try again."
        })),
    )
}

fn screenshot_file_write_error_response(
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

fn scan_instance_logs(logs_dir: &FsPath) -> Vec<InstanceLogInfo> {
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

fn mod_enabled_name(
    name: &str,
    enabled: bool,
) -> Result<String, (StatusCode, Json<serde_json::Value>)> {
    let lower = name.to_ascii_lowercase();
    if enabled {
        if lower.ends_with(".jar") {
            Ok(name.to_string())
        } else if lower.ends_with(".jar.disabled") {
            Ok(name[..name.len() - ".disabled".len()].to_string())
        } else {
            Err(json_error(StatusCode::BAD_REQUEST, "invalid mod filename"))
        }
    } else if lower.ends_with(".jar.disabled") {
        Ok(name.to_string())
    } else if lower.ends_with(".jar") {
        Ok(format!("{name}.disabled"))
    } else {
        Err(json_error(StatusCode::BAD_REQUEST, "invalid mod filename"))
    }
}

fn screenshot_content_type(name: &str) -> Option<&'static str> {
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

fn is_safe_resource_name(name: &str) -> bool {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{AppState, AppStateInit, InstallStore, SessionStore};
    use croopor_config::{AppPaths, ConfigStore, InstanceStore};
    use croopor_launcher::{LaunchSessionRecord, LaunchState, SessionId};
    use croopor_performance::PerformanceManager;
    use std::{collections::HashMap, io, sync::Arc};

    #[test]
    fn instance_write_error_mapper_preserves_safe_status_messages() {
        let cases = [
            (
                io::ErrorKind::NotFound,
                "instance not found",
                StatusCode::NOT_FOUND,
                "instance not found",
            ),
            (
                io::ErrorKind::AlreadyExists,
                "an instance with this name already exists",
                StatusCode::CONFLICT,
                "an instance with this name already exists",
            ),
            (
                io::ErrorKind::InvalidInput,
                "version_id is required",
                StatusCode::BAD_REQUEST,
                "version_id is required",
            ),
        ];

        for (kind, store_message, expected_status, expected_message) in cases {
            let (status, Json(body)) = instance_write_error_response(
                InstanceWriteOperation::Create,
                InstanceStoreError::Read(io::Error::new(kind, store_message)),
            );

            assert_eq!(status, expected_status);
            assert_bounded_error_body(&body, expected_message);
        }
    }

    #[test]
    fn instance_write_error_mapper_bounds_internal_operation_errors() {
        let cases = [
            (
                InstanceWriteOperation::Create,
                "failed to initialize instance files: /home/zero/.config/Croopor/instances/new/logs",
                "Could not create the instance. Check app data permissions and try again.",
            ),
            (
                InstanceWriteOperation::Update,
                "failed to persist /home/zero/.config/Croopor/instances.json",
                "Could not save the instance. Check app data permissions and try again.",
            ),
            (
                InstanceWriteOperation::Delete,
                "failed to delete C:\\Users\\Zero\\AppData\\Roaming\\Croopor\\instances\\old",
                "Could not delete the instance. Check app data permissions and try again.",
            ),
        ];

        for (operation, store_message, expected_message) in cases {
            let (status, Json(body)) = instance_write_error_response(
                operation,
                InstanceStoreError::Read(io::Error::other(store_message)),
            );

            assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
            assert_bounded_error_body(&body, expected_message);
            let public_message = error_body_text(&body);
            assert!(!public_message.contains("/home/zero"));
            assert!(!public_message.contains("C:\\Users\\Zero"));
            assert!(!public_message.contains("instances.json"));
        }
    }

    #[test]
    fn duplicate_instance_write_error_hides_layout_and_persist_paths() {
        let store_message = concat!(
            "failed to duplicate instance files: ",
            "/home/zero/.config/Croopor/instances/source/mods/example.jar; ",
            "failed to roll back persisted instance: ",
            "C:\\Users\\Zero\\AppData\\Roaming\\Croopor\\config\\instances.json"
        );

        let (status, Json(body)) = instance_write_error_response(
            InstanceWriteOperation::Duplicate,
            InstanceStoreError::Read(io::Error::other(store_message)),
        );

        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_bounded_error_body(
            &body,
            "Could not duplicate the instance. Check app data permissions and try again.",
        );
        let public_message = error_body_text(&body);
        for hidden_fragment in [
            "/home/zero",
            ".config",
            "C:\\Users\\Zero",
            "AppData",
            "example.jar",
            "instances.json",
            "failed to duplicate instance files",
        ] {
            assert!(
                !public_message.contains(hidden_fragment),
                "{hidden_fragment:?} leaked in {public_message:?}"
            );
        }
    }

    #[test]
    fn instance_folder_prepare_error_response_bounds_public_message() {
        assert_instance_folder_error_response_is_bounded(
            instance_folder_prepare_error_response,
            "Could not prepare the instance folder. Check app data permissions and try again.",
        );
    }

    #[test]
    fn instance_folder_open_error_response_bounds_public_message() {
        assert_instance_folder_error_response_is_bounded(
            instance_folder_open_error_response,
            "Could not open the instance folder. Check desktop permissions and try again.",
        );
    }

    #[test]
    fn instance_log_read_error_response_bounds_public_metadata_open_and_read_messages() {
        let cases = [
            "metadata failed for /home/zero/.config/Croopor/instances/test/logs/latest.log",
            "open failed for C:\\Users\\Zero\\AppData\\Roaming\\Croopor\\instances\\test\\logs\\debug.log",
            "Permission denied (os error 13) while reading logs/latest.log",
        ];

        for internal_message in cases {
            let (status, Json(body)) =
                instance_log_read_error_response(io::Error::other(internal_message));

            assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
            assert_bounded_error_body(&body, INSTANCE_LOG_READ_ERROR_MESSAGE);
            let public_message = error_body_text(&body);
            for hidden_fragment in [
                "/home/zero",
                ".config",
                "C:\\Users\\Zero",
                "AppData",
                "Permission denied",
                "os error 13",
                "latest.log",
                "debug.log",
                "logs/",
                "\\logs\\",
            ] {
                assert!(
                    !public_message.contains(hidden_fragment),
                    "{hidden_fragment:?} leaked in {public_message:?}"
                );
            }
        }
    }

    #[test]
    fn instance_folder_resolver_returns_root_when_subfolder_is_omitted() {
        let game_dir = FsPath::new("/tmp/croopor-instance");

        assert_eq!(
            resolve_instance_folder(game_dir, None).expect("resolve root"),
            game_dir
        );
    }

    #[test]
    fn instance_folder_resolver_accepts_allowed_subfolder() {
        let game_dir = FsPath::new("/tmp/croopor-instance");

        assert_eq!(
            resolve_instance_folder(game_dir, Some("mods")).expect("resolve mods"),
            game_dir.join("mods")
        );
    }

    #[test]
    fn instance_folder_resolver_rejects_unknown_subfolder() {
        let game_dir = FsPath::new("/tmp/croopor-instance");

        assert_eq!(
            resolve_instance_folder(game_dir, Some("versions")),
            Err("invalid instance folder")
        );
    }

    #[test]
    fn instance_folder_resolver_rejects_traversal_like_subfolders() {
        let game_dir = FsPath::new("/tmp/croopor-instance");

        for subfolder in ["..", "../mods", "mods/..", "mods/../logs", "mods\\..\\logs"] {
            assert_eq!(
                resolve_instance_folder(game_dir, Some(subfolder)),
                Err("invalid instance folder"),
                "{subfolder:?} should be rejected"
            );
        }
    }

    #[test]
    fn resource_names_reject_path_traversal_hidden_and_control_names() {
        for name in ["latest.log", "2026-05-30-1.log.gz", "debug.log"] {
            assert!(is_safe_resource_name(name), "{name} should be accepted");
        }

        for name in [
            "",
            "   ",
            " World",
            "World ",
            ".",
            "..",
            ".hidden.log",
            "../latest.log",
            "nested/latest.log",
            "nested\\latest.log",
            "bad\nname.log",
        ] {
            assert!(!is_safe_resource_name(name), "{name:?} should be rejected");
        }
    }

    #[test]
    fn log_scanner_returns_only_safe_instance_local_file_names() {
        let root = std::env::temp_dir().join(format!(
            "croopor-api-instance-logs-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|value| value.as_nanos())
                .unwrap_or_default()
        ));
        let logs_dir = root.join("logs");
        fs::create_dir_all(&logs_dir).expect("create logs dir");
        fs::write(logs_dir.join("latest.log"), "latest").expect("write latest");
        fs::write(logs_dir.join("debug.log"), "debug").expect("write debug");
        fs::write(logs_dir.join(".hidden.log"), "hidden").expect("write hidden");
        fs::create_dir_all(logs_dir.join("nested")).expect("create nested dir");
        fs::write(logs_dir.join("nested").join("nested.log"), "nested").expect("write nested");

        let names = scan_instance_logs(&logs_dir)
            .into_iter()
            .map(|log| log.name)
            .collect::<Vec<_>>();

        assert_eq!(
            names,
            vec!["latest.log".to_string(), "debug.log".to_string()]
        );
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn instance_log_tail_rejects_unsafe_log_name() {
        let fixture = TestFixture::new("log-tail-invalid-name");
        let instance = fixture
            .state
            .instances()
            .add(
                "Tail invalid log".to_string(),
                "1.21.1".to_string(),
                String::new(),
                String::new(),
                None,
            )
            .expect("add instance");

        let (status, Json(body)) = handle_instance_log_tail(
            State(fixture.state.clone()),
            Path((instance.id, "../latest.log".to_string())),
        )
        .await
        .expect_err("unsafe log name should fail");

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_bounded_error_body(&body, "invalid log filename");
    }

    #[tokio::test]
    async fn instance_log_tail_returns_bounded_truncated_tail() {
        let fixture = TestFixture::new("log-tail-truncated");
        let instance = fixture
            .state
            .instances()
            .add(
                "Tail truncated log".to_string(),
                "1.21.1".to_string(),
                String::new(),
                String::new(),
                None,
            )
            .expect("add instance");
        let logs_dir = fixture
            .state
            .instances()
            .game_dir(&instance.id)
            .join("logs");
        fs::create_dir_all(&logs_dir).expect("create logs dir");
        let discarded = b"discarded";
        let mut bytes = discarded.to_vec();
        bytes.resize(discarded.len() + LOG_TAIL_LIMIT as usize, b't');
        fs::write(logs_dir.join("latest.log"), &bytes).expect("write log");

        let Json(response) = handle_instance_log_tail(
            State(fixture.state.clone()),
            Path((instance.id, "latest.log".to_string())),
        )
        .await
        .expect("tail log");

        assert_eq!(response.name, "latest.log");
        assert_eq!(response.size, LOG_TAIL_LIMIT + discarded.len() as u64);
        assert!(response.truncated);
        assert_eq!(response.text.len(), LOG_TAIL_LIMIT as usize);
        assert!(response.text.bytes().all(|byte| byte == b't'));
    }

    #[test]
    fn instance_screenshot_names_reject_path_traversal_hidden_and_control_names() {
        for name in [
            "2026-05-31_12.00.00.png",
            "castle build.jpg",
            "base.jpeg",
            "nether.webp",
        ] {
            assert!(
                validate_screenshot_name(name).is_ok(),
                "{name} should be accepted"
            );
        }

        for name in [
            "",
            "   ",
            ".",
            "..",
            ".hidden.png",
            "../shot.png",
            "nested/shot.png",
            "nested\\shot.png",
            "bad\nshot.png",
            " shot.png",
            "shot.png ",
            "notes.txt",
        ] {
            let (status, Json(body)) =
                validate_screenshot_name(name).expect_err("invalid screenshot name should fail");
            assert_eq!(status, StatusCode::BAD_REQUEST, "{name:?}");
            assert_bounded_error_body(&body, "invalid screenshot filename");
        }
    }

    #[test]
    fn instance_screenshot_content_type_maps_supported_extensions() {
        assert_eq!(screenshot_content_type("shot.png"), Some("image/png"));
        assert_eq!(screenshot_content_type("shot.JPG"), Some("image/jpeg"));
        assert_eq!(screenshot_content_type("shot.jpeg"), Some("image/jpeg"));
        assert_eq!(screenshot_content_type("shot.webp"), Some("image/webp"));
        assert_eq!(screenshot_content_type("shot.gif"), None);
    }

    #[tokio::test]
    async fn instance_screenshot_file_serves_valid_local_image() {
        let fixture = TestFixture::new("screenshot-file");
        let instance = fixture
            .state
            .instances()
            .add(
                "Serve screenshots".to_string(),
                "1.21.1".to_string(),
                String::new(),
                String::new(),
                None,
            )
            .expect("add instance");
        let screenshots_dir = fixture
            .state
            .instances()
            .game_dir(&instance.id)
            .join("screenshots");
        fs::create_dir_all(&screenshots_dir).expect("create screenshots dir");
        fs::write(screenshots_dir.join("shot.PNG"), [137, 80, 78, 71]).expect("write screenshot");

        let response = handle_instance_screenshot_file(
            State(fixture.state.clone()),
            Path((instance.id.clone(), "shot.PNG".to_string())),
        )
        .await
        .expect("serve screenshot");

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE),
            Some(&HeaderValue::from_static("image/png"))
        );
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .expect("read screenshot body");
        assert_eq!(&body[..], &[137, 80, 78, 71]);

        let (status, Json(body)) = handle_instance_screenshot_file(
            State(fixture.state.clone()),
            Path((instance.id, "../shot.PNG".to_string())),
        )
        .await
        .expect_err("traversal should fail");
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_bounded_error_body(&body, "invalid screenshot filename");
    }

    #[tokio::test]
    async fn instance_screenshot_file_rejects_too_large_image() {
        let fixture = TestFixture::new("screenshot-file-too-large");
        let instance = fixture
            .state
            .instances()
            .add(
                "Large screenshot".to_string(),
                "1.21.1".to_string(),
                String::new(),
                String::new(),
                None,
            )
            .expect("add instance");
        let screenshots_dir = fixture
            .state
            .instances()
            .game_dir(&instance.id)
            .join("screenshots");
        fs::create_dir_all(&screenshots_dir).expect("create screenshots dir");
        let file = fs::File::create(screenshots_dir.join("too-large.png"))
            .expect("create large screenshot");
        file.set_len(SCREENSHOT_FILE_MAX_BYTES + 1)
            .expect("size large screenshot");

        let (status, Json(body)) = handle_instance_screenshot_file(
            State(fixture.state.clone()),
            Path((instance.id, "too-large.png".to_string())),
        )
        .await
        .expect_err("too-large screenshot should fail");

        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
        assert_bounded_error_body(&body, "screenshot file is too large");
    }

    #[tokio::test]
    async fn instance_screenshot_rename_reports_not_found_conflict_and_success() {
        let fixture = TestFixture::new("screenshot-rename");
        let instance = fixture
            .state
            .instances()
            .add(
                "Rename screenshots".to_string(),
                "1.21.1".to_string(),
                String::new(),
                String::new(),
                None,
            )
            .expect("add instance");
        let screenshots_dir = fixture
            .state
            .instances()
            .game_dir(&instance.id)
            .join("screenshots");
        fs::create_dir_all(&screenshots_dir).expect("create screenshots dir");

        let (status, Json(body)) = handle_rename_instance_screenshot(
            State(fixture.state.clone()),
            Path((instance.id.clone(), "missing.png".to_string())),
            Json(RenameScreenshotRequest {
                name: "target.png".to_string(),
            }),
        )
        .await
        .expect_err("missing source should fail");
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_bounded_error_body(&body, "screenshot not found");

        fs::write(screenshots_dir.join("source.png"), "source").expect("write source");
        fs::write(screenshots_dir.join("target.png"), "target").expect("write target");
        let (status, Json(body)) = handle_rename_instance_screenshot(
            State(fixture.state.clone()),
            Path((instance.id.clone(), "source.png".to_string())),
            Json(RenameScreenshotRequest {
                name: "target.png".to_string(),
            }),
        )
        .await
        .expect_err("existing target should fail");
        assert_eq!(status, StatusCode::CONFLICT);
        assert_bounded_error_body(&body, "screenshot already exists");
        assert_eq!(
            fs::read_to_string(screenshots_dir.join("source.png")).expect("read source"),
            "source"
        );

        let (status, Json(body)) = handle_rename_instance_screenshot(
            State(fixture.state.clone()),
            Path((instance.id.clone(), "source.png".to_string())),
            Json(RenameScreenshotRequest {
                name: "renamed.webp".to_string(),
            }),
        )
        .await
        .expect_err("changing screenshot type should fail");
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_bounded_error_body(&body, "screenshot file type cannot change");
        assert_eq!(
            fs::read_to_string(screenshots_dir.join("source.png")).expect("read source"),
            "source"
        );

        let Json(body) = handle_rename_instance_screenshot(
            State(fixture.state.clone()),
            Path((instance.id.clone(), "source.png".to_string())),
            Json(RenameScreenshotRequest {
                name: "renamed.png".to_string(),
            }),
        )
        .await
        .expect("rename screenshot");
        assert_eq!(
            body,
            serde_json::json!({ "status": "ok", "name": "renamed.png" })
        );
        assert!(!screenshots_dir.join("source.png").exists());
        assert_eq!(
            fs::read_to_string(screenshots_dir.join("renamed.png")).expect("read renamed"),
            "source"
        );
    }

    #[tokio::test]
    async fn instance_screenshot_delete_removes_only_named_file() {
        let fixture = TestFixture::new("screenshot-delete");
        let instance = fixture
            .state
            .instances()
            .add(
                "Delete screenshots".to_string(),
                "1.21.1".to_string(),
                String::new(),
                String::new(),
                None,
            )
            .expect("add instance");
        let screenshots_dir = fixture
            .state
            .instances()
            .game_dir(&instance.id)
            .join("screenshots");
        fs::create_dir_all(&screenshots_dir).expect("create screenshots dir");
        fs::write(screenshots_dir.join("delete.png"), "deleted").expect("write deleted");
        fs::write(screenshots_dir.join("keep.png"), "kept").expect("write kept");

        let Json(body) = handle_delete_instance_screenshot(
            State(fixture.state.clone()),
            Path((instance.id, "delete.png".to_string())),
        )
        .await
        .expect("delete screenshot");

        assert_eq!(body, serde_json::json!({ "status": "ok" }));
        assert!(!screenshots_dir.join("delete.png").exists());
        assert_eq!(
            fs::read_to_string(screenshots_dir.join("keep.png")).expect("read kept"),
            "kept"
        );
    }

    #[test]
    fn instance_screenshot_error_responses_do_not_leak_paths() {
        for mapper in [
            screenshot_file_read_error_response
                as fn(io::Error) -> (StatusCode, Json<serde_json::Value>),
            screenshot_file_write_error_response,
        ] {
            let (status, Json(body)) = mapper(io::Error::other(
                "failed for /home/zero/.config/Croopor/instances/test/screenshots/shot.png",
            ));

            assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
            let public_message = error_body_text(&body);
            assert!(!public_message.contains("/home/zero"));
            assert!(!public_message.contains("shot.png"));
            assert!(public_message.len() <= 180);
        }
    }

    #[test]
    fn instance_mod_names_reject_path_traversal_hidden_and_non_mod_names() {
        for name in ["sodium.jar", "Sodium.JAR", "sodium.jar.disabled"] {
            assert!(validate_mod_name(name).is_ok(), "{name} should be accepted");
        }

        for name in [
            "",
            "   ",
            ".",
            "..",
            ".hidden.jar",
            "../mod.jar",
            "nested/mod.jar",
            "nested\\mod.jar",
            "bad\nmod.jar",
            "notes.txt",
            "mod.disabled",
        ] {
            let (status, Json(body)) =
                validate_mod_name(name).expect_err("invalid mod name should fail");
            assert_eq!(status, StatusCode::BAD_REQUEST, "{name:?}");
            assert_bounded_error_body(&body, "invalid mod filename");
        }
    }

    #[tokio::test]
    async fn instance_mod_update_reports_not_found_conflict_and_success() {
        let fixture = TestFixture::new("mod-update");
        let instance = fixture
            .state
            .instances()
            .add(
                "Update mods".to_string(),
                "1.21.1".to_string(),
                String::new(),
                String::new(),
                None,
            )
            .expect("add instance");
        let mods_dir = fixture
            .state
            .instances()
            .game_dir(&instance.id)
            .join("mods");
        fs::create_dir_all(&mods_dir).expect("create mods dir");

        let (status, Json(body)) = handle_update_instance_mod(
            State(fixture.state.clone()),
            Path((instance.id.clone(), "missing.jar".to_string())),
            Json(UpdateModRequest { enabled: false }),
        )
        .await
        .expect_err("missing source should fail");
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_bounded_error_body(&body, "mod not found");

        fs::write(mods_dir.join("source.jar.disabled"), "source").expect("write disabled mod");
        fs::write(mods_dir.join("source.jar"), "target").expect("write existing target");
        let (status, Json(body)) = handle_update_instance_mod(
            State(fixture.state.clone()),
            Path((instance.id.clone(), "source.jar.disabled".to_string())),
            Json(UpdateModRequest { enabled: true }),
        )
        .await
        .expect_err("existing target should fail");
        assert_eq!(status, StatusCode::CONFLICT);
        assert_bounded_error_body(&body, "mod already exists");
        assert!(mods_dir.join("source.jar.disabled").is_file());

        fs::write(mods_dir.join("toggle.jar"), "toggle").expect("write enabled mod");
        let Json(body) = handle_update_instance_mod(
            State(fixture.state.clone()),
            Path((instance.id.clone(), "toggle.jar".to_string())),
            Json(UpdateModRequest { enabled: false }),
        )
        .await
        .expect("disable mod");
        assert_eq!(
            body,
            serde_json::json!({ "status": "ok", "name": "toggle.jar.disabled", "enabled": false })
        );
        assert!(!mods_dir.join("toggle.jar").exists());
        assert_eq!(
            fs::read_to_string(mods_dir.join("toggle.jar.disabled")).expect("read disabled mod"),
            "toggle"
        );

        let Json(body) = handle_update_instance_mod(
            State(fixture.state.clone()),
            Path((instance.id.clone(), "toggle.jar.disabled".to_string())),
            Json(UpdateModRequest { enabled: true }),
        )
        .await
        .expect("enable mod");
        assert_eq!(
            body,
            serde_json::json!({ "status": "ok", "name": "toggle.jar", "enabled": true })
        );
        assert!(mods_dir.join("toggle.jar").is_file());
        assert!(!mods_dir.join("toggle.jar.disabled").exists());
    }

    #[tokio::test]
    async fn instance_mod_delete_removes_only_named_mod_file() {
        let fixture = TestFixture::new("mod-delete");
        let instance = fixture
            .state
            .instances()
            .add(
                "Delete mods".to_string(),
                "1.21.1".to_string(),
                String::new(),
                String::new(),
                None,
            )
            .expect("add instance");
        let mods_dir = fixture
            .state
            .instances()
            .game_dir(&instance.id)
            .join("mods");
        fs::create_dir_all(&mods_dir).expect("create mods dir");
        fs::write(mods_dir.join("delete.jar"), "deleted").expect("write deleted mod");
        fs::write(mods_dir.join("keep.jar"), "kept").expect("write kept mod");

        let Json(body) = handle_delete_instance_mod(
            State(fixture.state.clone()),
            Path((instance.id, "delete.jar".to_string())),
        )
        .await
        .expect("delete mod");

        assert_eq!(body, serde_json::json!({ "status": "ok" }));
        assert!(!mods_dir.join("delete.jar").exists());
        assert_eq!(
            fs::read_to_string(mods_dir.join("keep.jar")).expect("read kept mod"),
            "kept"
        );
    }

    #[test]
    fn instance_world_names_reject_path_traversal_hidden_and_control_names() {
        for name in ["World", "My World", "World-2026_05_31"] {
            assert!(
                validate_world_name(name).is_ok(),
                "{name} should be accepted"
            );
        }

        for name in [
            "",
            "   ",
            ".",
            "..",
            ".hidden",
            "../World",
            "nested/World",
            "nested\\World",
            "bad\nworld",
        ] {
            let (status, Json(body)) =
                validate_world_name(name).expect_err("invalid world name should fail");
            assert_eq!(status, StatusCode::BAD_REQUEST, "{name:?}");
            assert_bounded_error_body(&body, "invalid world name");
        }
    }

    #[tokio::test]
    async fn instance_world_rename_reports_not_found_conflict_and_success() {
        let fixture = TestFixture::new("world-rename");
        let instance = fixture
            .state
            .instances()
            .add(
                "Rename worlds".to_string(),
                "1.21.1".to_string(),
                String::new(),
                String::new(),
                None,
            )
            .expect("add instance");
        let saves_dir = fixture
            .state
            .instances()
            .game_dir(&instance.id)
            .join("saves");

        let (status, Json(body)) = handle_rename_instance_world(
            State(fixture.state.clone()),
            Path((instance.id.clone(), "Missing".to_string())),
            Json(RenameWorldRequest {
                name: "Target".to_string(),
            }),
        )
        .await
        .expect_err("missing source should fail");
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_bounded_error_body(&body, "world not found");

        fs::create_dir_all(saves_dir.join("World A")).expect("create source world");
        fs::create_dir_all(saves_dir.join("Existing")).expect("create existing world");
        let (status, Json(body)) = handle_rename_instance_world(
            State(fixture.state.clone()),
            Path((instance.id.clone(), "World A".to_string())),
            Json(RenameWorldRequest {
                name: "Existing".to_string(),
            }),
        )
        .await
        .expect_err("existing target should fail");
        assert_eq!(status, StatusCode::CONFLICT);
        assert_bounded_error_body(&body, "world already exists");
        assert!(saves_dir.join("World A").is_dir());

        let Json(body) = handle_rename_instance_world(
            State(fixture.state.clone()),
            Path((instance.id.clone(), "World A".to_string())),
            Json(RenameWorldRequest {
                name: "Renamed".to_string(),
            }),
        )
        .await
        .expect("rename world");
        assert_eq!(
            body,
            serde_json::json!({ "status": "ok", "name": "Renamed" })
        );
        assert!(!saves_dir.join("World A").exists());
        assert!(saves_dir.join("Renamed").is_dir());
    }

    #[tokio::test]
    async fn instance_world_delete_removes_only_named_world_directory() {
        let fixture = TestFixture::new("world-delete");
        let instance = fixture
            .state
            .instances()
            .add(
                "Delete worlds".to_string(),
                "1.21.1".to_string(),
                String::new(),
                String::new(),
                None,
            )
            .expect("add instance");
        let saves_dir = fixture
            .state
            .instances()
            .game_dir(&instance.id)
            .join("saves");
        fs::create_dir_all(saves_dir.join("Delete Me")).expect("create deleted world");
        fs::write(saves_dir.join("Delete Me").join("level.dat"), "deleted").expect("write level");
        fs::create_dir_all(saves_dir.join("Keep Me")).expect("create kept world");
        fs::write(saves_dir.join("Keep Me").join("level.dat"), "kept").expect("write kept");

        let Json(body) = handle_delete_instance_world(
            State(fixture.state.clone()),
            Path((instance.id.clone(), "Delete Me".to_string())),
        )
        .await
        .expect("delete world");

        assert_eq!(body, serde_json::json!({ "status": "ok" }));
        assert!(!saves_dir.join("Delete Me").exists());
        assert_eq!(
            fs::read_to_string(saves_dir.join("Keep Me").join("level.dat")).expect("read kept"),
            "kept"
        );
    }

    #[tokio::test]
    async fn instance_world_backup_copies_directory_to_instance_local_label() {
        let fixture = TestFixture::new("world-backup");
        let instance = fixture
            .state
            .instances()
            .add(
                "Backup worlds".to_string(),
                "1.21.1".to_string(),
                String::new(),
                String::new(),
                None,
            )
            .expect("add instance");
        let game_dir = fixture.state.instances().game_dir(&instance.id);
        let world_dir = game_dir.join("saves").join("Backup Me");
        fs::create_dir_all(world_dir.join("data")).expect("create world data");
        fs::write(world_dir.join("level.dat"), "level").expect("write level");
        fs::write(world_dir.join("data").join("map.dat"), "map").expect("write map");

        let Json(body) = handle_backup_instance_world(
            State(fixture.state.clone()),
            Path((instance.id.clone(), "Backup Me".to_string())),
        )
        .await
        .expect("backup world");

        assert_eq!(body.status, "ok");
        assert!(body.backup.starts_with("Backup Me-"));
        assert_eq!(body.location, format!("backups/worlds/{}", body.backup));
        assert!(
            !body
                .location
                .contains(&game_dir.to_string_lossy().to_string())
        );

        let backup_dir = game_dir.join("backups").join("worlds").join(&body.backup);
        assert_eq!(
            fs::read_to_string(backup_dir.join("level.dat")).expect("read backup level"),
            "level"
        );
        assert_eq!(
            fs::read_to_string(backup_dir.join("data").join("map.dat")).expect("read backup map"),
            "map"
        );
        assert_eq!(
            fs::read_to_string(world_dir.join("level.dat")).expect("read original level"),
            "level"
        );
    }

    #[test]
    fn instance_world_backup_cleans_temp_directory_after_copy_failure() {
        let root = test_root("world-backup-copy-failure");
        let source = root.join("source");
        let backup_root = root.join("backups").join("worlds");
        fs::create_dir_all(&source).expect("create source");
        fs::create_dir_all(&backup_root).expect("create backup root");

        let mut nested = source.clone();
        for index in 0..=WORLD_BACKUP_MAX_DEPTH + 1 {
            nested = nested.join(format!("d{index}"));
            fs::create_dir_all(&nested).expect("create nested source");
        }

        let error = copy_world_backup_staged(&source, &backup_root, "Failed Backup")
            .expect_err("deep source should fail bounded copy");
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert!(!backup_root.join("Failed Backup").exists());
        let leftovers = fs::read_dir(&backup_root)
            .expect("read backup root")
            .filter_map(Result::ok)
            .collect::<Vec<_>>();
        assert!(
            leftovers.is_empty(),
            "backup temp entries should be removed after failure"
        );

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn instance_world_mutations_reject_active_instance() {
        let fixture = TestFixture::new("world-running-conflict");
        let instance = fixture
            .state
            .instances()
            .add(
                "Running worlds".to_string(),
                "1.21.1".to_string(),
                String::new(),
                String::new(),
                None,
            )
            .expect("add instance");
        let game_dir = fixture.state.instances().game_dir(&instance.id);
        fs::create_dir_all(game_dir.join("saves").join("World")).expect("create world");
        fixture
            .state
            .sessions()
            .insert(test_launch_record("active-world-session", &instance.id))
            .await;

        let (status, Json(body)) = handle_delete_instance_world(
            State(fixture.state.clone()),
            Path((instance.id, "World".to_string())),
        )
        .await
        .expect_err("running instance should reject world mutation");

        assert_eq!(status, StatusCode::CONFLICT);
        assert_bounded_error_body(
            &body,
            "cannot change worlds while the instance is running; stop the game first",
        );
        assert!(game_dir.join("saves").join("World").is_dir());
    }

    #[tokio::test]
    async fn update_instance_allows_unchanged_name_and_maps_name_collision_to_conflict() {
        let fixture = TestFixture::new("update-name-collision");
        let alpha = fixture
            .state
            .instances()
            .add(
                "Alpha".to_string(),
                "1.21.1".to_string(),
                String::new(),
                String::new(),
                None,
            )
            .expect("add alpha");
        let beta = fixture
            .state
            .instances()
            .add(
                "Beta".to_string(),
                "1.21.1".to_string(),
                String::new(),
                String::new(),
                None,
            )
            .expect("add beta");

        let Json(updated) = handle_update_instance(
            State(fixture.state.clone()),
            Path(alpha.id.clone()),
            Json(InstancePatch {
                name: Some(alpha.name.clone()),
                version_id: Some("1.21.2".to_string()),
                ..InstancePatch::default()
            }),
        )
        .await
        .expect("unchanged name update should succeed");
        assert_eq!(updated.name, "Alpha");
        assert_eq!(updated.version_id, "1.21.2");

        let (status, Json(body)) = handle_update_instance(
            State(fixture.state.clone()),
            Path(alpha.id.clone()),
            Json(InstancePatch {
                name: Some(beta.name.clone()),
                ..InstancePatch::default()
            }),
        )
        .await
        .expect_err("duplicate name update should fail");

        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(
            body,
            serde_json::json!({ "error": "an instance with this name already exists" })
        );
        assert_eq!(
            fixture
                .state
                .instances()
                .get(&alpha.id)
                .expect("alpha remains")
                .name,
            "Alpha"
        );
    }

    #[tokio::test]
    async fn instance_crud_handlers_create_list_get_update_and_delete() {
        let fixture = TestFixture::new("crud-happy-path");

        let Json(created) = handle_create_instance(
            State(fixture.state.clone()),
            Json(CreateInstanceRequest {
                name: "Survival".to_string(),
                version_id: "1.21.1".to_string(),
                icon: "grass".to_string(),
                accent: "#5aa469".to_string(),
            }),
        )
        .await
        .expect("create instance");
        assert_eq!(created.name, "Survival");
        assert_eq!(created.version_id, "1.21.1");
        assert_eq!(created.icon, "grass");
        assert_eq!(created.accent, "#5aa469");

        let Json(listed) = handle_list_instances(State(fixture.state.clone())).await;
        assert_eq!(listed.last_instance_id, None);
        assert_eq!(listed.instances.len(), 1);
        assert_eq!(listed.instances[0].instance.id, created.id);
        assert_eq!(listed.instances[0].instance.name, "Survival");
        assert!(!listed.instances[0].launchable);
        assert_eq!(listed.instances[0].status_detail, "version not installed");

        let Json(fetched) =
            handle_get_instance(State(fixture.state.clone()), Path(created.id.clone()))
                .await
                .expect("get instance");
        assert_eq!(fetched, created);

        let Json(updated) = handle_update_instance(
            State(fixture.state.clone()),
            Path(created.id.clone()),
            Json(InstancePatch {
                name: Some("Skyblock".to_string()),
                version_id: Some("1.21.2".to_string()),
                max_memory_mb: Some(4096),
                icon: Some("cloud".to_string()),
                ..InstancePatch::default()
            }),
        )
        .await
        .expect("update instance");
        assert_eq!(updated.id, created.id);
        assert_eq!(updated.name, "Skyblock");
        assert_eq!(updated.version_id, "1.21.2");
        assert_eq!(updated.max_memory_mb, 4096);
        assert_eq!(updated.icon, "cloud");

        let game_dir = fixture.state.instances().game_dir(&created.id);
        fs::write(game_dir.join("logs").join("latest.log"), "started").expect("write log");

        let Json(body) = handle_delete_instance(
            State(fixture.state.clone()),
            Path(created.id.clone()),
            Query(HashMap::new()),
        )
        .await
        .expect("delete instance");
        assert_eq!(body, serde_json::json!({ "status": "ok" }));
        assert!(fixture.state.instances().get(&created.id).is_none());
        assert!(!game_dir.exists());
    }

    #[tokio::test]
    async fn create_instance_duplicate_name_maps_to_conflict_json_error() {
        let fixture = TestFixture::new("create-name-conflict");
        let Json(original) = handle_create_instance(
            State(fixture.state.clone()),
            Json(CreateInstanceRequest {
                name: "Survival".to_string(),
                version_id: "1.21.1".to_string(),
                icon: String::new(),
                accent: String::new(),
            }),
        )
        .await
        .expect("create original instance");
        assert_eq!(original.name, "Survival");

        let (status, Json(body)) = handle_create_instance(
            State(fixture.state.clone()),
            Json(CreateInstanceRequest {
                name: "Survival".to_string(),
                version_id: "1.21.2".to_string(),
                icon: String::new(),
                accent: String::new(),
            }),
        )
        .await
        .expect_err("duplicate name should fail");

        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(
            body,
            serde_json::json!({ "error": "an instance with this name already exists" })
        );
        assert_eq!(fixture.state.instances().list().len(), 1);
    }

    #[tokio::test]
    async fn duplicate_instance_existing_name_maps_to_conflict_json_error() {
        let fixture = TestFixture::new("duplicate-name-conflict");
        let source = fixture
            .state
            .instances()
            .add(
                "Source".to_string(),
                "1.21.1".to_string(),
                String::new(),
                String::new(),
                None,
            )
            .expect("add source instance");
        fixture
            .state
            .instances()
            .add(
                "Existing".to_string(),
                "1.21.1".to_string(),
                String::new(),
                String::new(),
                None,
            )
            .expect("add existing instance");

        let (status, Json(body)) = handle_duplicate_instance(
            State(fixture.state.clone()),
            Path(source.id),
            Some(Json(DuplicateInstanceRequest {
                name: Some("Existing".to_string()),
            })),
        )
        .await
        .expect_err("duplicate name should fail");

        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(
            body,
            serde_json::json!({ "error": "an instance with this name already exists" })
        );
        assert_eq!(fixture.state.instances().list().len(), 2);
    }

    #[tokio::test]
    async fn open_instance_folder_missing_instance_returns_not_found_json_error() {
        let fixture = TestFixture::new("open-folder-missing");

        let (status, Json(body)) = handle_open_instance_folder(
            State(fixture.state.clone()),
            Path("missing".to_string()),
            Query(OpenFolderQuery { sub: None }),
        )
        .await
        .expect_err("missing open-folder should fail");

        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_bounded_error_body(&body, "instance not found");
    }

    #[tokio::test]
    async fn open_instance_folder_rejects_traversal_subfolder_without_creating_escape_path() {
        let fixture = TestFixture::new("open-folder-traversal");
        let instance = fixture
            .state
            .instances()
            .add(
                "Traversal".to_string(),
                "1.21.1".to_string(),
                String::new(),
                String::new(),
                None,
            )
            .expect("add instance");
        let game_dir = fixture.state.instances().game_dir(&instance.id);
        let escaped_dir = game_dir
            .parent()
            .expect("game dir parent")
            .join("escaped-open-folder");
        assert!(!escaped_dir.exists());

        let (status, Json(body)) = handle_open_instance_folder(
            State(fixture.state.clone()),
            Path(instance.id),
            Query(OpenFolderQuery {
                sub: Some("../escaped-open-folder".to_string()),
            }),
        )
        .await
        .expect_err("traversal open-folder should fail");

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_bounded_error_body(&body, "invalid instance folder");
        assert!(!escaped_dir.exists());
    }

    #[tokio::test]
    async fn missing_instance_crud_handlers_return_not_found_json_error() {
        let fixture = TestFixture::new("missing-crud");

        let (status, Json(body)) =
            handle_get_instance(State(fixture.state.clone()), Path("missing".to_string()))
                .await
                .expect_err("missing get should fail");
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_bounded_error_body(&body, "instance not found");

        let (status, Json(body)) = handle_update_instance(
            State(fixture.state.clone()),
            Path("missing".to_string()),
            Json(InstancePatch {
                name: Some("Nope".to_string()),
                ..InstancePatch::default()
            }),
        )
        .await
        .expect_err("missing update should fail");
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_bounded_error_body(&body, "instance not found");

        let (status, Json(body)) = handle_delete_instance(
            State(fixture.state.clone()),
            Path("missing".to_string()),
            Query(HashMap::new()),
        )
        .await
        .expect_err("missing delete should fail");
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_bounded_error_body(&body, "instance not found");
    }

    #[tokio::test]
    async fn delete_instance_default_removes_files_and_keep_files_preserves_them() {
        let fixture = TestFixture::new("delete-files");
        let remove_files = fixture
            .state
            .instances()
            .add(
                "Remove files".to_string(),
                "1.21.1".to_string(),
                String::new(),
                String::new(),
                None,
            )
            .expect("add remove-files instance");
        let remove_game_dir = fixture.state.instances().game_dir(&remove_files.id);
        fs::write(remove_game_dir.join("mods").join("example.jar"), "mod").expect("write mod");

        let Json(body) = handle_delete_instance(
            State(fixture.state.clone()),
            Path(remove_files.id.clone()),
            Query(HashMap::new()),
        )
        .await
        .expect("delete with default file removal");
        assert_eq!(body, serde_json::json!({ "status": "ok" }));
        assert!(!remove_game_dir.exists());

        let keep_files = fixture
            .state
            .instances()
            .add(
                "Keep files".to_string(),
                "1.21.1".to_string(),
                String::new(),
                String::new(),
                None,
            )
            .expect("add keep-files instance");
        let keep_game_dir = fixture.state.instances().game_dir(&keep_files.id);
        let keep_marker = keep_game_dir.join("saves").join("world").join("level.dat");
        fs::create_dir_all(keep_marker.parent().expect("marker parent")).expect("create world");
        fs::write(&keep_marker, "level").expect("write level");

        let Json(body) = handle_delete_instance(
            State(fixture.state.clone()),
            Path(keep_files.id.clone()),
            Query(HashMap::from([(
                "keep_files".to_string(),
                "true".to_string(),
            )])),
        )
        .await
        .expect("delete while keeping files");
        assert_eq!(body, serde_json::json!({ "status": "ok" }));

        assert!(fixture.state.instances().get(&keep_files.id).is_none());
        assert!(keep_marker.exists());
    }

    fn assert_bounded_error_body(body: &serde_json::Value, expected: &str) {
        let object = body.as_object().expect("error body should be an object");
        assert_eq!(object.len(), 1);
        assert_eq!(
            body.get("error").and_then(serde_json::Value::as_str),
            Some(expected)
        );
    }

    fn error_body_text(body: &serde_json::Value) -> &str {
        body.get("error")
            .and_then(serde_json::Value::as_str)
            .expect("error message should be a string")
    }

    fn assert_instance_folder_error_response_is_bounded(
        mapper: fn(io::Error) -> (StatusCode, Json<serde_json::Value>),
        expected_message: &str,
    ) {
        for internal_message in [
            "failed for /home/zero/.config/Croopor/instances/test/mods",
            "failed for C:\\Users\\Zero\\AppData\\Roaming\\Croopor\\instances\\test\\logs",
            "Permission denied (os error 13)",
        ] {
            let (status, Json(body)) = mapper(io::Error::other(internal_message));

            assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
            assert_bounded_error_body(&body, expected_message);
            let public_message = error_body_text(&body);
            for hidden_fragment in [
                "/home/zero",
                ".config",
                "C:\\Users\\Zero",
                "AppData",
                "Permission denied",
                "os error 13",
            ] {
                assert!(
                    !public_message.contains(hidden_fragment),
                    "{hidden_fragment:?} leaked in {public_message:?}"
                );
            }
        }
    }

    struct TestFixture {
        state: AppState,
        root: PathBuf,
    }

    impl TestFixture {
        fn new(name: &str) -> Self {
            let root = test_root(name);
            let paths = test_paths(&root);
            let config = Arc::new(ConfigStore::load_from(paths.clone()).expect("load config"));
            let instances =
                Arc::new(InstanceStore::load_from(paths.clone()).expect("load instances"));
            let state = AppState::new(AppStateInit {
                app_name: "Croopor".to_string(),
                version: "test".to_string(),
                config,
                instances,
                installs: Arc::new(InstallStore::new()),
                sessions: Arc::new(SessionStore::new()),
                performance: Arc::new(PerformanceManager::new().expect("performance manager")),
                startup_warnings: Vec::new(),
                frontend_dir: root.join("frontend"),
            });

            Self { state, root }
        }
    }

    impl Drop for TestFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn test_root(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "croopor-api-instances-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|value| value.as_nanos())
                .unwrap_or_default()
        ));
        fs::create_dir_all(&path).expect("create test root");
        path
    }

    fn test_paths(root: &FsPath) -> AppPaths {
        let config_dir = root.join("config");
        AppPaths {
            config_file: config_dir.join("config.json"),
            instances_file: config_dir.join("instances.json"),
            instances_dir: root.join("instances"),
            music_dir: root.join("music"),
            library_dir: root.join("library"),
            config_dir,
        }
    }

    fn test_launch_record(session_id: &str, instance_id: &str) -> LaunchSessionRecord {
        LaunchSessionRecord {
            session_id: SessionId(session_id.to_string()),
            instance_id: instance_id.to_string(),
            version_id: "1.21.1".to_string(),
            launched_at: Some("2026-01-01T00:00:00.000Z".to_string()),
            benchmark: None,
            state: LaunchState::Queued,
            pid: None,
            process_started_at_ms: None,
            boot_completed_at_ms: None,
            boot_duration_ms: None,
            priority: None,
            exit_code: None,
            command: Vec::new(),
            java_path: None,
            natives_dir: None,
            failure: None,
            healing: None,
            guardian: None,
            stages: Vec::new(),
        }
    }
}
