use super::{
    InstallApplicationError, install_operation_id,
    operation::install_progress_history_from_journal, sanitize_install_progress,
};
use crate::state::AppState;
use axum::{
    Json,
    http::StatusCode,
    response::sse::{Event, Sse},
};
use croopor_minecraft::DownloadProgress;
use std::convert::Infallible;

pub async fn install_events_stream(
    state: &AppState,
    id: &str,
) -> Result<
    Sse<impl futures_util::Stream<Item = Result<Event, Infallible>> + use<>>,
    InstallApplicationError,
> {
    install_progress_events_stream(state, id, "install session not found", false).await
}

pub async fn loader_install_events_stream(
    state: &AppState,
    id: &str,
) -> Result<
    Sse<impl futures_util::Stream<Item = Result<Event, Infallible>> + use<>>,
    InstallApplicationError,
> {
    install_progress_events_stream(state, id, "loader install session not found", true).await
}

async fn install_progress_events_stream(
    state: &AppState,
    id: &str,
    missing_message: &'static str,
    loader_install: bool,
) -> Result<
    Sse<impl futures_util::Stream<Item = Result<Event, Infallible>> + use<>>,
    InstallApplicationError,
> {
    let subscription = state.installs().subscribe(id).await;
    let operation_id = install_operation_id(id);
    let journal = state.journals().get(&operation_id);
    if subscription.is_none() && journal.is_none() {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": missing_message })),
        ));
    }

    let (history, mut receiver, done) = if let Some((history, receiver, done)) = subscription {
        (history, Some(receiver), done)
    } else {
        (
            journal
                .as_ref()
                .map(install_progress_history_from_journal)
                .unwrap_or_default(),
            None,
            true,
        )
    };

    let store = state.installs().clone();
    let install_id = id.to_string();
    let stream = async_stream::stream! {
        for progress in history {
            let progress = sanitize_install_progress(progress);
            let terminal = progress.done;
            yield Ok(install_progress_event(&progress, loader_install));
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
            match receiver.recv().await {
                Ok(progress) => {
                    let progress = sanitize_install_progress(progress);
                    let terminal = progress.done;
                    yield Ok(install_progress_event(&progress, loader_install));
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

fn install_progress_event(progress: &DownloadProgress, loader_install: bool) -> Event {
    let payload = if loader_install {
        super::public_loader_install_progress_json(progress)
    } else {
        super::public_vanilla_install_progress_json(progress)
    };
    Event::default()
        .event("progress")
        .data(serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_string()))
}
