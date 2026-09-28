use axial_app::instances::setup::{SetupService, SetupStatusResponse};
use axum::{Json, Router, extract::State, http::StatusCode, routing::post};
use std::sync::Arc;

pub fn router(setup: Arc<SetupService>) -> Router<()> {
    Router::new()
        .route("/api/v1/setup/init", post(initialize))
        .with_state(setup)
}

async fn initialize(
    State(setup): State<Arc<SetupService>>,
) -> Result<Json<SetupStatusResponse>, (StatusCode, Json<serde_json::Value>)> {
    setup
        .initialize()
        .await
        .map(Json)
        .map_err(super::instances::error)
}
