use crate::{
    auth_chain::{
        AuthChainClient, AuthChainError, AuthChainErrorKind, MinecraftCape, MinecraftProfile,
        MinecraftSkin,
    },
    microsoft_auth::{MicrosoftAuthError, MicrosoftAuthErrorKind, MicrosoftAuthStep},
    routes::accounts,
    state::{
        ActiveMinecraftAccountState, ActiveMsaTokenState, AppState, AuthLoginAccountState,
        AuthLoginMinecraftAccount, AuthLoginMinecraftCape, AuthLoginMinecraftProfile,
        AuthLoginMinecraftSkin, AuthLoginStore,
    },
};
use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    routing::{get, post},
};
use croopor_config::{
    AppConfig, LAUNCH_AUTH_MODE_OFFLINE, LAUNCH_AUTH_MODE_ONLINE, validate_username,
};
use croopor_minecraft::offline_uuid;
use serde::Serialize;
use std::sync::Arc;

const LOGIN_UNAVAILABLE_REASON: &str = "Microsoft sign-in is available in the desktop app";

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
    accounts: Vec<AuthAccountResponse>,
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

#[derive(Clone, Debug, Serialize, Eq, PartialEq)]
pub(crate) struct AuthAccountResponse {
    login_id: String,
    active: bool,
    msa_authenticated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    msa_token_expires_in: Option<u64>,
    msa_refresh_available: bool,
    minecraft_profile_ready: bool,
    minecraft_ownership_verified: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    minecraft_profile: Option<AuthMinecraftProfileResponse>,
    #[serde(skip_serializing_if = "Option::is_none")]
    minecraft_token_expires_in: Option<u64>,
}

#[derive(Debug, Serialize)]
struct AuthProfileSyncResponse {
    status: &'static str,
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
    had_msa_auth: bool,
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
    MissingRefreshToken,
    RefreshRejected,
    MicrosoftClientBuild,
    MicrosoftRequest,
    MicrosoftUpstreamRejected,
    MicrosoftUpstreamUnavailable,
    MicrosoftParse,
    AuthChainFailed,
    StoreUnavailable,
}

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
    auth_status_for_store(&state.config().current(), state.auth_logins())
        .await
        .map(Json)
}

async fn handle_auth_refresh(
    State(state): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    let response = auth_refresh(state.auth_logins()).await;
    if response.0 == StatusCode::OK {
        if let Some(active) = state
            .auth_logins()
            .active_current_minecraft_account_state()
            .await
        {
            if let Ok(account) = accounts::upsert_microsoft_account(&state, &active.account) {
                if let Err(error) = accounts::sync_config_for_account(&state, &account) {
                    tracing::warn!("account config sync after auth refresh failed: {error}");
                }
            } else {
                tracing::warn!("account store sync after auth refresh failed");
            }
        }
    }
    response
}

async fn handle_auth_profile_sync(
    State(state): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    let auth_chain_client = match AuthChainClient::new() {
        Ok(client) => client,
        Err(error) => return auth_chain_error_response(error),
    };

    let response = auth_profile_sync(state.auth_logins(), &auth_chain_client).await;
    if response.0 == StatusCode::OK {
        if let Some(active) = state
            .auth_logins()
            .active_current_minecraft_account_state()
            .await
        {
            if let Ok(account) = accounts::upsert_microsoft_account(&state, &active.account) {
                if let Err(error) = accounts::sync_config_for_account(&state, &account) {
                    tracing::warn!("account config sync after profile sync failed: {error}");
                }
            } else {
                tracing::warn!("account store sync after profile sync failed");
            }
        }
    }
    response
}

async fn handle_auth_logout(
    State(state): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    auth_logout_for_state(&state).await
}

async fn active_minecraft_login_id(login_store: &Arc<AuthLoginStore>) -> Option<String> {
    login_store.active_minecraft_login_id().await
}

async fn auth_status_for_store(
    config: &AppConfig,
    login_store: &Arc<AuthLoginStore>,
) -> Result<AuthStatusResponse, (StatusCode, Json<serde_json::Value>)> {
    let token_expires_in = login_store.active_msa_auth_remaining_seconds().await;
    let refresh_available = login_store.active_msa_refresh_token().await.is_some();
    let minecraft_state = login_store
        .active_current_minecraft_account_state()
        .await
        .map(AuthStatusMinecraftState::from)
        .unwrap_or_else(|| AuthStatusMinecraftState {
            account: None,
            token_expires_in: None,
        });
    let accounts = login_store
        .account_states()
        .await
        .into_iter()
        .map(auth_account_response)
        .collect();
    auth_status_from_username(
        &config.username,
        &config.launch_auth_mode,
        AuthStatusMsaState {
            authenticated: token_expires_in.is_some(),
            token_expires_in,
            refresh_available,
        },
        minecraft_state,
        accounts,
    )
}

