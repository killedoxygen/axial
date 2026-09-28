pub mod accounts;
pub mod auth;
pub mod benchmarks;
pub mod config;
pub mod content;
pub mod flags;
pub mod import;
pub mod install;
pub mod instances;
pub mod java;
pub mod launch;
pub mod loaders;
pub mod music;
pub mod performance;
pub mod resources;
pub mod setup;
pub mod skin;
pub mod system;
pub mod telemetry;
pub mod update;
pub mod versions;

use axial_app::settings::SettingsStore;
use axum::{Json, Router, extract::State, http::StatusCode, routing::get};
use std::sync::Arc;

pub(crate) fn status_router(settings: Arc<SettingsStore>) -> Router {
    Router::new()
        .route("/api/v1/status", get(status))
        .with_state(settings)
}

async fn status(
    State(settings): State<Arc<SettingsStore>>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let config = tokio::task::spawn_blocking(move || settings.current())
        .await
        .map_err(|_| status_unavailable())?
        .map_err(|_| status_unavailable())?;
    Ok(Json(serde_json::json!({
        "dev_mode": cfg!(debug_assertions),
        "setup_required": !config.onboarding_done,
        "warnings": ["This isolated rewrite is under development. Some retained features are still being integrated."],
    })))
}

fn status_unavailable() -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({ "error": "Application status is unavailable." })),
    )
}
