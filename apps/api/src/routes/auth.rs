use crate::state::{AppState, AuthLoginSession, AuthLoginStore, NewAuthLoginSession};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::{get, post},
};
use croopor_config::validate_username;
use croopor_minecraft::offline_uuid;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::{sync::Arc, time::Duration};

const MSA_CLIENT_ID_ENV: &str = "CROOPOR_MSA_CLIENT_ID";
const MSA_DEVICE_CODE_ENDPOINT: &str =
    "https://login.microsoftonline.com/consumers/oauth2/v2.0/devicecode";
const MSA_DEVICE_CODE_SCOPE: &str = "XboxLive.signin offline_access";
const MSA_DEVICE_CODE_TIMEOUT: Duration = Duration::from_secs(20);
const LOGIN_UNAVAILABLE_REASON: &str = "Microsoft sign-in is not configured in this build";
const LOGIN_AVAILABLE_REASON: &str = "Microsoft sign-in is configured";

#[derive(Debug, Serialize)]
struct AuthStatusResponse {
    mode: &'static str,
    username: String,
    uuid: String,
    provider: &'static str,
    verified: bool,
    online_mode_ready: bool,
    skin_source: &'static str,
    login_available: bool,
    login_reason: &'static str,
}

#[derive(Debug, Serialize)]
struct AuthLoginUnavailableResponse {
    error: &'static str,
    status: &'static str,
    login_available: bool,
    login_reason: &'static str,
}

#[derive(Debug, Serialize)]
struct AuthLoginPendingResponse {
    status: &'static str,
    login_id: String,
    user_code: String,
    verification_uri: String,
    expires_in: u64,
    interval: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

#[derive(Debug, Deserialize)]
struct MsaDeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    expires_in: u64,
    interval: u64,
    message: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct AuthLoginConfig {
    client_id: Option<String>,
}

#[derive(Debug)]
enum AuthLoginError {
    ClientBuild,
    Request,
    UpstreamStatus(StatusCode),
    Parse,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/auth/status", get(handle_auth_status))
        .route("/api/v1/auth/login", post(handle_auth_login))
        .route(
            "/api/v1/auth/login/{login_id}",
            get(handle_auth_login_status),
        )
}

async fn handle_auth_status(
    State(state): State<AppState>,
) -> Result<Json<AuthStatusResponse>, (StatusCode, Json<serde_json::Value>)> {
    auth_status_from_username(
        &state.config().current().username,
        AuthLoginConfig::from_env(),
    )
    .map(Json)
}

async fn handle_auth_login(State(state): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    auth_login_for_config(AuthLoginConfig::from_env(), state.auth_logins()).await
}

async fn handle_auth_login_status(
    Path(login_id): Path<String>,
    State(state): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    auth_login_status(&login_id, state.auth_logins()).await
}

fn auth_status_from_username(
    config_username: &str,
    login_config: AuthLoginConfig,
) -> Result<AuthStatusResponse, (StatusCode, Json<serde_json::Value>)> {
    let username = validate_username(config_username).map_err(|error| {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": error })),
        )
    })?;

    Ok(AuthStatusResponse {
        mode: "offline",
        uuid: offline_uuid(&username),
        username,
        provider: "offline",
        verified: false,
        online_mode_ready: false,
        skin_source: "default",
        login_available: login_config.is_available(),
        login_reason: login_config.reason(),
    })
}

async fn auth_login_for_config(
    config: AuthLoginConfig,
    login_store: &Arc<AuthLoginStore>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(client_id) = config.client_id.as_deref() else {
        return auth_login_unavailable();
    };

    match request_msa_device_code(client_id).await {
        Ok(response) => {
            let session = login_store
                .insert(NewAuthLoginSession::from(response))
                .await;
            (
                StatusCode::OK,
                Json(serde_json::json!(auth_login_pending_response(&session))),
            )
        }
        Err(error) => auth_login_error_response(error),
    }
}

fn auth_login_unavailable() -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(serde_json::json!(AuthLoginUnavailableResponse {
            error: LOGIN_UNAVAILABLE_REASON,
            status: "unavailable",
            login_available: false,
            login_reason: LOGIN_UNAVAILABLE_REASON,
        })),
    )
}

