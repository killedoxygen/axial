use crate::{
    application::{self, FlagOverridePatch},
    state::AppState,
};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::{get, put},
};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/flags", get(handle_list_flags))
        .route("/api/v1/flags/{key}", put(handle_update_flag))
}

async fn handle_list_flags(State(state): State<AppState>) -> Json<application::FlagsResponse> {
    Json(application::list_flags(&state))
}

async fn handle_update_flag(
    State(state): State<AppState>,
    Path(key): Path<String>,
    Json(patch): Json<FlagOverridePatch>,
) -> Result<Json<application::FlagsResponse>, (StatusCode, Json<serde_json::Value>)> {
    application::update_flag(&state, &key, patch)
        .await
        .map(Json)
}

#[cfg(test)]
mod tests {
    use crate::state::{AppState, AppStateInit, InstallStore, SessionStore};
    use axial_config::{
        AppConfig, AppPaths, ConfigStore, FEATURE_FLAGS, InstanceRegistrySnapshot, InstanceStore,
    };
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

    const RETIRED_CACHE_SENTINEL: &[u8] = b"\0invalid retired flag cache\xff";

    #[tokio::test]
    async fn p00_b10_contract_cross_owner_local_override_round_trips_api_config_and_shutdown() {
        let fixture = TestFixture::load("mounted").await;
        let retired_cache = fixture.root.join("config/flags/remote-cache.json");
        assert_retired_cache_untouched(&retired_cache);
        assert!(crate::app::start_application_background_workflows(&fixture.state).await);
        assert_retired_cache_untouched(&retired_cache);
        let app = crate::routes::router(fixture.state.clone());

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/api/v1/flags")
                    .body(Body::empty())
                    .expect("flags list request"),
            )
            .await
            .expect("flags list route should respond");

        assert_eq!(response.status(), StatusCode::OK);
        let body = response_json(response.into_body()).await;
        let flag = body["flags"]
            .as_array()
            .expect("flags should be an array")
            .iter()
            .find(|flag| flag["key"] == seed_key())
            .expect("development flag should be visible");
        assert_eq!(flag["enabled"], false);
        assert_eq!(flag["source"], "default");
        assert_retired_cache_untouched(&retired_cache);

        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::PUT)
                    .uri(format!("/api/v1/flags/{}", seed_key()))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"enabled":true}"#))
                    .expect("flags update request"),
            )
            .await
            .expect("flags update route should respond");

        assert_eq!(response.status(), StatusCode::OK);
        let body = response_json(response.into_body()).await;
        let flag = body["flags"]
            .as_array()
            .expect("flags should be an array")
            .iter()
            .find(|flag| flag["key"] == seed_key())
            .expect("development flag should remain visible");
        assert_eq!(flag["enabled"], true);
        assert_eq!(flag["source"], "override");
        assert_eq!(
            fixture
                .state
                .config()
                .current()
                .feature_overrides
                .get(seed_key()),
            Some(&true)
        );

        let persisted = fs::read_to_string(fixture.root.join("config/config.json"))
            .expect("config override should persist");
        let persisted = serde_json::from_str::<AppConfig>(&persisted)
            .expect("persisted config should remain valid");
        assert_eq!(persisted.feature_overrides.get(seed_key()), Some(&true));
        assert_retired_cache_untouched(&retired_cache);

        fixture
            .state
            .shutdown()
            .await
            .expect("local flag mutation should leave shutdown settled");
        assert_retired_cache_untouched(&retired_cache);
    }

    async fn response_json(body: Body) -> serde_json::Value {
        let body = to_bytes(body, usize::MAX)
            .await
            .expect("response body should read");
        serde_json::from_slice(&body).expect("response body should be json")
    }

    fn seed_key() -> &'static str {
        FEATURE_FLAGS[0].key
    }

    fn assert_retired_cache_untouched(path: &Path) {
        assert_eq!(
            fs::read(path).expect("retired cache sentinel should remain readable"),
            RETIRED_CACHE_SENTINEL
        );
    }

    struct TestFixture {
        state: AppState,
        root: PathBuf,
    }

    impl TestFixture {
        async fn load(name: &str) -> Self {
            let root = test_root(name);
            let paths = test_paths(&root);
            let retired_cache = paths.config_dir.join("flags/remote-cache.json");
            fs::create_dir_all(
                retired_cache
                    .parent()
                    .expect("retired cache should have a parent"),
            )
            .expect("create retired cache fixture directory");
            fs::write(&retired_cache, RETIRED_CACHE_SENTINEL)
                .expect("seed retired cache sentinel before application load");
            let config = Arc::new(
                ConfigStore::from_config(paths.clone(), AppConfig::default()).expect("set config"),
            );
            let instances = Arc::new(
                InstanceStore::from_snapshot(paths.clone(), InstanceRegistrySnapshot::default())
                    .expect("load instances"),
            );
            let state = AppState::load(AppStateInit {
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
            })
            .await
            .expect("load application state");

            Self { state, root }
        }
    }

    impl Drop for TestFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn test_paths(root: &Path) -> AppPaths {
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

    fn test_root(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "axial-flags-routes-{name}-{}-{nonce}",
            std::process::id()
        ))
    }
}
