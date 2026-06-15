use crate::application::{self, DeleteVersionRequest, VersionInfoResponse};
use crate::state::AppState;
use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::{delete, get, post},
};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/versions/{id}/info", get(handle_version_info))
        .route("/api/v1/versions/{id}", delete(handle_delete_version))
        .route(
            "/api/v1/versions/{id}/open-folder",
            post(handle_open_version_folder),
        )
}

async fn handle_version_info(
    State(state): State<AppState>,
    Path(version_id): Path<String>,
) -> Result<Json<VersionInfoResponse>, (StatusCode, Json<serde_json::Value>)> {
    application::version_info(&state, &version_id)
        .await
        .map(Json)
}

async fn handle_open_version_folder(
    State(state): State<AppState>,
    Path(version_id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    application::open_version_folder(&state, &version_id).map(Json)
}

async fn handle_delete_version(
    State(state): State<AppState>,
    Path(version_id): Path<String>,
    Json(payload): Json<DeleteVersionRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    application::delete_version(&state, &version_id, payload)
        .await
        .map(Json)
}
