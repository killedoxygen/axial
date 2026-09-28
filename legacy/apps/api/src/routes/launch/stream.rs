use crate::application::launch as launch_app;
use crate::observability::{RedactionAudience, sanitize_evidence_token, sanitize_public_log_line};
use crate::state::{AppState, LaunchEvent, LaunchLogEvent, ProducerLease};
use axum::{
    Json,
    http::StatusCode,
    response::sse::{Event, Sse},
};
use std::convert::Infallible;

pub(super) async fn launch_events_sse(
    state: AppState,
    id: String,
    producer: ProducerLease,
) -> Result<
    Sse<impl futures_util::Stream<Item = Result<Event, Infallible>>>,
    (StatusCode, Json<serde_json::Value>),
> {
    let mut subscription = state
        .sessions()
        .subscribe_events(&id)
        .await
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": "session not found" })),
            )
        })?;
    let stream = async_stream::stream! {
        let request_drain = producer.wait_for_request_drain_start();
        tokio::pin!(request_drain);
        let status = launch_app::public_launch_status(subscription.retained_status());
        let mut last_revision = status.revision;
        let terminal = status.view_model.terminal;
        yield Ok(status_event(&status));
        if terminal {
            return;
        }

        loop {
            let event = tokio::select! {
                biased;
                _ = &mut request_drain => return,
                event = subscription.recv() => event,
            };
            match event {
                Ok(LaunchEvent::Status(status)) => {
                    if status.revision <= last_revision {
                        continue;
                    }
                    let status = launch_app::public_launch_status(&status);
                    last_revision = status.revision;
                    let terminal = status.view_model.terminal;
                    yield Ok(status_event(&status));
                    if terminal {
                        return;
                    }
                }
                Ok(LaunchEvent::Log(log)) => {
                    yield Ok(log_event(&log));
                }
                Ok(LaunchEvent::ProcessSettled { .. }) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    let Some(status) = subscription.rebase().await else {
                        return;
                    };
                    if status.revision <= last_revision {
                        continue;
                    }
                    let status = launch_app::public_launch_status(&status);
                    last_revision = status.revision;
                    let terminal = status.view_model.terminal;
                    yield Ok(status_event(&status));
                    if terminal {
                        return;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            }
        }
    };

    Ok(Sse::new(stream))
}

fn status_event(status: &launch_app::PublicLaunchStatus) -> Event {
    Event::default()
        .event("status")
        .data(serde_json::to_string(status).unwrap_or_else(|_| "{}".to_string()))
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
