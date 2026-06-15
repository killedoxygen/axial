use crate::application::{
    LoaderBuildsRequest, LoaderInstallStartRequest, loader_builds, loader_components,
    loader_game_versions, sanitize_install_progress, start_loader_install,
};
use crate::state::AppState;
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    response::sse::{Event, Sse},
    routing::{get, post},
};
use croopor_minecraft::{DownloadProgress, LoaderComponentId};
use serde::Deserialize;
use std::convert::Infallible;

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

async fn handle_loader_components() -> Json<crate::dto::loaders::LoaderComponentsResponse> {
    Json(loader_components())
}

async fn handle_loader_builds(
    Path(component_id): Path<String>,
    Query(query): Query<LoaderBuildQuery>,
    State(state): State<AppState>,
) -> Result<Json<crate::dto::loaders::LoaderBuildsResponse>, (StatusCode, Json<serde_json::Value>)>
{
    let component_id = parse_component_id(&component_id)?;
    loader_builds(
        &state,
        LoaderBuildsRequest {
            component_id,
            mc_version: query.mc_version,
        },
    )
    .await
    .map(Json)
}

async fn handle_loader_game_versions(
    Path(component_id): Path<String>,
    State(state): State<AppState>,
) -> Result<
    Json<crate::dto::loaders::LoaderGameVersionsResponse>,
    (StatusCode, Json<serde_json::Value>),
> {
    let component_id = parse_component_id(&component_id)?;
    loader_game_versions(&state, component_id).await.map(Json)
}

async fn handle_loader_install(
    State(state): State<AppState>,
    Json(payload): Json<LoaderInstallRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let component_id = parse_component_id(&payload.component_id)?;
    let response = start_loader_install(
        &state,
        LoaderInstallStartRequest {
            component_id,
            build_id: payload.build_id,
        },
    )
    .await?;

    Ok(Json(serde_json::json!(response)))
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
            let progress = sanitize_install_progress(progress);
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
                    let progress = sanitize_install_progress(progress);
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

fn parse_component_id(
    component_id: &str,
) -> Result<LoaderComponentId, (StatusCode, Json<serde_json::Value>)> {
    LoaderComponentId::parse(component_id).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": "unknown loader component"
            })),
        )
    })
}