fn auth_status_from_username(
    config_username: &str,
    launch_auth_mode: &str,
    msa_state: AuthStatusMsaState,
    minecraft_state: AuthStatusMinecraftState,
    accounts: Vec<AuthAccountResponse>,
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
        accounts,
        login_available: false,
        login_reason: LOGIN_UNAVAILABLE_REASON,
    })
}

fn auth_account_response(state: AuthLoginAccountState) -> AuthAccountResponse {
    let minecraft_profile = state
        .minecraft_account
        .as_ref()
        .map(|account| auth_minecraft_profile_response(&account.profile));
    AuthAccountResponse {
        login_id: state.login_id,
        active: state.active,
        msa_authenticated: state.msa_authenticated,
        msa_token_expires_in: state.msa_token_expires_in,
        msa_refresh_available: state.msa_refresh_available,
        minecraft_profile_ready: state.minecraft_account.is_some(),
        minecraft_ownership_verified: state
            .minecraft_account
            .as_ref()
            .is_some_and(|account| account.owns_minecraft_java),
        minecraft_profile,
        minecraft_token_expires_in: state.minecraft_token_expires_in,
    }
}

fn minecraft_account_can_launch_online(account: &AuthLoginMinecraftAccount) -> bool {
    account.owns_minecraft_java
        && !account.access_token.trim().is_empty()
        && !account.profile.id.trim().is_empty()
        && !account.profile.name.trim().is_empty()
}

async fn auth_logout(login_store: &Arc<AuthLoginStore>) -> (StatusCode, Json<serde_json::Value>) {
    let active_login_id = active_minecraft_login_id(login_store).await;
    match login_store.clear_all().await {
        Ok(had_msa_auth) => {
            if let Some(login_id) = active_login_id {
                super::skin::clear_pending_saved_skin_apply_for_login_id(&login_id).await;
            }
            (
                StatusCode::OK,
                Json(serde_json::json!(AuthLogoutResponse {
                    status: "logged_out",
                    had_msa_auth,
                })),
            )
        }
        Err(_) => auth_clear_failed_response(),
    }
}

async fn auth_logout_for_state(state: &AppState) -> (StatusCode, Json<serde_json::Value>) {
    if let Err(error) = state.accounts().remove_all_microsoft_accounts() {
        tracing::warn!("account store cleanup before auth logout failed: {error}");
        return auth_logout_cleanup_failed_response();
    }

    let response = auth_logout(state.auth_logins()).await;
    if response.0 != StatusCode::OK {
        return response;
    }

    let mut next = state.config().current();
    next.launch_auth_mode = LAUNCH_AUTH_MODE_OFFLINE.to_string();
    match state.config().update(next) {
        Ok(config) => state.set_library_dir(config.library_dir),
        Err(error) => {
            tracing::warn!("config sync after auth logout failed: {error}");
            return auth_logout_cleanup_failed_response();
        }
    }

    response
}

async fn auth_refresh(login_store: &Arc<AuthLoginStore>) -> (StatusCode, Json<serde_json::Value>) {
    match refresh_active_auth(login_store).await {
        Ok(success) => (StatusCode::OK, Json(serde_json::json!(success))),
        Err(error) => auth_refresh_error_response(error),
    }
}

pub(crate) async fn refresh_active_auth(
    login_store: &Arc<AuthLoginStore>,
) -> Result<AuthRefreshSuccess, AuthRefreshFailure> {
    if let Some(success) = active_auth_refresh_success_from_store(login_store).await {
        return Ok(success);
    }

    match crate::microsoft_auth::refresh_login(login_store).await {
        Ok(_) => active_auth_refresh_success_from_store(login_store)
            .await
            .ok_or_else(|| AuthRefreshFailure::new(AuthRefreshFailureKind::StoreUnavailable)),
        Err(error) => Err(AuthRefreshFailure::from(error)),
    }
}

