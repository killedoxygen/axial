use crate::{
    application::{self, ConfigPatch},
    state::AppState,
};
use axum::{
    Json, Router,
    extract::State,
    routing::{get, put},
};

use super::ApiJson;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/config", get(handle_get_config))
        .route("/api/v1/config", put(handle_update_config))
}

async fn handle_get_config(State(state): State<AppState>) -> Json<application::ConfigView> {
    Json(application::current_config(&state))
}

async fn handle_update_config(
    State(state): State<AppState>,
    ApiJson(patch): ApiJson<ConfigPatch>,
) -> Result<Json<application::ConfigView>, (axum::http::StatusCode, Json<serde_json::Value>)> {
    application::update_config(&state, patch).await.map(Json)
}

#[cfg(test)]
mod tests {
    use crate::state::{AppState, AppStateInit, InstallStore, SessionStore};
    use axial_config::{AppConfig, AppPaths, ConfigStore, InstanceRegistrySnapshot, InstanceStore};
    use axial_performance::PerformanceManager;
    use axum::{
        body::{Body, to_bytes},
        http::{Method, Request, StatusCode, header},
    };
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::Arc,
        time::{SystemTime, UNIX_EPOCH},
    };
    use tower::ServiceExt;

    #[tokio::test]
    async fn retired_library_fields_return_bounded_json_rejections() {
        let fixture = TestFixture::new("retired-library-fields");
        let app = super::router().with_state(fixture.state.clone());

        for field in ["library_dir", "library_mode"] {
            let sensitive_value = format!("/Users/private/{field}/secret-token");
            let payload = serde_json::json!({ (field): sensitive_value.clone() });
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(Method::PUT)
                        .uri("/api/v1/config")
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(payload.to_string()))
                        .expect("retired config field request"),
                )
                .await
                .expect("config route should return a bounded rejection");

            assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
            assert_eq!(
                response
                    .headers()
                    .get(header::CONTENT_TYPE)
                    .and_then(|value| value.to_str().ok()),
                Some("application/json")
            );
            let bytes = to_bytes(response.into_body(), 1024)
                .await
                .expect("config rejection body should read");
            let body: serde_json::Value =
                serde_json::from_slice(&bytes).expect("config rejection body should be json");
            assert_eq!(
                body,
                serde_json::json!({ "error": "Invalid JSON request." })
            );

            let rendered = String::from_utf8(bytes.to_vec()).expect("json should be utf-8");
            assert!(!rendered.contains(field));
            assert!(!rendered.contains(&sensitive_value));
        }

        let sensitive_value = "private-malformed-json-token";
        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::PUT)
                    .uri("/api/v1/config")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(format!(r#"{{"username":"{sensitive_value}"#)))
                    .expect("malformed config request"),
            )
            .await
            .expect("config route should return a bounded syntax rejection");

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let bytes = to_bytes(response.into_body(), 1024)
            .await
            .expect("config syntax rejection body should read");
        let body: serde_json::Value =
            serde_json::from_slice(&bytes).expect("config syntax rejection body should be json");
        assert_eq!(body, serde_json::json!({ "error": "Invalid JSON syntax." }));
        assert!(!String::from_utf8_lossy(&bytes).contains(sensitive_value));
    }

    #[tokio::test]
    async fn behavior_contract_public_config_route_is_revisioned_and_omits_internal_fields() {
        let fixture = TestFixture::with_config(
            "public-view",
            AppConfig {
                telemetry_enabled: true,
                telemetry_install_id: "123e4567-e89b-12d3-a456-426614174000".to_string(),
                feature_overrides: [("developer.inspector".to_string(), true)].into(),
                library_dir: "/private/library".to_string(),
                library_mode: "existing".to_string(),
                ..AppConfig::default()
            },
        );
        let response = super::router()
            .with_state(fixture.state.clone())
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/api/v1/config")
                    .body(Body::empty())
                    .expect("config request"),
            )
            .await
            .expect("config response");

        assert_eq!(response.status(), StatusCode::OK);
        let body: serde_json::Value = serde_json::from_slice(
            &to_bytes(response.into_body(), 16 * 1024)
                .await
                .expect("config response body"),
        )
        .expect("config response json");
        assert_eq!(body["revision"], 0);
        for internal in [
            "telemetry_install_id",
            "feature_overrides",
            "library_dir",
            "library_mode",
        ] {
            assert!(
                body.get(internal).is_none(),
                "{internal} must remain private"
            );
        }
    }

    #[tokio::test]
    async fn behavior_contract_cross_owner_rejects_invalid_wire_settings_before_persistence() {
        let fixture = TestFixture::new("invalid-wire-settings");
        let app = super::router().with_state(fixture.state.clone());

        let unknown = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::PUT)
                    .uri("/api/v1/config")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"guardian_mode":"legacy"}"#))
                    .expect("unknown mode request"),
            )
            .await
            .expect("unknown mode response");
        assert_eq!(unknown.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let unknown_body: serde_json::Value = serde_json::from_slice(
            &to_bytes(unknown.into_body(), 1024)
                .await
                .expect("unknown mode response body"),
        )
        .expect("unknown mode response json");
        assert_eq!(
            unknown_body,
            serde_json::json!({ "error": "Invalid JSON request." })
        );

        let extreme = app
            .oneshot(
                Request::builder()
                    .method(Method::PUT)
                    .uri("/api/v1/config")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"max_memory_mb":2147483647}"#))
                    .expect("extreme memory request"),
            )
            .await
            .expect("extreme memory response");
        assert_eq!(extreme.status(), StatusCode::BAD_REQUEST);
        assert_eq!(fixture.state.config().current(), AppConfig::default());
        assert!(!fixture.root.join("config.json").exists());
    }

    struct TestFixture {
        state: AppState,
        root: PathBuf,
    }

    impl TestFixture {
        fn new(name: &str) -> Self {
            Self::with_config(name, AppConfig::default())
        }

        fn with_config(name: &str, initial: AppConfig) -> Self {
            let root = test_root(name);
            let paths = test_paths(&root);
            let root_session = crate::state::test_root_session(&paths);
            let config = Arc::new(
                ConfigStore::from_config(paths.clone(), Arc::clone(&root_session), initial)
                    .expect("set config"),
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
    }

    impl Drop for TestFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn test_paths(root: &Path) -> AppPaths {
        AppPaths::from_root(root.to_path_buf()).expect("absolute test app root")
    }

    fn test_root(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "axial-config-routes-{name}-{}-{nonce}",
            std::process::id()
        ))
    }
}