async fn request_msa_device_code(client_id: &str) -> Result<MsaDeviceCodeResponse, AuthLoginError> {
    let client = Client::builder()
        .timeout(MSA_DEVICE_CODE_TIMEOUT)
        .build()
        .map_err(|_| AuthLoginError::ClientBuild)?;

    let response = client
        .post(MSA_DEVICE_CODE_ENDPOINT)
        .form(&[("client_id", client_id), ("scope", MSA_DEVICE_CODE_SCOPE)])
        .send()
        .await
        .map_err(|_| AuthLoginError::Request)?;

    let status = response.status();
    if !status.is_success() {
        return Err(AuthLoginError::UpstreamStatus(status));
    }

    response
        .json::<MsaDeviceCodeResponse>()
        .await
        .map_err(|_| AuthLoginError::Parse)
}

fn auth_login_pending_response(session: &AuthLoginSession) -> AuthLoginPendingResponse {
    AuthLoginPendingResponse {
        status: "pending",
        login_id: session.login_id.clone(),
        user_code: session.user_code.clone(),
        verification_uri: session.verification_uri.clone(),
        expires_in: session.expires_in,
        interval: session.interval,
        message: session.message.clone(),
    }
}

async fn auth_login_status(
    login_id: &str,
    login_store: &Arc<AuthLoginStore>,
) -> (StatusCode, Json<serde_json::Value>) {
    if let Some(session) = login_store.get(login_id).await {
        return (
            StatusCode::OK,
            Json(serde_json::json!(auth_login_status_pending_response(
                &session
            ))),
        );
    }

    if login_store.remove_expired(login_id).await {
        return (
            StatusCode::GONE,
            Json(serde_json::json!({
                "error": "Microsoft sign-in code expired",
                "status": "expired",
            })),
        );
    }

    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({
            "error": "Microsoft sign-in session not found",
        })),
    )
}

fn auth_login_status_pending_response(session: &AuthLoginSession) -> AuthLoginPendingResponse {
    AuthLoginPendingResponse {
        expires_in: bounded_remaining_seconds(session),
        ..auth_login_pending_response(session)
    }
}

fn bounded_remaining_seconds(session: &AuthLoginSession) -> u64 {
    let remaining = (session.expires_at - chrono::Utc::now()).num_milliseconds();
    if remaining <= 0 {
        return 0;
    }

    (((remaining as u64) + 999) / 1000).min(session.expires_in)
}

fn auth_login_error_response(error: AuthLoginError) -> (StatusCode, Json<serde_json::Value>) {
    let (status, message) = match error {
        AuthLoginError::ClientBuild => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to initialize Microsoft sign-in request",
        ),
        AuthLoginError::Request => (
            StatusCode::BAD_GATEWAY,
            "failed to reach Microsoft sign-in service",
        ),
        AuthLoginError::UpstreamStatus(status) => {
            if status.as_u16() >= 500 {
                (
                    StatusCode::BAD_GATEWAY,
                    "Microsoft sign-in service is unavailable",
                )
            } else {
                (
                    StatusCode::BAD_GATEWAY,
                    "Microsoft sign-in request was rejected",
                )
            }
        }
        AuthLoginError::Parse => (
            StatusCode::BAD_GATEWAY,
            "failed to parse Microsoft sign-in response",
        ),
    };

    (status, Json(serde_json::json!({ "error": message })))
}

impl From<MsaDeviceCodeResponse> for NewAuthLoginSession {
    fn from(response: MsaDeviceCodeResponse) -> Self {
        Self {
            device_code: response.device_code,
            user_code: response.user_code,
            verification_uri: response.verification_uri,
            expires_in: response.expires_in,
            interval: response.interval,
            message: response.message,
        }
    }
}

impl AuthLoginConfig {
    fn from_env() -> Self {
        Self::from_env_value(std::env::var(MSA_CLIENT_ID_ENV).ok().as_deref())
    }

