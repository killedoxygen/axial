use crate::state::{AppState, LaunchEvent, LaunchLogEvent, LaunchStatusEvent};
use axum::{
    Json,
    http::StatusCode,
    response::sse::{Event, Sse},
};
use croopor_launcher::{is_terminal_state, is_terminal_status, snapshot_status};
use std::convert::Infallible;

pub(super) async fn launch_events_sse(
    state: AppState,
    id: String,
) -> Result<
    Sse<impl futures_util::Stream<Item = Result<Event, Infallible>>>,
    (StatusCode, Json<serde_json::Value>),
> {
    let snapshot = state.sessions().get(&id).await.ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "session not found" })),
        )
    })?;
    let mut receiver = state.sessions().subscribe(&id).await.ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "session not found" })),
        )
    })?;

    let stream = async_stream::stream! {
        yield Ok(status_event(&snapshot_status(&snapshot)));
        if is_terminal_state(snapshot.state) {
            return;
        }

        loop {
            match receiver.recv().await {
                Ok(LaunchEvent::Status(status)) => {
                    let terminal = is_terminal_status(&status);
                    yield Ok(status_event(&status));
                    if terminal {
                        return;
                    }
                }
                Ok(LaunchEvent::Log(log)) => {
                    yield Ok(log_event(&log));
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            }
        }
    };

    Ok(Sse::new(stream))
}

fn status_event(status: &LaunchStatusEvent) -> Event {
    Event::default()
        .event("status")
        .data(serde_json::to_string(status).unwrap_or_else(|_| "{}".to_string()))
}

fn log_event(log: &LaunchLogEvent) -> Event {
    Event::default()
        .event("log")
        .data(serde_json::to_string(log).unwrap_or_else(|_| "{}".to_string()))
}
