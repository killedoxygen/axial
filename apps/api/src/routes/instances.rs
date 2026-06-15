use crate::{
    application::instances::{
        self, CreateInstanceRequest, DuplicateInstanceRequest, InstanceLogInfo,
        InstanceLogTailResponse, InstanceModInfo, InstancePatch, InstanceResourcesResponse,
        InstanceScreenshotInfo, InstanceWorldInfo, OpenFolderQuery, RenameScreenshotRequest,
        RenameWorldRequest, UpdateModRequest, WorldBackupResponse,
    },
    state::AppState,
};
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    response::Response,
    routing::{get, post, put},
};
use croopor_config::EnrichedInstance;
use std::collections::HashMap;

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

async fn handle_list_instances(
    State(state): State<AppState>,
) -> Json<instances::InstancesResponse> {
    Json(instances::handle_list_instances(&state).await)
}

async fn handle_get_instance(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<EnrichedInstance>, (StatusCode, Json<serde_json::Value>)> {
    instances::handle_get_instance(&state, &id).await.map(Json)
}

async fn handle_create_instance(
    State(state): State<AppState>,
    Json(payload): Json<CreateInstanceRequest>,
) -> Result<Json<EnrichedInstance>, (StatusCode, Json<serde_json::Value>)> {
    instances::handle_create_instance(&state, payload)
        .await
        .map(Json)
}

async fn handle_duplicate_instance(
    State(state): State<AppState>,
    Path(id): Path<String>,
    payload: Option<Json<DuplicateInstanceRequest>>,
) -> Result<Json<EnrichedInstance>, (StatusCode, Json<serde_json::Value>)> {
    instances::handle_duplicate_instance(&state, &id, payload.map(|Json(payload)| payload))
        .await
        .map(Json)
}

async fn handle_update_instance(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(patch): Json<InstancePatch>,
) -> Result<Json<EnrichedInstance>, (StatusCode, Json<serde_json::Value>)> {
    instances::handle_update_instance(&state, &id, patch)
        .await
        .map(Json)
}

async fn handle_open_instance_folder(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<OpenFolderQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    instances::handle_open_instance_folder(&state, &id, query)
        .await
        .map(Json)
}

async fn handle_instance_resources(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<InstanceResourcesResponse>, (StatusCode, Json<serde_json::Value>)> {
    instances::handle_instance_resources(&state, &id)
        .await
        .map(Json)
}

async fn handle_instance_worlds(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Vec<InstanceWorldInfo>>, (StatusCode, Json<serde_json::Value>)> {
    instances::handle_instance_worlds(&state, &id)
        .await
        .map(Json)
}

async fn handle_rename_instance_world(
    State(state): State<AppState>,
    Path((id, name)): Path<(String, String)>,
    Json(payload): Json<RenameWorldRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    instances::handle_rename_instance_world(&state, &id, &name, payload)
        .await
        .map(Json)
}

async fn handle_delete_instance_world(
    State(state): State<AppState>,
    Path((id, name)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    instances::handle_delete_instance_world(&state, &id, &name)
        .await
        .map(Json)
}

async fn handle_backup_instance_world(
    State(state): State<AppState>,
    Path((id, name)): Path<(String, String)>,
) -> Result<Json<WorldBackupResponse>, (StatusCode, Json<serde_json::Value>)> {
    instances::handle_backup_instance_world(&state, &id, &name)
        .await
        .map(Json)
}

async fn handle_instance_mods(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Vec<InstanceModInfo>>, (StatusCode, Json<serde_json::Value>)> {
    instances::handle_instance_mods(&state, &id).await.map(Json)
}

async fn handle_update_instance_mod(
    State(state): State<AppState>,
    Path((id, name)): Path<(String, String)>,
    Json(payload): Json<UpdateModRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    instances::handle_update_instance_mod(&state, &id, &name, payload)
        .await
        .map(Json)
}

async fn handle_delete_instance_mod(
    State(state): State<AppState>,
    Path((id, name)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    instances::handle_delete_instance_mod(&state, &id, &name)
        .await
        .map(Json)
}

async fn handle_instance_screenshots(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Vec<InstanceScreenshotInfo>>, (StatusCode, Json<serde_json::Value>)> {
    instances::handle_instance_screenshots(&state, &id)
        .await
        .map(Json)
}

async fn handle_instance_screenshot_file(
    State(state): State<AppState>,
    Path((id, name)): Path<(String, String)>,
) -> Result<Response, (StatusCode, Json<serde_json::Value>)> {
    instances::handle_instance_screenshot_file(&state, &id, &name).await
}

async fn handle_rename_instance_screenshot(
    State(state): State<AppState>,
    Path((id, name)): Path<(String, String)>,
    Json(payload): Json<RenameScreenshotRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    instances::handle_rename_instance_screenshot(&state, &id, &name, payload)
        .await
        .map(Json)
}

async fn handle_delete_instance_screenshot(
    State(state): State<AppState>,
    Path((id, name)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    instances::handle_delete_instance_screenshot(&state, &id, &name)
        .await
        .map(Json)
}

async fn handle_instance_logs(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Vec<InstanceLogInfo>>, (StatusCode, Json<serde_json::Value>)> {
    instances::handle_instance_logs(&state, &id).await.map(Json)
}

async fn handle_instance_log_tail(
    State(state): State<AppState>,
    Path((id, name)): Path<(String, String)>,
) -> Result<Json<InstanceLogTailResponse>, (StatusCode, Json<serde_json::Value>)> {
    instances::handle_instance_log_tail(&state, &id, &name)
        .await
        .map(Json)
}

async fn handle_delete_instance(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    instances::handle_delete_instance(&state, &id, query)
        .await
        .map(Json)
}
