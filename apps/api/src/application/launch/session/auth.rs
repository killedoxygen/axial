use crate::application::{
    AuthRefreshFailure, refresh_active_auth, sync_active_offline_account_from_username,
};
use crate::state::{
    ActiveMinecraftAccountState, AppState, LauncherAccountKind, LauncherAccountRecord,
};
use axial_config::{AppConfig, LAUNCH_AUTH_MODE_ONLINE, validate_username};
use axial_launcher::{
    LaunchAuthContext, LaunchFailureClass, LaunchNotice, LaunchNoticeTone, failure_class_name,
};
use axum::{Json, http::StatusCode};
use serde_json::json;

#[derive(Clone, Copy)]
pub(super) struct LaunchAuthRefreshOptions;

pub(super) struct LaunchAuthContextResolution {
    pub(super) auth: LaunchAuthContext,
    pub(super) online_launch: bool,
}

pub(super) async fn resolve_launch_auth_context(
    state: &AppState,
    config: &AppConfig,
    requested_username: Option<&str>,
    auth_refresh: Option<LaunchAuthRefreshOptions>,
) -> Result<LaunchAuthContextResolution, (StatusCode, Json<serde_json::Value>)> {
    let requested_offline_username = requested_username
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(config.username.as_str());
    let requested_offline_username = validate_username(requested_offline_username)
        .map_err(|error| (StatusCode::BAD_REQUEST, Json(json!({ "error": error }))))?;
    let active_account =
        sync_active_offline_account_from_username(state, &requested_offline_username)
            .await
            .map_err(launch_account_store_error_response)?;
    let requested_username = active_account
        .as_ref()
        .filter(|account| account.kind == LauncherAccountKind::Offline)
        .map(|account| account.display_name.as_str())
        .unwrap_or(requested_offline_username.as_str())
        .to_string();
    let offline_username = validate_username(&requested_username)
        .map_err(|error| (StatusCode::BAD_REQUEST, Json(json!({ "error": error }))))?;
    let auth = if let Some(auth_refresh) = auth_refresh {
        launch_auth_context_for_config_with_refresh(
            state,
            config,
            active_account.as_ref(),
            &offline_username,
            auth_refresh,
        )
        .await?
    } else {
        launch_auth_context_for_config(state, config, active_account.as_ref(), &offline_username)
            .await?
    };
    let online_launch = active_account
        .as_ref()
        .is_some_and(|account| account.kind == LauncherAccountKind::Microsoft)
        || (active_account.is_none() && config.launch_auth_mode == LAUNCH_AUTH_MODE_ONLINE);

    Ok(LaunchAuthContextResolution {
        auth,
        online_launch,
    })
}

async fn launch_auth_context_for_config(
    state: &AppState,
    config: &AppConfig,
    active_account: Option<&LauncherAccountRecord>,
    offline_username: &str,
) -> Result<LaunchAuthContext, (StatusCode, Json<serde_json::Value>)> {
    launch_auth_context_for_config_with_refresh(
        state,
        config,
        active_account,
        offline_username,
        LaunchAuthRefreshOptions,
    )
    .await
}

async fn launch_auth_context_for_config_with_refresh(
    state: &AppState,
    config: &AppConfig,
    active_account: Option<&LauncherAccountRecord>,
    offline_username: &str,
    auth_refresh: LaunchAuthRefreshOptions,
) -> Result<LaunchAuthContext, (StatusCode, Json<serde_json::Value>)> {
    // Online launches try one active-account refresh before returning a user-facing auth block.
    if let Some(account) = active_account {
        return match account.kind {
            LauncherAccountKind::Offline => Ok(LaunchAuthContext::offline(&account.display_name)),
            LauncherAccountKind::Microsoft => {
                let Some(login_id) = account.login_id.as_deref() else {
                    return Err(online_auth_refresh_unavailable_response(
                        "refresh_failed",
                        "account_login_missing",
                    ));
                };
                match state.auth_logins().switch_active_account(login_id).await {
                    Ok(true) => {}
                    Ok(false) => {
                        return Err(online_auth_refresh_unavailable_response(
                            "refresh_failed",
                            "account_auth_missing",
                        ));
                    }
                    Err(_) => {
                        return Err(online_auth_refresh_unavailable_response(
                            "refresh_failed",
                            "account_selection_failed",
                        ));
                    }
                }
                online_launch_auth_context_with_refresh(state, auth_refresh).await
            }
        };
    }

    if config.launch_auth_mode != LAUNCH_AUTH_MODE_ONLINE {
        return Ok(LaunchAuthContext::offline(offline_username));
    }

    online_launch_auth_context_with_refresh(state, auth_refresh).await
}

