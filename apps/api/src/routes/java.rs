use crate::{application, state::AppState};
use axum::{Json, Router, extract::State, routing::get};

pub fn router() -> Router<AppState> {
    Router::new().route("/api/v1/java", get(handle_java))
}

async fn handle_java(State(state): State<AppState>) -> Json<application::JavaRuntimesResponse> {
    Json(application::java_runtimes(&state))
}