async fn active_auth_refresh_success_from_store(
    login_store: &Arc<AuthLoginStore>,
) -> Option<AuthRefreshSuccess> {
    let msa_state = login_store.active_msa_token_state().await?;
    let minecraft_state = login_store.active_current_minecraft_account_state().await?;
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

async fn auth_profile_sync(
    login_store: &Arc<AuthLoginStore>,
    auth_chain_client: &AuthChainClient,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(active) = login_store.active_current_minecraft_account_state().await else {
        return auth_profile_sync_account_required_response();
    };
    if active.account.access_token.trim().is_empty() {
        return auth_profile_sync_account_required_response();
    }

    let profile = match auth_chain_client
        .minecraft_profile(&active.account.access_token)
        .await
    {
        Ok(profile) => AuthLoginMinecraftProfile::from(profile),
        Err(error) => return auth_chain_error_response(error),
    };
    let ownership = match auth_chain_client
        .minecraft_ownership(&active.account.access_token)
        .await
    {
        Ok(ownership) => ownership,
        Err(error) => return auth_chain_error_response(error),
    };

    let Some(updated) = login_store
        .update_active_current_minecraft_profile_and_ownership(
            &active.account.login_id,
            profile,
            Some(ownership.owns_minecraft_java),
        )
        .await
    else {
        return auth_profile_sync_account_required_response();
    };

    (
        StatusCode::OK,
        Json(serde_json::json!(AuthProfileSyncResponse {
            status: "profile_synced",
            minecraft_profile_ready: true,
            minecraft_ownership_verified: updated.account.owns_minecraft_java,
            minecraft_profile: auth_minecraft_profile_response(&updated.account.profile),
            minecraft_token_expires_in: updated.token_expires_in,
        })),
    )
}

fn auth_refresh_error_response(error: AuthRefreshFailure) -> (StatusCode, Json<serde_json::Value>) {
    match error.kind {
        AuthRefreshFailureKind::MissingRefreshToken => auth_refresh_sign_in_required_response(
            StatusCode::PRECONDITION_FAILED,
            "Microsoft sign-in refresh is unavailable; sign in again",
        ),
        AuthRefreshFailureKind::StoreUnavailable => auth_clear_failed_response(),
        AuthRefreshFailureKind::RefreshRejected => auth_refresh_sign_in_required_response(
            StatusCode::UNAUTHORIZED,
            "Microsoft sign-in expired; sign in again",
        ),
        AuthRefreshFailureKind::MicrosoftClientBuild => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "error": "Could not start Microsoft sign-in. Restart Croopor and try again.",
            })),
        ),
        AuthRefreshFailureKind::MicrosoftRequest => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({
                "error": "Could not reach Microsoft sign-in. Check your connection and try again.",
            })),
        ),
        AuthRefreshFailureKind::MicrosoftUpstreamRejected => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({ "error": "Microsoft sign-in request was rejected" })),
        ),
        AuthRefreshFailureKind::MicrosoftUpstreamUnavailable => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({ "error": "Microsoft sign-in service is unavailable" })),
        ),
        AuthRefreshFailureKind::MicrosoftParse => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({
                "error": "Microsoft sign-in returned an unexpected response. Try again later.",
            })),
        ),
        AuthRefreshFailureKind::AuthChainFailed => match error.auth_chain_error {
            Some(error) => auth_chain_error_response(error),
            None => (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({
                    "error": "Microsoft sign-in refresh failed",
                    "status": "refresh_failed",
                })),
            ),
        },
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

fn auth_profile_sync_account_required_response() -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::PRECONDITION_FAILED,
        Json(serde_json::json!({
            "error": "Minecraft profile sync needs an active Minecraft account. Sign in or refresh the account, then try again.",
            "status": "minecraft_account_required",
        })),
    )
}

fn auth_clear_failed_response() -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({
            "error": "Could not clear Microsoft sign-in. Restart Croopor and try again.",
            "status": "auth_clear_failed",
        })),
    )
}

fn auth_logout_cleanup_failed_response() -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({
            "error": "Could not finish logout. Restart Croopor and try again.",
            "status": "logout_cleanup_failed",
        })),
    )
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

