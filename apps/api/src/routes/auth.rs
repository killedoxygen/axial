use crate::state::{
    AppState, AuthLoginSession, AuthLoginStore, NewAuthLoginMsaToken, NewAuthLoginSession,
};
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
const MSA_TOKEN_ENDPOINT: &str = "https://login.microsoftonline.com/consumers/oauth2/v2.0/token";
const MSA_DEVICE_CODE_SCOPE: &str = "XboxLive.signin offline_access";
const MSA_TOKEN_GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:device_code";
const MSA_DEVICE_CODE_TIMEOUT: Duration = Duration::from_secs(20);
const MSA_TOKEN_POLL_TIMEOUT: Duration = Duration::from_secs(20);
const MSA_SLOW_DOWN_INTERVAL_INCREMENT: u64 = 5;
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
    #[serde(skip_serializing_if = "Option::is_none")]
    poll_hint: Option<&'static str>,
}

#[derive(Debug, Serialize)]
struct AuthLoginMsaAuthenticatedResponse {
    status: &'static str,
    login_id: String,
    token_type: String,
    expires_in: u64,
    has_refresh_token: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    token_scope: Option<String>,
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

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
struct MsaTokenSuccessResponse {
    access_token: String,
    refresh_token: Option<String>,
    id_token: Option<String>,
    token_type: String,
    expires_in: u64,
    scope: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
struct MsaTokenErrorResponse {
    error: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct AuthLoginConfig {
    client_id: Option<String>,
}

#[derive(Debug, Eq, PartialEq)]
enum AuthLoginError {
    ClientBuild,
    Request,
    UpstreamStatus(StatusCode),
    Parse,
}

#[derive(Debug, Eq, PartialEq)]
enum AuthLoginPollError {
    Request(AuthLoginError),
    OAuth(MsaTokenErrorCode),
}

#[derive(Debug, Eq, PartialEq)]
enum MsaTokenErrorCode {
    AuthorizationPending,
    SlowDown,
    AuthorizationDeclined,
    BadVerificationCode,
    ExpiredToken,
    Other,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/auth/status", get(handle_auth_status))
        .route("/api/v1/auth/login", post(handle_auth_login))
        .route(
            "/api/v1/auth/login/{login_id}",
            get(handle_auth_login_status),
        )
        .route(
            "/api/v1/auth/login/{login_id}/poll",
            post(handle_auth_login_poll),
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

async fn handle_auth_login_poll(
    Path(login_id): Path<String>,
    State(state): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    auth_login_poll_for_config(
        &login_id,
        AuthLoginConfig::from_env(),
        state.auth_logins(),
        MSA_TOKEN_ENDPOINT,
    )
    .await
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

async fn auth_login_poll_for_config(
    login_id: &str,
    config: AuthLoginConfig,
    login_store: &Arc<AuthLoginStore>,
    token_endpoint: &str,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(client_id) = config.client_id.as_deref() else {
        return auth_login_unavailable();
    };

    let Some(session) = login_store.get(login_id).await else {
        return auth_login_missing_or_expired(login_id, login_store).await;
    };

    match request_msa_token(token_endpoint, client_id, &session.device_code).await {
        Ok(response) => auth_login_poll_success_response(login_id, response, login_store).await,
        Err(AuthLoginPollError::OAuth(error)) => {
            auth_login_poll_oauth_error_response(login_id, &session, login_store, error).await
        }
        Err(AuthLoginPollError::Request(error)) => auth_login_error_response(error),
    }
}

async fn request_msa_token(
    token_endpoint: &str,
    client_id: &str,
    device_code: &str,
) -> Result<MsaTokenSuccessResponse, AuthLoginPollError> {
    let client = Client::builder()
        .timeout(MSA_TOKEN_POLL_TIMEOUT)
        .build()
        .map_err(|_| AuthLoginPollError::Request(AuthLoginError::ClientBuild))?;

    let response = client
        .post(token_endpoint)
        .form(&[
            ("grant_type", MSA_TOKEN_GRANT_TYPE),
            ("client_id", client_id),
            ("device_code", device_code),
        ])
        .send()
        .await
        .map_err(|_| AuthLoginPollError::Request(AuthLoginError::Request))?;

    let status = response.status();
    if status.is_success() {
        return response
            .json::<MsaTokenSuccessResponse>()
            .await
            .map_err(|_| AuthLoginPollError::Request(AuthLoginError::Parse));
    }

    match response.json::<MsaTokenErrorResponse>().await {
        Ok(response) => Err(AuthLoginPollError::OAuth(MsaTokenErrorCode::from_error(
            &response.error,
        ))),
        Err(_) => Err(AuthLoginPollError::Request(AuthLoginError::UpstreamStatus(
            status,
        ))),
    }
}

async fn auth_login_poll_success_response(
    login_id: &str,
    response: MsaTokenSuccessResponse,
    login_store: &Arc<AuthLoginStore>,
) -> (StatusCode, Json<serde_json::Value>) {
    let public_response = AuthLoginMsaAuthenticatedResponse {
        status: "msa_authenticated",
        login_id: login_id.to_string(),
        token_type: response.token_type.clone(),
        expires_in: response.expires_in,
        has_refresh_token: response.refresh_token.is_some(),
        token_scope: response.scope.clone(),
    };
    if login_store
        .complete_with_msa_token(login_id, NewAuthLoginMsaToken::from(response))
        .await
        .is_none()
    {
        return auth_login_missing_or_expired(login_id, login_store).await;
    }

    (StatusCode::OK, Json(serde_json::json!(public_response)))
}

async fn auth_login_poll_oauth_error_response(
    login_id: &str,
    session: &AuthLoginSession,
    login_store: &Arc<AuthLoginStore>,
    error: MsaTokenErrorCode,
) -> (StatusCode, Json<serde_json::Value>) {
    match error {
        MsaTokenErrorCode::AuthorizationPending => (
            StatusCode::ACCEPTED,
            Json(serde_json::json!(auth_login_status_pending_response(
                session
            ))),
        ),
        MsaTokenErrorCode::SlowDown => {
            let interval = login_store
                .increase_interval(login_id, MSA_SLOW_DOWN_INTERVAL_INCREMENT)
                .await
                .unwrap_or_else(|| {
                    session
                        .interval
                        .saturating_add(MSA_SLOW_DOWN_INTERVAL_INCREMENT)
                });
            let mut response = auth_login_status_pending_response(session);
            response.interval = interval;
            response.poll_hint = Some("slow_down");
            (StatusCode::ACCEPTED, Json(serde_json::json!(response)))
        }
        MsaTokenErrorCode::AuthorizationDeclined => {
            let _ = login_store.remove(login_id).await;
            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "error": "Microsoft sign-in was declined",
                    "status": "authorization_declined",
                })),
            )
        }
        MsaTokenErrorCode::ExpiredToken => {
            let _ = login_store.remove(login_id).await;
            (
                StatusCode::GONE,
                Json(serde_json::json!({
                    "error": "Microsoft sign-in code expired",
                    "status": "expired",
                })),
            )
        }
        MsaTokenErrorCode::BadVerificationCode => {
            let _ = login_store.remove(login_id).await;
            (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({
                    "error": "Microsoft sign-in verification code was rejected",
                    "status": "bad_verification_code",
                })),
            )
        }
        MsaTokenErrorCode::Other => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({
                "error": "Microsoft sign-in polling failed",
                "status": "error",
            })),
        ),
    }
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
        poll_hint: None,
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

    auth_login_missing_or_expired(login_id, login_store).await
}

