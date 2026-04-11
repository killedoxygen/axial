use crate::state::AppState;
use axum::{Json, Router, extract::State, routing::get};
use serde::Serialize;

#[derive(Debug, Serialize)]
struct UpdateResponse {
    current_version: String,
    latest_version: String,
    available: bool,
    platform: String,
    arch: String,
    kind: &'static str,
    notes_url: String,
    action_url: String,
    action_label: String,
    checked_at: String,
}

pub fn router() -> Router<AppState> {
    Router::new().route("/api/v1/update", get(handle_update))
}

async fn handle_update(State(state): State<AppState>) -> Json<UpdateResponse> {
    let version = state.version().to_string();
    Json(UpdateResponse {
        current_version: version.clone(),
        latest_version: version,
        available: false,
        platform: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        kind: "none",
        notes_url: String::new(),
        action_url: String::new(),
        action_label: String::new(),
        checked_at: chrono::DateTime::<chrono::Utc>::from(std::time::SystemTime::now())
            .to_rfc3339(),
    })
}