fn progress_event(progress: &DownloadProgress) -> Event {
    Event::default()
        .event("progress")
        .data(serde_json::to_string(progress).unwrap_or_else(|_| "{}".to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::install::{
        BASE_INSTALL_FAILED_MESSAGE, loader_error_progress, loader_install_done_progress,
        loader_install_key_fields, prewarm_runtime_error_progress,
        wait_for_active_vanilla_base_install,
    };
    use crate::application::loader_error_response;
    use crate::state::{AppStateInit, InstallStore, SessionStore};
    use axum::{body::to_bytes, response::IntoResponse};
    use croopor_config::{AppPaths, ConfigStore, InstanceStore};
    use croopor_minecraft::LoaderError;
    use croopor_performance::PerformanceManager;
    use serde_json::json;
    use std::{fs, path::Path as FsPath, sync::Arc, time::Duration};
    use tokio::sync::mpsc;
    use tokio::time::timeout;

    #[test]
    fn error_response_keeps_status_and_failure_kind_without_raw_details() {
        let (status, Json(body)) = loader_error_response(LoaderError::CatalogUnavailable(
            "GET https://loader.example.invalid/catalog.json timed out".to_string(),
        ));

        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert_eq!(body["failure_kind"], json!("catalog_unavailable"));
        assert_eq!(
            body["error"],
            json!("Loader catalog is unavailable. Check your connection and try again.")
        );
        assert_no_raw_fragments(body["error"].as_str().expect("error is a string"));

        let (status, Json(body)) = loader_error_response(LoaderError::Io(std::io::Error::new(
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
        let (status, Json(body)) = loader_error_response(LoaderError::Parse(parse_error));

        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body["failure_kind"], json!("parse_failed"));
        assert_eq!(
            body["error"],
            json!("Loader service returned unreadable data. Try again later.")
        );
        assert_no_raw_fragments(body["error"].as_str().expect("error is a string"));

        let (status, Json(body)) = loader_error_response(LoaderError::ArtifactMissing(
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
        let (status, Json(body)) = loader_error_response(LoaderError::InvalidMinecraftVersion);

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["failure_kind"], json!("other"));
        assert_eq!(body["error"], json!("Invalid Minecraft version."));

        let (status, Json(body)) = loader_error_response(LoaderError::MissingLibraryDir);

        assert_eq!(status, StatusCode::PRECONDITION_FAILED);
        assert_eq!(body["failure_kind"], json!("other"));
        assert_eq!(body["error"], json!("Croopor library is not configured"));
    }

    #[test]
    fn parse_component_id_error_does_not_echo_raw_component() {
        let (status, Json(body)) =
            parse_component_id(r"C:\Users\Alice\.minecraft --accessToken raw-secret")
                .expect_err("invalid component should fail");

        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["error"], json!("unknown loader component"));
        assert_no_raw_fragments(&serde_json::to_string(&body).expect("error json"));
    }

    #[test]
    fn error_progress_hides_raw_details_and_keeps_terminal_shape() {
        let progress = loader_error_progress(LoaderError::ArtifactMissing(
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

    #[tokio::test]
    async fn loader_install_events_keep_terminal_installs_subscribable_after_stream_ends() {
        let root = test_root("loader-install-events-terminal-retention");
        let state = build_test_state(&root);
        state.installs().insert("done-install".to_string()).await;
        state.installs().emit("done-install", done_progress()).await;

        let response =
            handle_loader_install_events(State(state.clone()), Path("done-install".to_string()))
                .await
                .expect("terminal loader install events should be served")
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
            .expect("terminal loader install remains subscribable after stream completion");
        assert!(done);
        assert_eq!(history.len(), 1);

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn loader_install_events_redact_raw_progress_history() {
        let root = test_root("loader-install-events-redaction");
        let state = build_test_state(&root);
        state
            .installs()
            .insert("raw-loader-install".to_string())
            .await;
        state
            .installs()
            .emit(
                "raw-loader-install",
                DownloadProgress {
                    phase: r"C:\Users\Alice\.minecraft --accessToken raw-secret".to_string(),
                    current: 2,
                    total: 5,
                    file: Some("/Users/alice/.croopor/libraries/secret.jar".to_string()),
                    error: Some(
                        "provider_payload={\"token\":\"secret\"} account_id=account-secret username=SecretPlayer -Xmx8192M"
                            .to_string(),
                    ),
                    done: false,
                },
            )
            .await;
        state
            .installs()
            .emit("raw-loader-install", done_progress())
            .await;

        let response = handle_loader_install_events(
            State(state.clone()),
            Path("raw-loader-install".to_string()),
        )
        .await
        .expect("loader install events should be served")
        .into_response();
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("sse body should complete");
        let body = String::from_utf8(body.to_vec()).expect("sse body is utf8");

        assert!(body.contains("\"phase\":\"install\""));
        assert!(body.contains("Install failed. Check your connection"));
        assert_no_raw_fragments(&body);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn loader_install_key_fields_are_scoped_to_component_and_build() {
        let fabric_key = loader_install_key_fields(
            LoaderComponentId::Fabric,
            "fabric:1.21.5:0.16.14",
            "fabric-loader-0.16.14-1.21.5",
        );
        let quilt_key = loader_install_key_fields(
            LoaderComponentId::Quilt,
            "quilt:1.21.5:0.16.14",
            "fabric-loader-0.16.14-1.21.5",
        );
        let next_build_key = loader_install_key_fields(
            LoaderComponentId::Fabric,
            "fabric:1.21.5:0.16.15",
            "fabric-loader-0.16.14-1.21.5",
        );

        assert_ne!(fabric_key, quilt_key);
        assert_ne!(fabric_key, next_build_key);
        assert!(fabric_key.0.starts_with("loader:"));
        assert!(fabric_key.1.starts_with("loader:"));
    }

    #[test]
    fn loader_install_key_fields_trim_resolved_fields() {
        assert_eq!(
            loader_install_key_fields(
                LoaderComponentId::Forge,
                " forge:1.20.1:47.4.0 ",
                " 1.20.1-forge-47.4.0 ",
            ),
            (
                "loader:net.minecraftforge:1.20.1-forge-47.4.0".to_string(),
                "loader:net.minecraftforge:forge:1.20.1:47.4.0".to_string()
            )
        );
    }

    #[tokio::test]
    async fn wait_for_active_vanilla_base_install_waits_and_forwards_progress() {
        let store = Arc::new(InstallStore::new());
        store
            .insert_or_existing_active(
                "vanilla-install".to_string(),
                "1.21.5".to_string(),
                String::new(),
            )
            .await;
        let (progress_tx, mut progress_rx) = mpsc::unbounded_channel();

        let wait_store = store.clone();
        let waiter = tokio::spawn(async move {
            wait_for_active_vanilla_base_install(wait_store.as_ref(), "1.21.5", &progress_tx).await
        });

        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!waiter.is_finished());

        let progress = base_progress("client");
        store.emit("vanilla-install", progress.clone()).await;
        assert_eq!(
            timeout(Duration::from_secs(1), progress_rx.recv())
                .await
                .expect("progress should arrive"),
            Some(progress)
        );

        store.emit("vanilla-install", done_progress()).await;
        timeout(Duration::from_secs(1), waiter)
            .await
            .expect("waiter should finish")
            .expect("waiter should not panic")
            .expect("successful base install should not fail loader wait");
        assert_eq!(
            timeout(Duration::from_millis(50), progress_rx.recv())
                .await
                .expect("progress sender should close"),
            None
        );
    }

    #[tokio::test]
    async fn wait_for_active_vanilla_base_install_does_not_block_done_removed_or_failed_sessions() {
        let store = InstallStore::new();
        let (progress_tx, _progress_rx) = mpsc::unbounded_channel();

        store
            .insert_or_existing_active(
                "done-install".to_string(),
                "1.21.5".to_string(),
                String::new(),
            )
            .await;
        store.emit("done-install", done_progress()).await;
        timeout(
            Duration::from_secs(1),
            wait_for_active_vanilla_base_install(&store, "1.21.5", &progress_tx),
        )
        .await
        .expect("done session should not block")
        .expect("done session should not fail loader wait");

        store
            .insert_or_existing_active(
                "failed-install".to_string(),
                "1.21.5".to_string(),
                String::new(),
            )
            .await;
        store.emit("failed-install", failed_progress()).await;
        timeout(
            Duration::from_secs(1),
            wait_for_active_vanilla_base_install(&store, "1.21.5", &progress_tx),
        )
        .await
        .expect("failed session should not block")
        .expect("already failed session should not fail loader wait");

        store
            .insert_or_existing_active(
                "removed-install".to_string(),
                "1.21.5".to_string(),
                String::new(),
            )
            .await;
        store.remove("removed-install").await;
        timeout(
            Duration::from_secs(1),
            wait_for_active_vanilla_base_install(&store, "1.21.5", &progress_tx),
        )
        .await
        .expect("removed session should not block")
        .expect("removed session should not fail loader wait");
    }

    #[tokio::test]
    async fn wait_for_active_vanilla_base_install_fails_loader_when_base_fails_while_waiting() {
        let store = Arc::new(InstallStore::new());
        store
            .insert_or_existing_active(
                "vanilla-install".to_string(),
                "1.21.5".to_string(),
                String::new(),
            )
            .await;
        let (progress_tx, mut progress_rx) = mpsc::unbounded_channel();

        let wait_store = store.clone();
        let waiter = tokio::spawn(async move {
            wait_for_active_vanilla_base_install(wait_store.as_ref(), "1.21.5", &progress_tx).await
        });

        tokio::time::sleep(Duration::from_millis(20)).await;
        store.emit("vanilla-install", failed_progress()).await;

        let progress = timeout(Duration::from_secs(1), waiter)
            .await
            .expect("waiter should finish")
            .expect("waiter should not panic")
            .expect_err("base failure should fail loader wait");

        assert_eq!(progress.phase, "error");
        assert_eq!(progress.current, 0);
        assert_eq!(progress.total, 0);
        assert_eq!(progress.file, None);
        assert_eq!(progress.error.as_deref(), Some(BASE_INSTALL_FAILED_MESSAGE));
        assert!(progress.done);
        assert_eq!(
            timeout(Duration::from_millis(50), progress_rx.recv())
                .await
                .expect("progress sender should close"),
            None
        );
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
            "/Users/alice",
            "C:\\Users\\Alice",
            "provider_payload",
            "account_id",
            "account-secret",
            "username",
            "SecretPlayer",
            "raw-secret",
            "java.exe",
            "-Xmx8192M",
        ] {
            assert!(
                !message.contains(fragment),
                "message leaked raw fragment {fragment:?}: {message}"
            );
        }
    }

    fn base_progress(phase: &str) -> DownloadProgress {
        DownloadProgress {
            phase: phase.to_string(),
            current: 1,
            total: 2,
            file: Some("base game".to_string()),
            error: None,
            done: false,
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

    fn failed_progress() -> DownloadProgress {
        DownloadProgress {
            phase: "error".to_string(),
            current: 0,
            total: 0,
            file: None,
            error: Some("failed".to_string()),
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
            "croopor-api-loaders-{name}-{}-{}",
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
