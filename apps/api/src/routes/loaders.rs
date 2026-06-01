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
        } else if prewarm_version_runtime(&library_dir, &version_id, |progress| {
            let _ = progress_tx.send(progress);
        })
        .await
        .is_err()
        {
            let _ = progress_tx.send(prewarm_runtime_error_progress());
        } else if let Some(progress) = final_progress {
            let _ = progress_tx.send(progress);
        } else {
            let _ = progress_tx.send(loader_install_done_progress());
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
        error: Some(public_loader_error_message(&error).to_string()),
        done: true,
    }
}

fn prewarm_runtime_error_progress() -> DownloadProgress {
    DownloadProgress {
        phase: "error".to_string(),
        current: 0,
        total: 0,
        file: None,
        error: Some(public_runtime_error_message().to_string()),
        done: true,
    }
}

fn loader_install_done_progress() -> DownloadProgress {
    DownloadProgress {
        phase: "done".to_string(),
        current: 1,
        total: 1,
        file: None,
        error: None,
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
        LoaderError::CatalogUnavailable(_) | LoaderError::ArtifactMissing(_) => {
            StatusCode::BAD_GATEWAY
        }
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (
        status,
        Json(serde_json::json!({
            "error": public_loader_error_message(&error),
            "failure_kind": error.failure_kind(),
        })),
    )
}

fn public_loader_error_message(error: &LoaderError) -> &'static str {
    match error {
        LoaderError::InvalidMinecraftVersion => "Invalid Minecraft version.",
        LoaderError::InvalidBuildId => "Invalid loader build.",
        LoaderError::InvalidComponentId => "Invalid loader component.",
        LoaderError::MissingLibraryDir => "Croopor library is not configured",
        LoaderError::CatalogUnavailable(_) => {
            "Loader catalog is unavailable. Check your connection and try again."
        }
        LoaderError::BuildNotFound(_) => "Selected loader build is not available.",
        LoaderError::ArtifactMissing(_) => {
            "Loader artifact is unavailable. Try another build or component."
        }
        LoaderError::InvalidProfile(_) => "Loader profile is invalid. Try another build.",
        LoaderError::Verify(_) => {
            "Loader install verification failed. Try again or choose another build."
        }
        LoaderError::Request(_) => {
            "Loader service request failed. Check your connection and try again."
        }
        LoaderError::Download(_) => "Loader download failed. Check your connection and try again.",
        LoaderError::Parse(_) => "Loader service returned unreadable data. Try again later.",
        LoaderError::Io(_) => {
            "Could not write loader files. Check app data permissions and try again."
        }
        LoaderError::Other(_) => "Loader operation failed. Try again.",
    }
}

fn public_runtime_error_message() -> &'static str {
    "Could not prepare the Java runtime. Check your connection and try again."
}

fn generate_install_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|value| value.as_nanos())
        .unwrap_or_default();
    format!("loader-install-{:032x}", nanos)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn error_response_keeps_status_and_failure_kind_without_raw_details() {
        let (status, Json(body)) = error_response(LoaderError::CatalogUnavailable(
            "GET https://loader.example.invalid/catalog.json timed out".to_string(),
        ));

        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert_eq!(body["failure_kind"], json!("catalog_unavailable"));
        assert_eq!(
            body["error"],
            json!("Loader catalog is unavailable. Check your connection and try again.")
        );
        assert_no_raw_fragments(body["error"].as_str().expect("error is a string"));

        let (status, Json(body)) = error_response(LoaderError::Io(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "permission denied: /home/zero/.croopor/libraries/example.jar",
        )));

        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body["failure_kind"], json!("io_failed"));
        assert_eq!(
            body["error"],
            json!("Could not write loader files. Check app data permissions and try again.")
        );
        assert_no_raw_fragments(body["error"].as_str().expect("error is a string"));

        let parse_error = serde_json::from_str::<serde_json::Value>("{\"loader\":")
            .expect_err("invalid json should fail");
        let (status, Json(body)) = error_response(LoaderError::Parse(parse_error));

        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body["failure_kind"], json!("parse_failed"));
        assert_eq!(
            body["error"],
            json!("Loader service returned unreadable data. Try again later.")
        );
        assert_no_raw_fragments(body["error"].as_str().expect("error is a string"));

        let (status, Json(body)) = error_response(LoaderError::ArtifactMissing(
            "missing https://cdn.example.invalid/path/mod-loader.jar in /tmp/croopor".to_string(),
        ));

        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert_eq!(body["failure_kind"], json!("artifact_missing"));
        assert_eq!(
            body["error"],
            json!("Loader artifact is unavailable. Try another build or component.")
        );
        assert_no_raw_fragments(body["error"].as_str().expect("error is a string"));
    }

    #[test]
    fn error_response_preserves_safe_explicit_messages() {
        let (status, Json(body)) = error_response(LoaderError::InvalidMinecraftVersion);

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["failure_kind"], json!("other"));
        assert_eq!(body["error"], json!("Invalid Minecraft version."));

        let (status, Json(body)) = error_response(LoaderError::MissingLibraryDir);

        assert_eq!(status, StatusCode::PRECONDITION_FAILED);
        assert_eq!(body["failure_kind"], json!("other"));
        assert_eq!(body["error"], json!("Croopor library is not configured"));
    }

    #[test]
    fn error_progress_hides_raw_details_and_keeps_terminal_shape() {
        let progress = error_progress(LoaderError::ArtifactMissing(
            "missing https://cdn.example.invalid/path/mod-loader.jar in /tmp/croopor".to_string(),
        ));

        assert_eq!(progress.phase, "error");
        assert_eq!(progress.current, 0);
        assert_eq!(progress.total, 0);
        assert_eq!(progress.file, None);
        assert_eq!(
            progress.error.as_deref(),
            Some("Loader artifact is unavailable. Try another build or component.")
        );
        assert!(progress.done);
        assert_no_raw_fragments(progress.error.as_deref().expect("error is present"));
    }

    #[test]
    fn prewarm_runtime_error_progress_hides_raw_runtime_error() {
        let progress = prewarm_runtime_error_progress();

        assert_eq!(progress.phase, "error");
        assert_eq!(progress.current, 0);
        assert_eq!(progress.total, 0);
        assert_eq!(progress.file, None);
        assert_eq!(
            progress.error.as_deref(),
            Some("Could not prepare the Java runtime. Check your connection and try again.")
        );
        assert!(progress.done);
        assert_no_raw_fragments(progress.error.as_deref().expect("error is present"));
    }

    #[test]
    fn loader_install_done_progress_marks_session_terminal() {
        let progress = loader_install_done_progress();

        assert_eq!(progress.phase, "done");
        assert_eq!(progress.current, 1);
        assert_eq!(progress.total, 1);
        assert_eq!(progress.file, None);
        assert_eq!(progress.error, None);
        assert!(progress.done);
    }

    fn assert_no_raw_fragments(message: &str) {
        for fragment in [
            "https://",
            "loader.example.invalid",
            "cdn.example.invalid",
            "/home/zero",
            "/tmp/croopor",
            "EOF while parsing",
            "line 1 column",
            "mod-loader.jar",
        ] {
            assert!(
                !message.contains(fragment),
                "message leaked raw fragment {fragment:?}: {message}"
            );
        }
    }
}