    fn from_env_value(value: Option<&str>) -> Self {
        Self {
            client_id: value
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned),
        }
    }

    fn is_available(&self) -> bool {
        self.client_id.is_some()
    }

    fn reason(&self) -> &'static str {
        if self.is_available() {
            LOGIN_AVAILABLE_REASON
        } else {
            LOGIN_UNAVAILABLE_REASON
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{AppStateInit, InstallStore, SessionStore};
    use croopor_config::{AppConfig, AppPaths, ConfigStore, InstanceStore};
    use croopor_performance::PerformanceManager;
    use std::{fs, path::PathBuf, sync::Arc};

    #[tokio::test]
    async fn auth_status_uses_configured_offline_identity() {
        let fixture = TestFixture::new("configured-identity", "ConfigUser");

        let response = fixture.status().await.expect("auth status").0;

        assert_eq!(response.mode, "offline");
        assert_eq!(response.username, "ConfigUser");
        assert_eq!(response.uuid, offline_uuid("ConfigUser"));
        assert_eq!(response.provider, "offline");
        assert!(!response.verified);
        assert!(!response.online_mode_ready);
        assert_eq!(response.skin_source, "default");
        assert!(!response.login_available);
        assert_eq!(response.login_reason, LOGIN_UNAVAILABLE_REASON);
    }

    #[test]
    fn auth_status_marks_login_available_when_client_id_is_configured() {
        let response = auth_status_from_username(
            "ConfigUser",
            AuthLoginConfig::from_env_value(Some(" public-client-id ")),
        )
        .expect("auth status");

        assert!(response.login_available);
        assert_eq!(response.login_reason, LOGIN_AVAILABLE_REASON);
    }

    #[test]
    fn auth_status_rejects_invalid_configured_username() {
        let error = auth_status_from_username("bad name", AuthLoginConfig::from_env_value(None))
            .expect_err("invalid username should fail");

        assert_eq!(error.0, StatusCode::BAD_REQUEST);
        assert_eq!(
            error.1.0,
            serde_json::json!({ "error": "Letters, numbers, and underscores only." })
        );
    }

    #[tokio::test]
    async fn auth_login_returns_unavailable_response() {
        let store = Arc::new(AuthLoginStore::new());
        let response = auth_login_for_config(AuthLoginConfig::from_env_value(None), &store).await;

        assert_eq!(response.0, StatusCode::NOT_IMPLEMENTED);
        assert_eq!(response.1.0, unavailable_json());
    }

    #[test]
    fn auth_login_unavailable_helper_returns_json_error() {
        let response = auth_login_unavailable();

        assert_eq!(response.0, StatusCode::NOT_IMPLEMENTED);
        assert_eq!(response.1.0, unavailable_json());
    }

    #[test]
    fn auth_login_config_treats_missing_or_blank_client_id_as_unavailable() {
        assert_eq!(AuthLoginConfig::from_env_value(None).client_id, None);
        assert_eq!(AuthLoginConfig::from_env_value(Some("")).client_id, None);
        assert_eq!(AuthLoginConfig::from_env_value(Some("   ")).client_id, None);
    }

    #[test]
    fn auth_login_config_trims_configured_client_id() {
        assert_eq!(
            AuthLoginConfig::from_env_value(Some(" public-client-id ")).client_id,
            Some("public-client-id".to_string())
        );
    }

    #[tokio::test]
    async fn auth_login_stores_device_code_and_returns_public_login_id() {
        let store = AuthLoginStore::new();
        let response: MsaDeviceCodeResponse = serde_json::from_value(serde_json::json!({
            "device_code": "device-code-value",
            "user_code": "ABCD-EFGH",
            "verification_uri": "https://www.microsoft.com/link",
            "expires_in": 900,
            "interval": 5,
            "message": "To sign in, use a web browser to open the page and enter the code."
        }))
        .expect("parse device code response");

        let session = store.insert(NewAuthLoginSession::from(response)).await;
        let login_response = auth_login_pending_response(&session);
        let value = serde_json::to_value(login_response).expect("serialize response");

        assert_eq!(session.device_code, "device-code-value");
        assert_eq!(value.get("device_code"), None);
        assert_eq!(value["status"], "pending");
        assert_eq!(value["login_id"], session.login_id);
        assert_eq!(value["user_code"], "ABCD-EFGH");
        assert_eq!(value["verification_uri"], "https://www.microsoft.com/link");
        assert_eq!(value["expires_in"], 900);
        assert_eq!(value["interval"], 5);
        assert_eq!(
            value["message"],
            "To sign in, use a web browser to open the page and enter the code."
        );
    }

    #[tokio::test]
    async fn auth_login_omits_message_when_microsoft_does_not_return_one() {
        let store = AuthLoginStore::new();
        let response: MsaDeviceCodeResponse = serde_json::from_value(serde_json::json!({
            "device_code": "device-code-value",
            "user_code": "ABCD-EFGH",
            "verification_uri": "https://www.microsoft.com/link",
            "expires_in": 900,
            "interval": 5
        }))
        .expect("parse device code response");

        let session = store.insert(NewAuthLoginSession::from(response)).await;
        let value = serde_json::to_value(auth_login_pending_response(&session))
            .expect("serialize response");

        assert_eq!(value.get("message"), None);
        assert_eq!(value.get("device_code"), None);
        assert_eq!(value["status"], "pending");
        assert_eq!(value["login_id"], session.login_id);
    }

    #[tokio::test]
    async fn auth_login_status_returns_pending_public_response() {
        let store = Arc::new(AuthLoginStore::new());
        let session = store
            .insert(NewAuthLoginSession {
                device_code: "raw-device-code".to_string(),
                user_code: "ABCD-EFGH".to_string(),
                verification_uri: "https://www.microsoft.com/link".to_string(),
                expires_in: 900,
                interval: 5,
                message: Some("Use this code.".to_string()),
            })
            .await;

        let response = auth_login_status(&session.login_id, &store).await;
        let value = response.1.0;

        assert_eq!(response.0, StatusCode::OK);
        assert_eq!(value.get("device_code"), None);
        assert_eq!(value["status"], "pending");
        assert_eq!(value["login_id"], session.login_id);
        assert_eq!(value["user_code"], "ABCD-EFGH");
        assert_eq!(value["verification_uri"], "https://www.microsoft.com/link");
        assert!(
            value["expires_in"]
                .as_u64()
                .is_some_and(|value| value > 0 && value <= 900)
        );
        assert_eq!(value["interval"], 5);
        assert_eq!(value["message"], "Use this code.");
    }

    #[tokio::test]
    async fn auth_login_status_returns_expired_error_and_prunes_known_session() {
        let store = Arc::new(AuthLoginStore::new());
        let session = store
            .insert(NewAuthLoginSession {
                device_code: "raw-device-code".to_string(),
                user_code: "ABCD-EFGH".to_string(),
                verification_uri: "https://www.microsoft.com/link".to_string(),
                expires_in: 0,
                interval: 5,
                message: None,
            })
            .await;

        let response = auth_login_status(&session.login_id, &store).await;

        assert_eq!(response.0, StatusCode::GONE);
        assert_eq!(
            response.1.0,
            serde_json::json!({
                "error": "Microsoft sign-in code expired",
                "status": "expired",
            })
        );
        assert_eq!(store.len().await, 0);

        let second_response = auth_login_status(&session.login_id, &store).await;
        assert_eq!(second_response.0, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn auth_login_status_returns_not_found_for_unknown_session() {
        let store = Arc::new(AuthLoginStore::new());

        let response = auth_login_status("missing-login", &store).await;

        assert_eq!(response.0, StatusCode::NOT_FOUND);
        assert_eq!(
            response.1.0,
            serde_json::json!({ "error": "Microsoft sign-in session not found" })
        );
    }

    struct TestFixture {
        state: AppState,
        root: PathBuf,
    }

    impl TestFixture {
        fn new(name: &str, username: &str) -> Self {
            let root = test_root(name);
            let paths = test_paths(&root);
            let config = Arc::new(ConfigStore::load_from(paths.clone()).expect("load config"));
            config
                .replace_in_memory(AppConfig {
                    username: username.to_string(),
                    ..AppConfig::default()
                })
                .expect("set username");
            let instances =
                Arc::new(InstanceStore::load_from(paths.clone()).expect("load instances"));
            let state = AppState::new(AppStateInit {
                app_name: "Croopor".to_string(),
                version: "test".to_string(),
                config,
                instances,
                installs: Arc::new(InstallStore::new()),
                sessions: Arc::new(SessionStore::new()),
                performance: Arc::new(PerformanceManager::new().expect("performance manager")),
                frontend_dir: root.join("frontend"),
            });

            Self { state, root }
        }

        async fn status(
            &self,
        ) -> Result<Json<AuthStatusResponse>, (StatusCode, Json<serde_json::Value>)> {
            auth_status_from_username(
                &self.state.config().current().username,
                AuthLoginConfig::from_env_value(None),
            )
            .map(Json)
        }
    }

    impl Drop for TestFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn test_root(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "croopor-api-auth-{name}-{}-{}",
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

    fn unavailable_json() -> serde_json::Value {
        serde_json::json!({
            "error": LOGIN_UNAVAILABLE_REASON,
            "status": "unavailable",
            "login_available": false,
            "login_reason": LOGIN_UNAVAILABLE_REASON,
        })
    }
}
