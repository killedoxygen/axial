use crate::application::{
    InstallQueueRequest, InstallQueueStateResponse, InstallStatusResponse, enqueue_install_owned,
    install_events_stream, install_queue_status_owned, install_status, remove_queued_install_owned,
    retry_install_owned,
};
use crate::state::{AppState, RequestProducerHandoff};
use axum::{
    Json, Router,
    extract::{Extension, Path, State},
    http::StatusCode,
    routing::{delete, get, post},
};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct InstallRequest {
    version_id: String,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/install", post(handle_install))
        .route(
            "/api/v1/install/queue",
            get(handle_install_queue_status).post(handle_install_queue_enqueue),
        )
        .route(
            "/api/v1/install/queue/retry",
            post(handle_install_queue_retry),
        )
        .route(
            "/api/v1/install/queue/{id}",
            delete(handle_install_queue_remove),
        )
        .route("/api/v1/install/{id}/status", get(handle_install_status))
        .route("/api/v1/install/{id}/events", get(handle_install_events))
}

async fn handle_install(
    State(state): State<AppState>,
    Extension(handoff): Extension<RequestProducerHandoff>,
    Json(payload): Json<InstallRequest>,
) -> Result<Json<InstallQueueStateResponse>, (StatusCode, Json<serde_json::Value>)> {
    enqueue_install_owned(
        &state,
        InstallQueueRequest::Vanilla {
            version_id: payload.version_id,
        },
        handoff,
    )
    .await
    .map(Json)
}

