use crate::{
    application::{self, AuthStatusResponse},
    state::AppState,
};
use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    routing::{get, post},
};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/auth/status", get(handle_auth_status))
        .route("/api/v1/auth/refresh", post(handle_auth_refresh))
        .route("/api/v1/auth/profile/sync", post(handle_auth_profile_sync))
        .route("/api/v1/auth/logout", post(handle_auth_logout))
}

async fn handle_auth_status(
    State(state): State<AppState>,
) -> Result<Json<AuthStatusResponse>, (StatusCode, Json<serde_json::Value>)> {
    application::auth_status(&state).await
}

async fn handle_auth_refresh(
    State(state): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    application::auth_refresh_for_state(&state).await
}

async fn handle_auth_profile_sync(
    State(state): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    application::auth_profile_sync_for_state(&state).await
}

async fn handle_auth_logout(
    State(state): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    application::auth_logout_for_state(&state).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        auth_chain::{AuthChainClient, AuthChainEndpoints},
        state::{
            AppStateInit, AuthLoginMinecraftProfile, InstallStore, NewAuthLoginMinecraftAccount,
            NewAuthLoginMsaToken, SessionStore,
        },
    };
    use axial_config::{
        AppConfig, AppPaths, ConfigStore, InstanceRegistrySnapshot, InstanceStore,
        LAUNCH_AUTH_MODE_ONLINE,
    };
    use axial_performance::PerformanceManager;
    use axum::{
        body::{Body, Bytes, to_bytes},
        extract::State,
        http::{HeaderMap, Method, Request},
        routing::get,
    };
    use serde_json::Value;
    use std::{fs, path::PathBuf, sync::Arc};
    use tokio::sync::mpsc;
    use tower::ServiceExt;

    #[tokio::test]
    async fn auth_status_route_serializes_backend_owned_action_state() {
        let fixture = RouteAuthFixture::new("auth-status-route");
        fixture.set_launch_auth_mode(LAUNCH_AUTH_MODE_ONLINE);
        insert_active_current_login(
            fixture.state.auth_logins(),
            Some("msa-refresh-token"),
            false,
        )
        .await;

        let (status, payload) = fixture
            .request_json(Method::GET, "/api/v1/auth/status")
            .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(payload["launch_auth_mode"], "online");
        assert_eq!(payload["online_mode_ready"], false);
        assert_eq!(payload["msa_refresh_available"], true);
        assert_eq!(payload["minecraft_profile_ready"], true);
        assert_eq!(payload["minecraft_ownership_verified"], false);
        assert_eq!(
            payload["online_action"]["state_id"],
            "online_refresh_available"
        );
        assert_eq!(payload["online_action"]["enabled"], true);
        assert_eq!(payload["refresh_action"]["state_id"], "refresh_recommended");
        assert_eq!(payload["refresh_action"]["enabled"], true);
        assert_eq!(
            payload["profile_sync_action"]["state_id"],
            "profile_sync_available"
        );
        assert_eq!(payload["profile_sync_action"]["enabled"], true);
        assert_eq!(
            payload["skin_action"]["disabled_reason"],
            "The selected Microsoft account has not verified Minecraft Java ownership."
        );
        assert_no_sensitive_public_fields(&payload);
    }

    #[tokio::test]
    async fn auth_refresh_route_returns_backend_precondition_without_provider_policy() {
        let fixture = RouteAuthFixture::new("auth-refresh-route-missing-token");
        insert_active_msa_login(fixture.state.auth_logins(), None).await;

        let (status, payload) = fixture
            .request_json(Method::POST, "/api/v1/auth/refresh")
            .await;

        assert_eq!(status, StatusCode::PRECONDITION_FAILED);
        assert_eq!(payload["status"], "sign_in_required");
        assert_eq!(
            payload["error"],
            "Microsoft sign-in refresh is unavailable; sign in again"
        );
        assert_no_sensitive_public_fields(&payload);
    }

    #[tokio::test]
    async fn auth_profile_sync_route_bounds_provider_failure_and_preserves_auth() {
        let fixture = RouteAuthFixture::new("auth-profile-sync-route-provider-failure");
        insert_active_current_login(fixture.state.auth_logins(), Some("msa-refresh-token"), true)
            .await;
        let (client, mut requests) = profile_rejected_auth_chain_client().await;
        fixture.state.set_auth_chain_client_override(client);

        let (status, payload) = fixture
            .request_json(Method::POST, "/api/v1/auth/profile/sync")
            .await;

        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert_eq!(payload["status"], "minecraft_auth_chain_failed");
        assert_eq!(payload["auth_chain_error"], "upstream_rejected");
        assert_eq!(payload["error"], "Minecraft account verification failed");
        assert_no_sensitive_public_fields(&payload);
        let active = fixture
            .state
            .auth_logins()
            .active_current_minecraft_account_state()
            .await
            .expect("active auth preserved");
        assert_eq!(active.account.profile.name, "OldProfileName");
        assert_eq!(
            fixture.state.auth_logins().active_msa_refresh_token().await,
            Some("msa-refresh-token".to_string())
        );
        let request = requests.recv().await.expect("profile request recorded");
        assert_eq!(request.path, "/minecraft/profile");
        assert_eq!(
            request.authorization.as_deref(),
            Some("Bearer old-minecraft-access-token")
        );
    }

    struct RouteAuthFixture {
        state: AppState,
        root: PathBuf,
    }

    impl RouteAuthFixture {
        fn new(name: &str) -> Self {
            let root = test_root(name);
            let paths = test_paths(&root);
            let config = Arc::new(
                ConfigStore::from_config(
                    paths.clone(),
                    AppConfig {
                        username: "ConfigUser".to_string(),
                        ..AppConfig::default()
                    },
                )
                .expect("set config"),
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

        fn set_launch_auth_mode(&self, mode: &str) {
            let mut config = self.state.config().current();
            config.launch_auth_mode = mode.to_string();
            self.state
                .config()
                .replace_for_test(config)
                .expect("set launch auth mode");
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
    }

    impl Drop for RouteAuthFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    async fn insert_active_msa_login(
        store: &Arc<crate::state::AuthLoginStore>,
        refresh_token: Option<&str>,
    ) {
        store
            .replace_with_msa_token(NewAuthLoginMsaToken {
                access_token: "old-msa-access-token".to_string(),
                refresh_token: refresh_token.map(ToOwned::to_owned),
                id_token: Some("old-msa-id-token".to_string()),
                token_type: "Bearer".to_string(),
                expires_in: 3600,
                scope: Some("XboxLive.signin offline_access".to_string()),
            })
            .await
            .expect("insert active MSA login");
    }

    async fn insert_active_current_login(
        store: &Arc<crate::state::AuthLoginStore>,
        refresh_token: Option<&str>,
        owns_minecraft_java: bool,
    ) {
        store
            .replace_with_msa_and_minecraft_account(
                NewAuthLoginMsaToken {
                    access_token: "old-msa-access-token".to_string(),
                    refresh_token: refresh_token.map(ToOwned::to_owned),
                    id_token: Some("old-msa-id-token".to_string()),
                    token_type: "Bearer".to_string(),
                    expires_in: 3600,
                    scope: Some("XboxLive.signin offline_access".to_string()),
                },
                NewAuthLoginMinecraftAccount {
                    access_token: "old-minecraft-access-token".to_string(),
                    token_type: Some("Bearer".to_string()),
                    expires_in: 86_400,
                    profile: AuthLoginMinecraftProfile {
                        id: "old-minecraft-profile-id".to_string(),
                        name: "OldProfileName".to_string(),
                        skins: Vec::new(),
                        capes: Vec::new(),
                    },
                    owns_minecraft_java,
                },
            )
            .await
            .expect("insert active Minecraft login");
    }

    async fn profile_rejected_auth_chain_client() -> (
        AuthChainClient,
        mpsc::UnboundedReceiver<RecordedAuthChainRequest>,
    ) {
        let (tx, rx) = mpsc::unbounded_channel();
        let app = axum::Router::new()
            .route("/minecraft/profile", get(record_rejected_minecraft_profile))
            .route("/minecraft/ownership", get(record_minecraft_ownership))
            .with_state(AuthChainRouteState { tx });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind auth chain route test server");
        let base_url = format!("http://{}", listener.local_addr().expect("local addr"));
        tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("auth chain route test server");
        });
        let client = AuthChainClient::with_endpoints(AuthChainEndpoints {
            minecraft_profile: format!("{base_url}/minecraft/profile"),
            minecraft_ownership: format!("{base_url}/minecraft/ownership"),
        })
        .expect("auth chain route test client");

        (client, rx)
    }

    #[derive(Clone)]
    struct AuthChainRouteState {
        tx: mpsc::UnboundedSender<RecordedAuthChainRequest>,
    }

    #[derive(Debug, Eq, PartialEq)]
    struct RecordedAuthChainRequest {
        path: String,
        authorization: Option<String>,
    }

    async fn record_rejected_minecraft_profile(
        State(state): State<AuthChainRouteState>,
        headers: HeaderMap,
    ) -> (StatusCode, Json<Value>) {
        record_auth_chain_route_request(&state.tx, "/minecraft/profile", &headers, &Bytes::new());
        (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({
                "error": "provider-secret-payload",
                "access_token": "minecraft-access-token"
            })),
        )
    }

    async fn record_minecraft_ownership(
        State(state): State<AuthChainRouteState>,
        headers: HeaderMap,
    ) -> (StatusCode, Json<Value>) {
        record_auth_chain_route_request(&state.tx, "/minecraft/ownership", &headers, &Bytes::new());
        (
            StatusCode::OK,
            Json(serde_json::json!({ "items": [{ "name": "game_minecraft" }] })),
        )
    }

    fn record_auth_chain_route_request(
        tx: &mpsc::UnboundedSender<RecordedAuthChainRequest>,
        path: &str,
        headers: &HeaderMap,
        _body: &Bytes,
    ) {
        tx.send(RecordedAuthChainRequest {
            path: path.to_string(),
            authorization: headers
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                .map(ToOwned::to_owned),
        })
        .expect("record auth chain route request");
    }

    fn test_root(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "axial-api-auth-route-{name}-{}-{}",
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

    fn assert_no_sensitive_public_fields(value: &Value) {
        assert_no_sensitive_public_field_keys(value);
        let text = value.to_string();
        for material in [
            "msa-access-token",
            "msa-refresh-token",
            "msa-id-token",
            "minecraft-access-token",
            "old-msa-access-token",
            "old-msa-refresh-token",
            "old-msa-id-token",
            "old-minecraft-access-token",
            "provider-secret-payload",
        ] {
            assert!(
                !text.contains(material),
                "public JSON exposed sensitive material {material}"
            );
        }
    }

    fn assert_no_sensitive_public_field_keys(value: &Value) {
        match value {
            Value::Object(map) => {
                for (key, value) in map {
                    assert!(
                        !matches!(key.as_str(), "access_token" | "refresh_token" | "id_token"),
                        "public JSON exposed {key}"
                    );
                    assert_no_sensitive_public_field_keys(value);
                }
            }
            Value::Array(values) => {
                for value in values {
                    assert_no_sensitive_public_field_keys(value);
                }
            }
            _ => {}
        }
    }
}
