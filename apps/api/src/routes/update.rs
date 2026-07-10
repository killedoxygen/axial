use crate::application::{self, UpdateDownloadRequest, UpdateFlowResponse, UpdateResponse};
use crate::state::AppState;
use axum::{
    Json, Router,
    extract::{Query, State},
    http::StatusCode,
    routing::{get, post},
};
use serde::Deserialize;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/update", get(handle_update))
        .route("/api/v1/update/flow", get(handle_update_flow))
        .route("/api/v1/update/download", post(handle_update_download))
        .route("/api/v1/update/apply", post(handle_update_apply))
}

#[derive(Debug, Deserialize)]
struct UpdateQuery {
    #[serde(default)]
    force: Option<String>,
}

fn force_requested(query: &UpdateQuery) -> bool {
    matches!(query.force.as_deref(), Some("1") | Some("true"))
}

async fn handle_update(
    State(state): State<AppState>,
    Query(query): Query<UpdateQuery>,
) -> Result<Json<UpdateResponse>, (StatusCode, Json<serde_json::Value>)> {
    application::update_status(&state, force_requested(&query))
        .await
        .map(Json)
}

async fn handle_update_flow(State(state): State<AppState>) -> Json<UpdateFlowResponse> {
    Json(application::update_flow_state(&state))
}

async fn handle_update_download(
    State(state): State<AppState>,
    Json(request): Json<UpdateDownloadRequest>,
) -> Result<Json<UpdateFlowResponse>, (StatusCode, Json<serde_json::Value>)> {
    application::start_update_download(&state, request).await
}

async fn handle_update_apply(
    State(state): State<AppState>,
) -> Result<Json<UpdateFlowResponse>, (StatusCode, Json<serde_json::Value>)> {
    application::apply_staged_update(&state).await
}
