use crate::{application, state::AppState};
use axum::{Json, Router, extract::State, http::StatusCode, routing::post};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/dev/cleanup-versions", post(handle_dev_cleanup))
        .route("/api/v1/dev/flush", post(handle_dev_flush))
}

async fn handle_dev_cleanup(
    State(state): State<AppState>,
) -> Result<Json<application::DevCleanupResponse>, (StatusCode, Json<serde_json::Value>)> {
    application::dev_cleanup_versions(&state).await.map(Json)
}

async fn handle_dev_flush(
    State(state): State<AppState>,
) -> Result<Json<application::DevFlushResponse>, (StatusCode, Json<serde_json::Value>)> {
    application::dev_flush(&state).await.map(Json)
}
