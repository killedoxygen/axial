//! HTTP adapts domain-owned sessions; reconnects never repeat a launch mutation.

use super::error::{ApiError, ApiResult, Extracted};
use axial_app::{
    instances::model::InstanceId,
    launch::{
        coordinator::{
            LaunchCoordinator, LaunchError, LaunchIntentView, LaunchPreflight, LaunchRequest,
        },
        logs::LogEntry,
        reports::{LaunchReportStore, LaunchReportsResponse, valid_report_id},
        session::{
            SessionLogsResponse, SessionManager, SessionPhase, SessionSnapshot, SessionsResponse,
        },
    },
    performance::proofs::proof_payload,
};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, State, rejection::JsonRejection},
    response::sse::{Event, KeepAlive, Sse},
    routing::{get, post},
};
use futures_util::{Stream, stream};
use std::{collections::VecDeque, convert::Infallible};

#[derive(Clone)]
struct LaunchState {
    coordinator: LaunchCoordinator,
    sessions: SessionManager,
}

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

async fn launch(
    State(state): State<LaunchState>,
    request: Result<Json<LaunchRequest>, JsonRejection>,
) -> ApiResult<SessionSnapshot> {
    let request = request.or_refuse(|| LaunchError::InvalidRequest)?;
    Ok(Json(state.coordinator.launch(request).await?))
}

async fn list_sessions(State(state): State<LaunchState>) -> Json<SessionsResponse> {
    Json(SessionsResponse {
        sessions: state.sessions.snapshots(),
    })
}

async fn intent(
    State(state): State<LaunchState>,
    Path(key): Path<String>,
) -> ApiResult<LaunchIntentView> {
    Ok(Json(
        state
            .coordinator
            .intent_view(&key)?
            .ok_or(LaunchError::InstanceNotFound)?,
    ))
}

async fn preflight(
    State(state): State<LaunchState>,
    Path(id): Path<String>,
) -> ApiResult<LaunchPreflight> {
    let id: InstanceId = id.parse().map_err(|_| LaunchError::InvalidRequest)?;
    Ok(Json(state.coordinator.preflight(id).await))
}

fn session_id(id: &str) -> Result<(), LaunchError> {
    uuid::Uuid::parse_str(id)
        .ok()
        .filter(|uuid| !uuid.is_nil() && uuid.to_string() == id)
        .map(|_| ())
        .ok_or(LaunchError::InvalidRequest)
}

async fn status(
    State(state): State<LaunchState>,
    Path(id): Path<String>,
) -> ApiResult<SessionSnapshot> {
    session_id(&id)?;
    Ok(Json(
        state
            .sessions
            .snapshot_by_session_id(&id)
            .ok_or(LaunchError::InstanceNotFound)?,
    ))
}

async fn kill(
    State(state): State<LaunchState>,
    Path(id): Path<String>,
) -> ApiResult<SessionSnapshot> {
    session_id(&id)?;
    Ok(Json(
        state
            .sessions
            .stop_by_session_id(&id)
            .map_err(|_| LaunchError::InstanceNotFound)?,
    ))
}

async fn logs(
    State(state): State<LaunchState>,
    Path(id): Path<String>,
) -> ApiResult<SessionLogsResponse> {
    session_id(&id)?;
    let entries = state
        .sessions
        .logs_by_session_id(&id)
        .ok_or(LaunchError::InstanceNotFound)?;
    Ok(Json(SessionLogsResponse { entries }))
}

#[cfg(debug_assertions)]
async fn command(
    State(state): State<LaunchState>,
    Path(id): Path<String>,
) -> ApiResult<axial_app::developer::command::LaunchCommandInspection> {
    session_id(&id)?;
    let command = state
        .sessions
        .command_inspection(&id)
        .ok_or(LaunchError::InstanceNotFound)?;
    Ok(Json(axial_app::developer::command::inspect_launch_command(
        &command.session_id,
        command.command_arg_count,
        command.java_path_present,
    )))
}

#[cfg(not(debug_assertions))]
async fn command(_state: State<LaunchState>, _id: Path<String>) -> Result<(), ApiError> {
    Err(LaunchError::InstanceNotFound.into())
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
        .ok_or(LaunchError::InstanceNotFound)?;
    let logs = state
        .sessions
        .subscribe_logs_by_session_id(&id)
        .ok_or(LaunchError::InstanceNotFound)?;
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

async fn reports_list(
    State(reports): State<LaunchReportStore>,
) -> ApiResult<LaunchReportsResponse> {
    let records = reports
        .list_recent(64)
        .map_err(|_| LaunchError::PreparationFailed)?;
    let reports = records
        .iter()
        .map(proof_payload)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| LaunchError::PreparationFailed)?;
    Ok(Json(LaunchReportsResponse { reports }))
}

async fn report(
    State(reports): State<LaunchReportStore>,
    Path(id): Path<String>,
) -> ApiResult<serde_json::Value> {
    if !valid_report_id(&id) {
        return Err(LaunchError::InvalidRequest.into());
    }
    let report = reports
        .get(&id)
        .map_err(|_| LaunchError::PreparationFailed)?
        .ok_or(LaunchError::InstanceNotFound)?;
    Ok(Json(
        proof_payload(&report).map_err(|_| LaunchError::PreparationFailed)?,
    ))
}

#[cfg(test)]
mod tests {
    use crate::fixture::Profile;
    use axial_app::launch::{
        outcome::SessionOutcome,
        reports::{LaunchReportStore, SessionReportInput},
    };

    #[tokio::test]
    async fn historical_reports_include_the_retained_history_view_model() {
        let profile = Profile::start().await;
        let reports = LaunchReportStore::new(profile.services.settings.metadata().clone()).unwrap();
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
        let list = profile.api.get("/api/v1/launch/reports").await;
        let single = profile
            .api
            .get(&format!("/api/v1/launch/reports/{session_id}"))
            .await;
        for report in [&list["reports"][0], &single] {
            assert_eq!(report["session_id"], session_id);
            assert_eq!(report["view_model"]["outcome_tone"], "err");
            assert!(report["view_model"]["outcome_label"].is_string());
            assert!(report["view_model"]["comparison"]["detail"].is_string());
        }
        profile.close().await;
    }
}
