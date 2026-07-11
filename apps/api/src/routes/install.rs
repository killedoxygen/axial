use crate::application::{
    InstallQueueRequest, InstallQueueStateResponse, InstallStatusResponse, enqueue_install_owned,
    install_events_stream, install_queue_status_owned, install_status, remove_queued_install,
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
struct InstallRequest {
    version_id: String,
    #[serde(default)]
    manifest_url: String,
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
        InstallQueueRequest {
            kind: "vanilla".to_string(),
            version_id: payload.version_id,
            manifest_url: payload.manifest_url,
            component_id: String::new(),
            build_id: String::new(),
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
) -> Result<Json<InstallQueueStateResponse>, (StatusCode, Json<serde_json::Value>)> {
    remove_queued_install(&state, &id).await.map(Json)
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
    use crate::{
        application::{
            begin_install_operation_journal, install::INSTALL_FAILURE_MESSAGE,
            install_operation_id, record_install_operation_guardian_repair_outcome,
            record_install_operation_progress,
        },
        guardian::{
            DiagnosisId, GuardianActionKind, GuardianArtifactRepairOutcome,
            GuardianArtifactRepairStatus,
        },
        state::{AppStateInit, InstallStore, SessionStore},
    };
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
    async fn install_status_route_serializes_guardian_repair_status_payload() {
        let fixture = RouteInstallFixture::new("install-status-guardian-repair-route");
        let install_id = "repair-status-route-install";
        let operation_id = install_operation_id(install_id);
        let failed_progress = DownloadProgress {
            phase: "error".to_string(),
            current: 0,
            total: 0,
            file: Some("/Users/alice/.axial/libraries/secret-client.jar".to_string()),
            error: Some(
                "checksum failed in /Users/alice/.axial with token secret provider_payload"
                    .to_string(),
            ),
            done: true,
            bytes_done: None,
            bytes_total: None,
        };

        fixture
            .state
            .installs()
            .insert(install_id.to_string())
            .await;
        fixture
            .state
            .installs()
            .emit(install_id, failed_progress.clone())
            .await;
        begin_install_operation_journal(fixture.state.journals(), &operation_id, "1.21.5")
            .await
            .expect("record install journal");
        let mut last_phase = None;
        record_install_operation_progress(
            fixture.state.journals(),
            &operation_id,
            &failed_progress,
            &mut last_phase,
        )
        .await
        .expect("record install journal");
        record_install_operation_guardian_repair_outcome(
            fixture.state.journals(),
            &operation_id,
            &GuardianArtifactRepairOutcome {
                operation_id: crate::state::contracts::OperationId::new(
                    "guardian-artifact-repair:123e4567-e89b-12d3-a456-426614174002",
                ),
                diagnosis_id: DiagnosisId::new("launcher_managed_artifact_corrupt"),
                action: GuardianActionKind::Repair,
                status: GuardianArtifactRepairStatus::Repaired,
                facts: vec![
                    "https://example.invalid/client.jar?token=secret".to_string(),
                    "/Users/alice/.axial/libraries/secret-client.jar".to_string(),
                ],
                summary: "guardian_artifact_repaired".to_string(),
            },
        )
        .await
        .expect("record install journal");

        let (status, payload) = fixture
            .request_json(
                Method::GET,
                "/api/v1/install/repair-status-route-install/status",
            )
            .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(payload["install_id"], install_id);
        assert_eq!(payload["operation_id"], operation_id.as_str());
        assert_eq!(payload["done"], true);
        assert_eq!(payload["progress"][0]["phase"], "error");
        assert_eq!(payload["progress"][0]["error"], INSTALL_FAILURE_MESSAGE);
        assert_eq!(
            payload["guardian_repair"]["diagnosis_id"],
            "launcher_managed_artifact_corrupt"
        );
        assert_eq!(payload["guardian_repair"]["status"], "repaired");
        assert_eq!(
            payload["guardian_repair"]["repair_operation_id"],
            "guardian-artifact-repair:123e4567-e89b-12d3-a456-426614174002"
        );
        assert!(
            payload["guardian_repair"]["label"]
                .as_str()
                .is_some_and(|label| label.contains("repaired"))
        );
        assert_eq!(
            payload["failure_view_model"]["state_id"],
            "failed_repair_applied"
        );
        assert_eq!(payload["failure_view_model"]["title"], "Install failed");
        assert_eq!(
            payload["failure_view_model"]["retry_action"]["action"],
            "retry"
        );
        assert_eq!(
            payload["failure_view_model"]["retry_action"]["enabled"],
            true
        );
        assert_eq!(
            payload["failure_view_model"]["repair_action"]["action"],
            "repair"
        );
        assert_eq!(
            payload["failure_view_model"]["repair_action"]["enabled"],
            false
        );
        assert!(
            payload["proof"]["guardian_diagnosis_ids"]
                .as_array()
                .expect("proof diagnosis ids")
                .iter()
                .any(|id| id == "launcher_managed_artifact_corrupt")
        );
        assert_no_install_status_route_sensitive_fragments(&payload);
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
            .insert_or_existing_active(
                "active-install".to_string(),
                "1.21.5".to_string(),
                String::new(),
            )
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
                crate::state::InstallQueueSpec::vanilla("1.21.5".to_string(), String::new()),
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
                    "version_id": "1.21.6",
                    "manifest_url": ""
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
                    "version_id": "1.21.6",
                    "manifest_url": "http://127.0.0.1:9/version.json"
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
                performance: Arc::new(PerformanceManager::new().expect("performance manager")),
                startup_warnings: Vec::new(),
                frontend_dir: root.join("frontend"),
            });

            Self { state, root }
        }

        async fn request_json(&self, method: Method, uri: &str) -> (StatusCode, Value) {
            let request_lease = self
                .state
                .try_admit_request()
                .expect("admit route install request");
            let response = router()
                .with_state(self.state.clone())
                .oneshot(
                    Request::builder()
                        .extension(request_lease.producer_handoff())
                        .method(method)
                        .uri(uri)
                        .body(Body::empty())
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
            let response = router()
                .with_state(self.state.clone())
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

    fn assert_no_install_status_route_sensitive_fragments(value: &Value) {
        let text = value.to_string();
        for material in [
            "/Users/alice",
            ".axial",
            ".minecraft",
            "secret-client.jar",
            "provider_payload",
            "accessToken",
            "token=secret",
            "https://example.invalid",
            "client.jar?token",
        ] {
            assert!(
                !text.contains(material),
                "public install status JSON exposed sensitive material {material}: {text}"
            );
        }
    }
}
