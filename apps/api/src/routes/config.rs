use crate::{
    application::{self, ConfigPatch},
    state::AppState,
};
use axial_config::AppConfig;
use axum::{
    Json, Router,
    extract::{State, rejection::JsonRejection},
    http::StatusCode,
    routing::{get, put},
};

const CONFIG_REQUEST_ERROR_MESSAGE: &str = "Invalid settings request.";

type ApiError = (StatusCode, Json<serde_json::Value>);

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/config", get(handle_get_config))
        .route("/api/v1/config", put(handle_update_config))
}

async fn handle_get_config(State(state): State<AppState>) -> Json<AppConfig> {
    Json(application::current_config(&state))
}

async fn handle_update_config(
    State(state): State<AppState>,
    payload: Result<Json<ConfigPatch>, JsonRejection>,
) -> Result<Json<AppConfig>, ApiError> {
    let Json(patch) = payload.map_err(config_request_error)?;
    application::update_config(&state, patch).await.map(Json)
}

fn config_request_error(rejection: JsonRejection) -> ApiError {
    let status = if matches!(rejection, JsonRejection::JsonDataError(_)) {
        StatusCode::UNPROCESSABLE_ENTITY
    } else {
        StatusCode::BAD_REQUEST
    };
    (
        status,
        Json(serde_json::json!({ "error": CONFIG_REQUEST_ERROR_MESSAGE })),
    )
}

#[cfg(test)]
mod tests {
    use super::CONFIG_REQUEST_ERROR_MESSAGE;
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
                serde_json::json!({ "error": CONFIG_REQUEST_ERROR_MESSAGE })
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
                    .body(Body::from(format!(r#"{{"theme":"{sensitive_value}""#)))
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
        assert_eq!(
            body,
            serde_json::json!({ "error": CONFIG_REQUEST_ERROR_MESSAGE })
        );
        assert!(!String::from_utf8_lossy(&bytes).contains(sensitive_value));
    }

    struct TestFixture {
        state: AppState,
        root: PathBuf,
    }

    impl TestFixture {
        fn new(name: &str) -> Self {
            let root = test_root(name);
            let paths = test_paths(&root);
            let config = Arc::new(
                ConfigStore::from_config(paths.clone(), AppConfig::default()).expect("set config"),
            );
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