pub(crate) fn auth_minecraft_profile_response(
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

impl From<ActiveMinecraftAccountState> for AuthStatusMinecraftState {
    fn from(state: ActiveMinecraftAccountState) -> Self {
        Self {
            account: Some(state.account),
            token_expires_in: Some(state.token_expires_in),
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

impl AuthRefreshFailure {
    fn new(kind: AuthRefreshFailureKind) -> Self {
        Self {
            kind,
            auth_chain_error: None,
        }
    }

    pub(crate) fn launch_status_id(&self) -> &'static str {
        match self.kind {
            AuthRefreshFailureKind::MissingRefreshToken
            | AuthRefreshFailureKind::RefreshRejected
            | AuthRefreshFailureKind::StoreUnavailable => "sign_in_required",
            AuthRefreshFailureKind::MicrosoftClientBuild
            | AuthRefreshFailureKind::MicrosoftRequest
            | AuthRefreshFailureKind::MicrosoftUpstreamRejected
            | AuthRefreshFailureKind::MicrosoftUpstreamUnavailable
            | AuthRefreshFailureKind::MicrosoftParse
            | AuthRefreshFailureKind::AuthChainFailed => "refresh_failed",
        }
    }

    pub(crate) fn launch_reason_id(&self) -> &'static str {
        match self.kind {
            AuthRefreshFailureKind::MissingRefreshToken => "refresh_token_missing",
            AuthRefreshFailureKind::RefreshRejected => "refresh_token_rejected",
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

impl From<MicrosoftAuthError> for AuthRefreshFailure {
    fn from(error: MicrosoftAuthError) -> Self {
        let kind = match error.kind() {
            MicrosoftAuthErrorKind::ClientBuild => AuthRefreshFailureKind::MicrosoftClientBuild,
            MicrosoftAuthErrorKind::Request => AuthRefreshFailureKind::MicrosoftRequest,
            MicrosoftAuthErrorKind::UpstreamRejected
                if error.step() == MicrosoftAuthStep::OAuthRefresh =>
            {
                AuthRefreshFailureKind::RefreshRejected
            }
            MicrosoftAuthErrorKind::UpstreamRejected => {
                AuthRefreshFailureKind::MicrosoftUpstreamRejected
            }
            MicrosoftAuthErrorKind::UpstreamUnavailable => {
                AuthRefreshFailureKind::MicrosoftUpstreamUnavailable
            }
            MicrosoftAuthErrorKind::Parse => AuthRefreshFailureKind::MicrosoftParse,
            MicrosoftAuthErrorKind::MissingRefreshToken => {
                AuthRefreshFailureKind::MissingRefreshToken
            }
            MicrosoftAuthErrorKind::MissingSessionId | MicrosoftAuthErrorKind::MissingUserHash => {
                AuthRefreshFailureKind::AuthChainFailed
            }
            MicrosoftAuthErrorKind::StoreUnavailable => AuthRefreshFailureKind::StoreUnavailable,
        };
        Self::new(kind)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth_chain::AuthChainEndpoints;
    use crate::state::{
        AppStateInit, AuthLoginMsaToken, InstallStore, NewAuthLoginMinecraftAccount,
        NewAuthLoginMsaToken, SessionStore,
    };
    use axum::{
        body::Bytes,
        extract::State,
        http::HeaderMap,
        routing::{get, post},
    };
    use croopor_config::{AppConfig, AppPaths, ConfigStore, InstanceStore};
    use croopor_performance::PerformanceManager;
    use std::{fs, path::PathBuf, sync::Arc};
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
        assert!(response.accounts.is_empty());
        assert!(!response.login_available);
        assert_eq!(response.login_reason, LOGIN_UNAVAILABLE_REASON);
    }

    #[tokio::test]
    async fn auth_status_reports_refresh_available_without_enabling_http_login() {
        let fixture = TestFixture::new("refresh-available", "ConfigUser");
        insert_active_refresh_login(fixture.state.auth_logins(), Some("msa-refresh-token")).await;

        let response = auth_status_for_store(
            &fixture.state.config().current(),
            fixture.state.auth_logins(),
        )
        .await
        .expect("auth status");

        assert!(response.msa_refresh_available);
        assert!(!response.msa_authenticated);
        assert!(!response.login_available);
        assert_eq!(response.login_reason, LOGIN_UNAVAILABLE_REASON);
    }

    #[tokio::test]
    async fn auth_status_reports_refresh_unavailable_without_msa_refresh_token() {
        let fixture = TestFixture::new("refresh-unavailable", "ConfigUser");
        insert_active_refresh_login(fixture.state.auth_logins(), None).await;

        let response = auth_status_for_store(
            &fixture.state.config().current(),
            fixture.state.auth_logins(),
        )
        .await
        .expect("auth status");

        assert!(!response.msa_refresh_available);
        assert!(!response.msa_authenticated);
        assert!(!response.login_available);
    }

    #[test]
    fn auth_status_rejects_invalid_configured_username() {
        let error = auth_status_from_username(
            "bad name",
            "offline",
            AuthStatusMsaState::unauthenticated(),
            AuthStatusMinecraftState::unauthenticated(),
            Vec::new(),
        )
        .expect_err("invalid username should fail");

        assert_eq!(error.0, StatusCode::BAD_REQUEST);
        assert_eq!(
            error.1.0,
            serde_json::json!({ "error": "Letters, numbers, and underscores only." })
        );
    }

    #[test]
    fn auth_refresh_error_response_uses_native_product_copy() {
        for (kind, expected_status, expected_message) in [
            (
                AuthRefreshFailureKind::MicrosoftClientBuild,
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not start Microsoft sign-in. Restart Croopor and try again.",
            ),
            (
                AuthRefreshFailureKind::MicrosoftRequest,
                StatusCode::BAD_GATEWAY,
                "Could not reach Microsoft sign-in. Check your connection and try again.",
            ),
            (
                AuthRefreshFailureKind::MicrosoftParse,
                StatusCode::BAD_GATEWAY,
                "Microsoft sign-in returned an unexpected response. Try again later.",
            ),
        ] {
            let response = auth_refresh_error_response(AuthRefreshFailure::new(kind));

            assert_eq!(response.0, expected_status);
            assert_eq!(
                response.1.0,
                serde_json::json!({ "error": expected_message })
            );
            assert_no_sensitive_public_fields(&response.1.0);
        }
    }

    #[test]
    fn auth_refresh_error_response_handles_missing_auth_chain_detail() {
        let response = auth_refresh_error_response(AuthRefreshFailure::new(
            AuthRefreshFailureKind::AuthChainFailed,
        ));

        assert_eq!(response.0, StatusCode::BAD_GATEWAY);
        assert_eq!(
            response.1.0,
            serde_json::json!({
                "error": "Microsoft sign-in refresh failed",
                "status": "refresh_failed",
            })
        );
        assert_no_sensitive_public_fields(&response.1.0);
    }

    #[tokio::test]
    async fn auth_refresh_returns_stored_ready_account_without_provider_request() {
        let store = Arc::new(AuthLoginStore::new());
        insert_active_current_login(&store, Some("old-msa-refresh-token")).await;

        let response = auth_refresh(&store).await;

        assert_eq!(response.0, StatusCode::OK);
        assert_eq!(response.1.0["status"], "refreshed");
        assert_eq!(response.1.0["token_type"], "Bearer");
        assert_eq!(response.1.0["has_refresh_token"], true);
        assert_eq!(response.1.0["minecraft_profile_ready"], true);
        assert_eq!(response.1.0["minecraft_ownership_verified"], true);
        assert_eq!(response.1.0["minecraft_profile"]["name"], "OldProfileName");
        assert_eq!(response.1.0["minecraft_token_expires_in"], 86400);
        assert_no_sensitive_public_fields(&response.1.0);
    }

    #[tokio::test]
    async fn auth_refresh_missing_refresh_token_returns_bounded_precondition() {
        let store = Arc::new(AuthLoginStore::new());
        insert_active_refresh_login(&store, None).await;

        let response = auth_refresh(&store).await;

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
    async fn auth_profile_sync_updates_profile_and_ownership_with_current_minecraft_token() {
        let store = Arc::new(AuthLoginStore::new());
        insert_active_current_login(&store, Some("msa-refresh-token")).await;
        let (auth_chain_client, mut auth_chain_requests) =
            auth_chain_route_test_client(AuthChainRouteServerMode::NoJavaOwnership).await;

        let response = auth_profile_sync(&store, &auth_chain_client).await;

        assert_eq!(response.0, StatusCode::OK);
        assert_eq!(response.1.0["status"], "profile_synced");
        assert_eq!(response.1.0["minecraft_profile_ready"], true);
        assert_eq!(response.1.0["minecraft_ownership_verified"], false);
        assert_eq!(response.1.0["minecraft_profile"]["name"], "ProfileName");
        assert_eq!(
            response.1.0["minecraft_profile"]["skins"][0]["variant"],
            "SLIM"
        );
        assert!(
            response.1.0["minecraft_token_expires_in"]
                .as_u64()
                .is_some_and(|value| value > 0 && value <= 86400)
        );
        assert_no_sensitive_public_fields(&response.1.0);

        let active_msa = store.active_msa_token().await.expect("active msa token");
        assert_eq!(active_msa.access_token, "old-msa-access-token");
        assert_eq!(
            active_msa.refresh_token,
            Some("msa-refresh-token".to_string())
        );
        let minecraft = store
            .active_minecraft_account()
            .await
            .expect("active minecraft account");
        assert_eq!(minecraft.access_token, "old-minecraft-access-token");
        assert_eq!(minecraft.profile.name, "ProfileName");
        assert!(!minecraft.owns_minecraft_java);

        let status = auth_status_for_store(
            &AppConfig {
                username: "ConfigUser".to_string(),
                launch_auth_mode: "online".to_string(),
                ..AppConfig::default()
            },
            &store,
        )
        .await
        .expect("auth status");
        assert_eq!(status.mode, "offline");
        assert!(!status.online_mode_ready);
        assert!(status.minecraft_profile_ready);
        assert!(!status.minecraft_ownership_verified);
        assert_eq!(
            status.minecraft_profile.expect("minecraft profile").name,
            "ProfileName"
        );

        assert_eq!(
            auth_chain_requests
                .recv()
                .await
                .expect("minecraft profile request")
                .authorization
                .as_deref(),
            Some("Bearer old-minecraft-access-token")
        );
        assert_eq!(
            auth_chain_requests
                .recv()
                .await
                .expect("minecraft ownership request")
                .authorization
                .as_deref(),
            Some("Bearer old-minecraft-access-token")
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
    async fn auth_profile_sync_missing_account_returns_bounded_response() {
        let store = Arc::new(AuthLoginStore::new());

        let response = auth_profile_sync(&store, &unused_auth_chain_client()).await;

        assert_eq!(response.0, StatusCode::PRECONDITION_FAILED);
        assert_eq!(response.1.0["status"], "minecraft_account_required");
        assert_eq!(
            response.1.0["error"],
            "Minecraft profile sync needs an active Minecraft account. Sign in or refresh the account, then try again."
        );
        assert_no_sensitive_public_fields(&response.1.0);
    }

    #[tokio::test]
    async fn auth_profile_sync_provider_failure_is_bounded_and_preserves_auth() {
        let store = Arc::new(AuthLoginStore::new());
        insert_active_current_login(&store, Some("msa-refresh-token")).await;
        let (auth_chain_client, mut auth_chain_requests) =
            auth_chain_route_test_client(AuthChainRouteServerMode::ProfileRejected).await;

        let response = auth_profile_sync(&store, &auth_chain_client).await;

        assert_eq!(response.0, StatusCode::BAD_GATEWAY);
        assert_eq!(response.1.0["status"], "minecraft_auth_chain_failed");
        assert_eq!(response.1.0["auth_chain_error"], "upstream_rejected");
        assert_no_sensitive_public_fields(&response.1.0);
        let active_msa = store.active_msa_token().await.expect("active msa token");
        assert_eq!(active_msa.access_token, "old-msa-access-token");
        assert_eq!(
            active_msa.refresh_token,
            Some("msa-refresh-token".to_string())
        );
        let minecraft = store
            .active_minecraft_account()
            .await
            .expect("active minecraft account");
        assert_eq!(minecraft.access_token, "old-minecraft-access-token");
        assert_eq!(minecraft.profile.name, "OldProfileName");
        assert!(minecraft.owns_minecraft_java);

        assert_eq!(
            auth_chain_requests
                .recv()
                .await
                .expect("profile request")
                .path,
            "/minecraft/profile"
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
        store
            .replace_with_msa_token(NewAuthLoginMsaToken {
                access_token: "msa-access-token".to_string(),
                refresh_token: Some("msa-refresh-token".to_string()),
                id_token: Some("msa-id-token".to_string()),
                token_type: "Bearer".to_string(),
                expires_in: 3600,
                scope: Some("XboxLive.signin offline_access".to_string()),
            })
            .await;

        let response = auth_status_for_store(
            &AppConfig {
                username: "ConfigUser".to_string(),
                ..AppConfig::default()
            },
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
        assert!(!response.login_available);
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
    async fn auth_status_marks_online_mode_ready_for_owned_minecraft_account() {
        let store = Arc::new(AuthLoginStore::new());
        insert_active_current_login(&store, None).await;

        let response = auth_status_for_store(
            &AppConfig {
                username: "ConfigUser".to_string(),
                launch_auth_mode: "online".to_string(),
                ..AppConfig::default()
            },
            &store,
        )
        .await
        .expect("auth status");

        assert_eq!(response.launch_auth_mode, "online");
        assert_eq!(response.mode, "online");
        assert_eq!(response.username, "OldProfileName");
        assert_eq!(response.uuid, "old-minecraft-profile-id");
        assert_eq!(response.provider, "microsoft");
        assert!(response.verified);
        assert!(response.online_mode_ready);
        assert!(response.minecraft_profile_ready);
        assert!(response.minecraft_ownership_verified);
        assert_eq!(
            response.minecraft_profile.expect("minecraft profile").name,
            "OldProfileName"
        );
    }

    #[tokio::test]
    async fn auth_status_keeps_online_mode_not_ready_without_java_ownership() {
        let store = Arc::new(AuthLoginStore::new());
        let mut account = test_minecraft_account("ProfileName");
        account.owns_minecraft_java = false;
        insert_active_current_login_with_account(&store, None, account).await;

        let response = auth_status_for_store(
            &AppConfig {
                username: "ConfigUser".to_string(),
                launch_auth_mode: "online".to_string(),
                ..AppConfig::default()
            },
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
    async fn auth_status_lists_signed_in_microsoft_accounts() {
        let store = Arc::new(AuthLoginStore::new());
        let first = insert_active_current_login_with_account(
            &store,
            Some("first-refresh-token"),
            test_minecraft_account_with_id("first-profile-id", "FirstProfile"),
        )
        .await;
        let second = insert_active_current_login_with_account(
            &store,
            Some("second-refresh-token"),
            test_minecraft_account_with_id("second-profile-id", "SecondProfile"),
        )
        .await;

        let response = auth_status_for_store(
            &AppConfig {
                username: "ConfigUser".to_string(),
                launch_auth_mode: "online".to_string(),
                ..AppConfig::default()
            },
            &store,
        )
        .await
        .expect("auth status");

        assert_eq!(response.mode, "online");
        assert_eq!(response.username, "SecondProfile");
        assert_eq!(response.uuid, "second-profile-id");
        assert_eq!(response.accounts.len(), 2);
        assert_eq!(response.accounts[0].login_id, second.login_id);
        assert!(response.accounts[0].active);
        assert!(response.accounts[0].minecraft_profile_ready);
        assert!(response.accounts[0].minecraft_ownership_verified);
        assert_eq!(
            response.accounts[0]
                .minecraft_profile
                .as_ref()
                .expect("second profile")
                .name,
            "SecondProfile"
        );
        assert_eq!(response.accounts[1].login_id, first.login_id);
        assert!(!response.accounts[1].active);
        assert_eq!(
            response.accounts[1]
                .minecraft_profile
                .as_ref()
                .expect("first profile")
                .name,
            "FirstProfile"
        );
    }

    #[tokio::test]
    async fn auth_logout_clears_active_msa_auth() {
        let store = Arc::new(AuthLoginStore::new());
        store
            .replace_with_msa_token(NewAuthLoginMsaToken {
                access_token: "msa-access-token".to_string(),
                refresh_token: Some("msa-refresh-token".to_string()),
                id_token: Some("msa-id-token".to_string()),
                token_type: "Bearer".to_string(),
                expires_in: 3600,
                scope: None,
            })
            .await;

        let response = auth_logout(&store).await;

        assert_eq!(response.0, StatusCode::OK);
        assert_eq!(response.1.0["status"], "logged_out");
        assert_eq!(response.1.0["had_msa_auth"], true);
        assert_eq!(store.active_msa_token().await, None);
        assert!(store.account_states().await.is_empty());

        let second_response = auth_logout(&store).await;
        assert_eq!(second_response.0, StatusCode::OK);
        assert_eq!(second_response.1.0["status"], "logged_out");
        assert_eq!(second_response.1.0["had_msa_auth"], false);
    }

    #[tokio::test]
    async fn auth_logout_clears_launcher_accounts_and_online_config() {
        let fixture = TestFixture::new("logout-clears-accounts", "Player");
        let offline = fixture
            .state
            .accounts()
            .create_offline_account("LocalPlayer")
            .expect("create offline account");
        insert_active_current_login(fixture.state.auth_logins(), Some("msa-refresh-token")).await;
        let minecraft = fixture
            .state
            .auth_logins()
            .active_current_minecraft_account_state()
            .await
            .expect("active minecraft account")
            .account;
        fixture
            .state
            .accounts()
            .upsert_microsoft_account(
                &minecraft.login_id,
                &minecraft.profile.id,
                &minecraft.profile.name,
            )
            .expect("upsert microsoft account");
        let mut config = fixture.state.config().current();
        config.launch_auth_mode = LAUNCH_AUTH_MODE_ONLINE.to_string();
        config.username = minecraft.profile.name.clone();
        fixture
            .state
            .config()
            .replace_in_memory(config)
            .expect("set online config");

        let response = auth_logout_for_state(&fixture.state).await;

        assert_eq!(response.0, StatusCode::OK);
        assert_eq!(response.1.0["status"], "logged_out");
        assert_eq!(response.1.0["had_msa_auth"], true);
        assert!(
            fixture
                .state
                .auth_logins()
                .account_states()
                .await
                .is_empty()
        );
        let accounts = fixture.state.accounts().list().expect("list accounts");
        assert_eq!(accounts, vec![offline.clone()]);
        assert_eq!(
            fixture
                .state
                .accounts()
                .active_account()
                .expect("active account"),
            Some(offline)
        );
        assert_eq!(
            fixture.state.config().current().launch_auth_mode,
            LAUNCH_AUTH_MODE_OFFLINE
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
                startup_warnings: Vec::new(),
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
                AuthStatusMsaState::unauthenticated(),
                AuthStatusMinecraftState::unauthenticated(),
                Vec::new(),
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

    async fn insert_active_refresh_login(
        store: &Arc<AuthLoginStore>,
        refresh_token: Option<&str>,
    ) -> AuthLoginMsaToken {
        store
            .replace_with_msa_token(NewAuthLoginMsaToken {
                access_token: "old-msa-access-token".to_string(),
                refresh_token: refresh_token.map(ToOwned::to_owned),
                id_token: Some("old-msa-id-token".to_string()),
                token_type: "Bearer".to_string(),
                expires_in: 0,
                scope: Some("XboxLive.signin offline_access".to_string()),
            })
            .await
    }

    async fn insert_active_current_login(
        store: &Arc<AuthLoginStore>,
        refresh_token: Option<&str>,
    ) -> AuthLoginMsaToken {
        insert_active_current_login_with_account(
            store,
            refresh_token,
            test_minecraft_account("OldProfileName"),
        )
        .await
    }

    async fn insert_active_current_login_with_account(
        store: &Arc<AuthLoginStore>,
        refresh_token: Option<&str>,
        account: NewAuthLoginMinecraftAccount,
    ) -> AuthLoginMsaToken {
        let (token, _) = store
            .replace_with_msa_and_minecraft_account(
                NewAuthLoginMsaToken {
                    access_token: "old-msa-access-token".to_string(),
                    refresh_token: refresh_token.map(ToOwned::to_owned),
                    id_token: Some("old-msa-id-token".to_string()),
                    token_type: "Bearer".to_string(),
                    expires_in: 3600,
                    scope: Some("XboxLive.signin offline_access".to_string()),
                },
                account,
            )
            .await;
        token
    }

    fn test_minecraft_account(profile_name: &str) -> NewAuthLoginMinecraftAccount {
        test_minecraft_account_with_id("old-minecraft-profile-id", profile_name)
    }

    fn test_minecraft_account_with_id(
        profile_id: &str,
        profile_name: &str,
    ) -> NewAuthLoginMinecraftAccount {
        NewAuthLoginMinecraftAccount {
            access_token: "old-minecraft-access-token".to_string(),
            token_type: Some("Bearer".to_string()),
            expires_in: 86400,
            profile: AuthLoginMinecraftProfile {
                id: profile_id.to_string(),
                name: profile_name.to_string(),
                skins: vec![],
                capes: vec![],
            },
            owns_minecraft_java: true,
        }
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
        ProfileRejected,
        NoJavaOwnership,
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

        if matches!(state.mode, AuthChainRouteServerMode::ProfileRejected) {
            return (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({
                    "error": "provider-secret-payload",
                    "access_token": "minecraft-access-token"
                })),
            );
        }

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

        if matches!(state.mode, AuthChainRouteServerMode::NoJavaOwnership) {
            return (
                StatusCode::OK,
                Json(serde_json::json!({
                    "items": []
                })),
            );
        }

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
                        !matches!(key.as_str(), "access_token" | "refresh_token" | "id_token"),
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
}
