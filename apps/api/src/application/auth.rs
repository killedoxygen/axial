use crate::{
    application::accounts,
    application::skin::clear_all_pending_saved_skin_applies,
    auth_chain::{
        AuthChainClient, AuthChainError, AuthChainErrorKind, MinecraftCape, MinecraftProfile,
        MinecraftSkin,
    },
    microsoft_auth::{MicrosoftAuthError, MicrosoftAuthErrorKind, MicrosoftAuthStep},
    state::{
        ActiveMinecraftAccountState, ActiveMsaTokenState, AppState, AuthLoginMinecraftAccount,
        AuthLoginMinecraftCape, AuthLoginMinecraftProfile, AuthLoginMinecraftSkin, AuthLoginStore,
    },
};
use axial_config::{
    AppConfig, LAUNCH_AUTH_MODE_OFFLINE, LAUNCH_AUTH_MODE_ONLINE, validate_username,
};
use axial_minecraft::offline_uuid;
use axum::{Json, http::StatusCode};
use serde::Serialize;
use std::sync::Arc;

const LOGIN_UNAVAILABLE_REASON: &str = "Microsoft sign-in is available in the desktop app";

#[derive(Debug, Serialize)]
pub(crate) struct AuthStatusResponse {
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
    online_action: AuthActionState,
    refresh_action: AuthActionState,
    profile_sync_action: AuthActionState,
    skin_action: AuthActionState,
    login_available: bool,
    login_reason: &'static str,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct AuthActionState {
    pub(crate) state_id: &'static str,
    pub(crate) label: &'static str,
    pub(crate) enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) disabled_reason: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) detail: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) success_summary: Option<&'static str>,
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
struct AuthProfileSyncResponse {
    status: &'static str,
    minecraft_profile_ready: bool,
    minecraft_ownership_verified: bool,
    minecraft_profile: AuthMinecraftProfileResponse,
    minecraft_token_expires_in: u64,
    view_model: AuthCommandViewModel,
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
    view_model: AuthCommandViewModel,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct AuthCommandViewModel {
    summary: &'static str,
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
    view_model: AuthCommandViewModel,
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

pub(crate) async fn auth_status(
    state: &AppState,
) -> Result<Json<AuthStatusResponse>, (StatusCode, Json<serde_json::Value>)> {
    auth_status_for_store(&state.config().current(), state.auth_logins())
        .await
        .map(Json)
}

pub(crate) async fn auth_refresh_for_state(
    state: &AppState,
) -> (StatusCode, Json<serde_json::Value>) {
    let response = auth_refresh(state.auth_logins()).await;
    let active = if response.0 == StatusCode::OK {
        state
            .auth_logins()
            .active_current_minecraft_account_state()
            .await
    } else {
        None
    };
    if let Some(active) = active {
        let account = match accounts::upsert_microsoft_account(state, &active.account).await {
            Ok(account) => account,
            Err(error) => {
                tracing::warn!(error_kind = ?error.kind(), "account store sync after auth refresh failed");
                return accounts::account_persistence_failed_response();
            }
        };
        if let Err(error) = accounts::sync_config_for_account(state, &account).await {
            tracing::warn!("account config sync after auth refresh failed: {error}");
            return accounts::account_persistence_failed_response();
        }
    }
    response
}

pub(crate) async fn auth_profile_sync_for_state(
    state: &AppState,
) -> (StatusCode, Json<serde_json::Value>) {
    let auth_chain_client = match auth_profile_sync_client_for_state(state) {
        Ok(client) => client,
        Err(error) => return auth_chain_error_response(error),
    };

    let response = auth_profile_sync(state.auth_logins(), &auth_chain_client).await;
    let active = if response.0 == StatusCode::OK {
        state
            .auth_logins()
            .active_current_minecraft_account_state()
            .await
    } else {
        None
    };
    if let Some(active) = active {
        let account = match accounts::upsert_microsoft_account(state, &active.account).await {
            Ok(account) => account,
            Err(error) => {
                tracing::warn!(error_kind = ?error.kind(), "account store sync after profile sync failed");
                return accounts::account_persistence_failed_response();
            }
        };
        if let Err(error) = accounts::sync_config_for_account(state, &account).await {
            tracing::warn!("account config sync after profile sync failed: {error}");
            return accounts::account_persistence_failed_response();
        }
    }
    response
}

#[cfg(test)]
fn auth_profile_sync_client_for_state(state: &AppState) -> Result<AuthChainClient, AuthChainError> {
    state
        .auth_chain_client_override()
        .map(Ok)
        .unwrap_or_else(AuthChainClient::new)
}

#[cfg(not(test))]
fn auth_profile_sync_client_for_state(
    _state: &AppState,
) -> Result<AuthChainClient, AuthChainError> {
    AuthChainClient::new()
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
    auth_status_from_username(
        &config.username,
        &config.launch_auth_mode,
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
    let online_credentials_ready = minecraft_account_launch_ready(
        minecraft_state.account.as_ref(),
        minecraft_state.token_expires_in,
    );
    let online_mode_ready = launch_auth_mode == LAUNCH_AUTH_MODE_ONLINE && online_credentials_ready;
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
        online_action: online_action_state(
            minecraft_state.account.as_ref(),
            minecraft_state.token_expires_in,
            msa_state.refresh_available,
        ),
        refresh_action: refresh_action_state(msa_state.refresh_available, online_credentials_ready),
        profile_sync_action: profile_sync_action_state(minecraft_state.account.as_ref()),
        skin_action: skin_action_state(launch_auth_mode, &minecraft_state, online_mode_ready),
        login_available: false,
        login_reason: LOGIN_UNAVAILABLE_REASON,
    })
}

fn skin_action_state(
    launch_auth_mode: &str,
    minecraft_state: &AuthStatusMinecraftState,
    online_mode_ready: bool,
) -> AuthActionState {
    if online_mode_ready {
        return AuthActionState {
            state_id: "online_profile_ready",
            label: "Online profile ready",
            enabled: true,
            disabled_reason: None,
            detail: Some("Online profile skin and cape actions are available."),
            success_summary: Some("Online profile actions are ready."),
        };
    }
    let disabled_reason = if launch_auth_mode != LAUNCH_AUTH_MODE_ONLINE {
        "Online account actions require Online launch mode."
    } else if minecraft_state.account.is_none() {
        "Sign in with Microsoft to apply skins or capes online."
    } else if minecraft_state
        .account
        .as_ref()
        .is_some_and(|account| !account.owns_minecraft_java)
    {
        "The selected Microsoft account has not verified Minecraft Java ownership."
    } else {
        "The selected Microsoft account is not ready for online profile actions."
    };

    AuthActionState {
        state_id: "online_profile_unavailable",
        label: "Online profile unavailable",
        enabled: false,
        disabled_reason: Some(disabled_reason),
        detail: Some(disabled_reason),
        success_summary: None,
    }
}

pub(crate) fn online_action_state(
    account: Option<&AuthLoginMinecraftAccount>,
    minecraft_token_expires_in: Option<u64>,
    refresh_available: bool,
) -> AuthActionState {
    if minecraft_account_launch_ready(account, minecraft_token_expires_in) {
        return AuthActionState {
            state_id: "online_ready",
            label: "Online ready",
            enabled: true,
            disabled_reason: None,
            detail: Some("Launches can use this Microsoft account online."),
            success_summary: Some("Microsoft account verified. Online launch is ready."),
        };
    }
    if refresh_available {
        return AuthActionState {
            state_id: "online_refresh_available",
            label: "Refresh available",
            enabled: true,
            disabled_reason: None,
            detail: Some("Refresh Microsoft sign-in before using Online mode."),
            success_summary: Some("Microsoft sign-in can be refreshed for Online mode."),
        };
    }

    let disabled_reason = match account {
        None => "Sign in with Microsoft to use Online mode.",
        Some(account) if !account.owns_minecraft_java => {
            "The selected Microsoft account has not verified Minecraft Java ownership."
        }
        Some(account)
            if account.access_token.trim().is_empty()
                || account.profile.id.trim().is_empty()
                || account.profile.name.trim().is_empty() =>
        {
            "The selected Microsoft account is missing current Minecraft profile credentials."
        }
        Some(_) => "The selected Microsoft account needs refreshed Minecraft credentials.",
    };

    AuthActionState {
        state_id: "online_unavailable",
        label: "Online unavailable",
        enabled: false,
        disabled_reason: Some(disabled_reason),
        detail: Some(disabled_reason),
        success_summary: None,
    }
}

pub(crate) fn refresh_action_state(
    refresh_available: bool,
    online_credentials_ready: bool,
) -> AuthActionState {
    if refresh_available {
        return AuthActionState {
            state_id: if online_credentials_ready {
                "refresh_available"
            } else {
                "refresh_recommended"
            },
            label: "Refresh Microsoft sign-in",
            enabled: true,
            disabled_reason: None,
            detail: Some(
                "Refresh the current Microsoft sign-in and Minecraft profile credentials.",
            ),
            success_summary: Some("Microsoft sign-in refreshed."),
        };
    }

    AuthActionState {
        state_id: "refresh_unavailable",
        label: "Refresh unavailable",
        enabled: false,
        disabled_reason: Some("Microsoft sign-in refresh is unavailable; sign in again."),
        detail: Some("Microsoft sign-in refresh is unavailable; sign in again."),
        success_summary: None,
    }
}

pub(crate) fn profile_sync_action_state(
    account: Option<&AuthLoginMinecraftAccount>,
) -> AuthActionState {
    if account.is_some_and(|account| !account.access_token.trim().is_empty()) {
        return AuthActionState {
            state_id: "profile_sync_available",
            label: "Sync Minecraft profile",
            enabled: true,
            disabled_reason: None,
            detail: Some(
                "Refresh Minecraft profile and ownership from the active Microsoft account.",
            ),
            success_summary: Some("Minecraft profile synced."),
        };
    }

    AuthActionState {
        state_id: "profile_sync_unavailable",
        label: "Profile sync unavailable",
        enabled: false,
        disabled_reason: Some("Minecraft profile sync needs an active Microsoft account."),
        detail: Some("Minecraft profile sync needs an active Microsoft account."),
        success_summary: None,
    }
}

pub(crate) fn minecraft_account_launch_ready(
    account: Option<&AuthLoginMinecraftAccount>,
    minecraft_token_expires_in: Option<u64>,
) -> bool {
    account.is_some_and(minecraft_account_can_launch_online)
        && minecraft_token_expires_in.is_some_and(|expires_in| expires_in > 0)
}

fn minecraft_account_can_launch_online(account: &AuthLoginMinecraftAccount) -> bool {
    account.owns_minecraft_java
        && !account.access_token.trim().is_empty()
        && !account.profile.id.trim().is_empty()
        && !account.profile.name.trim().is_empty()
}

async fn auth_logout(login_store: &Arc<AuthLoginStore>) -> (StatusCode, Json<serde_json::Value>) {
    match login_store.clear_all().await {
        Ok(had_msa_auth) => {
            clear_all_pending_saved_skin_applies().await;
            (
                StatusCode::OK,
                Json(serde_json::json!(AuthLogoutResponse {
                    status: "logged_out",
                    had_msa_auth,
                    view_model: AuthCommandViewModel {
                        summary: if had_msa_auth {
                            "Microsoft account signed out."
                        } else {
                            "No Microsoft sign-in was active."
                        },
                    },
                })),
            )
        }
        Err(_) => auth_clear_failed_response(),
    }
}

pub(crate) async fn auth_logout_for_state(
    state: &AppState,
) -> (StatusCode, Json<serde_json::Value>) {
    if let Err(error) = state.accounts().remove_all_microsoft_accounts().await {
        tracing::warn!("account store cleanup before auth logout failed: {error}");
        return auth_logout_cleanup_failed_response();
    }

    let response = auth_logout(state.auth_logins()).await;
    if response.0 != StatusCode::OK {
        return response;
    }

    match state
        .mutate_config(move |latest| {
            latest.launch_auth_mode = LAUNCH_AUTH_MODE_OFFLINE.to_string();
            Ok(())
        })
        .await
    {
        Ok(_) => {}
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
        view_model: AuthCommandViewModel {
            summary: "Microsoft sign-in refreshed.",
        },
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

    let updated = match login_store
        .update_active_current_minecraft_profile_and_ownership(
            &active.account.login_id,
            profile,
            Some(ownership.owns_minecraft_java),
        )
        .await
    {
        Ok(Some(updated)) => updated,
        Ok(None) => return auth_profile_sync_account_required_response(),
        Err(_) => return auth_store_failed_response(),
    };

    (
        StatusCode::OK,
        Json(serde_json::json!(AuthProfileSyncResponse {
            status: "profile_synced",
            minecraft_profile_ready: true,
            minecraft_ownership_verified: updated.account.owns_minecraft_java,
            minecraft_profile: auth_minecraft_profile_response(&updated.account.profile),
            minecraft_token_expires_in: updated.token_expires_in,
            view_model: AuthCommandViewModel {
                summary: "Minecraft profile synced.",
            },
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
                "error": "Could not start Microsoft sign-in. Restart Axial and try again.",
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
            "error": "Could not clear Microsoft sign-in. Restart Axial and try again.",
            "status": "auth_clear_failed",
        })),
    )
}

fn auth_store_failed_response() -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({
            "error": "Could not save Microsoft account changes. Restart Axial and try again.",
            "status": "auth_store_failed",
        })),
    )
}