async fn auth_login_missing_or_expired(
    login_id: &str,
    login_store: &Arc<AuthLoginStore>,
) -> (StatusCode, Json<serde_json::Value>) {
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

impl From<MsaTokenSuccessResponse> for NewAuthLoginMsaToken {
    fn from(response: MsaTokenSuccessResponse) -> Self {
        Self {
            access_token: response.access_token,
            refresh_token: response.refresh_token,
            id_token: response.id_token,
            token_type: response.token_type,
            expires_in: response.expires_in,
            scope: response.scope,
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

impl MsaTokenErrorCode {
    fn from_error(error: &str) -> Self {
        match error {
            "authorization_pending" => Self::AuthorizationPending,
            "slow_down" => Self::SlowDown,
            "authorization_declined" => Self::AuthorizationDeclined,
            "bad_verification_code" => Self::BadVerificationCode,
            "expired_token" => Self::ExpiredToken,
            _ => Self::Other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{AppStateInit, InstallStore, SessionStore};
    use axum::extract::Form;
    use croopor_config::{AppConfig, AppPaths, ConfigStore, InstanceStore};
    use croopor_performance::PerformanceManager;
    use std::{collections::HashMap, fs, path::PathBuf, sync::Arc};
    use tokio::sync::mpsc;

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
    async fn auth_login_poll_posts_single_device_flow_form_to_configured_endpoint() {
        let store = Arc::new(AuthLoginStore::new());
        let session = insert_pending_login(&store).await;
        let (token_endpoint, mut requests) = token_test_server(
            StatusCode::BAD_REQUEST,
            serde_json::json!({ "error": "authorization_pending" }),
        )
        .await;

        let response = auth_login_poll_for_config(
            &session.login_id,
            AuthLoginConfig::from_env_value(Some("public-client-id")),
            &store,
            &token_endpoint,
        )
        .await;

        assert_eq!(response.0, StatusCode::ACCEPTED);
        assert_eq!(response.1.0["status"], "pending");
        assert_no_sensitive_public_fields(&response.1.0);

        let form = tokio::time::timeout(std::time::Duration::from_secs(1), requests.recv())
            .await
            .expect("token endpoint request")
            .expect("form body");
        assert_eq!(form["grant_type"], MSA_TOKEN_GRANT_TYPE);
        assert_eq!(form["client_id"], "public-client-id");
        assert_eq!(form["device_code"], "raw-device-code");
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), requests.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn auth_login_poll_returns_unavailable_when_client_id_is_missing() {
        let store = Arc::new(AuthLoginStore::new());
        let session = insert_pending_login(&store).await;

        let response = auth_login_poll_for_config(
            &session.login_id,
            AuthLoginConfig::from_env_value(None),
            &store,
            "unused",
        )
        .await;

        assert_eq!(response.0, StatusCode::NOT_IMPLEMENTED);
        assert_eq!(response.1.0, unavailable_json());
        assert_eq!(store.get(&session.login_id).await, Some(session));
    }

    #[tokio::test]
    async fn auth_login_poll_maps_declined_to_terminal_public_response() {
        let store = Arc::new(AuthLoginStore::new());
        let session = insert_pending_login(&store).await;

        let response = auth_login_poll_oauth_error_response(
            &session.login_id,
            &session,
            &store,
            MsaTokenErrorCode::AuthorizationDeclined,
        )
        .await;

        assert_eq!(response.0, StatusCode::OK);
        assert_eq!(response.1.0["status"], "authorization_declined");
        assert_no_sensitive_public_fields(&response.1.0);
        assert_eq!(store.get(&session.login_id).await, None);
    }

    #[tokio::test]
    async fn auth_login_poll_maps_expired_to_terminal_public_response() {
        let store = Arc::new(AuthLoginStore::new());
        let session = insert_pending_login(&store).await;

        let response = auth_login_poll_oauth_error_response(
            &session.login_id,
            &session,
            &store,
            MsaTokenErrorCode::ExpiredToken,
        )
        .await;

        assert_eq!(response.0, StatusCode::GONE);
        assert_eq!(response.1.0["status"], "expired");
        assert_no_sensitive_public_fields(&response.1.0);
        assert_eq!(store.get(&session.login_id).await, None);
    }

    #[tokio::test]
    async fn auth_login_poll_maps_bad_verification_code_to_bounded_terminal_error() {
        let store = Arc::new(AuthLoginStore::new());
        let session = insert_pending_login(&store).await;

        let response = auth_login_poll_oauth_error_response(
            &session.login_id,
            &session,
            &store,
            MsaTokenErrorCode::BadVerificationCode,
        )
        .await;

        assert_eq!(response.0, StatusCode::BAD_GATEWAY);
        assert_eq!(response.1.0["status"], "bad_verification_code");
        assert_no_sensitive_public_fields(&response.1.0);
        assert_eq!(store.get(&session.login_id).await, None);
    }

    #[tokio::test]
    async fn auth_login_poll_maps_slow_down_to_pending_public_response() {
        let store = Arc::new(AuthLoginStore::new());
        let session = insert_pending_login(&store).await;

        let response = auth_login_poll_oauth_error_response(
            &session.login_id,
            &session,
            &store,
            MsaTokenErrorCode::SlowDown,
        )
        .await;

        assert_eq!(response.0, StatusCode::ACCEPTED);
        assert_eq!(response.1.0["status"], "pending");
        assert_eq!(response.1.0["poll_hint"], "slow_down");
        assert_eq!(response.1.0["interval"], 10);
        assert_no_sensitive_public_fields(&response.1.0);
        assert_eq!(
            store
                .get(&session.login_id)
                .await
                .expect("session")
                .interval,
            10
        );
    }

    #[tokio::test]
    async fn auth_login_poll_success_stores_tokens_server_side_only() {
        let store = Arc::new(AuthLoginStore::new());
        let session = insert_pending_login(&store).await;

        let response = auth_login_poll_success_response(
            &session.login_id,
            MsaTokenSuccessResponse {
                access_token: "msa-access-token".to_string(),
                refresh_token: Some("msa-refresh-token".to_string()),
                id_token: Some("msa-id-token".to_string()),
                token_type: "Bearer".to_string(),
                expires_in: 3600,
                scope: Some("XboxLive.signin offline_access".to_string()),
            },
            &store,
        )
        .await;

        assert_eq!(response.0, StatusCode::OK);
        assert_eq!(response.1.0["status"], "msa_authenticated");
        assert_eq!(response.1.0["token_type"], "Bearer");
        assert_eq!(response.1.0["expires_in"], 3600);
        assert_eq!(response.1.0["has_refresh_token"], true);
        assert_eq!(
            response.1.0["token_scope"],
            "XboxLive.signin offline_access"
        );
        assert_no_sensitive_public_fields(&response.1.0);
        assert_eq!(store.get(&session.login_id).await, None);

        let token = store
            .get_msa_token(&session.login_id)
            .await
            .expect("stored msa token");
        assert_eq!(token.access_token, "msa-access-token");
        assert_eq!(token.refresh_token, Some("msa-refresh-token".to_string()));
        assert_eq!(token.id_token, Some("msa-id-token".to_string()));
        assert_eq!(token.token_type, "Bearer");
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

    async fn insert_pending_login(store: &Arc<AuthLoginStore>) -> AuthLoginSession {
        store
            .insert(NewAuthLoginSession {
                device_code: "raw-device-code".to_string(),
                user_code: "ABCD-EFGH".to_string(),
                verification_uri: "https://www.microsoft.com/link".to_string(),
                expires_in: 900,
                interval: 5,
                message: Some("Use this code.".to_string()),
            })
            .await
    }

    async fn token_test_server(
        status: StatusCode,
        body: serde_json::Value,
    ) -> (String, mpsc::UnboundedReceiver<HashMap<String, String>>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let app = axum::Router::new().route(
            "/",
            post(move |Form(form): Form<HashMap<String, String>>| {
                let tx = tx.clone();
                let body = body.clone();
                async move {
                    let _ = tx.send(form);
                    (status, Json(body))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind token test server");
        let url = format!("http://{}", listener.local_addr().expect("local addr"));
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("token test server");
        });
        (url, rx)
    }

    fn assert_no_sensitive_public_fields(value: &serde_json::Value) {
        for field in ["access_token", "refresh_token", "id_token", "device_code"] {
            assert_eq!(value.get(field), None, "public JSON exposed {field}");
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
