use crate::{
    auth_chain::{
        AuthChainClient, AuthChainError, AuthChainErrorKind, AuthChainExchange, MinecraftCape,
        MinecraftProfile, MinecraftSkin,
    },
    state::{
        ActiveMinecraftAccountState, ActiveMsaTokenState, AppState, AuthLoginMinecraftAccount,
        AuthLoginMinecraftCape, AuthLoginMinecraftProfile, AuthLoginMinecraftSkin,
        AuthLoginSession, AuthLoginStore, NewAuthLoginMinecraftAccount, NewAuthLoginMsaToken,
        NewAuthLoginSession,
    },
};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::{get, post},
};
use croopor_config::{AppConfig, LAUNCH_AUTH_MODE_ONLINE, validate_username};
use croopor_minecraft::offline_uuid;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::{sync::Arc, time::Duration};

const MSA_CLIENT_ID_ENV: &str = "CROOPOR_MSA_CLIENT_ID";
const MSA_DEVICE_CODE_ENDPOINT: &str =
    "https://login.microsoftonline.com/consumers/oauth2/v2.0/devicecode";
pub(crate) const MSA_TOKEN_ENDPOINT: &str =
    "https://login.microsoftonline.com/consumers/oauth2/v2.0/token";
const MSA_DEVICE_CODE_SCOPE: &str = "XboxLive.signin offline_access";
const MSA_TOKEN_GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:device_code";
const MSA_REFRESH_TOKEN_GRANT_TYPE: &str = "refresh_token";
const MSA_DEVICE_CODE_TIMEOUT: Duration = Duration::from_secs(20);
const MSA_TOKEN_POLL_TIMEOUT: Duration = Duration::from_secs(20);
const MSA_SLOW_DOWN_INTERVAL_INCREMENT: u64 = 5;
const LOGIN_UNAVAILABLE_REASON: &str = "Microsoft sign-in is not configured in this build";
const LOGIN_AVAILABLE_REASON: &str = "Microsoft sign-in is configured";

