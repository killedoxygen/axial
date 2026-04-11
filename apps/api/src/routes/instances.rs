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
use std::path::PathBuf;

#[derive(Debug, Serialize)]
struct InstancesResponse {
    instances: Vec<EnrichedInstance>,
    last_instance_id: Option<String>,
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
            "/api/v1/instances/{id}/open-folder",
            post(handle_open_instance_folder),
        )
}

async fn handle_list_instances(State(state): State<AppState>) -> Json<InstancesResponse> {
    let versions = state
        .mc_dir()
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
}

async fn handle_create_instance(
    State(state): State<AppState>,
    Json(payload): Json<CreateInstanceRequest>,
) -> Result<Json<croopor_config::Instance>, (StatusCode, Json<serde_json::Value>)> {
    let mc_dir = state.mc_dir().map(PathBuf::from);
    state
        .instances()
        .add(payload.name, payload.version_id, mc_dir.as_deref())
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
struct InstancePatch {
    name: Option<String>,
    version_id: Option<String>,
    max_memory_mb: Option<i32>,
    min_memory_mb: Option<i32>,
    java_path: Option<String>,
    window_width: Option<i32>,
    window_height: Option<i32>,
    jvm_preset: Option<String>,
    performance_mode: Option<String>,
    extra_jvm_args: Option<String>,
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

    state
        .instances()
        .update(instance)
        .map(Json)
        .map_err(|error| {
            let status = if error.to_string().contains("not found") {
                StatusCode::NOT_FOUND
            } else {
                StatusCode::BAD_REQUEST
            };
            (
                status,
                Json(serde_json::json!({ "error": error.to_string() })),
            )
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

    let mut dir = state.instances().game_dir(&instance.id);
    if let Some(sub) = query.sub.as_deref()
        && ["mods", "saves", "resourcepacks", "shaderpacks", "config"].contains(&sub)
    {
        dir = dir.join(sub);
    }

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
