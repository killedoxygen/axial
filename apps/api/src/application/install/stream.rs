use super::{
    InstallApplicationError,
    operation::{
        install_journal_is_terminal, install_operation_journal_for_session,
        install_progress_history_from_journal,
    },
};
use crate::state::{AppState, InstallProgressRecord, ProducerLease};
use axum::{
    Json,
    http::StatusCode,
    response::sse::{Event, Sse},
};
use std::convert::Infallible;

pub(crate) async fn install_events_stream(
    state: &AppState,
    id: &str,
    producer: ProducerLease,
) -> Result<
    Sse<impl futures_util::Stream<Item = Result<Event, Infallible>> + use<>>,
    InstallApplicationError,
> {
    install_progress_events_stream(state, id, producer, "install session not found", false).await
}

pub(crate) async fn loader_install_events_stream(
    state: &AppState,
    id: &str,
    producer: ProducerLease,
) -> Result<
    Sse<impl futures_util::Stream<Item = Result<Event, Infallible>> + use<>>,
    InstallApplicationError,
> {
    install_progress_events_stream(
        state,
        id,
        producer,
        "loader install session not found",
        true,
    )
    .await
}

async fn install_progress_events_stream(
    state: &AppState,
    id: &str,
    producer: ProducerLease,
    missing_message: &'static str,
    loader_install: bool,
) -> Result<
    Sse<impl futures_util::Stream<Item = Result<Event, Infallible>> + use<>>,
    InstallApplicationError,
> {
    let subscription = state.installs().subscribe_records(id).await;
    let journal = match subscription.as_ref() {
        Some((snapshot, _)) => state.journals().get(&snapshot.operation_id),
        None => install_operation_journal_for_session(state.journals(), id),
    };
    if subscription.is_none() && journal.is_none() {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": missing_message })),
        ));
    }
    if subscription.is_none()
        && journal
            .as_ref()
            .is_some_and(|journal| !install_journal_is_terminal(journal.status))
    {
        return Err((
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": "install operation is not currently streamable"
            })),
        ));
    }

    let (replay, mut receiver, done) = if let Some((snapshot, receiver)) = subscription {
        (snapshot.latest, Some(receiver), snapshot.done)
    } else {
        (
            journal
                .as_ref()
                .map(install_progress_history_from_journal)
                .and_then(|mut history| history.pop())
                .map(InstallProgressRecord::new),
            None,
            journal
                .as_ref()
                .is_some_and(|journal| install_journal_is_terminal(journal.status)),
        )
    };

    let store = state.installs().clone();
    let install_id = id.to_string();
    let stream = async_stream::stream! {
        let request_drain = producer.wait_for_request_drain_start();
        tokio::pin!(request_drain);
        if let Some(record) = replay {
            let terminal = record.progress.done;
            yield Ok(install_progress_event(&record, loader_install));
            if terminal {
                return;
            }
        }
        if done {
            return;
        }

        let Some(receiver) = receiver.as_mut() else {
            return;
        };

        loop {
            let record = tokio::select! {
                biased;
                _ = &mut request_drain => return,
                record = receiver.recv() => record,
            };
            match record {
                Ok(record) => {
                    let terminal = record.progress.done;
                    yield Ok(install_progress_event(&record, loader_install));
                    if terminal {
                        return;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    store.remove(&install_id).await;
                    return;
                }
            }
        }
    };

    Ok(Sse::new(stream))
}

fn install_progress_event(record: &InstallProgressRecord, loader_install: bool) -> Event {
    let payload = if loader_install {
        super::public_loader_install_progress_record_json(record)
    } else {
        super::public_vanilla_install_progress_record_json(record)
    };
    Event::default()
        .event("progress")
        .data(serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_string()))
}
