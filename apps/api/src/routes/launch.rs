//! HTTP adapts domain-owned sessions; reconnects never repeat a launch mutation.

use axial_app::{
    instances::model::InstanceId,
    launch::{
        coordinator::{
            LaunchCoordinator, LaunchError, LaunchErrorResponse, LaunchIntentStatus, LaunchRequest,
        },
        logs::LogEntry,
        reports::{LaunchReportStore, valid_report_id},
        session::{SessionManager, SessionPhase, SessionSnapshot},
    },
};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, State, rejection::JsonRejection},
    http::StatusCode,
    response::sse::{Event, KeepAlive, Sse},
    routing::{get, post},
};
use futures_util::{Stream, stream};
use serde_json::{Value, json};
use std::{collections::VecDeque, convert::Infallible};

#[derive(Clone)]
struct LaunchState {
    coordinator: LaunchCoordinator,
    sessions: SessionManager,
}
type ApiError = (StatusCode, Json<LaunchErrorResponse>);

pub fn router(coordinator: LaunchCoordinator, sessions: SessionManager) -> Router {
    Router::new()
        .route("/api/v1/launch", post(launch))
        .route("/api/v1/launch/sessions", get(list_sessions))
        .route("/api/v1/launch/intents/{key}", get(intent))
        .route("/api/v1/launch/preflight/{id}", get(preflight))
        .route("/api/v1/launch/{id}/status", get(status))
        .route("/api/v1/launch/{id}/kill", post(kill))
        .route("/api/v1/launch/{id}/events", get(events))
        .route("/api/v1/launch/{id}/logs", get(logs))
        .route("/api/v1/launch/{id}/command", get(command))
        .layer(DefaultBodyLimit::max(16 * 1024))
        .with_state(LaunchState {
            coordinator,
            sessions,
        })
}

fn error(code: LaunchError) -> ApiError {
    let status = match code {
        LaunchError::InvalidRequest
        | LaunchError::InvalidIntent
        | LaunchError::InvalidMemory
        | LaunchError::InvalidUsername => StatusCode::BAD_REQUEST,
        LaunchError::InstanceNotFound => StatusCode::NOT_FOUND,
        LaunchError::Closed
        | LaunchError::AtCapacity
        | LaunchError::IntentUnavailable
        | LaunchError::RecoveryIncomplete => StatusCode::SERVICE_UNAVAILABLE,
        _ => StatusCode::CONFLICT,
    };
    (status, Json(code.into()))
}

async fn launch(
    State(state): State<LaunchState>,
    request: Result<Json<LaunchRequest>, JsonRejection>,
) -> Result<Json<SessionSnapshot>, ApiError> {
    let Json(request) = request.map_err(|_| error(LaunchError::InvalidRequest))?;
    state
        .coordinator
        .launch(request)
        .await
        .map(Json)
        .map_err(error)
}

async fn list_sessions(State(state): State<LaunchState>) -> Json<Value> {
    Json(json!({ "sessions": state.sessions.snapshots() }))
}

async fn intent(
    State(state): State<LaunchState>,
    Path(key): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let result = state
        .coordinator
        .intent(&key)
        .map_err(error)?
        .ok_or_else(|| error(LaunchError::InstanceNotFound))?;
    Ok(Json(match result {
        LaunchIntentStatus::Preparing => json!({"state":"preparing"}),
        LaunchIntentStatus::Accepted { session } => {
            let session = state
                .sessions
                .snapshot_by_session_id(&session.session_id)
                .unwrap_or(session);
            json!({"state":"accepted", "session":session})
        }
        LaunchIntentStatus::Rejected { error } => {
            json!({"state":"rejected", "error":error.error, "code":error.code})
        }
        LaunchIntentStatus::Interrupted { session_id } => {
            let error = LaunchError::Interrupted;
            json!({"state":"interrupted", "session_id":session_id,"error":error.to_string(),"code":error})
        }
    }))
}

async fn preflight(
    State(state): State<LaunchState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let id: InstanceId = id.parse().map_err(|_| error(LaunchError::InvalidRequest))?;
    Ok(Json(json!(state.coordinator.preflight(id).await)))
}

fn session_id(id: &str) -> Result<(), ApiError> {
    uuid::Uuid::parse_str(id)
        .ok()
        .filter(|uuid| !uuid.is_nil() && uuid.to_string() == id)
        .map(|_| ())
        .ok_or_else(|| error(LaunchError::InvalidRequest))
}

async fn status(
    State(state): State<LaunchState>,
    Path(id): Path<String>,
) -> Result<Json<SessionSnapshot>, ApiError> {
    session_id(&id)?;
    state
        .sessions
        .snapshot_by_session_id(&id)
        .map(Json)
        .ok_or_else(|| error(LaunchError::InstanceNotFound))
}

async fn kill(
    State(state): State<LaunchState>,
    Path(id): Path<String>,
) -> Result<Json<SessionSnapshot>, ApiError> {
    session_id(&id)?;
    state
        .sessions
        .stop_by_session_id(&id)
        .map(Json)
        .map_err(|_| error(LaunchError::InstanceNotFound))
}

async fn logs(
    State(state): State<LaunchState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    session_id(&id)?;
    if let Some(entries) = state.sessions.logs_by_session_id(&id) {
        return Ok(Json(json!({"entries": entries})));
    }
    Err(error(LaunchError::InstanceNotFound))
}

