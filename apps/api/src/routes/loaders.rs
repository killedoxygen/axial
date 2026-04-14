use crate::dto::loaders::{
    LoaderBuildsResponse, LoaderComponentsResponse, LoaderGameVersionsResponse,
};
use crate::install_runtime::prewarm_version_runtime;
use crate::state::AppState;
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    response::sse::{Event, Sse},
    routing::{get, post},
};
use croopor_minecraft::{
    DownloadProgress, LoaderComponentId, LoaderError, fetch_builds, fetch_components,
    fetch_supported_versions, install_build, resolve_build_record,
};
use serde::Deserialize;
use std::{convert::Infallible, path::PathBuf, time::SystemTime};
use tokio::sync::mpsc;

#[derive(Debug, Deserialize)]
struct LoaderBuildQuery {
    mc_version: String,
}

#[derive(Debug, Deserialize)]
struct LoaderInstallRequest {
    component_id: String,
    build_id: String,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/loaders/components", get(handle_loader_components))
        .route(
            "/api/v1/loaders/components/{id}/builds",
            get(handle_loader_builds),
        )
        .route(
            "/api/v1/loaders/components/{id}/game-versions",
            get(handle_loader_game_versions),
        )
        .route("/api/v1/loaders/install", post(handle_loader_install))
        .route(
            "/api/v1/loaders/install/{id}/events",
            get(handle_loader_install_events),
        )
}

async fn handle_loader_components() -> Json<LoaderComponentsResponse> {
    Json(LoaderComponentsResponse {
        components: fetch_components(),
    })
}

async fn handle_loader_builds(
    Path(component_id): Path<String>,
    Query(query): Query<LoaderBuildQuery>,
    State(state): State<AppState>,
) -> Result<Json<LoaderBuildsResponse>, (StatusCode, Json<serde_json::Value>)> {
    let component_id = parse_component_id(&component_id)?;
    if query.mc_version.trim().is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "mc_version query parameter is required" })),
        ));
    }
    let library_dir = state.library_dir().ok_or_else(|| {
        (
            StatusCode::PRECONDITION_FAILED,
            Json(serde_json::json!({ "error": "Croopor library is not configured" })),
        )
    })?;

    match fetch_builds(
        PathBuf::from(library_dir).as_path(),
        component_id,
        &query.mc_version,
    )
    .await
    {
        Ok((builds, catalog)) => Ok(Json(LoaderBuildsResponse { builds, catalog })),
        Err(error) => Err(error_response(error)),
    }
}

async fn handle_loader_game_versions(
    Path(component_id): Path<String>,
    State(state): State<AppState>,
) -> Result<Json<LoaderGameVersionsResponse>, (StatusCode, Json<serde_json::Value>)> {
    let component_id = parse_component_id(&component_id)?;
    let library_dir = state.library_dir().ok_or_else(|| {
        (
            StatusCode::PRECONDITION_FAILED,
            Json(serde_json::json!({ "error": "Croopor library is not configured" })),
        )
    })?;

    match fetch_supported_versions(PathBuf::from(library_dir).as_path(), component_id).await {
        Ok((versions, catalog)) => Ok(Json(LoaderGameVersionsResponse { versions, catalog })),
        Err(error) => Err(error_response(error)),
    }
}

async fn handle_loader_install(
    State(state): State<AppState>,
    Json(payload): Json<LoaderInstallRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let component_id = parse_component_id(&payload.component_id)?;
    if payload.build_id.trim().is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "build_id is required" })),
        ));
    }

    let library_dir = state.library_dir().ok_or_else(|| {
        (
            StatusCode::PRECONDITION_FAILED,
            Json(serde_json::json!({ "error": "Croopor library is not configured" })),
        )
    })?;
    let library_dir_path = PathBuf::from(&library_dir);

    let build = resolve_build_record(
        library_dir_path.as_path(),
        component_id,
        payload.build_id.trim(),
    )
    .await
    .map_err(error_response)?;

    let install_id = generate_install_id();
    state.installs().insert(install_id.clone()).await;

    let store = state.installs().clone();
    let library_dir = PathBuf::from(library_dir);
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

        let version_id = build.version_id.clone();
        let mut final_progress: Option<DownloadProgress> = None;
        let result = install_build(&library_dir, build, |progress| {
            if progress.done && progress.phase == "done" {
                final_progress = Some(progress);
            } else {
                let _ = progress_tx.send(progress);
            }
        })
        .await;

        if let Err(error) = result {
            let _ = progress_tx.send(error_progress(error));
        } else if let Err(error) = prewarm_version_runtime(&library_dir, &version_id, |progress| {
            let _ = progress_tx.send(progress);
        })
        .await
        {
            let _ = progress_tx.send(DownloadProgress {
                phase: "error".to_string(),
                current: 0,
                total: 0,
                file: None,
                error: Some(format!("prepare java runtime: {error}")),
                done: true,
            });
        } else if let Some(progress) = final_progress {
            let _ = progress_tx.send(progress);
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

fn parse_component_id(
    component_id: &str,
) -> Result<LoaderComponentId, (StatusCode, Json<serde_json::Value>)> {
    LoaderComponentId::parse(component_id).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": format!("unknown loader component: {component_id}")
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

fn error_response(error: LoaderError) -> (StatusCode, Json<serde_json::Value>) {
    let status = match error {
        LoaderError::InvalidMinecraftVersion
        | LoaderError::InvalidBuildId
        | LoaderError::InvalidComponentId => StatusCode::BAD_REQUEST,
        LoaderError::BuildNotFound(_) => StatusCode::NOT_FOUND,
        LoaderError::MissingLibraryDir => StatusCode::PRECONDITION_FAILED,
        LoaderError::CatalogUnavailable(_) => StatusCode::BAD_GATEWAY,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (
        status,
        Json(serde_json::json!({
            "error": error.to_string(),
            "failure_kind": error.failure_kind(),
        })),
    )
}

fn generate_install_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|value| value.as_nanos())
        .unwrap_or_default();
    format!("loader-install-{:032x}", nanos)
}
