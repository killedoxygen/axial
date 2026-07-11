use crate::application::{self, DeleteVersionRequest, VersionInfoResponse};
use crate::state::{AppState, RequestProducerHandoff};
use axum::{
    Json, Router,
    extract::{Extension, Path, State},
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
    Extension(handoff): Extension<RequestProducerHandoff>,
    Path(version_id): Path<String>,
) -> Result<Json<VersionInfoResponse>, (StatusCode, Json<serde_json::Value>)> {
    let producer = handoff
        .try_claim()
        .map_err(super::producer_claim_error_response)?;
    application::version_info(&state, &producer, &version_id)
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
    Extension(handoff): Extension<RequestProducerHandoff>,
    Path(version_id): Path<String>,
    Json(payload): Json<DeleteVersionRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let producer = handoff
        .try_claim()
        .map_err(super::producer_claim_error_response)?;
    application::delete_version(&state, &producer, &version_id, payload)
        .await
        .map(Json)
}