async fn handle_install_status(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<InstallStatusResponse>, (StatusCode, Json<serde_json::Value>)> {
    install_status(&state, &id).await.map(Json)
}

async fn handle_install_queue_status(
    State(state): State<AppState>,
    Extension(handoff): Extension<RequestProducerHandoff>,
) -> Result<Json<InstallQueueStateResponse>, (StatusCode, Json<serde_json::Value>)> {
    install_queue_status_owned(&state, handoff).await.map(Json)
}

async fn handle_install_queue_enqueue(
    State(state): State<AppState>,
    Extension(handoff): Extension<RequestProducerHandoff>,
    Json(payload): Json<InstallQueueRequest>,
) -> Result<Json<InstallQueueStateResponse>, (StatusCode, Json<serde_json::Value>)> {
    enqueue_install_owned(&state, payload, handoff)
        .await
        .map(Json)
}

async fn handle_install_queue_retry(
    State(state): State<AppState>,
    Extension(handoff): Extension<RequestProducerHandoff>,
    Json(payload): Json<InstallQueueRequest>,
) -> Result<Json<InstallQueueStateResponse>, (StatusCode, Json<serde_json::Value>)> {
    retry_install_owned(&state, payload, handoff)
        .await
        .map(Json)
}

async fn handle_install_queue_remove(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Extension(handoff): Extension<RequestProducerHandoff>,
) -> Result<Json<InstallQueueStateResponse>, (StatusCode, Json<serde_json::Value>)> {
    remove_queued_install_owned(&state, &id, handoff)
        .await
        .map(Json)
}

async fn handle_install_events(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<impl axum::response::IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    install_events_stream(&state, &id).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{AppStateInit, InstallStore, SessionStore};
    use axial_config::{AppPaths, ConfigStore, InstanceRegistrySnapshot, InstanceStore};
    use axial_minecraft::DownloadProgress;
    use axial_performance::PerformanceManager;
    use axum::{
        body::{Body, to_bytes},
        http::{Method, Request},
    };
    use serde_json::Value;
    use std::{fs, path::PathBuf, sync::Arc};
    use tower::ServiceExt;

    #[tokio::test]
    async fn install_routes_reject_removed_and_cross_kind_fields() {
        let fixture = RouteInstallFixture::new("install-route-strict-request-shape");
        let build_id = axial_minecraft::build_id_for(
            axial_minecraft::LoaderComponentId::Fabric,
            "1.21.6",
            "0.16.10",
        );
        let requests = [
            (
                "/api/v1/install",
                serde_json::json!({
                    "version_id": "1.21.6",
                    "manifest_url": "https://example.invalid/version.json"
                }),
            ),
            (
                "/api/v1/install/queue",
                serde_json::json!({
                    "kind": "vanilla",
                    "version_id": "1.21.6",
                    "build_id": &build_id
                }),
            ),
            (
                "/api/v1/install/queue",
                serde_json::json!({
                    "kind": "loader",
                    "component_id": "net.fabricmc.fabric-loader",
                    "build_id": &build_id,
                    "version_id": "1.21.6"
                }),
            ),
            (
                "/api/v1/loaders/install",
                serde_json::json!({
                    "component_id": "net.fabricmc.fabric-loader",
                    "build_id": &build_id,
                    "manifest_url": "https://example.invalid/version.json"
                }),
            ),
            (
                "/api/v1/loaders/install",
                serde_json::json!({
                    "component_id": "net.fabricmc.fabric-loader",
                    "build_id": &build_id,
                    "version_id": "1.21.6"
                }),
            ),
        ];

        for (path, payload) in requests {
            let status = fixture
                .request_status_body(Method::POST, path, payload)
                .await;
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        }
    }

    #[tokio::test]
    async fn public_install_route_enqueues_behind_active_lane() {
        let fixture = RouteInstallFixture::new("public-install-route-queue-lane");
        let library_dir = fixture.root.join("library");
        fs::create_dir_all(&library_dir).expect("library dir");
        fixture
            .state
            .set_library_dir_for_test(library_dir.to_string_lossy().to_string());
        fixture
            .state
            .installs()
            .insert_or_existing_vanilla("active-install".to_string(), "1.21.5".to_string())
            .await;
        let install_started_at_ms = fixture
            .state
            .installs()
            .install_started_at_ms("active-install")
            .await
            .expect("active install start time");
        fixture
            .state
            .installs()
            .enqueue_queued_install(
                "queue-active".to_string(),
                crate::state::InstallQueueSpec::vanilla("1.21.5".to_string()),
                crate::state::InstallQueuePlacement::Back,
            )
            .await;
        fixture
            .state
            .installs()
            .reserve_next_queued_install()
            .await
            .expect("active queue item");
        assert!(
            fixture
                .state
                .installs()
                .mark_queued_install_started("queue-active", "active-install".to_string())
                .await
        );

        let (status, payload) = fixture
            .request_json_body(
                Method::POST,
                "/api/v1/install",
                serde_json::json!({
                    "version_id": "1.21.6"
                }),
            )
            .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(payload["active"]["queue_id"], "queue-active");
        assert_eq!(payload["active"]["install_id"], "active-install");
        assert_eq!(
            payload["active"]["install_started_at_ms"].as_u64(),
            Some(install_started_at_ms)
        );
        assert_eq!(payload["items"].as_array().expect("queue items").len(), 1);
        assert_eq!(payload["items"][0]["label"], "Minecraft 1.21.6");
        assert!(payload["started_install"].is_null());
        let snapshot = fixture.state.installs().queue_snapshot().await;
        assert_eq!(
            snapshot
                .active
                .as_ref()
                .map(|active| active.queue_id.as_str()),
            Some("queue-active")
        );
        assert_eq!(snapshot.pending.len(), 1);
    }

    #[tokio::test]
    async fn install_queue_enqueue_response_contains_started_active_item() {
        let fixture = RouteInstallFixture::new("install-queue-enqueue-started-active");
        let library_dir = fixture.root.join("library");
        fs::create_dir_all(&library_dir).expect("library dir");
        fixture
            .state
            .set_library_dir_for_test(library_dir.to_string_lossy().to_string());

        let (status, payload) = fixture
            .request_json_body(
                Method::POST,
                "/api/v1/install/queue",
                serde_json::json!({
                    "kind": "vanilla",
                    "version_id": "1.21.6"
                }),
            )
            .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(payload["items"].as_array().expect("queue items").len(), 0);
        assert_eq!(payload["active"]["kind"], "vanilla");
        assert_eq!(payload["active"]["label"], "Minecraft 1.21.6");
        assert_eq!(payload["active"]["install_item"]["version_id"], "1.21.6");
        let active_install_id = payload["active"]["install_id"]
            .as_str()
            .expect("active install id");
        assert!(!active_install_id.is_empty());
        assert_eq!(
            payload["started_install"]["install_id"].as_str(),
            Some(active_install_id)
        );
        assert!(
            payload["active"]["install_started_at_ms"]
                .as_u64()
                .is_some_and(|started_at| started_at > 0)
        );
        fixture
            .state
            .installs()
            .finish_if_active(active_install_id, done_progress())
            .await;
    }

    struct RouteInstallFixture {
        state: AppState,
        root: PathBuf,
    }

    impl RouteInstallFixture {
        fn new(name: &str) -> Self {
            let root = test_root(name);
            let paths = test_paths(&root);
            let config = Arc::new(ConfigStore::load_from(paths.clone()).expect("load config"));
            let instances = Arc::new(
                InstanceStore::from_snapshot(paths.clone(), InstanceRegistrySnapshot::default())
                    .expect("load instances"),
            );
            let state = AppState::new(AppStateInit {
                app_name: "Axial".to_string(),
                version: "test".to_string(),
                config,
                instances,
                installs: Arc::new(InstallStore::new()),
                sessions: Arc::new(SessionStore::new()),
                performance: Arc::new(
                    PerformanceManager::load_for_startup(&paths.config_dir)
                        .expect("performance manager"),
                ),
                startup_warnings: Vec::new(),
                frontend_dir: root.join("frontend"),
            });

            Self { state, root }
        }

        fn router(&self) -> Router {
            Router::new()
                .merge(router())
                .merge(crate::routes::loaders::router())
                .with_state(self.state.clone())
        }

        async fn request_json_body(
            &self,
            method: Method,
            uri: &str,
            payload: Value,
        ) -> (StatusCode, Value) {
            let request_lease = self
                .state
                .try_admit_request()
                .expect("admit route install request");
            let response = self
                .router()
                .oneshot(
                    Request::builder()
                        .extension(request_lease.producer_handoff())
                        .method(method)
                        .uri(uri)
                        .header("content-type", "application/json")
                        .body(Body::from(payload.to_string()))
                        .expect("request"),
                )
                .await
                .expect("route response");
            let status = response.status();
            let body = to_bytes(response.into_body(), usize::MAX)
                .await
                .expect("read body");
            let payload = serde_json::from_slice(&body).expect("json response");
            (status, payload)
        }

        async fn request_status_body(
            &self,
            method: Method,
            uri: &str,
            payload: Value,
        ) -> StatusCode {
            let request_lease = self
                .state
                .try_admit_request()
                .expect("admit route install request");
            self.router()
                .oneshot(
                    Request::builder()
                        .extension(request_lease.producer_handoff())
                        .method(method)
                        .uri(uri)
                        .header("content-type", "application/json")
                        .body(Body::from(payload.to_string()))
                        .expect("request"),
                )
                .await
                .expect("route response")
                .status()
        }
    }

    impl Drop for RouteInstallFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn test_root(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "axial-api-install-route-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|value| value.as_nanos())
                .unwrap_or_default()
        ));
        fs::create_dir_all(&path).expect("create test root");
        path
    }

    fn test_paths(root: &std::path::Path) -> AppPaths {
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

    fn done_progress() -> DownloadProgress {
        DownloadProgress {
            phase: "done".to_string(),
            current: 1,
            total: 1,
            file: None,
            error: None,
            done: true,
            bytes_done: None,
            bytes_total: None,
        }
    }
}
