use crate::state::{AppState, DownloadProgress, InstallStore};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    response::sse::{Event, Sse},
    routing::{get, post},
};
use croopor_minecraft::{Downloader, infer_build_from_version_id};
use serde::Deserialize;
use std::{convert::Infallible, path::PathBuf, time::SystemTime};
use tokio::sync::mpsc;

const INSTALL_FAILURE_MESSAGE: &str =
    "Install failed. Check your connection and app data permissions, then try again.";

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
    let (version_id, manifest_url) = effective_install_fields(&payload);
    if version_id.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "version_id is required" })),
        ));
    }

    if is_loader_version_id(&version_id) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "modloader versions must be installed through /api/v1/loaders/install"
            })),
        ));
    }

    let mc_dir = state.library_dir().ok_or_else(|| {
        (
            StatusCode::PRECONDITION_FAILED,
            Json(serde_json::json!({ "error": "Croopor library is not configured" })),
        )
    })?;

    let install_id = generate_install_id();
    let (install_id, inserted) = state
        .installs()
        .insert_or_existing_active(install_id, version_id.clone(), manifest_url.clone())
        .await;
    if !inserted {
        return Ok(Json(serde_json::json!({ "install_id": install_id })));
    }

    let store = state.installs().clone();
    let mc_dir = PathBuf::from(mc_dir);
    let install_id_task = install_id.clone();

    let worker_store = store.clone();
    let worker_install_id = install_id_task.clone();
    InstallStore::spawn_tracked_worker(
        store,
        install_id_task,
        interrupted_install_progress(),
        async move {
            let (progress_tx, mut progress_rx) = mpsc::unbounded_channel::<DownloadProgress>();
            let store_task = {
                let store = worker_store.clone();
                let install_id = worker_install_id.clone();
                tokio::spawn(async move {
                    while let Some(progress) = progress_rx.recv().await {
                        store
                            .emit(&install_id, sanitize_install_progress(progress))
                            .await;
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
        },
    );

    Ok(Json(serde_json::json!({ "install_id": install_id })))
}

fn effective_install_fields(payload: &InstallRequest) -> (String, String) {
    (
        payload.version_id.trim().to_string(),
        payload.manifest_url.trim().to_string(),
    )
}

fn is_loader_version_id(version_id: &str) -> bool {
    infer_build_from_version_id(version_id).is_some()
}

fn sanitize_install_progress(mut progress: DownloadProgress) -> DownloadProgress {
    if progress.done && progress.error.is_some() {
        progress.error = Some(INSTALL_FAILURE_MESSAGE.to_string());
    }
    progress
}

fn interrupted_install_progress() -> DownloadProgress {
    DownloadProgress {
        phase: "error".to_string(),
        current: 0,
        total: 0,
        file: None,
        error: Some(INSTALL_FAILURE_MESSAGE.to_string()),
        done: true,
    }
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
                return;
            }
        }
        if done {
            return;
        }

        loop {
            match receiver.recv().await {
                Ok(progress) => {
                    let terminal = progress.done;
                    yield Ok(progress_event(&progress));
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{AppStateInit, SessionStore};
    use axum::{body::to_bytes, response::IntoResponse};
    use croopor_config::{AppPaths, ConfigStore, InstanceStore};
    use croopor_performance::PerformanceManager;
    use std::{fs, path::Path as FsPath, sync::Arc};

    #[test]
    fn effective_install_fields_trims_version_id_and_manifest_url() {
        let payload = InstallRequest {
            version_id: " 1.21.5 ".to_string(),
            manifest_url: " https://example.invalid/manifest.json ".to_string(),
        };

        assert_eq!(
            effective_install_fields(&payload),
            (
                "1.21.5".to_string(),
                "https://example.invalid/manifest.json".to_string()
            )
        );
    }

    #[test]
    fn effective_install_fields_preserves_explicit_manifest_url() {
        let normal = InstallRequest {
            version_id: "1.21.5".to_string(),
            manifest_url: String::new(),
        };
        let explicit = InstallRequest {
            version_id: "1.21.5".to_string(),
            manifest_url: "https://example.invalid/manifest.json".to_string(),
        };

        assert_ne!(
            effective_install_fields(&normal),
            effective_install_fields(&explicit)
        );
    }

    #[test]
    fn sanitize_install_progress_leaves_non_error_progress_unchanged() {
        let progress = DownloadProgress {
            phase: "libraries".to_string(),
            current: 7,
            total: 42,
            file: Some("example-library.jar".to_string()),
            error: None,
            done: false,
        };

        assert_eq!(sanitize_install_progress(progress.clone()), progress);
    }

    #[test]
    fn sanitize_install_progress_hides_raw_terminal_error_fragments() {
        let progress = DownloadProgress {
            phase: "error".to_string(),
            current: 0,
            total: 0,
            file: None,
            error: Some(
                "request failed: GET https://piston-meta.mojang.com/mc/game/version_manifest_v2.json \
                 parse version json: expected value at line 1 column 1 \
                 prepare java runtime: failed in /home/zero/.croopor/runtime/java \
                 and C:\\Users\\zero\\AppData\\Roaming\\Croopor\\runtime\\java"
                    .to_string(),
            ),
            done: true,
        };

        let sanitized = sanitize_install_progress(progress);
        let message = sanitized.error.as_deref().expect("error is present");

        assert_eq!(message, INSTALL_FAILURE_MESSAGE);
        assert_no_raw_fragments(message);
    }

    #[test]
    fn sanitize_install_progress_preserves_shape_and_only_changes_error_text() {
        let progress = DownloadProgress {
            phase: "error".to_string(),
            current: 13,
            total: 21,
            file: Some("1.20.1.json".to_string()),
            error: Some(
                "request failed for https://example.invalid/manifest.json in /tmp/croopor"
                    .to_string(),
            ),
            done: true,
        };

        let sanitized = sanitize_install_progress(progress.clone());

        assert_eq!(sanitized.phase, progress.phase);
        assert_eq!(sanitized.current, progress.current);
        assert_eq!(sanitized.total, progress.total);
        assert_eq!(sanitized.file, progress.file);
        assert_eq!(sanitized.done, progress.done);
        assert_eq!(sanitized.error.as_deref(), Some(INSTALL_FAILURE_MESSAGE));
    }

    #[tokio::test]
    async fn install_events_keep_terminal_installs_subscribable_after_stream_ends() {
        let root = test_root("install-events-terminal-retention");
        let state = build_test_state(&root);
        state.installs().insert("done-install".to_string()).await;
        state.installs().emit("done-install", done_progress()).await;

        let response =
            handle_install_events(State(state.clone()), Path("done-install".to_string()))
                .await
                .expect("terminal install events should be served")
                .into_response();
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("sse body should complete");
        let body = String::from_utf8(body.to_vec()).expect("sse body is utf8");

        assert!(body.contains("event: progress"));
        assert!(body.contains("\"phase\":\"done\""));
        let (history, _, done) = state
            .installs()
            .subscribe("done-install")
            .await
            .expect("terminal install remains subscribable after stream completion");
        assert!(done);
        assert_eq!(history.len(), 1);

        let _ = fs::remove_dir_all(root);
    }

    fn assert_no_raw_fragments(message: &str) {
        for fragment in [
            "/home/zero",
            "/tmp/croopor",
            "C:\\Users\\zero",
            "AppData\\Roaming",
            "https://",
            "piston-meta.mojang.com",
            "request failed",
            "parse version json",
            "expected value",
            "line 1 column",
            "prepare java runtime",
        ] {
            assert!(
                !message.contains(fragment),
                "message exposed raw fragment {fragment:?}: {message}"
            );
        }
    }

    fn done_progress() -> DownloadProgress {
        DownloadProgress {
            phase: "done".to_string(),
            current: 1,
            total: 1,
            file: None,
            error: None,
            done: true,
        }
    }

    fn build_test_state(root: &FsPath) -> AppState {
        let paths = test_paths(root);
        let config = Arc::new(ConfigStore::load_from(paths.clone()).expect("load config"));
        let instances = Arc::new(InstanceStore::load_from(paths.clone()).expect("load instances"));
        AppState::new(AppStateInit {
            app_name: "Croopor".to_string(),
            version: "test".to_string(),
            config,
            instances,
            installs: Arc::new(InstallStore::new()),
            sessions: Arc::new(SessionStore::new()),
            performance: Arc::new(PerformanceManager::new().expect("performance manager")),
            startup_warnings: Vec::new(),
            frontend_dir: root.join("frontend"),
        })
    }

    fn test_paths(root: &FsPath) -> AppPaths {
        let config_dir = root.join("config");
        AppPaths {
            config_file: config_dir.join("config.json"),
            instances_file: config_dir.join("instances.json"),
            instances_dir: root.join("instances"),
            music_dir: root.join("music"),
            library_dir: root.join("library"),
            config_dir,
        }
    }

    fn test_root(name: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "croopor-api-install-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|value| value.as_nanos())
                .unwrap_or_default()
        ));
        fs::create_dir_all(&path).expect("create test root");
        path
    }
}
