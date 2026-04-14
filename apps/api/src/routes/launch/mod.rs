mod policy;
mod runner;
mod stream;
mod task;

use crate::state::{AppState, LaunchStatusEvent};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::{get, post},
};
use croopor_launcher::snapshot_status;
use serde_json::json;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/launch", post(handle_launch))
        .route("/api/v1/launch/{id}/events", get(handle_launch_events))
        .route("/api/v1/launch/{id}/status", get(handle_launch_status))
        .route("/api/v1/launch/{id}/command", get(handle_launch_command))
        .route("/api/v1/launch/{id}/kill", post(handle_launch_kill))
}

async fn handle_launch(
    State(state): State<AppState>,
    Json(payload): Json<task::LaunchRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let accepted = task::prepare_launch_session(&state, payload).await?;

    let state_task = state.clone();
    let task = accepted.task;
    tokio::spawn(async move {
        runner::run_launch_session(state_task, task).await;
    });

    Ok(Json(json!({
        "status": "accepted",
        "session_id": accepted.session_id.0,
        "instance_id": accepted.instance_id,
        "pid": 0,
        "launched_at": accepted.launched_at,
        "healing": null,
    })))
}

async fn handle_launch_events(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<
    axum::response::sse::Sse<
        impl futures_util::Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>>,
    >,
    (StatusCode, Json<serde_json::Value>),
> {
    stream::launch_events_sse(state, id).await
}

async fn handle_launch_command(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let record = state.sessions().get(&id).await.ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "session not found" })),
        )
    })?;

    Ok(Json(json!({
        "command": record.command,
        "java_path": record.java_path,
        "session_id": record.session_id.0,
        "healing": record.healing,
    })))
}

async fn handle_launch_status(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let record = state.sessions().get(&id).await.ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "session not found" })),
        )
    })?;

    let status = snapshot_status(&record);
    Ok(Json(json!({
        "state": status.state,
        "pid": status.pid,
        "exit_code": status.exit_code,
        "failure_class": status.failure_class,
        "failure_detail": status.failure_detail,
        "healing": status.healing,
        "session_id": record.session_id.0,
    })))
}

async fn handle_launch_kill(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let record = state.sessions().get(&id).await.ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "session not found" })),
        )
    })?;

    state.sessions().kill(&id).await.map_err(|error| {
        let status = if error.kind() == std::io::ErrorKind::NotFound {
            StatusCode::NOT_FOUND
        } else {
            StatusCode::INTERNAL_SERVER_ERROR
        };
        (status, Json(json!({ "error": error.to_string() })))
    })?;

    runner::trace_launch_event(&id, "kill requested by client");
    state
        .sessions()
        .emit_log(&id, "system", "Launch stopped by user.".to_string())
        .await;
    state
        .sessions()
        .emit_status(
            &id,
            LaunchStatusEvent {
                state: "exited".to_string(),
                pid: record.pid,
                exit_code: Some(-9),
                failure_class: None,
                failure_detail: Some("stopped by user".to_string()),
                healing: record.healing.clone(),
            },
        )
        .await;

    Ok(Json(json!({ "status": "killed" })))
}
