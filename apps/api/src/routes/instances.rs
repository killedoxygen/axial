use crate::state::AppState;
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{get, post},
};
use croopor_config::EnrichedInstance;
use croopor_minecraft::scan_versions;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{Read, Seek, SeekFrom},
    path::{Path as FsPath, PathBuf},
};

const LOG_TAIL_LIMIT: u64 = 128 * 1024;

const INSTANCE_SUBFOLDERS: [&str; 7] = [
    "mods",
    "saves",
    "resourcepacks",
    "shaderpacks",
    "config",
    "screenshots",
    "logs",
];

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
        .route("/api/v1/instances/{id}/mods", get(handle_instance_mods))
        .route(
            "/api/v1/instances/{id}/screenshots",
            get(handle_instance_screenshots),
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
        .map_err(|error| {
            let status = if error.to_string().contains("already exists") {
                StatusCode::CONFLICT
            } else {
                StatusCode::BAD_REQUEST
            };
            (
                status,
                Json(serde_json::json!({ "error": error.to_string() })),
            )
        })
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
        .map_err(|error| {
            let message = error.to_string();
            let status = if message.contains("not found") {
                StatusCode::NOT_FOUND
            } else if message.contains("already exists") {
                StatusCode::CONFLICT
            } else if message.contains("duplicate instance files") {
                StatusCode::INTERNAL_SERVER_ERROR
            } else {
                StatusCode::BAD_REQUEST
            };
            (status, Json(serde_json::json!({ "error": message })))
        })
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
        instance.art_preset = croopor_config::art_preset_for_seed(art_seed).to_string();
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
        .map_err(|error| {
            let message = error.to_string();
            let status = if message.contains("not found") {
                StatusCode::NOT_FOUND
            } else if message.contains("already exists") {
                StatusCode::CONFLICT
            } else {
                StatusCode::BAD_REQUEST
            };
            (status, Json(serde_json::json!({ "error": message })))
        })
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

    std::fs::create_dir_all(&dir).map_err(|error| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": format!("failed to create folder: {error}") })),
        )
    })?;
    open_path(&dir).map_err(|error| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": format!("failed to open folder: {error}") })),
        )
    })?;

    Ok(Json(serde_json::json!({ "status": "ok" })))
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

async fn handle_instance_mods(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Vec<InstanceModInfo>>, (StatusCode, Json<serde_json::Value>)> {
    let game_dir = instance_game_dir(&state, &id)?;
    Ok(Json(scan_instance_mods(&game_dir.join("mods"))))
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
    if !path.is_file() {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "log not found" })),
        ));
    }

    let metadata = fs::metadata(&path).map_err(|error| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": format!("failed to read log metadata: {error}") })),
        )
    })?;
    let size = metadata.len();
    let start = size.saturating_sub(LOG_TAIL_LIMIT);
    let mut file = fs::File::open(&path).map_err(|error| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": format!("failed to open log: {error}") })),
        )
    })?;
    file.seek(SeekFrom::Start(start)).map_err(|error| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": format!("failed to read log: {error}") })),
        )
    })?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).map_err(|error| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": format!("failed to read log: {error}") })),
        )
    })?;

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
                serde_json::json!({ "error": "cannot delete a running instance — stop the game first" }),
            ),
        ));
    }

    let keep_files = query.get("keep_files").is_some_and(|value| value == "true");
    state
        .instances()
        .remove(&id, !keep_files)
        .map_err(|error| {
            let status = if error.to_string().contains("not found") {
                StatusCode::NOT_FOUND
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            };
            (
                status,
                Json(serde_json::json!({ "error": format!("failed to delete: {error}") })),
            )
        })?;

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

fn is_safe_resource_name(name: &str) -> bool {
    !name.is_empty()
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
    use croopor_performance::PerformanceManager;
    use std::{collections::HashMap, sync::Arc};

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
            serde_json::json!({ "error": "failed to read instances: an instance with this name already exists" })
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
            serde_json::json!({ "error": "failed to read instances: an instance with this name already exists" })
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
            serde_json::json!({ "error": "failed to read instances: an instance with this name already exists" })
        );
        assert_eq!(fixture.state.instances().list().len(), 2);
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
}
