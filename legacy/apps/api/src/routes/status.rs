use crate::{application, state::AppState};
use axum::{Json, Router, extract::State, routing::get};

pub fn router() -> Router<AppState> {
    Router::new().route("/api/v1/status", get(handle_status))
}

async fn handle_status(State(state): State<AppState>) -> Json<application::StatusResponse> {
    Json(application::launcher_status(&state))
}
