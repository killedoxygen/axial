use crate::application::launch as launch_app;
use crate::observability::{RedactionAudience, sanitize_evidence_token, sanitize_public_log_line};
use crate::state::{AppState, LaunchEvent, LaunchLogEvent, LaunchStatusEvent};
use axial_launcher::{is_terminal_state, is_terminal_status};
use axum::{
    Json,
    http::StatusCode,
    response::sse::{Event, Sse},
};
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
        yield Ok(status_event(&launch_app::snapshot_status(&snapshot)));
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
    Event::default().event("status").data(
        serde_json::to_string(&launch_app::public_launch_status_json(status))
            .unwrap_or_else(|_| "{}".to_string()),
    )
}

fn log_event(log: &LaunchLogEvent) -> Event {
    let source = sanitize_evidence_token(&log.source, RedactionAudience::UserVisible, 32)
        .unwrap_or_else(|| "game".to_string());
    let text = sanitize_public_log_line(&log.text, RedactionAudience::UserVisible, 1_000);
    Event::default().event("log").data(
        serde_json::to_string(&serde_json::json!({ "source": source, "text": text }))
            .unwrap_or_else(|_| "{}".to_string()),
    )
}