fn auth_logout_cleanup_failed_response() -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({
            "error": "Could not finish logout. Restart Axial and try again.",
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
        | AuthChainErrorKind::Parse => StatusCode::BAD_GATEWAY,
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
    use crate::application::skin;
    use crate::auth_chain::AuthChainEndpoints;
    use crate::execution::persistence::{AtomicWriteBackend, PersistenceCoordinator};
    use crate::state::contracts::TargetDescriptor;
    use crate::state::{
        AppStateInit, AuthLoginMsaToken, InstallStore, LauncherAccountStore,
        NewAuthLoginMinecraftAccount, NewAuthLoginMsaToken, SessionStore,
    };
    use axial_config::{AppConfig, AppPaths, ConfigStore, InstanceRegistrySnapshot, InstanceStore};
    use axial_performance::PerformanceManager;
    use axum::{body::Bytes, extract::State, http::HeaderMap, routing::get};
    use std::{
        fs, io,
        path::{Path, PathBuf},
        sync::Arc,
        time::Duration,
    };
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
        assert_eq!(response.skin_action.state_id, "online_profile_unavailable");
        assert!(!response.skin_action.enabled);
        assert_eq!(
            response.skin_action.disabled_reason,
            Some("Online account actions require Online launch mode.")
        );
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
                "Could not start Microsoft sign-in. Restart Axial and try again.",
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
            .await
            .expect("insert MSA auth fixture");

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
    async fn auth_status_uses_active_microsoft_account_profile() {
        let store = Arc::new(AuthLoginStore::new());
        let _first = insert_active_current_login_with_account(
            &store,
            Some("first-refresh-token"),
            test_minecraft_account_with_id("first-profile-id", "FirstProfile"),
        )
        .await;
        insert_active_current_login_with_account(
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
        assert!(response.minecraft_profile_ready);
        assert!(response.minecraft_ownership_verified);
        assert_eq!(
            response
                .minecraft_profile
                .as_ref()
                .expect("second profile")
                .name,
            "SecondProfile"
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
            .await
            .expect("insert MSA auth fixture");

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
    async fn auth_logout_clears_all_pending_skin_applies() {
        let store = Arc::new(AuthLoginStore::new());
        skin::clear_all_pending_saved_skin_applies().await;
        skin::test_set_pending_saved_skin_apply_for_login_id("login-a").await;
        skin::test_set_pending_saved_skin_apply_for_login_id("login-b").await;

        let response = auth_logout(&store).await;

        assert_eq!(response.0, StatusCode::OK);
        assert_eq!(response.1.0["status"], "logged_out");
        assert_eq!(skin::clear_all_pending_saved_skin_applies().await, 0);
    }

    #[tokio::test]
    async fn auth_logout_clears_launcher_accounts_and_online_config() {
        let fixture = TestFixture::new("logout-clears-accounts", "Player");
        let offline = fixture
            .state
            .accounts()
            .create_offline_account("LocalPlayer")
            .await
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
            .await
            .expect("upsert microsoft account");
        let mut config = fixture.state.config().current();
        config.launch_auth_mode = LAUNCH_AUTH_MODE_ONLINE.to_string();
        config.username = minecraft.profile.name.clone();
        fixture
            .state
            .config()
            .replace_for_test(config)
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
        let accounts = fixture.state.accounts().list();
        assert_eq!(accounts, vec![offline.clone()]);
        assert_eq!(fixture.state.accounts().active_account(), Some(offline));
        assert_eq!(
            fixture.state.config().current().launch_auth_mode,
            LAUNCH_AUTH_MODE_OFFLINE
        );
    }

    #[tokio::test]
    async fn auth_refresh_does_not_report_success_or_publish_account_after_persistence_failure() {
        let fixture =
            TestFixture::new_with_failing_accounts("refresh-account-persistence-failure", "Player");
        insert_active_current_login(fixture.state.auth_logins(), Some("msa-refresh-token")).await;

        let response = auth_refresh_for_state(&fixture.state).await;

        assert_eq!(response.0, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(response.1.0["status"], "account_persistence_failed");
        assert!(fixture.state.accounts().list().is_empty());
    }

    #[tokio::test]
    async fn profile_sync_does_not_report_success_or_publish_account_after_persistence_failure() {
        let fixture = TestFixture::new_with_failing_accounts(
            "profile-sync-account-persistence-failure",
            "Player",
        );
        insert_active_current_login(fixture.state.auth_logins(), Some("msa-refresh-token")).await;
        let (client, _requests) =
            auth_chain_route_test_client(AuthChainRouteServerMode::NoJavaOwnership).await;
        fixture.state.set_auth_chain_client_override(client);

        let response = auth_profile_sync_for_state(&fixture.state).await;

        assert_eq!(response.0, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(response.1.0["status"], "account_persistence_failed");
        assert!(fixture.state.accounts().list().is_empty());
    }

    #[tokio::test]
    async fn auth_refresh_does_not_report_success_when_account_config_sync_fails() {
        let fixture = TestFixture::new("refresh-account-config-failure", "Player");
        insert_active_current_login(fixture.state.auth_logins(), Some("msa-refresh-token")).await;
        let config_path = fixture.state.config().paths().config_file().to_path_buf();
        if config_path.is_file() {
            fs::remove_file(&config_path).expect("remove config file");
        }
        fs::create_dir_all(&config_path).expect("block config file with directory");

        let response = auth_refresh_for_state(&fixture.state).await;

        assert_eq!(response.0, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(response.1.0["status"], "account_persistence_failed");
        assert_eq!(fixture.state.accounts().list().len(), 1);
    }

    struct FailingAccountBackend;

    impl AtomicWriteBackend for FailingAccountBackend {
        fn write(
            &self,
            _destination: &crate::execution::anchored_record::AnchoredRecordTarget,
            _effects: &axial_fs::EffectOwner,
            _contents: &[u8],
        ) -> io::Result<()> {
            Err(io::Error::other("injected account persistence failure"))
        }
    }

    struct TestFixture {
        state: AppState,
        root: PathBuf,
    }

    impl TestFixture {
        fn new(name: &str, username: &str) -> Self {
            Self::new_inner(name, username, false)
        }

        fn new_with_failing_accounts(name: &str, username: &str) -> Self {
            Self::new_inner(name, username, true)
        }

        fn new_inner(name: &str, username: &str, failing_accounts: bool) -> Self {
            let root = test_root(name);
            let paths = test_paths(&root);
            let root_session = crate::state::test_root_session(&paths);
            let config = Arc::new(
                ConfigStore::from_config(
                    paths.clone(),
                    Arc::clone(&root_session),
                    AppConfig {
                        username: username.to_string(),
                        ..AppConfig::default()
                    },
                )
                .expect("set username"),
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
            let state = if failing_accounts {
                let coordinator = PersistenceCoordinator::for_test(
                    Arc::new(FailingAccountBackend),
                    Duration::from_millis(20),
                    Duration::from_millis(100),
                );
                let accounts = Arc::new(
                    LauncherAccountStore::try_load_from_paths_with_coordinator(&paths, coordinator)
                        .expect("claim failing account persistence"),
                );
                state.with_accounts(accounts)
            } else {
                state
            };

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
            "axial-api-auth-{name}-{}-{}",
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
            .expect("insert active refresh login")
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
            .await
            .expect("insert active current login");
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