#[derive(Debug, Serialize)]
struct AuthStatusResponse {
    launch_auth_mode: String,
    mode: &'static str,
    username: String,
    uuid: String,
    provider: &'static str,
    verified: bool,
    online_mode_ready: bool,
    skin_source: &'static str,
    msa_authenticated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    msa_provider: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    msa_token_expires_in: Option<u64>,
    msa_refresh_available: bool,
    minecraft_profile_ready: bool,
    minecraft_ownership_verified: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    minecraft_profile: Option<AuthMinecraftProfileResponse>,
    #[serde(skip_serializing_if = "Option::is_none")]
    minecraft_token_expires_in: Option<u64>,
    login_available: bool,
    login_reason: &'static str,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct AuthStatusMsaState {
    authenticated: bool,
    token_expires_in: Option<u64>,
    refresh_available: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct AuthStatusMinecraftState {
    account: Option<AuthLoginMinecraftAccount>,
    token_expires_in: Option<u64>,
}

#[cfg(test)]
impl AuthStatusMsaState {
    fn unauthenticated() -> Self {
        Self {
            authenticated: false,
            token_expires_in: None,
            refresh_available: false,
        }
    }
}

#[cfg(test)]
impl AuthStatusMinecraftState {
    fn unauthenticated() -> Self {
        Self {
            account: None,
            token_expires_in: None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Eq, PartialEq)]
pub(crate) struct AuthMinecraftProfileResponse {
    id: String,
    name: String,
    skins: Vec<AuthMinecraftSkinResponse>,
    capes: Vec<AuthMinecraftCapeResponse>,
}

#[derive(Clone, Debug, Serialize, Eq, PartialEq)]
pub(crate) struct AuthMinecraftSkinResponse {
    id: String,
    state: String,
    url: String,
    variant: String,
}

#[derive(Clone, Debug, Serialize, Eq, PartialEq)]
pub(crate) struct AuthMinecraftCapeResponse {
    id: String,
    state: String,
    url: String,
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
    minecraft_profile_ready: bool,
    minecraft_ownership_verified: bool,
    minecraft_profile: AuthMinecraftProfileResponse,
    minecraft_token_expires_in: u64,
}

#[derive(Debug, Serialize)]
struct AuthRefreshAuthenticatedResponse {
    status: &'static str,
    token_type: String,
    expires_in: u64,
    has_refresh_token: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    token_scope: Option<String>,
    minecraft_profile_ready: bool,
    minecraft_ownership_verified: bool,
    minecraft_profile: AuthMinecraftProfileResponse,
    minecraft_token_expires_in: u64,
}

#[derive(Debug, Serialize)]
struct AuthLoginMinecraftChainErrorResponse {
    error: &'static str,
    status: &'static str,
    auth_chain_error: &'static str,
}

#[derive(Debug, Serialize)]
struct AuthLogoutResponse {
    status: &'static str,
    cleared_pending_logins: usize,
    had_msa_auth: bool,
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

#[derive(Clone, Deserialize, Eq, PartialEq)]
struct MsaTokenSuccessResponse {
    access_token: String,
    refresh_token: Option<String>,
    id_token: Option<String>,
    token_type: String,
    expires_in: u64,
    scope: Option<String>,
}

impl std::fmt::Debug for MsaTokenSuccessResponse {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MsaTokenSuccessResponse")
            .field("access_token", &"[redacted]")
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "[redacted]"),
            )
            .field("id_token", &self.id_token.as_ref().map(|_| "[redacted]"))
            .field("token_type", &self.token_type)
            .field("expires_in", &self.expires_in)
            .field("scope", &self.scope)
            .finish()
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
struct MsaTokenErrorResponse {
    error: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AuthLoginConfig {
    client_id: Option<String>,
}

#[derive(Clone, Debug, Serialize, Eq, PartialEq)]
pub(crate) struct AuthRefreshSuccess {
    status: &'static str,
    token_type: String,
    expires_in: u64,
    has_refresh_token: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    token_scope: Option<String>,
    minecraft_profile_ready: bool,
    minecraft_profile: AuthMinecraftProfileResponse,
    minecraft_ownership_verified: bool,
    minecraft_token_expires_in: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AuthRefreshFailure {
    kind: AuthRefreshFailureKind,
    auth_chain_error: Option<AuthChainError>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AuthRefreshFailureKind {
    LoginUnavailable,
    MissingRefreshToken,
    RefreshRejected,
    RefreshFailed,
    MicrosoftClientBuild,
    MicrosoftRequest,
    MicrosoftUpstreamRejected,
    MicrosoftUpstreamUnavailable,
    MicrosoftParse,
    AuthChainFailed,
    StoreUnavailable,
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
    InvalidGrant,
    Other,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/auth/status", get(handle_auth_status))
        .route("/api/v1/auth/login", post(handle_auth_login))
        .route("/api/v1/auth/refresh", post(handle_auth_refresh))
        .route("/api/v1/auth/logout", post(handle_auth_logout))
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
    auth_status_for_store(
        &state.config().current(),
        AuthLoginConfig::from_env(),
        state.auth_logins(),
    )
    .await
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
    let auth_chain_client = match AuthChainClient::new() {
        Ok(client) => client,
        Err(error) => return auth_chain_error_response(error),
    };

    auth_login_poll_for_config(
        &login_id,
        AuthLoginConfig::from_env(),
        state.auth_logins(),
        MSA_TOKEN_ENDPOINT,
        &auth_chain_client,
    )
    .await
}

async fn handle_auth_refresh(
    State(state): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    let auth_chain_client = match AuthChainClient::new() {
        Ok(client) => client,
        Err(error) => return auth_chain_error_response(error),
    };

    auth_refresh_for_config(
        AuthLoginConfig::from_env(),
        state.auth_logins(),
        MSA_TOKEN_ENDPOINT,
        &auth_chain_client,
    )
    .await
}

async fn handle_auth_logout(State(state): State<AppState>) -> Json<AuthLogoutResponse> {
    Json(auth_logout(state.auth_logins()).await)
}

async fn auth_status_for_store(
    config: &AppConfig,
    login_config: AuthLoginConfig,
    login_store: &Arc<AuthLoginStore>,
) -> Result<AuthStatusResponse, (StatusCode, Json<serde_json::Value>)> {
    let token_expires_in = login_store.active_msa_auth_remaining_seconds().await;
    let refresh_available = login_store.active_msa_refresh_token().await.is_some();
    let minecraft_state = login_store
        .active_minecraft_account_state()
        .await
        .map(AuthStatusMinecraftState::from)
        .unwrap_or_else(|| AuthStatusMinecraftState {
            account: None,
            token_expires_in: None,
        });
    auth_status_from_username(
        &config.username,
        &config.launch_auth_mode,
        login_config,
        AuthStatusMsaState {
            authenticated: token_expires_in.is_some(),
            token_expires_in,
            refresh_available,
        },
        minecraft_state,
    )
}

fn auth_status_from_username(
    config_username: &str,
    launch_auth_mode: &str,
    login_config: AuthLoginConfig,
    msa_state: AuthStatusMsaState,
    minecraft_state: AuthStatusMinecraftState,
) -> Result<AuthStatusResponse, (StatusCode, Json<serde_json::Value>)> {
    let username = validate_username(config_username).map_err(|error| {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": error })),
        )
    })?;
    let minecraft_profile = minecraft_state
        .account
        .as_ref()
        .map(|account| auth_minecraft_profile_response(&account.profile));
    let online_mode_ready = launch_auth_mode == LAUNCH_AUTH_MODE_ONLINE
        && minecraft_state
            .account
            .as_ref()
            .is_some_and(minecraft_account_can_launch_online);
    let (mode, provider, verified, skin_source, username, uuid) = if online_mode_ready {
        let account = minecraft_state
            .account
            .as_ref()
            .expect("ready online mode has account");
        (
            "online",
            "microsoft",
            true,
            "minecraft_profile",
            account.profile.name.clone(),
            account.profile.id.clone(),
        )
    } else {
        let uuid = offline_uuid(&username);
        ("offline", "offline", false, "default", username, uuid)
    };

    Ok(AuthStatusResponse {
        launch_auth_mode: launch_auth_mode.to_string(),
        mode,
        uuid,
        username,
        provider,
        verified,
        online_mode_ready,
        skin_source,
        msa_authenticated: msa_state.authenticated,
        msa_provider: msa_state.authenticated.then_some("microsoft"),
        msa_token_expires_in: msa_state.token_expires_in,
        msa_refresh_available: msa_state.refresh_available,
        minecraft_profile_ready: minecraft_state.account.is_some(),
        minecraft_ownership_verified: minecraft_state
            .account
            .as_ref()
            .is_some_and(|account| account.owns_minecraft_java),
        minecraft_profile,
        minecraft_token_expires_in: minecraft_state.token_expires_in,
        login_available: login_config.is_available(),
        login_reason: login_config.reason(),
    })
}

fn minecraft_account_can_launch_online(account: &AuthLoginMinecraftAccount) -> bool {
    account.owns_minecraft_java
        && !account.access_token.trim().is_empty()
        && !account.profile.id.trim().is_empty()
        && !account.profile.name.trim().is_empty()
}

async fn auth_logout(login_store: &Arc<AuthLoginStore>) -> AuthLogoutResponse {
    let (cleared_pending_logins, had_msa_auth) = login_store.clear_all().await;
    AuthLogoutResponse {
        status: "logged_out",
        cleared_pending_logins,
        had_msa_auth,
    }
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
    auth_chain_client: &AuthChainClient,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(client_id) = config.client_id.as_deref() else {
        return auth_login_unavailable();
    };

    let Some(session) = login_store.get(login_id).await else {
        return auth_login_missing_or_expired(login_id, login_store).await;
    };

    match request_msa_token(token_endpoint, client_id, &session.device_code).await {
        Ok(response) => {
            auth_login_poll_success_response(login_id, response, login_store, auth_chain_client)
                .await
        }
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

async fn request_msa_refresh_token(
    token_endpoint: &str,
    client_id: &str,
    refresh_token: &str,
) -> Result<MsaTokenSuccessResponse, AuthLoginPollError> {
    let client = Client::builder()
        .timeout(MSA_TOKEN_POLL_TIMEOUT)
        .build()
        .map_err(|_| AuthLoginPollError::Request(AuthLoginError::ClientBuild))?;

    let response = client
        .post(token_endpoint)
        .form(&[
            ("grant_type", MSA_REFRESH_TOKEN_GRANT_TYPE),
            ("client_id", client_id),
            ("refresh_token", refresh_token),
            ("scope", MSA_DEVICE_CODE_SCOPE),
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
    auth_chain_client: &AuthChainClient,
) -> (StatusCode, Json<serde_json::Value>) {
    let minecraft_account = match auth_chain_client
        .exchange_msa_access_token(&response.access_token)
        .await
    {
        Ok(exchange) => NewAuthLoginMinecraftAccount::from(exchange),
        Err(error) => {
            let _ = login_store.remove(login_id).await;
            let _ = login_store.clear_active_auth().await;
            return auth_chain_error_response(error);
        }
    };

    let public_response = AuthLoginMsaAuthenticatedResponse {
        status: "msa_authenticated",
        login_id: login_id.to_string(),
        token_type: response.token_type.clone(),
        expires_in: response.expires_in,
        has_refresh_token: response.refresh_token.is_some(),
        token_scope: response.scope.clone(),
        minecraft_profile_ready: true,
        minecraft_ownership_verified: minecraft_account.owns_minecraft_java,
        minecraft_profile: auth_minecraft_profile_response(&minecraft_account.profile),
        minecraft_token_expires_in: minecraft_account.expires_in,
    };
    if login_store
        .complete_with_msa_and_minecraft_account(
            login_id,
            NewAuthLoginMsaToken::from(response),
            minecraft_account,
        )
        .await
        .is_none()
    {
        return auth_login_missing_or_expired(login_id, login_store).await;
    }

    (StatusCode::OK, Json(serde_json::json!(public_response)))
}

async fn auth_refresh_for_config(
    config: AuthLoginConfig,
    login_store: &Arc<AuthLoginStore>,
    token_endpoint: &str,
    auth_chain_client: &AuthChainClient,
) -> (StatusCode, Json<serde_json::Value>) {
    match refresh_active_auth_for_config(config, login_store, token_endpoint, auth_chain_client)
        .await
    {
        Ok(success) => (StatusCode::OK, Json(serde_json::json!(success))),
        Err(error) => auth_refresh_error_response(error),
    }
}

pub(crate) async fn refresh_active_auth_for_config(
    config: AuthLoginConfig,
    login_store: &Arc<AuthLoginStore>,
    token_endpoint: &str,
    auth_chain_client: &AuthChainClient,
) -> Result<AuthRefreshSuccess, AuthRefreshFailure> {
    let Some(client_id) = config.client_id.as_deref() else {
        return Err(AuthRefreshFailure::new(
            AuthRefreshFailureKind::LoginUnavailable,
        ));
    };
    let initial_generation = login_store.active_auth_generation();
    let Some(_initial_refresh_token) = login_store.active_msa_refresh_token().await else {
        return Err(AuthRefreshFailure::new(
            AuthRefreshFailureKind::MissingRefreshToken,
        ));
    };

    let _refresh_guard = login_store.active_auth_refresh_guard().await;
    if login_store.active_auth_generation() != initial_generation {
        if let Some(success) = active_auth_refresh_success_from_store(login_store).await {
            return Ok(success);
        }
    }
    let Some(refresh_token) = login_store.active_msa_refresh_token().await else {
        return Err(AuthRefreshFailure::new(
            AuthRefreshFailureKind::MissingRefreshToken,
        ));
    };

    match request_msa_refresh_token(token_endpoint, client_id, &refresh_token).await {
        Ok(response) => {
            auth_refresh_success_response(response, &refresh_token, login_store, auth_chain_client)
                .await
        }
        Err(AuthLoginPollError::OAuth(error)) => auth_refresh_oauth_error(login_store, error).await,
        Err(AuthLoginPollError::Request(error)) => Err(AuthRefreshFailure::from(error)),
    }
}

async fn active_auth_refresh_success_from_store(
    login_store: &Arc<AuthLoginStore>,
) -> Option<AuthRefreshSuccess> {
    let msa_state = login_store.active_msa_token_state().await?;
    let minecraft_state = login_store.active_minecraft_account_state().await?;
    if !minecraft_account_can_launch_online(&minecraft_state.account) {
        return None;
    }

    Some(auth_refresh_success_from_active_state(
        msa_state,
        minecraft_state,
    ))
}

fn auth_refresh_success_from_active_state(
    msa_state: ActiveMsaTokenState,
    minecraft_state: ActiveMinecraftAccountState,
) -> AuthRefreshSuccess {
    let refresh_available = msa_state
        .token
        .refresh_token
        .as_deref()
        .is_some_and(|refresh_token: &str| !refresh_token.trim().is_empty());
    AuthRefreshSuccess {
        status: "refreshed",
        token_type: msa_state.token.token_type,
        expires_in: msa_state.token_expires_in,
        has_refresh_token: refresh_available,
        token_scope: msa_state.token.scope,
        minecraft_profile_ready: true,
        minecraft_profile: auth_minecraft_profile_response(&minecraft_state.account.profile),
        minecraft_ownership_verified: minecraft_state.account.owns_minecraft_java,
        minecraft_token_expires_in: minecraft_state.token_expires_in,
    }
}

async fn auth_refresh_success_response(
    response: MsaTokenSuccessResponse,
    fallback_refresh_token: &str,
    login_store: &Arc<AuthLoginStore>,
    auth_chain_client: &AuthChainClient,
) -> Result<AuthRefreshSuccess, AuthRefreshFailure> {
    let has_refresh_token = response
        .refresh_token
        .as_deref()
        .is_some_and(|refresh_token| !refresh_token.trim().is_empty())
        || !fallback_refresh_token.trim().is_empty();
    let minecraft_account = match auth_chain_client
        .exchange_msa_access_token(&response.access_token)
        .await
    {
        Ok(exchange) => NewAuthLoginMinecraftAccount::from(exchange),
        Err(error) => return Err(AuthRefreshFailure::auth_chain(error)),
    };

    let public_response = AuthRefreshAuthenticatedResponse {
        status: "refreshed",
        token_type: response.token_type.clone(),
        expires_in: response.expires_in,
        has_refresh_token,
        token_scope: response.scope.clone(),
        minecraft_profile_ready: true,
        minecraft_ownership_verified: minecraft_account.owns_minecraft_java,
        minecraft_profile: auth_minecraft_profile_response(&minecraft_account.profile),
        minecraft_token_expires_in: minecraft_account.expires_in,
    };

    if login_store
        .refresh_with_msa_and_minecraft_account(
            NewAuthLoginMsaToken::from(response),
            minecraft_account,
            fallback_refresh_token,
        )
        .await
        .is_none()
    {
        return Err(AuthRefreshFailure::new(
            AuthRefreshFailureKind::StoreUnavailable,
        ));
    }

    Ok(AuthRefreshSuccess {
        status: public_response.status,
        token_type: public_response.token_type,
        expires_in: public_response.expires_in,
        has_refresh_token: public_response.has_refresh_token,
        token_scope: public_response.token_scope,
        minecraft_profile_ready: public_response.minecraft_profile_ready,
        minecraft_profile: public_response.minecraft_profile,
        minecraft_ownership_verified: public_response.minecraft_ownership_verified,
        minecraft_token_expires_in: public_response.minecraft_token_expires_in,
    })
}

async fn auth_refresh_oauth_error(
    login_store: &Arc<AuthLoginStore>,
    error: MsaTokenErrorCode,
) -> Result<AuthRefreshSuccess, AuthRefreshFailure> {
    if matches!(
        error,
        MsaTokenErrorCode::InvalidGrant
            | MsaTokenErrorCode::AuthorizationDeclined
            | MsaTokenErrorCode::BadVerificationCode
            | MsaTokenErrorCode::ExpiredToken
    ) {
        let _ = login_store.clear_active_auth().await;
        return Err(AuthRefreshFailure::new(
            AuthRefreshFailureKind::RefreshRejected,
        ));
    }

    Err(AuthRefreshFailure::new(
        AuthRefreshFailureKind::RefreshFailed,
    ))
}

fn auth_refresh_error_response(error: AuthRefreshFailure) -> (StatusCode, Json<serde_json::Value>) {
    match error.kind {
        AuthRefreshFailureKind::LoginUnavailable => auth_login_unavailable(),
        AuthRefreshFailureKind::MissingRefreshToken | AuthRefreshFailureKind::StoreUnavailable => {
            auth_refresh_sign_in_required_response(
                StatusCode::PRECONDITION_FAILED,
                "Microsoft sign-in refresh is unavailable; sign in again",
            )
        }
        AuthRefreshFailureKind::RefreshRejected => auth_refresh_sign_in_required_response(
            StatusCode::UNAUTHORIZED,
            "Microsoft sign-in expired; sign in again",
        ),
        AuthRefreshFailureKind::RefreshFailed => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({
                "error": "Microsoft sign-in refresh failed",
                "status": "refresh_failed",
            })),
        ),
        AuthRefreshFailureKind::MicrosoftClientBuild => {
            auth_login_error_response(AuthLoginError::ClientBuild)
        }
        AuthRefreshFailureKind::MicrosoftRequest => {
            auth_login_error_response(AuthLoginError::Request)
        }
        AuthRefreshFailureKind::MicrosoftUpstreamRejected => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({ "error": "Microsoft sign-in request was rejected" })),
        ),
        AuthRefreshFailureKind::MicrosoftUpstreamUnavailable => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({ "error": "Microsoft sign-in service is unavailable" })),
        ),
        AuthRefreshFailureKind::MicrosoftParse => auth_login_error_response(AuthLoginError::Parse),
        AuthRefreshFailureKind::AuthChainFailed => auth_chain_error_response(
            error
                .auth_chain_error
                .expect("auth-chain failure has error"),
        ),
    }
}

fn auth_refresh_sign_in_required_response(
    status: StatusCode,
    message: &'static str,
) -> (StatusCode, Json<serde_json::Value>) {
    (
        status,
        Json(serde_json::json!({
            "error": message,
            "status": "sign_in_required",
        })),
    )
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
        MsaTokenErrorCode::InvalidGrant | MsaTokenErrorCode::Other => (
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

fn auth_chain_error_response(error: AuthChainError) -> (StatusCode, Json<serde_json::Value>) {
    let status = match error.kind() {
        AuthChainErrorKind::ClientBuild => StatusCode::INTERNAL_SERVER_ERROR,
        AuthChainErrorKind::Request
        | AuthChainErrorKind::UpstreamRejected
        | AuthChainErrorKind::UpstreamUnavailable
        | AuthChainErrorKind::Parse
        | AuthChainErrorKind::MissingUserHash => StatusCode::BAD_GATEWAY,
    };

    (
        status,
        Json(serde_json::json!(AuthLoginMinecraftChainErrorResponse {
            error: "Minecraft account verification failed",
            status: "minecraft_auth_chain_failed",
            auth_chain_error: auth_chain_error_code(error.kind()),
        })),
    )
}

fn auth_chain_error_code(kind: AuthChainErrorKind) -> &'static str {
    match kind {
        AuthChainErrorKind::ClientBuild => "client_build",
        AuthChainErrorKind::Request => "request",
        AuthChainErrorKind::UpstreamRejected => "upstream_rejected",
        AuthChainErrorKind::UpstreamUnavailable => "upstream_unavailable",
        AuthChainErrorKind::Parse => "parse",
        AuthChainErrorKind::MissingUserHash => "missing_user_hash",
    }
}

fn auth_minecraft_profile_response(
    profile: &AuthLoginMinecraftProfile,
) -> AuthMinecraftProfileResponse {
    AuthMinecraftProfileResponse {
        id: profile.id.clone(),
        name: profile.name.clone(),
        skins: profile
            .skins
            .iter()
            .map(|skin| AuthMinecraftSkinResponse {
                id: skin.id.clone(),
                state: skin.state.clone(),
                url: skin.url.clone(),
                variant: skin.variant.clone(),
            })
            .collect(),
        capes: profile
            .capes
            .iter()
            .map(|cape| AuthMinecraftCapeResponse {
                id: cape.id.clone(),
                state: cape.state.clone(),
                url: cape.url.clone(),
            })
            .collect(),
    }
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

impl From<ActiveMinecraftAccountState> for AuthStatusMinecraftState {
    fn from(state: ActiveMinecraftAccountState) -> Self {
        Self {
            account: Some(state.account),
            token_expires_in: Some(state.token_expires_in),
        }
    }
}

impl From<AuthChainExchange> for NewAuthLoginMinecraftAccount {
    fn from(exchange: AuthChainExchange) -> Self {
        Self {
            access_token: exchange.minecraft.access_token().to_string(),
            token_type: exchange.minecraft.token_type,
            expires_in: exchange.minecraft.expires_in,
            profile: AuthLoginMinecraftProfile::from(exchange.profile),
            owns_minecraft_java: exchange.ownership.owns_minecraft_java,
        }
    }
}

impl From<MinecraftProfile> for AuthLoginMinecraftProfile {
    fn from(profile: MinecraftProfile) -> Self {
        Self {
            id: profile.id,
            name: profile.name,
            skins: profile
                .skins
                .into_iter()
                .map(AuthLoginMinecraftSkin::from)
                .collect(),
            capes: profile
                .capes
                .into_iter()
                .map(AuthLoginMinecraftCape::from)
                .collect(),
        }
    }
}

impl From<MinecraftSkin> for AuthLoginMinecraftSkin {
    fn from(skin: MinecraftSkin) -> Self {
        Self {
            id: skin.id,
            state: skin.state,
            url: skin.url,
            variant: skin.variant,
        }
    }
}

impl From<MinecraftCape> for AuthLoginMinecraftCape {
    fn from(cape: MinecraftCape) -> Self {
        Self {
            id: cape.id,
            state: cape.state,
            url: cape.url,
        }
    }
}

impl AuthLoginConfig {
    pub(crate) fn from_env() -> Self {
        Self::from_env_value(std::env::var(MSA_CLIENT_ID_ENV).ok().as_deref())
    }

    pub(crate) fn from_env_value(value: Option<&str>) -> Self {
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

impl AuthRefreshFailure {
    fn new(kind: AuthRefreshFailureKind) -> Self {
        Self {
            kind,
            auth_chain_error: None,
        }
    }

    fn auth_chain(error: AuthChainError) -> Self {
        Self {
            kind: AuthRefreshFailureKind::AuthChainFailed,
            auth_chain_error: Some(error),
        }
    }

    pub(crate) fn launch_status_id(&self) -> &'static str {
        match self.kind {
            AuthRefreshFailureKind::LoginUnavailable => "refresh_unavailable",
            AuthRefreshFailureKind::MissingRefreshToken
            | AuthRefreshFailureKind::RefreshRejected
            | AuthRefreshFailureKind::StoreUnavailable => "sign_in_required",
            AuthRefreshFailureKind::RefreshFailed
            | AuthRefreshFailureKind::MicrosoftClientBuild
            | AuthRefreshFailureKind::MicrosoftRequest
            | AuthRefreshFailureKind::MicrosoftUpstreamRejected
            | AuthRefreshFailureKind::MicrosoftUpstreamUnavailable
            | AuthRefreshFailureKind::MicrosoftParse
            | AuthRefreshFailureKind::AuthChainFailed => "refresh_failed",
        }
    }

    pub(crate) fn launch_reason_id(&self) -> &'static str {
        match self.kind {
            AuthRefreshFailureKind::LoginUnavailable => "client_id_missing",
            AuthRefreshFailureKind::MissingRefreshToken => "refresh_token_missing",
            AuthRefreshFailureKind::RefreshRejected => "refresh_token_rejected",
            AuthRefreshFailureKind::RefreshFailed => "oauth_refresh_failed",
            AuthRefreshFailureKind::MicrosoftClientBuild => "token_client_unavailable",
            AuthRefreshFailureKind::MicrosoftRequest => "token_endpoint_unreachable",
            AuthRefreshFailureKind::MicrosoftUpstreamRejected => "token_endpoint_rejected",
            AuthRefreshFailureKind::MicrosoftUpstreamUnavailable => "token_endpoint_unavailable",
            AuthRefreshFailureKind::MicrosoftParse => "token_endpoint_parse_failed",
            AuthRefreshFailureKind::AuthChainFailed => "auth_chain_failed",
            AuthRefreshFailureKind::StoreUnavailable => "refresh_state_unavailable",
        }
    }
}

impl From<AuthLoginError> for AuthRefreshFailure {
    fn from(error: AuthLoginError) -> Self {
        let kind = match error {
            AuthLoginError::ClientBuild => AuthRefreshFailureKind::MicrosoftClientBuild,
            AuthLoginError::Request => AuthRefreshFailureKind::MicrosoftRequest,
            AuthLoginError::UpstreamStatus(status) if status.as_u16() >= 500 => {
                AuthRefreshFailureKind::MicrosoftUpstreamUnavailable
            }
            AuthLoginError::UpstreamStatus(_) => AuthRefreshFailureKind::MicrosoftUpstreamRejected,
            AuthLoginError::Parse => AuthRefreshFailureKind::MicrosoftParse,
        };
        Self::new(kind)
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
            "invalid_grant" => Self::InvalidGrant,
            _ => Self::Other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth_chain::AuthChainEndpoints;
    use crate::state::{AppStateInit, InstallStore, SessionStore};
    use axum::{
        body::Bytes,
        extract::{Form, State},
        http::HeaderMap,
        routing::{get, post},
    };
    use croopor_config::{AppConfig, AppPaths, ConfigStore, InstanceStore};
    use croopor_performance::PerformanceManager;
    use std::{collections::HashMap, fs, path::PathBuf, sync::Arc};
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn auth_status_uses_configured_offline_identity() {
        let fixture = TestFixture::new("configured-identity", "ConfigUser");

        let response = fixture.status().await.expect("auth status").0;

        assert_eq!(response.launch_auth_mode, "offline");
        assert_eq!(response.mode, "offline");
        assert_eq!(response.username, "ConfigUser");
        assert_eq!(response.uuid, offline_uuid("ConfigUser"));
        assert_eq!(response.provider, "offline");
        assert!(!response.verified);
        assert!(!response.online_mode_ready);
        assert_eq!(response.skin_source, "default");
        assert!(!response.msa_authenticated);
        assert_eq!(response.msa_provider, None);
        assert_eq!(response.msa_token_expires_in, None);
        assert!(!response.msa_refresh_available);
        assert!(!response.minecraft_profile_ready);
        assert!(!response.minecraft_ownership_verified);
        assert_eq!(response.minecraft_profile, None);
        assert_eq!(response.minecraft_token_expires_in, None);
        assert!(!response.login_available);
        assert_eq!(response.login_reason, LOGIN_UNAVAILABLE_REASON);
    }

    #[test]
    fn auth_status_marks_login_available_when_client_id_is_configured() {
        let response = auth_status_from_username(
            "ConfigUser",
            "offline",
            AuthLoginConfig::from_env_value(Some(" public-client-id ")),
            AuthStatusMsaState::unauthenticated(),
            AuthStatusMinecraftState::unauthenticated(),
        )
        .expect("auth status");

        assert!(response.login_available);
        assert_eq!(response.login_reason, LOGIN_AVAILABLE_REASON);
    }

    #[tokio::test]
    async fn auth_status_reports_refresh_available_when_msa_refresh_token_exists() {
        let fixture = TestFixture::new("refresh-available", "ConfigUser");
        insert_active_refresh_login(fixture.state.auth_logins(), Some("msa-refresh-token")).await;

        let response = auth_status_for_store(
            &fixture.state.config().current(),
            AuthLoginConfig::from_env_value(Some(" public-client-id ")),
            fixture.state.auth_logins(),
        )
        .await
        .expect("auth status");

        assert!(response.msa_refresh_available);
        assert!(!response.msa_authenticated);
        assert!(response.login_available);
    }

    #[tokio::test]
    async fn auth_status_reports_refresh_unavailable_without_msa_refresh_token() {
        let fixture = TestFixture::new("refresh-unavailable", "ConfigUser");
        insert_active_refresh_login(fixture.state.auth_logins(), None).await;

        let response = auth_status_for_store(
            &fixture.state.config().current(),
            AuthLoginConfig::from_env_value(Some(" public-client-id ")),
            fixture.state.auth_logins(),
        )
        .await
        .expect("auth status");

        assert!(!response.msa_refresh_available);
        assert!(!response.msa_authenticated);
        assert!(response.login_available);
    }

    #[test]
    fn auth_status_rejects_invalid_configured_username() {
        let error = auth_status_from_username(
            "bad name",
            "offline",
            AuthLoginConfig::from_env_value(None),
            AuthStatusMsaState::unauthenticated(),
            AuthStatusMinecraftState::unauthenticated(),
        )
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
            &unused_auth_chain_client(),
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
            &unused_auth_chain_client(),
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
    async fn auth_login_poll_success_stores_profile_and_tokens_server_side_only() {
        let store = Arc::new(AuthLoginStore::new());
        let session = insert_pending_login(&store).await;
        let (token_endpoint, mut token_requests) = token_test_server(
            StatusCode::OK,
            serde_json::json!({
                "access_token": "msa-access-token",
                "refresh_token": "msa-refresh-token",
                "id_token": "msa-id-token",
                "token_type": "Bearer",
                "expires_in": 3600,
                "scope": "XboxLive.signin offline_access"
            }),
        )
        .await;
        let (auth_chain_client, mut auth_chain_requests) =
            auth_chain_route_test_client(AuthChainRouteServerMode::Success).await;

        let response = auth_login_poll_for_config(
            &session.login_id,
            AuthLoginConfig::from_env_value(Some("public-client-id")),
            &store,
            &token_endpoint,
            &auth_chain_client,
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
        assert_eq!(response.1.0["minecraft_profile_ready"], true);
        assert_eq!(response.1.0["minecraft_ownership_verified"], true);
        assert_eq!(
            response.1.0["minecraft_profile"]["id"],
            "4f9c7f7d0b1245d9a5c2f03a8c120001"
        );
        assert_eq!(response.1.0["minecraft_profile"]["name"], "ProfileName");
        assert_eq!(
            response.1.0["minecraft_profile"]["skins"][0]["variant"],
            "SLIM"
        );
        assert_eq!(response.1.0["minecraft_token_expires_in"], 86400);
        assert_no_sensitive_public_fields(&response.1.0);
        assert_eq!(store.get(&session.login_id).await, None);
        assert!(store.has_active_msa_auth().await);
        assert!(
            store
                .active_msa_auth_remaining_seconds()
                .await
                .is_some_and(|value| value > 0 && value <= 3600)
        );
        let minecraft = store
            .active_minecraft_account_state()
            .await
            .expect("minecraft account");
        assert_eq!(
            minecraft.account.profile.id,
            "4f9c7f7d0b1245d9a5c2f03a8c120001"
        );
        assert!(minecraft.account.owns_minecraft_java);
        assert!(minecraft.token_expires_in > 0);

        let status = auth_status_for_store(
            &AppConfig {
                username: "ConfigUser".to_string(),
                ..AppConfig::default()
            },
            AuthLoginConfig::from_env_value(Some("public-client-id")),
            &store,
        )
        .await
        .expect("auth status");
        assert_eq!(status.mode, "offline");
        assert!(!status.online_mode_ready);
        assert!(status.msa_authenticated);
        assert!(status.minecraft_profile_ready);
        assert!(status.minecraft_ownership_verified);
        assert_eq!(
            status.minecraft_profile.expect("minecraft profile").name,
            "ProfileName"
        );

        let form = token_requests.recv().await.expect("token request");
        assert_eq!(form["device_code"], "raw-device-code");
        assert_eq!(
            auth_chain_requests.recv().await.expect("xbl request").path,
            "/xbl"
        );
        assert_eq!(
            auth_chain_requests.recv().await.expect("xsts request").path,
            "/xsts"
        );
        assert_eq!(
            auth_chain_requests
                .recv()
                .await
                .expect("minecraft login request")
                .path,
            "/minecraft/login"
        );
        assert_eq!(
            auth_chain_requests
                .recv()
                .await
                .expect("minecraft profile request")
                .authorization
                .as_deref(),
            Some("Bearer minecraft-access-token")
        );
        assert_eq!(
            auth_chain_requests
                .recv()
                .await
                .expect("minecraft ownership request")
                .authorization
                .as_deref(),
            Some("Bearer minecraft-access-token")
        );
    }

    #[tokio::test]
    async fn auth_refresh_success_posts_refresh_form_and_rotates_token() {
        let store = Arc::new(AuthLoginStore::new());
        insert_active_refresh_login(&store, Some("old-msa-refresh-token")).await;
        let (token_endpoint, mut token_requests) = token_test_server(
            StatusCode::OK,
            serde_json::json!({
                "access_token": "msa-access-token",
                "refresh_token": "new-msa-refresh-token",
                "id_token": "msa-id-token",
                "token_type": "Bearer",
                "expires_in": 3600,
                "scope": "XboxLive.signin offline_access"
            }),
        )
        .await;
        let (auth_chain_client, mut auth_chain_requests) =
            auth_chain_route_test_client(AuthChainRouteServerMode::Success).await;

        let response = auth_refresh_for_config(
            AuthLoginConfig::from_env_value(Some("public-client-id")),
            &store,
            &token_endpoint,
            &auth_chain_client,
        )
        .await;

        assert_eq!(response.0, StatusCode::OK);
        assert_eq!(response.1.0["status"], "refreshed");
        assert_eq!(response.1.0["token_type"], "Bearer");
        assert_eq!(response.1.0["expires_in"], 3600);
        assert_eq!(response.1.0["has_refresh_token"], true);
        assert_eq!(
            response.1.0["token_scope"],
            "XboxLive.signin offline_access"
        );
        assert_eq!(response.1.0["minecraft_profile_ready"], true);
        assert_eq!(response.1.0["minecraft_ownership_verified"], true);
        assert_eq!(response.1.0["minecraft_profile"]["name"], "ProfileName");
        assert_eq!(response.1.0["minecraft_token_expires_in"], 86400);
        assert_no_sensitive_public_fields(&response.1.0);

        let active = store.active_msa_token().await.expect("active msa token");
        assert_eq!(active.access_token, "msa-access-token");
        assert_eq!(
            active.refresh_token,
            Some("new-msa-refresh-token".to_string())
        );
        assert!(
            store
                .active_minecraft_account_state()
                .await
                .expect("minecraft account")
                .account
                .owns_minecraft_java
        );

        let form = token_requests.recv().await.expect("token request");
        assert_eq!(form["grant_type"], MSA_REFRESH_TOKEN_GRANT_TYPE);
        assert_eq!(form["client_id"], "public-client-id");
        assert_eq!(form["refresh_token"], "old-msa-refresh-token");
        assert_eq!(form["scope"], MSA_DEVICE_CODE_SCOPE);
        assert_eq!(
            auth_chain_requests.recv().await.expect("xbl request").path,
            "/xbl"
        );
        assert_eq!(
            auth_chain_requests.recv().await.expect("xsts request").path,
            "/xsts"
        );
        assert_eq!(
            auth_chain_requests
                .recv()
                .await
                .expect("minecraft login request")
                .path,
            "/minecraft/login"
        );
    }

    #[tokio::test]
    async fn auth_refresh_success_preserves_old_refresh_token_when_omitted() {
        let store = Arc::new(AuthLoginStore::new());
        insert_active_refresh_login(&store, Some("old-msa-refresh-token")).await;
        let (token_endpoint, _token_requests) = token_test_server(
            StatusCode::OK,
            serde_json::json!({
                "access_token": "msa-access-token",
                "token_type": "Bearer",
                "expires_in": 3600,
                "scope": "XboxLive.signin offline_access"
            }),
        )
        .await;
        let (auth_chain_client, _auth_chain_requests) =
            auth_chain_route_test_client(AuthChainRouteServerMode::Success).await;

        let response = auth_refresh_for_config(
            AuthLoginConfig::from_env_value(Some("public-client-id")),
            &store,
            &token_endpoint,
            &auth_chain_client,
        )
        .await;

        assert_eq!(response.0, StatusCode::OK);
        assert_eq!(response.1.0["status"], "refreshed");
        assert_no_sensitive_public_fields(&response.1.0);
        assert_eq!(
            store
                .active_msa_token()
                .await
                .expect("active msa token")
                .refresh_token,
            Some("old-msa-refresh-token".to_string())
        );
    }

    #[tokio::test]
    async fn concurrent_auth_refresh_requests_reuse_single_rotated_refresh() {
        let store = Arc::new(AuthLoginStore::new());
        insert_active_refresh_login(&store, Some("old-msa-refresh-token")).await;
        let (token_endpoint, mut token_requests) = delayed_token_test_server(
            StatusCode::OK,
            serde_json::json!({
                "access_token": "msa-access-token",
                "refresh_token": "new-msa-refresh-token",
                "token_type": "Bearer",
                "expires_in": 3600,
                "scope": "XboxLive.signin offline_access"
            }),
            std::time::Duration::from_millis(100),
        )
        .await;
        let (auth_chain_client, mut auth_chain_requests) =
            auth_chain_route_test_client(AuthChainRouteServerMode::Success).await;

        let first = auth_refresh_for_config(
            AuthLoginConfig::from_env_value(Some("public-client-id")),
            &store,
            &token_endpoint,
            &auth_chain_client,
        );
        let second = auth_refresh_for_config(
            AuthLoginConfig::from_env_value(Some("public-client-id")),
            &store,
            &token_endpoint,
            &auth_chain_client,
        );
        let (first, second) = tokio::join!(first, second);

        assert_eq!(first.0, StatusCode::OK);
        assert_eq!(second.0, StatusCode::OK);
        assert_eq!(first.1.0["status"], "refreshed");
        assert_eq!(second.1.0["status"], "refreshed");
        assert_no_sensitive_public_fields(&first.1.0);
        assert_no_sensitive_public_fields(&second.1.0);

        let form = token_requests.recv().await.expect("token request");
        assert_eq!(form["grant_type"], MSA_REFRESH_TOKEN_GRANT_TYPE);
        assert_eq!(form["refresh_token"], "old-msa-refresh-token");
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), token_requests.recv())
                .await
                .is_err()
        );
        assert_eq!(
            store
                .active_msa_token()
                .await
                .expect("active msa token")
                .refresh_token,
            Some("new-msa-refresh-token".to_string())
        );

        let mut paths = Vec::new();
        for _ in 0..5 {
            paths.push(
                auth_chain_requests
                    .recv()
                    .await
                    .expect("auth-chain request")
                    .path,
            );
        }
        assert_eq!(
            paths,
            vec![
                "/xbl",
                "/xsts",
                "/minecraft/login",
                "/minecraft/profile",
                "/minecraft/ownership",
            ]
        );
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(100),
                auth_chain_requests.recv()
            )
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn auth_refresh_missing_refresh_token_returns_bounded_precondition() {
        let store = Arc::new(AuthLoginStore::new());
        insert_active_refresh_login(&store, None).await;

        let response = auth_refresh_for_config(
            AuthLoginConfig::from_env_value(Some("public-client-id")),
            &store,
            "unused",
            &unused_auth_chain_client(),
        )
        .await;

        assert_eq!(response.0, StatusCode::PRECONDITION_FAILED);
        assert_eq!(response.1.0["status"], "sign_in_required");
        assert_eq!(
            response.1.0["error"],
            "Microsoft sign-in refresh is unavailable; sign in again"
        );
        assert_no_sensitive_public_fields(&response.1.0);
        assert!(store.active_msa_token().await.is_some());
    }

    #[tokio::test]
    async fn auth_refresh_invalid_grant_clears_active_auth() {
        let store = Arc::new(AuthLoginStore::new());
        insert_active_refresh_login(&store, Some("old-msa-refresh-token")).await;
        let (token_endpoint, mut token_requests) = token_test_server(
            StatusCode::BAD_REQUEST,
            serde_json::json!({
                "error": "invalid_grant",
                "error_description": "provider-secret-payload"
            }),
        )
        .await;

        let response = auth_refresh_for_config(
            AuthLoginConfig::from_env_value(Some("public-client-id")),
            &store,
            &token_endpoint,
            &unused_auth_chain_client(),
        )
        .await;

        assert_eq!(response.0, StatusCode::UNAUTHORIZED);
        assert_eq!(response.1.0["status"], "sign_in_required");
        assert_eq!(
            response.1.0["error"],
            "Microsoft sign-in expired; sign in again"
        );
        assert_no_sensitive_public_fields(&response.1.0);
        assert_eq!(store.active_msa_token().await, None);
        assert_eq!(store.active_minecraft_account().await, None);

        let form = token_requests.recv().await.expect("token request");
        assert_eq!(form["grant_type"], MSA_REFRESH_TOKEN_GRANT_TYPE);
        assert_eq!(form["refresh_token"], "old-msa-refresh-token");
    }

    #[tokio::test]
    async fn auth_refresh_auth_chain_failure_preserves_existing_auth() {
        let store = Arc::new(AuthLoginStore::new());
        insert_active_refresh_login(&store, Some("old-msa-refresh-token")).await;
        let (token_endpoint, _token_requests) = token_test_server(
            StatusCode::OK,
            serde_json::json!({
                "access_token": "msa-access-token",
                "refresh_token": "new-msa-refresh-token",
                "token_type": "Bearer",
                "expires_in": 3600,
                "scope": "XboxLive.signin offline_access"
            }),
        )
        .await;
        let (auth_chain_client, _auth_chain_requests) =
            auth_chain_route_test_client(AuthChainRouteServerMode::XstsRejected).await;

        let response = auth_refresh_for_config(
            AuthLoginConfig::from_env_value(Some("public-client-id")),
            &store,
            &token_endpoint,
            &auth_chain_client,
        )
        .await;

        assert_eq!(response.0, StatusCode::BAD_GATEWAY);
        assert_eq!(response.1.0["status"], "minecraft_auth_chain_failed");
        assert_no_sensitive_public_fields(&response.1.0);
        assert_eq!(
            store
                .active_msa_token()
                .await
                .expect("active msa token")
                .refresh_token,
            Some("old-msa-refresh-token".to_string())
        );
        assert!(store.active_minecraft_account().await.is_some());
    }

    #[tokio::test]
    async fn auth_login_poll_auth_chain_failure_does_not_leave_active_profile_state() {
        let store = Arc::new(AuthLoginStore::new());
        let old_session = insert_pending_login(&store).await;
        store
            .complete_with_msa_and_minecraft_account(
                &old_session.login_id,
                NewAuthLoginMsaToken {
                    access_token: "old-msa-access-token".to_string(),
                    refresh_token: Some("old-msa-refresh-token".to_string()),
                    id_token: None,
                    token_type: "Bearer".to_string(),
                    expires_in: 3600,
                    scope: Some("XboxLive.signin offline_access".to_string()),
                },
                test_minecraft_account("OldProfileName"),
            )
            .await
            .expect("old active auth");
        let session = insert_pending_login(&store).await;
        let (token_endpoint, _token_requests) = token_test_server(
            StatusCode::OK,
            serde_json::json!({
                "access_token": "msa-access-token",
                "refresh_token": "msa-refresh-token",
                "token_type": "Bearer",
                "expires_in": 3600,
                "scope": "XboxLive.signin offline_access"
            }),
        )
        .await;
        let (auth_chain_client, mut auth_chain_requests) =
            auth_chain_route_test_client(AuthChainRouteServerMode::XstsRejected).await;

        let response = auth_login_poll_for_config(
            &session.login_id,
            AuthLoginConfig::from_env_value(Some("public-client-id")),
            &store,
            &token_endpoint,
            &auth_chain_client,
        )
        .await;

        assert_eq!(response.0, StatusCode::BAD_GATEWAY);
        assert_eq!(response.1.0["status"], "minecraft_auth_chain_failed");
        assert_eq!(
            response.1.0["error"],
            "Minecraft account verification failed"
        );
        assert_eq!(response.1.0["auth_chain_error"], "upstream_rejected");
        assert_no_sensitive_public_fields(&response.1.0);
        assert_eq!(store.get(&session.login_id).await, None);
        assert_eq!(store.get(&old_session.login_id).await, None);
        assert!(!store.has_active_msa_auth().await);
        assert_eq!(store.active_minecraft_account().await, None);

        let status = auth_status_for_store(
            &AppConfig {
                username: "ConfigUser".to_string(),
                ..AppConfig::default()
            },
            AuthLoginConfig::from_env_value(Some("public-client-id")),
            &store,
        )
        .await
        .expect("auth status");
        assert!(!status.msa_authenticated);
        assert!(!status.minecraft_profile_ready);
        assert!(!status.minecraft_ownership_verified);
        assert_eq!(status.minecraft_profile, None);

        assert_eq!(
            auth_chain_requests.recv().await.expect("xbl request").path,
            "/xbl"
        );
        assert_eq!(
            auth_chain_requests.recv().await.expect("xsts request").path,
            "/xsts"
        );
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(100),
                auth_chain_requests.recv()
            )
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn auth_status_reports_volatile_msa_auth_without_online_identity_claims() {
        let store = Arc::new(AuthLoginStore::new());
        let session = insert_pending_login(&store).await;
        store
            .complete_with_msa_token(
                &session.login_id,
                NewAuthLoginMsaToken {
                    access_token: "msa-access-token".to_string(),
                    refresh_token: Some("msa-refresh-token".to_string()),
                    id_token: Some("msa-id-token".to_string()),
                    token_type: "Bearer".to_string(),
                    expires_in: 3600,
                    scope: Some("XboxLive.signin offline_access".to_string()),
                },
            )
            .await
            .expect("complete login");

        let response = auth_status_for_store(
            &AppConfig {
                username: "ConfigUser".to_string(),
                ..AppConfig::default()
            },
            AuthLoginConfig::from_env_value(Some("public-client-id")),
            &store,
        )
        .await
        .expect("auth status");

        assert_eq!(response.mode, "offline");
        assert_eq!(response.username, "ConfigUser");
        assert_eq!(response.uuid, offline_uuid("ConfigUser"));
        assert_eq!(response.provider, "offline");
        assert!(!response.verified);
        assert!(!response.online_mode_ready);
        assert_eq!(response.skin_source, "default");
        assert!(response.msa_authenticated);
        assert_eq!(response.msa_provider, Some("microsoft"));
        assert!(
            response
                .msa_token_expires_in
                .is_some_and(|value| value > 0 && value <= 3600)
        );
        assert!(!response.minecraft_profile_ready);
        assert!(!response.minecraft_ownership_verified);
        assert_eq!(response.minecraft_profile, None);
        assert_eq!(response.minecraft_token_expires_in, None);
        assert!(response.login_available);
    }

    #[tokio::test]
    async fn auth_status_reports_selected_online_mode_without_ready_credentials() {
        let store = Arc::new(AuthLoginStore::new());

        let response = auth_status_for_store(
            &AppConfig {
                username: "ConfigUser".to_string(),
                launch_auth_mode: "online".to_string(),
                ..AppConfig::default()
            },
            AuthLoginConfig::from_env_value(Some("public-client-id")),
            &store,
        )
        .await
        .expect("auth status");

        assert_eq!(response.launch_auth_mode, "online");
        assert_eq!(response.mode, "offline");
        assert_eq!(response.username, "ConfigUser");
        assert_eq!(response.uuid, offline_uuid("ConfigUser"));
        assert_eq!(response.provider, "offline");
        assert!(!response.verified);
        assert!(!response.online_mode_ready);
        assert!(!response.minecraft_profile_ready);
        assert!(!response.minecraft_ownership_verified);
    }

    #[tokio::test]
    async fn auth_status_marks_online_mode_ready_for_owned_volatile_minecraft_account() {
        let store = Arc::new(AuthLoginStore::new());
        let session = insert_pending_login(&store).await;
        store
            .complete_with_msa_and_minecraft_account(
                &session.login_id,
                NewAuthLoginMsaToken {
                    access_token: "msa-access-token".to_string(),
                    refresh_token: None,
                    id_token: None,
                    token_type: "Bearer".to_string(),
                    expires_in: 3600,
                    scope: Some("XboxLive.signin offline_access".to_string()),
                },
                test_minecraft_account("ProfileName"),
            )
            .await
            .expect("complete login");

        let response = auth_status_for_store(
            &AppConfig {
                username: "ConfigUser".to_string(),
                launch_auth_mode: "online".to_string(),
                ..AppConfig::default()
            },
            AuthLoginConfig::from_env_value(Some("public-client-id")),
            &store,
        )
        .await
        .expect("auth status");

        assert_eq!(response.launch_auth_mode, "online");
        assert_eq!(response.mode, "online");
        assert_eq!(response.username, "ProfileName");
        assert_eq!(response.uuid, "old-minecraft-profile-id");
        assert_eq!(response.provider, "microsoft");
        assert!(response.verified);
        assert!(response.online_mode_ready);
        assert!(response.minecraft_profile_ready);
        assert!(response.minecraft_ownership_verified);
        assert_eq!(
            response.minecraft_profile.expect("minecraft profile").name,
            "ProfileName"
        );
    }

    #[tokio::test]
    async fn auth_status_keeps_online_mode_not_ready_without_java_ownership() {
        let store = Arc::new(AuthLoginStore::new());
        let session = insert_pending_login(&store).await;
        let mut account = test_minecraft_account("ProfileName");
        account.owns_minecraft_java = false;
        store
            .complete_with_msa_and_minecraft_account(
                &session.login_id,
                NewAuthLoginMsaToken {
                    access_token: "msa-access-token".to_string(),
                    refresh_token: None,
                    id_token: None,
                    token_type: "Bearer".to_string(),
                    expires_in: 3600,
                    scope: Some("XboxLive.signin offline_access".to_string()),
                },
                account,
            )
            .await
            .expect("complete login");

        let response = auth_status_for_store(
            &AppConfig {
                username: "ConfigUser".to_string(),
                launch_auth_mode: "online".to_string(),
                ..AppConfig::default()
            },
            AuthLoginConfig::from_env_value(Some("public-client-id")),
            &store,
        )
        .await
        .expect("auth status");

        assert_eq!(response.launch_auth_mode, "online");
        assert_eq!(response.mode, "offline");
        assert_eq!(response.username, "ConfigUser");
        assert!(!response.online_mode_ready);
        assert!(response.minecraft_profile_ready);
        assert!(!response.minecraft_ownership_verified);
    }

    #[tokio::test]
    async fn auth_logout_clears_pending_sessions_and_active_msa_auth() {
        let store = Arc::new(AuthLoginStore::new());
        let session = insert_pending_login(&store).await;
        store
            .complete_with_msa_token(
                &session.login_id,
                NewAuthLoginMsaToken {
                    access_token: "msa-access-token".to_string(),
                    refresh_token: Some("msa-refresh-token".to_string()),
                    id_token: Some("msa-id-token".to_string()),
                    token_type: "Bearer".to_string(),
                    expires_in: 3600,
                    scope: None,
                },
            )
            .await
            .expect("complete login");
        let pending = insert_pending_login(&store).await;

        let response = auth_logout(&store).await;

        assert_eq!(response.status, "logged_out");
        assert_eq!(response.cleared_pending_logins, 1);
        assert!(response.had_msa_auth);
        assert_eq!(store.get(&pending.login_id).await, None);
        assert!(!store.has_active_msa_auth().await);

        let second_response = auth_logout(&store).await;
        assert_eq!(second_response.status, "logged_out");
        assert_eq!(second_response.cleared_pending_logins, 0);
        assert!(!second_response.had_msa_auth);
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
                &self.state.config().current().launch_auth_mode,
                AuthLoginConfig::from_env_value(None),
                AuthStatusMsaState::unauthenticated(),
                AuthStatusMinecraftState::unauthenticated(),
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

    async fn insert_active_refresh_login(
        store: &Arc<AuthLoginStore>,
        refresh_token: Option<&str>,
    ) -> AuthLoginSession {
        let session = insert_pending_login(store).await;
        store
            .complete_with_msa_and_minecraft_account(
                &session.login_id,
                NewAuthLoginMsaToken {
                    access_token: "old-msa-access-token".to_string(),
                    refresh_token: refresh_token.map(ToOwned::to_owned),
                    id_token: Some("old-msa-id-token".to_string()),
                    token_type: "Bearer".to_string(),
                    expires_in: 0,
                    scope: Some("XboxLive.signin offline_access".to_string()),
                },
                test_minecraft_account("OldProfileName"),
            )
            .await
            .expect("active refresh login");
        session
    }

    fn test_minecraft_account(profile_name: &str) -> NewAuthLoginMinecraftAccount {
        NewAuthLoginMinecraftAccount {
            access_token: "old-minecraft-access-token".to_string(),
            token_type: Some("Bearer".to_string()),
            expires_in: 86400,
            profile: AuthLoginMinecraftProfile {
                id: "old-minecraft-profile-id".to_string(),
                name: profile_name.to_string(),
                skins: vec![],
                capes: vec![],
            },
            owns_minecraft_java: true,
        }
    }

    async fn token_test_server(
        status: StatusCode,
        body: serde_json::Value,
    ) -> (String, mpsc::UnboundedReceiver<HashMap<String, String>>) {
        delayed_token_test_server(status, body, std::time::Duration::ZERO).await
    }

    async fn delayed_token_test_server(
        status: StatusCode,
        body: serde_json::Value,
        delay: std::time::Duration,
    ) -> (String, mpsc::UnboundedReceiver<HashMap<String, String>>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let app = axum::Router::new().route(
            "/",
            post(move |Form(form): Form<HashMap<String, String>>| {
                let tx = tx.clone();
                let body = body.clone();
                async move {
                    let _ = tx.send(form);
                    tokio::time::sleep(delay).await;
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

    fn unused_auth_chain_client() -> AuthChainClient {
        AuthChainClient::with_endpoints(AuthChainEndpoints {
            xbox_user_authenticate: "http://127.0.0.1:9/xbl".to_string(),
            xsts_authorize: "http://127.0.0.1:9/xsts".to_string(),
            minecraft_login_with_xbox: "http://127.0.0.1:9/minecraft/login".to_string(),
            minecraft_profile: "http://127.0.0.1:9/minecraft/profile".to_string(),
            minecraft_ownership: "http://127.0.0.1:9/minecraft/ownership".to_string(),
        })
        .expect("unused auth chain client")
    }

    async fn auth_chain_route_test_client(
        mode: AuthChainRouteServerMode,
    ) -> (
        AuthChainClient,
        mpsc::UnboundedReceiver<RecordedAuthChainRequest>,
    ) {
        let (tx, rx) = mpsc::unbounded_channel();
        let app = axum::Router::new()
            .route("/xbl", post(record_route_xbl))
            .route("/xsts", post(record_route_xsts))
            .route("/minecraft/login", post(record_route_minecraft_login))
            .route("/minecraft/profile", get(record_route_minecraft_profile))
            .route(
                "/minecraft/ownership",
                get(record_route_minecraft_ownership),
            )
            .with_state(AuthChainRouteState { tx, mode });
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
            xbox_user_authenticate: format!("{base_url}/xbl"),
            xsts_authorize: format!("{base_url}/xsts"),
            minecraft_login_with_xbox: format!("{base_url}/minecraft/login"),
            minecraft_profile: format!("{base_url}/minecraft/profile"),
            minecraft_ownership: format!("{base_url}/minecraft/ownership"),
        })
        .expect("auth chain route test client");

        (client, rx)
    }

    #[derive(Clone, Copy)]
    enum AuthChainRouteServerMode {
        Success,
        XstsRejected,
    }

    #[derive(Clone)]
    struct AuthChainRouteState {
        tx: mpsc::UnboundedSender<RecordedAuthChainRequest>,
        mode: AuthChainRouteServerMode,
    }

    #[derive(Debug, Eq, PartialEq)]
    struct RecordedAuthChainRequest {
        path: String,
        authorization: Option<String>,
    }

    async fn record_route_xbl(
        State(state): State<AuthChainRouteState>,
        headers: HeaderMap,
        body: Bytes,
    ) -> (StatusCode, Json<serde_json::Value>) {
        record_auth_chain_route_request(&state.tx, "/xbl", &headers, &body);

        (
            StatusCode::OK,
            Json(serde_json::json!({
                "Token": "xbl-token",
                "DisplayClaims": {
                    "xui": [{ "uhs": "xbl-user-hash" }]
                },
            })),
        )
    }

    async fn record_route_xsts(
        State(state): State<AuthChainRouteState>,
        headers: HeaderMap,
        body: Bytes,
    ) -> (StatusCode, Json<serde_json::Value>) {
        record_auth_chain_route_request(&state.tx, "/xsts", &headers, &body);

        if matches!(state.mode, AuthChainRouteServerMode::XstsRejected) {
            return (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({
                    "error": "provider-secret-payload",
                    "Token": "xsts-token"
                })),
            );
        }

        (
            StatusCode::OK,
            Json(serde_json::json!({
                "Token": "xsts-token",
                "DisplayClaims": {
                    "xui": [{ "uhs": "xsts-user-hash" }]
                },
            })),
        )
    }

    async fn record_route_minecraft_login(
        State(state): State<AuthChainRouteState>,
        headers: HeaderMap,
        body: Bytes,
    ) -> (StatusCode, Json<serde_json::Value>) {
        record_auth_chain_route_request(&state.tx, "/minecraft/login", &headers, &body);

        (
            StatusCode::OK,
            Json(serde_json::json!({
                "access_token": "minecraft-access-token",
                "expires_in": 86400,
                "token_type": "Bearer"
            })),
        )
    }

    async fn record_route_minecraft_profile(
        State(state): State<AuthChainRouteState>,
        headers: HeaderMap,
    ) -> (StatusCode, Json<serde_json::Value>) {
        record_auth_chain_route_request(&state.tx, "/minecraft/profile", &headers, &Bytes::new());

        (
            StatusCode::OK,
            Json(serde_json::json!({
                "id": "4f9c7f7d0b1245d9a5c2f03a8c120001",
                "name": "ProfileName",
                "skins": [{
                    "id": "skin-id",
                    "state": "ACTIVE",
                    "url": "https://textures.minecraft.net/texture/skin",
                    "variant": "SLIM"
                }],
                "capes": [{
                    "id": "cape-id",
                    "state": "INACTIVE",
                    "url": "https://textures.minecraft.net/texture/cape"
                }]
            })),
        )
    }

    async fn record_route_minecraft_ownership(
        State(state): State<AuthChainRouteState>,
        headers: HeaderMap,
    ) -> (StatusCode, Json<serde_json::Value>) {
        record_auth_chain_route_request(&state.tx, "/minecraft/ownership", &headers, &Bytes::new());

        (
            StatusCode::OK,
            Json(serde_json::json!({
                "items": [{ "name": "game_minecraft" }]
            })),
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
            authorization: header_value(headers, "authorization"),
        })
        .expect("record auth chain route request");
    }

    fn header_value(headers: &HeaderMap, name: &str) -> Option<String> {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(ToOwned::to_owned)
    }

    fn assert_no_sensitive_public_fields(value: &serde_json::Value) {
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
            "new-msa-refresh-token",
            "new-msa-access-token",
            "old-minecraft-access-token",
            "xbl-token",
            "xsts-token",
            "raw-device-code",
            "provider-secret-payload",
        ] {
            assert!(
                !text.contains(material),
                "public JSON exposed sensitive material {material}"
            );
        }
    }

    fn assert_no_sensitive_public_field_keys(value: &serde_json::Value) {
        match value {
            serde_json::Value::Object(map) => {
                for (key, value) in map {
                    assert!(
                        !matches!(
                            key.as_str(),
                            "access_token" | "refresh_token" | "id_token" | "device_code"
                        ),
                        "public JSON exposed {key}"
                    );
                    assert_no_sensitive_public_field_keys(value);
                }
            }
            serde_json::Value::Array(values) => {
                for value in values {
                    assert_no_sensitive_public_field_keys(value);
                }
            }
            _ => {}
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
