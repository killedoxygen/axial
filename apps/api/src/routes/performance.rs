use crate::application::{
    self, PerformanceHealthRequest, PerformanceHealthResponse, PerformanceInstallRequest,
    PerformanceInstallResponse, PerformanceInstanceOperationResponse, PerformancePlanRequest,
    PerformancePlanResponse, PerformanceRollbackListRequest, PerformanceRollbackListResponse,
};
use crate::state::{AppState, RequestProducerHandoff};
use axum::{
    Json, Router,
    extract::{Extension, Path, Query, State},
    http::StatusCode,
    routing::{get, post},
};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct PlanQuery {
    game_version: Option<String>,
    loader: Option<String>,
    mode: Option<String>,
    instance_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct HealthQuery {
    instance_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RollbackQuery {
    instance_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct InstallRequest {
    instance_id: Option<String>,
    game_version: Option<String>,
    loader: Option<String>,
    mode: Option<String>,
    action: Option<String>,
    rollback_id: Option<String>,
    queued: Option<bool>,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/performance/status", get(handle_status))
        .route(
            "/api/v1/performance/rules/refresh",
            post(handle_rules_refresh),
        )
        .route("/api/v1/performance/plan", get(handle_plan))
        .route("/api/v1/performance/health", get(handle_health))
        .route("/api/v1/performance/rollback", get(handle_rollback_list))
        .route("/api/v1/performance/install", post(handle_install))
        .route(
            "/api/v1/performance/instances/{instance_id}/operation",
            get(handle_instance_operation),
        )
        .route(
            "/api/v1/performance/operations/{id}",
            get(handle_operation_status),
        )
}

async fn handle_status(
    State(state): State<AppState>,
) -> Result<Json<application::PerformanceRulesStatusResponse>, (StatusCode, Json<serde_json::Value>)>
{
    Ok(Json(application::performance_rules_status(&state)))
}

async fn handle_rules_refresh(
    State(state): State<AppState>,
    Extension(handoff): Extension<RequestProducerHandoff>,
) -> Result<Json<application::PerformanceRulesStatusResponse>, (StatusCode, Json<serde_json::Value>)>
{
    application::refresh_performance_rules(&state, handoff)
        .await
        .map(Json)
        .map_err(application::refresh_performance_rules_error_response)
}

async fn handle_plan(
    State(state): State<AppState>,
    Query(query): Query<PlanQuery>,
) -> Result<Json<PerformancePlanResponse>, (StatusCode, Json<serde_json::Value>)> {
    application::performance_plan(
        &state,
        PerformancePlanRequest {
            game_version: query.game_version,
            loader: query.loader,
            mode: query.mode,
            instance_id: query.instance_id,
        },
    )
    .await
    .map(Json)
}

async fn handle_health(
    State(state): State<AppState>,
    Query(query): Query<HealthQuery>,
) -> Result<Json<PerformanceHealthResponse>, (StatusCode, Json<serde_json::Value>)> {
    application::performance_health(
        &state,
        PerformanceHealthRequest {
            instance_id: query.instance_id,
        },
    )
    .await
    .map(Json)
}

async fn handle_rollback_list(
    State(state): State<AppState>,
    Query(query): Query<RollbackQuery>,
) -> Result<Json<PerformanceRollbackListResponse>, (StatusCode, Json<serde_json::Value>)> {
    application::performance_rollback_list(
        &state,
        PerformanceRollbackListRequest {
            instance_id: query.instance_id,
        },
    )
    .await
    .map(Json)
}

async fn handle_install(
    State(state): State<AppState>,
    Extension(handoff): Extension<RequestProducerHandoff>,
    Json(payload): Json<InstallRequest>,
) -> Result<Json<PerformanceInstallResponse>, (StatusCode, Json<serde_json::Value>)> {
    application::performance_install(
        state,
        PerformanceInstallRequest {
            instance_id: payload.instance_id,
            game_version: payload.game_version,
            loader: payload.loader,
            mode: payload.mode,
            action: payload.action,
            rollback_id: payload.rollback_id,
            queued: payload.queued,
        },
        handoff,
    )
    .await
    .map(Json)
}

async fn handle_operation_status(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<
    Json<crate::application::performance::PerformanceOperationStatusResponse>,
    (StatusCode, Json<serde_json::Value>),
> {
    application::performance_operation_status(&state, &id)
        .await
        .map(Json)
}

async fn handle_instance_operation(
    State(state): State<AppState>,
    Path(instance_id): Path<String>,
) -> Result<Json<PerformanceInstanceOperationResponse>, (StatusCode, Json<serde_json::Value>)> {
    application::performance_instance_operation(&state, &instance_id)
        .await
        .map(Json)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{
        AppStateInit, InstallStore, SessionStore,
        performance_operations::PerformanceOperationPayload,
    };
    use axial_config::{AppPaths, ConfigStore, InstanceStore};
    use axial_performance::PerformanceManager;
    use axum::{
        body::{Body, to_bytes},
        http::{Method, Request},
    };
    use serde_json::Value;
    use std::{fs, path::PathBuf, sync::Arc};
    use tower::ServiceExt;

    #[tokio::test]
    async fn operation_status_route_redacts_payload_through_production_router() {
        let fixture = RoutePerformanceFixture::new("operation-status-production-route");
        let instance_id = fixture.add_instance("Managed", "1.20.4-fabric");
        let operation = fixture
            .state
            .performance_operations()
            .start(
                instance_id.clone(),
                "install/provider_payload=secret-token".to_string(),
                PerformanceOperationPayload {
                    game_version: Some("/Users/alice/.minecraft/private-version".to_string()),
                    loader: Some("fabric".to_string()),
                    mode: Some("managed --accessToken secret-token".to_string()),
                    rollback_id: Some("rb-old\\secret".to_string()),
                },
            )
            .await
            .expect("operation starts");
        fixture
            .state
            .performance_operations()
            .record_failed(
                &operation.id,
                "provider_payload={\"url\":\"https://cdn.example.test/private/sodium-secret.jar?token=secret-token\"}; java_path=C:\\Users\\Alice\\Java\\bin\\java.exe; -Xmx8192M",
            )
            .await
            .expect("failure accepted");

        let (status, payload) = fixture
            .request_json(
                Method::GET,
                &format!("/api/v1/performance/operations/{}", operation.id),
            )
            .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(payload["id"], operation.id);
        assert_eq!(payload["instance_id"], instance_id);
        assert_eq!(payload["state"], "failed");
        assert_eq!(payload["action"], "unknown");
        assert_eq!(payload["error"], "performance operation failed");
        assert_eq!(payload["view_model"]["tone"], "err");
        assert_eq!(
            payload["view_model"]["detail"],
            "performance operation failed"
        );
        assert_eq!(payload["view_model"]["progress"]["phase"], "error");
        assert_eq!(payload["payload"]["game_version"], "redacted");
        assert_eq!(payload["payload"]["loader"], "fabric");
        assert_eq!(payload["payload"]["mode"], "redacted");
        assert_eq!(payload["payload"]["rollback_id"], "redacted");
        assert_no_performance_route_sensitive_fragments(&payload);

        let (status, payload) = fixture
            .request_json(
                Method::GET,
                &format!("/api/v1/performance/instances/{instance_id}/operation"),
            )
            .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(payload["operation"]["id"], operation.id);
        assert_eq!(payload["operation"]["action"], "unknown");
        assert_eq!(
            payload["operation"]["error"],
            "performance operation failed"
        );
        assert_eq!(payload["operation"]["view_model"]["tone"], "err");
        assert_eq!(payload["operation"]["payload"]["game_version"], "redacted");
        assert_eq!(payload["operation"]["payload"]["loader"], "fabric");
        assert_eq!(payload["operation"]["payload"]["mode"], "redacted");
        assert_eq!(payload["operation"]["payload"]["rollback_id"], "redacted");
        assert_no_performance_route_sensitive_fragments(&payload);
    }

    #[tokio::test]
    async fn rules_refresh_receives_request_handoff_from_production_middleware() {
        let fixture = RoutePerformanceFixture::new("rules-refresh-request-handoff");
        let response = crate::routes::router(fixture.state.clone())
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/v1/performance/rules/refresh")
                    .body(Body::empty())
                    .expect("refresh request"),
            )
            .await
            .expect("refresh response");

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        drop(response);
        fixture
            .state
            .quiesce()
            .await
            .expect("refresh producer settles before quiescence");
    }

    struct RoutePerformanceFixture {
        state: AppState,
        root: PathBuf,
    }

    impl RoutePerformanceFixture {
        fn new(name: &str) -> Self {
            let root = test_root(name);
            let paths = test_paths(&root);
            let config = Arc::new(ConfigStore::load_from(paths.clone()).expect("load config"));
            let instances =
                Arc::new(InstanceStore::load_from(paths.clone()).expect("load instances"));
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
            let response = router()
                .with_state(self.state.clone())
                .oneshot(
                    Request::builder()
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

        fn add_instance(&self, name: &str, version_id: &str) -> String {
            self.state
                .instances()
                .add(
                    name.to_string(),
                    version_id.to_string(),
                    String::new(),
                    String::new(),
                    None,
                )
                .expect("add instance")
                .id
        }
    }

    impl Drop for RoutePerformanceFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn test_root(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "axial-api-performance-route-{name}-{}-{}",
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

    fn assert_no_performance_route_sensitive_fragments(value: &Value) {
        let text = value.to_string();
        for material in [
            "/Users/alice",
            "C:\\Users\\Alice",
            ".minecraft",
            "provider_payload",
            "private",
            "sodium-secret.jar",
            "secret-token",
            "accessToken",
            "token=secret",
            "-Xmx8192M",
            "java_path",
        ] {
            assert!(
                !text.contains(material),
                "public performance JSON exposed sensitive material {material}: {text}"
            );
        }
    }
}