async fn command(
    State(state): State<LaunchState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    if !cfg!(debug_assertions) {
        return Err(error(LaunchError::InstanceNotFound));
    }
    session_id(&id)?;
    let command = state
        .sessions
        .command_inspection(&id)
        .ok_or_else(|| error(LaunchError::InstanceNotFound))?;
    Ok(Json(
        json!({"session_id": command.session_id,"command_arg_count": command.command_arg_count,
        "java_path_present": command.java_path_present}),
    ))
}

struct EventState {
    sessions: SessionManager,
    id: String,
    status: tokio::sync::watch::Receiver<SessionSnapshot>,
    logs: tokio::sync::broadcast::Receiver<LogEntry>,
    pending: VecDeque<LogEntry>,
    first: bool,
    terminal: bool,
    last_log: u64,
}

async fn events(
    State(state): State<LaunchState>,
    Path(id): Path<String>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ApiError> {
    session_id(&id)?;
    let status = state
        .sessions
        .subscribe_by_session_id(&id)
        .ok_or_else(|| error(LaunchError::InstanceNotFound))?;
    let logs = state
        .sessions
        .subscribe_logs_by_session_id(&id)
        .ok_or_else(|| error(LaunchError::InstanceNotFound))?;
    let initial = EventState {
        sessions: state.sessions,
        id,
        status,
        logs: logs.events,
        pending: logs.entries.into(),
        first: true,
        terminal: false,
        last_log: 0,
    };
    let output = stream::unfold(initial, |mut state| async move {
        loop {
            if let Some(log) = state.pending.pop_front() {
                if log.sequence <= state.last_log {
                    continue;
                }
                state.last_log = log.sequence;
                let event = Event::default()
                    .event("log")
                    .json_data(log)
                    .expect("bounded log serialization");
                return Some((Ok(event), state));
            }
            if state.first {
                state.first = false;
                let snapshot = state.status.borrow_and_update().clone();
                state.terminal = snapshot.phase == SessionPhase::Exited;
                let event = Event::default()
                    .event("status")
                    .json_data(snapshot)
                    .expect("session serialization");
                return Some((Ok(event), state));
            }
            if state.terminal {
                return None;
            }
            tokio::select! {
                biased;
                log = state.logs.recv() => match log {
                    Ok(log) => state.pending.push_back(log),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        let replacement = state.sessions.subscribe_logs_by_session_id(&state.id)?;
                        state.logs = replacement.events;
                        state.pending.extend(replacement.entries);
                    },
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
                },
                changed = state.status.changed() => {
                    if changed.is_err() { return None; }
                    state.first = true;
                },
            }
        }
    });
    Ok(Sse::new(output).keep_alive(KeepAlive::default()))
}

pub fn reports_router(reports: LaunchReportStore) -> Router {
    Router::new()
        .route("/api/v1/launch/reports", get(reports_list))
        .route("/api/v1/launch/reports/{id}", get(report))
        .with_state(reports)
}

async fn reports_list(State(reports): State<LaunchReportStore>) -> Result<Json<Value>, ApiError> {
    let records = reports
        .list_recent(64)
        .map_err(|_| error(LaunchError::PreparationFailed))?;
    let reports = records
        .iter()
        .map(axial_app::performance::proofs::proof_payload)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| error(LaunchError::PreparationFailed))?;
    Ok(Json(json!({"reports":reports})))
}

async fn report(
    State(reports): State<LaunchReportStore>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    if !valid_report_id(&id) {
        return Err(error(LaunchError::InvalidRequest));
    }
    let report = reports
        .get(&id)
        .map_err(|_| error(LaunchError::PreparationFailed))?
        .ok_or_else(|| error(LaunchError::InstanceNotFound))?;
    axial_app::performance::proofs::proof_payload(&report)
        .map(Json)
        .map_err(|_| error(LaunchError::PreparationFailed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axial_app::{
        launch::{outcome::SessionOutcome, reports::SessionReportInput},
        storage::MetadataStore,
    };
    use axum::{
        body::{Body, to_bytes},
        http::Request,
    };
    use std::sync::Arc;
    use tower::ServiceExt;

    #[tokio::test]
    async fn historical_reports_include_the_retained_history_view_model() {
        let reports =
            LaunchReportStore::new(Arc::new(MetadataStore::in_memory().unwrap())).unwrap();
        let session_id = uuid::Uuid::new_v4().to_string();
        reports
            .record_session(SessionReportInput {
                session_id: session_id.clone(),
                instance_id: uuid::Uuid::new_v4().to_string(),
                version_id: "1.21.1".into(),
                launched_at: "2026-09-26T10:00:00.000Z".into(),
                ended_at: "2026-09-26T10:00:01.000Z".into(),
                outcome: SessionOutcome::spawn_failed(),
                entries: vec![],
                exit_code: None,
                boot_duration_ms: None,
                logs_dropped: 0,
            })
            .unwrap();
        let app = reports_router(reports);
        for path in [
            "/api/v1/launch/reports".to_owned(),
            format!("/api/v1/launch/reports/{session_id}"),
        ] {
            let response = app
                .clone()
                .oneshot(Request::builder().uri(&path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let bytes = to_bytes(response.into_body(), 512 * 1024).await.unwrap();
            let body: Value = serde_json::from_slice(&bytes).unwrap();
            let report = if path.ends_with("reports") {
                &body["reports"][0]
            } else {
                &body
            };
            assert_eq!(report["session_id"], session_id);
            assert_eq!(report["view_model"]["outcome_tone"], "err");
            assert!(report["view_model"]["outcome_label"].is_string());
            assert!(report["view_model"]["comparison"]["detail"].is_string());
        }
    }
}
