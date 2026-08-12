use super::ApiJson;
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

pub fn router() -> Router<AppState> {
    Router::new()
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
    ApiJson(payload): ApiJson<InstallQueueRequest>,
) -> Result<Json<InstallQueueStateResponse>, (StatusCode, Json<serde_json::Value>)> {
    enqueue_install_owned(&state, payload, handoff)
        .await
        .map(Json)
}

async fn handle_install_queue_retry(
    State(state): State<AppState>,
    Extension(handoff): Extension<RequestProducerHandoff>,
    ApiJson(payload): ApiJson<InstallQueueRequest>,
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
    Extension(handoff): Extension<RequestProducerHandoff>,
    Path(id): Path<String>,
) -> Result<impl axum::response::IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let producer = state
        .try_claim_request_producer(&handoff)
        .map_err(super::producer_claim_error_response)?;
    install_events_stream(&state, &id, producer).await
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
    async fn install_queue_rejects_cross_kind_fields() {
        let fixture = RouteInstallFixture::new("install-route-strict-request-shape");
        let build_id = axial_minecraft::build_id_for(
            axial_minecraft::LoaderComponentId::Fabric,
            "1.21.6",
            "0.16.10",
        );
        let requests = [
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
        ];

        for (path, payload) in requests {
            let status = fixture
                .request_status_body(Method::POST, path, payload)
                .await;
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        }
    }

    #[tokio::test]
    async fn install_queue_enqueue_response_contains_started_active_item() {
        let fixture = RouteInstallFixture::new("install-queue-enqueue-started-active");
        fixture.state.configure_managed_library_for_test().await;

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
            let root_session = crate::state::test_root_session(&paths);
            let config = Arc::new(
                ConfigStore::load_from(paths.clone(), Arc::clone(&root_session))
                    .expect("load config"),
            );
            let instances = Arc::new(
                InstanceStore::from_snapshot(
                    paths.clone(),
                    root_session,
                    InstanceRegistrySnapshot::default(),
                )
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
                    PerformanceManager::load_for_startup(paths.performance_dir())
                        .expect("performance manager"),
                ),
                startup_warnings: Vec::new(),
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
        AppPaths::from_root(root.to_path_buf()).expect("absolute test app root")
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
