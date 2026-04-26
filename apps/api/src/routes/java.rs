use crate::state::AppState;
use axum::{Json, Router, extract::State, routing::get};
use croopor_minecraft::list_java_runtimes;
use serde::Serialize;
use std::path::PathBuf;

#[derive(Debug, Serialize)]
struct JavaResponse {
    runtimes: Vec<croopor_minecraft::JavaRuntimeResult>,
}

pub fn router() -> Router<AppState> {
    Router::new().route("/api/v1/java", get(handle_java))
}

async fn handle_java(State(state): State<AppState>) -> Json<JavaResponse> {
    let runtimes = state
        .library_dir()
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .map(|path| list_java_runtimes(&path))
        .unwrap_or_default();

    Json(JavaResponse { runtimes })
}