async fn online_launch_auth_context_with_refresh(
    state: &AppState,
    _auth_refresh: LaunchAuthRefreshOptions,
) -> Result<LaunchAuthContext, (StatusCode, Json<serde_json::Value>)> {
    if let Some(auth) = state
        .auth_logins()
        .active_current_minecraft_account_state()
        .await
        .and_then(online_launch_auth_context)
    {
        return Ok(auth);
    }

    refresh_active_auth(state.auth_logins())
        .await
        .map_err(online_auth_refresh_failure_response)?;

    state
        .auth_logins()
        .active_current_minecraft_account_state()
        .await
        .and_then(online_launch_auth_context)
        .ok_or_else(|| {
            online_auth_refresh_unavailable_response("refresh_failed", "refreshed_account_unusable")
        })
}

fn launch_account_store_error_response(
    _error: std::io::Error,
) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({
            "error": "Could not save account changes. Check app data permissions and try again.",
            "failure_class": failure_class_name(LaunchFailureClass::AuthModeIncompatible),
            "status": "account_persistence_failed",
        })),
    )
}

fn online_launch_auth_context(
    account_state: ActiveMinecraftAccountState,
) -> Option<LaunchAuthContext> {
    let account = account_state.account;
    if !account.owns_minecraft_java
        || account.access_token.trim().is_empty()
        || account.profile.name.trim().is_empty()
        || account.profile.id.trim().is_empty()
    {
        return None;
    }

    Some(LaunchAuthContext {
        player_name: account.profile.name,
        uuid: account.profile.id,
        access_token: account.access_token,
        client_id: String::new(),
        xuid: String::new(),
        user_type: "msa".to_string(),
    })
}

fn online_auth_refresh_failure_response(
    error: AuthRefreshFailure,
) -> (StatusCode, Json<serde_json::Value>) {
    online_auth_unavailable_response_with_refresh(Some((
        error.launch_status_id(),
        error.launch_reason_id(),
    )))
}

fn online_auth_refresh_unavailable_response(
    refresh_status: &'static str,
    refresh_reason: &'static str,
) -> (StatusCode, Json<serde_json::Value>) {
    online_auth_unavailable_response_with_refresh(Some((refresh_status, refresh_reason)))
}

fn online_auth_unavailable_response_with_refresh(
    refresh: Option<(&'static str, &'static str)>,
) -> (StatusCode, Json<serde_json::Value>) {
    let mut response = json!({
        "error": "Online launch requires an active verified Minecraft Java account",
        "failure_class": failure_class_name(LaunchFailureClass::AuthModeIncompatible),
        "launch_auth_mode": LAUNCH_AUTH_MODE_ONLINE,
        "online_mode_ready": false,
    });
    if let Some((refresh_status, refresh_reason)) = refresh {
        response["auth_refresh_status"] = json!(refresh_status);
        response["auth_refresh_reason"] = json!(refresh_reason);
    }
    response["notice"] = json!(online_auth_launch_notice(refresh));

    (StatusCode::PRECONDITION_FAILED, Json(response))
}

fn online_auth_launch_notice(refresh: Option<(&'static str, &'static str)>) -> LaunchNotice {
    let reason = refresh.map(|(_, reason)| reason).unwrap_or_default();
    let sign_in_required = matches!(
        reason,
        "refresh_token_missing" | "refresh_token_rejected" | "refresh_state_unavailable"
    ) || refresh
        .map(|(status, _)| status == "sign_in_required")
        .unwrap_or(false);
    let message = if sign_in_required {
        "Online launch needs you to sign in again."
    } else {
        "Online launch could not verify your Minecraft account."
    };
    let first_detail = if sign_in_required {
        match reason {
            "refresh_token_missing" => {
                "Axial could not refresh the Microsoft session because the saved sign-in is missing or expired."
            }
            "refresh_token_rejected" => "Microsoft rejected the saved sign-in session.",
            "refresh_state_unavailable" => "Axial could not read the saved sign-in session.",
            _ => "Axial could not use the saved Microsoft session for Online launch.",
        }
    } else {
        match reason {
            "auth_chain_failed" => {
                "Axial refreshed Microsoft sign-in, but Minecraft account verification did not complete."
            }
            "client_id_missing" => "Microsoft sign-in is not configured for this build.",
            "client_build" | "token_client_unavailable" => {
                "Axial could not start Microsoft sign-in refresh."
            }
            "oauth_refresh_failed"
            | "token_endpoint_unreachable"
            | "token_endpoint_rejected"
            | "token_endpoint_unavailable"
            | "token_endpoint_parse_failed" => {
                "Microsoft sign-in refresh is unavailable or did not complete."
            }
            "refreshed_account_unusable" => {
                "The refreshed account could not be used for a verified Minecraft Java launch."
            }
            _ => "Axial could not verify the Microsoft account for Online launch.",
        }
    };
    let second_detail = if sign_in_required {
        "Sign in again from Accounts, then retry Online launch."
    } else {
        "Refresh or re-verify the account from Accounts, then retry Online launch."
    };
    let details = vec![
        first_detail.to_string(),
        second_detail.to_string(),
        "Offline launch remains available for singleplayer and offline-mode servers.".to_string(),
    ];
    LaunchNotice {
        message: message.to_string(),
        detail: details.first().cloned(),
        details,
        tone: LaunchNoticeTone::Error,
    }
}
