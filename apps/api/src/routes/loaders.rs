use crate::state::AppState;
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    response::sse::{Event, Sse},
    routing::{get, post},
};
use croopor_minecraft::{
    DownloadProgress, LoaderError, LoaderType, fetch_game_versions, fetch_loader_versions,
    install_loader,
};
use serde::Deserialize;
use std::{convert::Infallible, path::PathBuf, time::SystemTime};
use tokio::sync::mpsc;

#[derive(Debug, Deserialize)]
struct LoaderVersionQuery {
    mc_version: String,
}

#[derive(Debug, Deserialize)]
struct LoaderInstallRequest {
    loader_type: String,
    game_version: String,
    loader_version: String,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/api/v1/loaders/{type}/game-versions",
            get(handle_loader_game_versions),
        )
        .route(
            "/api/v1/loaders/{type}/loader-versions",
            get(handle_loader_versions),
        )
        .route("/api/v1/loaders/install", post(handle_loader_install))
        .route(
            "/api/v1/loaders/install/{id}/events",
            get(handle_loader_install_events),
        )
}

async fn handle_loader_game_versions(
    Path(loader_type): Path<String>,
    State(_state): State<AppState>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let loader_type = parse_loader_type(&loader_type)?;
    match fetch_game_versions(loader_type).await {
        Ok(game_versions) => Ok(Json(serde_json::json!({ "game_versions": game_versions }))),
        Err(error) => Ok(Json(serde_json::json!({
            "game_versions": [],
            "error": error.to_string(),
        }))),
    }
}

async fn handle_loader_versions(
    Path(loader_type): Path<String>,
    Query(query): Query<LoaderVersionQuery>,
    State(_state): State<AppState>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let loader_type = parse_loader_type(&loader_type)?;
    if query.mc_version.trim().is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "mc_version query parameter is required" })),
        ));
    }

    match fetch_loader_versions(loader_type, &query.mc_version).await {
        Ok(loader_versions) => Ok(Json(serde_json::json!({
            "loader_versions": loader_versions
        }))),
        Err(error) => Ok(Json(serde_json::json!({
            "loader_versions": [],
            "error": error.to_string(),
        }))),
    }
}

async fn handle_loader_install(
    State(state): State<AppState>,
    Json(payload): Json<LoaderInstallRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let loader_type = parse_loader_type(&payload.loader_type)?;
    if payload.game_version.trim().is_empty() || payload.loader_version.trim().is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "loader_type, game_version, and loader_version are required"
            })),
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
    let mc_dir = PathBuf::from(mc_dir);
    let game_version = payload.game_version.trim().to_string();
    let loader_version = payload.loader_version.trim().to_string();
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

        let result = install_loader(
            &mc_dir,
            loader_type,
            &game_version,
            &loader_version,
            |progress| {
                let _ = progress_tx.send(progress);
            },
        )
        .await;

        if let Err(error) = result {
            let _ = progress_tx.send(error_progress(error));
        }

        drop(progress_tx);
        let _ = store_task.await;
    });

    Ok(Json(serde_json::json!({ "install_id": install_id })))
}

async fn handle_loader_install_events(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<
    Sse<impl futures_util::Stream<Item = Result<Event, Infallible>>>,
    (StatusCode, Json<serde_json::Value>),
> {
    let (history, mut receiver, done) = state.installs().subscribe(&id).await.ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "loader install session not found" })),
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

fn parse_loader_type(
    loader_type: &str,
) -> Result<LoaderType, (StatusCode, Json<serde_json::Value>)> {
    LoaderType::parse(loader_type).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": format!("unknown loader type: {loader_type}")
            })),
        )
    })
}

fn progress_event(progress: &DownloadProgress) -> Event {
    Event::default()
        .event("progress")
        .data(serde_json::to_string(progress).unwrap_or_else(|_| "{}".to_string()))
}

fn error_progress(error: LoaderError) -> DownloadProgress {
    DownloadProgress {
        phase: "error".to_string(),
        current: 0,
        total: 0,
        file: None,
        error: Some(error.to_string()),
        done: true,
    }
}

fn generate_install_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|value| value.as_nanos())
        .unwrap_or_default();
    format!("loader-install-{:032x}", nanos)
}
