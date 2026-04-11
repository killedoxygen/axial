use crate::state::AppState;
use axum::{Json, Router, extract::State, routing::get};
use serde::Serialize;

#[derive(Debug, Serialize)]
struct StatusResponse {
    status: &'static str,
    mc_dir: String,
    setup_required: bool,
    app_name: String,
    version: String,
    dev_mode: bool,
}

pub fn router() -> Router<AppState> {
    Router::new().route("/api/v1/status", get(handle_status))
}

async fn handle_status(State(state): State<AppState>) -> Json<StatusResponse> {
    let mc_dir = state.mc_dir().unwrap_or_default();

    Json(StatusResponse {
        status: "ok",
        setup_required: mc_dir.is_empty(),
        mc_dir,
        app_name: state.app_name().to_string(),
        version: state.version().to_string(),
        dev_mode: cfg!(debug_assertions),
    })
}
