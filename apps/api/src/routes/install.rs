use crate::state::{AppState, DownloadProgress};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    response::sse::{Event, Sse},
    routing::{get, post},
};
use croopor_minecraft::Downloader;
use serde::Deserialize;
use std::{convert::Infallible, path::PathBuf, time::SystemTime};
use tokio::sync::mpsc;

#[derive(Debug, Deserialize)]
struct InstallRequest {
    version_id: String,
    #[serde(default)]
    manifest_url: String,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/install", post(handle_install))
        .route("/api/v1/install/{id}/events", get(handle_install_events))
}

async fn handle_install(
    State(state): State<AppState>,
    Json(payload): Json<InstallRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    if payload.version_id.trim().is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "version_id is required" })),
        ));
    }

    let mc_dir = state.mc_dir().ok_or_else(|| {
        (
            StatusCode::PRECONDITION_FAILED,
            Json(serde_json::json!({ "error": "minecraft directory not configured" })),
        )
    })?;

    let install_id = generate_install_id();
    state.installs().insert(install_id.clone()).await;

    let store = state.installs().clone();
    let version_id = payload.version_id.trim().to_string();
    let manifest_url = payload.manifest_url.trim().to_string();
    let mc_dir = PathBuf::from(mc_dir);
    let install_id_task = install_id.clone();

    tokio::spawn(async move {
        let (progress_tx, mut progress_rx) = mpsc::unbounded_channel::<DownloadProgress>();
        let store_task = {
            let store = store.clone();
            let install_id = install_id_task.clone();
            tokio::spawn(async move {
                while let Some(progress) = progress_rx.recv().await {
                    store.emit(&install_id, progress).await;
                }
            })
        };

        let downloader = Downloader::new(mc_dir);
        let _ = downloader
            .install_version(
                &version_id,
                (!manifest_url.is_empty()).then_some(manifest_url.as_str()),
                |progress| {
                    let _ = progress_tx.send(progress);
                },
            )
            .await;
        drop(progress_tx);
        let _ = store_task.await;
    });

    Ok(Json(serde_json::json!({ "install_id": install_id })))
}

async fn handle_install_events(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<
    Sse<impl futures_util::Stream<Item = Result<Event, Infallible>>>,
    (StatusCode, Json<serde_json::Value>),
> {
    let (history, mut receiver, done) = state.installs().subscribe(&id).await.ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "install session not found" })),
        )
    })?;

    let store = state.installs().clone();
    let install_id = id.clone();
    let stream = async_stream::stream! {
        for progress in history {
            let terminal = progress.done;
            yield Ok(progress_event(&progress));
            if terminal {
                store.remove(&install_id).await;
                return;
            }
        }
        if done {
            store.remove(&install_id).await;
            return;
        }

        loop {
            match receiver.recv().await {
                Ok(progress) => {
                    let terminal = progress.done;
                    yield Ok(progress_event(&progress));
                    if terminal {
                        store.remove(&install_id).await;
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

fn progress_event(progress: &DownloadProgress) -> Event {
    Event::default()
        .event("progress")
        .data(serde_json::to_string(progress).unwrap_or_else(|_| "{}".to_string()))
}

fn generate_install_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|value| value.as_nanos())
        .unwrap_or_default();
    format!("install-{:032x}", nanos)
}
