use crate::application::skin::clear_pending_saved_skin_apply_for_login_id;
use crate::state::{
    AppState, AuthLoginAccountState, AuthLoginMinecraftAccount, LauncherAccountKind,
    LauncherAccountRecord,
};
use axial_config::{
    ConfigStoreError, LAUNCH_AUTH_MODE_OFFLINE, LAUNCH_AUTH_MODE_ONLINE, validate_username,
};
use axial_minecraft::offline_uuid;
use axum::{Json, http::StatusCode};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

const ACCOUNT_SAVE_ERROR_MESSAGE: &str =
    "Could not save account changes. Check app data permissions and try again.";

#[derive(Debug, Serialize)]
pub(crate) struct AccountListResponse {
    active_account_id: Option<String>,
    accounts: Vec<AccountResponse>,
}

#[derive(Clone, Debug, Serialize, Eq, PartialEq)]
pub(crate) struct AccountResponse {
    account_id: String,
    kind: LauncherAccountKind,
    display_name: String,
    active: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    login_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    minecraft_profile_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    offline_uuid: Option<String>,
    msa_authenticated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    msa_token_expires_in: Option<u64>,
    msa_refresh_available: bool,
    minecraft_profile_ready: bool,
    minecraft_ownership_verified: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    minecraft_profile: Option<super::auth::AuthMinecraftProfileResponse>,
    #[serde(skip_serializing_if = "Option::is_none")]
    minecraft_token_expires_in: Option<u64>,
    online_action: super::auth::AuthActionState,
    refresh_action: super::auth::AuthActionState,
    profile_sync_action: super::auth::AuthActionState,
    view_model: AccountViewModel,
}

#[derive(Clone, Debug, Serialize, Eq, PartialEq)]
pub(crate) struct AccountViewModel {
    detail: &'static str,
}

#[derive(Debug, Deserialize)]
pub(crate) struct OfflineAccountCreateRequest {
    username: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct AccountPatchRequest {
    username: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct AccountActionResponse {
    status: &'static str,
    account: AccountResponse,
    view_model: AccountCommandViewModel,
}

#[derive(Debug, Serialize)]
pub(crate) struct AccountRemoveResponse {
    status: &'static str,
    account_id: String,
    view_model: AccountCommandViewModel,
}

#[derive(Debug, Serialize)]
pub(crate) struct AccountCommandViewModel {
    summary: &'static str,
}

pub(crate) async fn accounts(
    state: &AppState,
) -> Result<Json<AccountListResponse>, (StatusCode, Json<serde_json::Value>)> {
    account_list_response(state).await.map(Json)
}

pub(crate) async fn create_offline_account(
    state: &AppState,
    request: OfflineAccountCreateRequest,
) -> Result<Json<AccountActionResponse>, (StatusCode, Json<serde_json::Value>)> {
    let account = state
        .accounts()
        .create_offline_account(&request.username)
        .await
        .map_err(account_store_error)?;
    sync_config_for_account(state, &account)
        .await
        .map_err(config_error)?;
    let response = account_response_for_record(state, account, true).await;
    Ok(Json(AccountActionResponse {
        status: "account_created",
        account: response,
        view_model: AccountCommandViewModel {
            summary: "Offline identity created.",
        },
    }))
}

pub(crate) async fn patch_account(
    state: &AppState,
    account_id: &str,
    request: AccountPatchRequest,
) -> Result<Json<AccountActionResponse>, (StatusCode, Json<serde_json::Value>)> {
    let Some(username) = request.username else {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "username is required",
                "status": "account_update_invalid",
            })),
        ));
    };
    let account = state
        .accounts()
        .rename_offline_account(account_id, &username)
        .await
        .map_err(account_store_error)?
        .ok_or_else(account_missing_error)?;
    let active =
        state.accounts().active_account_id().as_deref() == Some(account.account_id.as_str());
    if active {
        sync_config_for_account(state, &account)
            .await
            .map_err(config_error)?;
    }
    let response = account_response_for_record(state, account, active).await;
    Ok(Json(AccountActionResponse {
        status: "account_updated",
        account: response,
        view_model: AccountCommandViewModel {
            summary: "Offline identity updated.",
        },
    }))
}

pub(crate) async fn select_account(
    state: &AppState,
    account_id: &str,
) -> Result<Json<AccountActionResponse>, (StatusCode, Json<serde_json::Value>)> {
    let account = state
        .accounts()
        .list()
        .into_iter()
        .find(|account| account.account_id == account_id)
        .ok_or_else(account_missing_error)?;
    activate_account_auth(state, &account).await?;
    let account = state
        .accounts()
        .select(account_id)
        .await
        .map_err(account_store_error)?
        .ok_or_else(account_missing_error)?;
    sync_config_for_account(state, &account)
        .await
        .map_err(config_error)?;
    let response = account_response_for_record(state, account, true).await;
    Ok(Json(AccountActionResponse {
        status: "account_selected",
        account: response,
        view_model: AccountCommandViewModel {
            summary: "Account selected.",
        },
    }))
}

pub(crate) async fn remove_account(
    state: &AppState,
    account_id: &str,
) -> Result<Json<AccountRemoveResponse>, (StatusCode, Json<serde_json::Value>)> {
    let existing = state
        .accounts()
        .list()
        .into_iter()
        .find(|account| account.account_id == account_id)
        .ok_or_else(account_missing_error)?;

    if let Some(login_id) = existing.login_id.as_deref()
        && let Err(_) = state.auth_logins().remove_account(login_id).await
    {
        return Err(account_remove_failed_error());
    }
    let removed = state
        .accounts()
        .remove(account_id)
        .await
        .map_err(account_store_error)?
        .ok_or_else(account_missing_error)?;
    if let Some(login_id) = removed.login_id.as_deref() {
        clear_pending_saved_skin_apply_for_login_id(login_id).await;
    }

    let next_active = match state.accounts().active_account() {
        Some(active) => Some(active),
        None => select_authenticated_microsoft_replacement(state).await?,
    };

    match next_active {
        Some(active) => {
            activate_account_auth(state, &active).await?;
            sync_config_for_account(state, &active)
                .await
                .map_err(config_error)?;
        }
        None => {
            state
                .mutate_config(move |latest| {
                    latest.launch_auth_mode = LAUNCH_AUTH_MODE_OFFLINE.to_string();
                    Ok(())
                })
                .await
                .map_err(config_error)?;
        }
    }

    Ok(Json(AccountRemoveResponse {
        status: "account_removed",
        account_id: removed.account_id,
        view_model: AccountCommandViewModel {
            summary: if removed.kind == LauncherAccountKind::Microsoft {
                "Microsoft account signed out."
            } else {
                "Offline identity removed."
            },
        },
    }))
}

async fn select_authenticated_microsoft_replacement(
    state: &AppState,
) -> Result<Option<LauncherAccountRecord>, (StatusCode, Json<serde_json::Value>)> {
    let accounts = state.accounts().list();
    let auth_states = auth_state_map(state.auth_logins().account_states().await);
    for account in accounts
        .into_iter()
        .filter(|account| account.kind == LauncherAccountKind::Microsoft)
    {
        let Some(login_id) = account.login_id.as_deref() else {
            continue;
        };
        let Some(auth_state) = auth_states.get(login_id) else {
            continue;
        };
        if !auth_state.msa_authenticated && !auth_state.msa_refresh_available {
            continue;
        }
        if !state
            .auth_logins()
            .switch_active_account(login_id)
            .await
            .map_err(|_| account_selection_failed_error())?
        {
            continue;
        }
        let selected = state
            .accounts()
            .select(&account.account_id)
            .await
            .map_err(account_store_error)?
            .ok_or_else(account_missing_error)?;
        return Ok(Some(selected));
    }
    Ok(None)
}

pub(crate) async fn account_list_response(
    state: &AppState,
) -> Result<AccountListResponse, (StatusCode, Json<serde_json::Value>)> {
    if let Some(repaired_active) = reconcile_microsoft_accounts_from_auth(state).await?
        && let Err(error) = sync_config_for_account(state, &repaired_active).await
    {
        tracing::warn!("account config sync after auth account repair failed: {error}");
    }
    let accounts = state.accounts().list();
    let active_account_id = state.accounts().active_account_id();
    let auth_states = auth_state_map(state.auth_logins().account_states().await);
    Ok(AccountListResponse {
        active_account_id: active_account_id.clone(),
        accounts: accounts
            .into_iter()
            .map(|account| {
                let active = active_account_id.as_deref() == Some(account.account_id.as_str());
                account_response(account, active, &auth_states)
            })
            .collect(),
    })
}

pub(crate) async fn account_response_for_record(
    state: &AppState,
    record: LauncherAccountRecord,
    active: bool,
) -> AccountResponse {
    let auth_states = auth_state_map(state.auth_logins().account_states().await);
    account_response(record, active, &auth_states)
}

pub(crate) fn account_response(
    record: LauncherAccountRecord,
    active: bool,
    auth_states: &HashMap<String, AuthLoginAccountState>,
) -> AccountResponse {
    let auth_state = record
        .login_id
        .as_deref()
        .and_then(|login_id| auth_states.get(login_id));
    let minecraft_account = auth_state.and_then(|state| state.minecraft_account.as_ref());
    let minecraft_token_expires_in = auth_state.and_then(|state| state.minecraft_token_expires_in);
    let minecraft_profile = minecraft_account
        .map(|account| super::auth::auth_minecraft_profile_response(&account.profile));
    let msa_refresh_available = auth_state.is_some_and(|state| state.msa_refresh_available);
    let online_ready =
        super::auth::minecraft_account_launch_ready(minecraft_account, minecraft_token_expires_in);
    let online_action = super::auth::online_action_state(
        minecraft_account,
        minecraft_token_expires_in,
        msa_refresh_available,
    );
    let refresh_action = super::auth::refresh_action_state(msa_refresh_available, online_ready);
    let profile_sync_action = super::auth::profile_sync_action_state(minecraft_account);
    let view_model = account_view_model(record.kind, &online_action);

    AccountResponse {
        account_id: record.account_id,
        kind: record.kind,
        display_name: record.display_name,
        active,
        login_id: record.login_id,
        minecraft_profile_id: record.minecraft_profile_id,
        offline_uuid: record.offline_uuid,
        msa_authenticated: auth_state.is_some_and(|state| state.msa_authenticated),
        msa_token_expires_in: auth_state.and_then(|state| state.msa_token_expires_in),
        msa_refresh_available,
        minecraft_profile_ready: minecraft_account.is_some(),
        minecraft_ownership_verified: minecraft_account
            .is_some_and(|account| account.owns_minecraft_java),
        minecraft_profile,
        minecraft_token_expires_in,
        online_action,
        refresh_action,
        profile_sync_action,
        view_model,
    }
}

fn account_view_model(
    kind: LauncherAccountKind,
    online_action: &super::auth::AuthActionState,
) -> AccountViewModel {
    let detail = match kind {
        LauncherAccountKind::Offline => "Offline identity",
        LauncherAccountKind::Microsoft if online_action.enabled => "Microsoft account",
        LauncherAccountKind::Microsoft => online_action
            .disabled_reason
            .unwrap_or("Microsoft account unavailable"),
    };
    AccountViewModel { detail }
}

pub(crate) async fn sync_config_for_account(
    state: &AppState,
    account: &LauncherAccountRecord,
) -> Result<(), ConfigStoreError> {
    let username = account.display_name.clone();
    let launch_auth_mode = match account.kind {
        LauncherAccountKind::Microsoft => LAUNCH_AUTH_MODE_ONLINE.to_string(),
        LauncherAccountKind::Offline => LAUNCH_AUTH_MODE_OFFLINE.to_string(),
    };
    state
        .mutate_config(move |latest| {
            latest.launch_auth_mode = launch_auth_mode;
            latest.username = username;
            Ok(())
        })
        .await
        .map(|_| ())
}

pub(crate) async fn sync_active_offline_account_from_username(
    state: &AppState,
    username: &str,
) -> std::io::Result<Option<LauncherAccountRecord>> {
    let Some(active) = state.accounts().active_account() else {
        return Ok(None);
    };
    if active.kind != LauncherAccountKind::Offline {
        return Ok(Some(active));
    }

    let display_name =
        validate_username(username).map_err(|error| invalid_account_input(error.to_string()))?;
    if active.display_name == display_name {
        return Ok(Some(active));
    }

    match state
        .accounts()
        .rename_offline_account(&active.account_id, &display_name)
        .await
    {
        Ok(account) => Ok(account),
        Err(error) if error.kind() == std::io::ErrorKind::InvalidInput => {
            let uuid = offline_uuid(&display_name);
            let existing = state.accounts().list().into_iter().find(|account| {
                account.kind == LauncherAccountKind::Offline
                    && account.offline_uuid.as_deref() == Some(uuid.as_str())
            });
            match existing {
                Some(account) => state.accounts().select(&account.account_id).await,
                None => Err(error),
            }
        }
        Err(error) => Err(error),
    }
}

pub(crate) async fn upsert_microsoft_account(
    state: &AppState,
    account: &AuthLoginMinecraftAccount,
) -> Result<LauncherAccountRecord, std::io::Error> {
    state
        .accounts()
        .upsert_microsoft_account(
            &account.login_id,
            &account.profile.id,
            &account.profile.name,
        )
        .await
}

async fn reconcile_microsoft_accounts_from_auth(
    state: &AppState,
) -> Result<Option<LauncherAccountRecord>, (StatusCode, Json<serde_json::Value>)> {
    let auth_states = state.auth_logins().account_states().await;
    let ready_login_ids = auth_states
        .iter()
        .filter(|state| state.minecraft_account.is_some())
        .map(|state| state.login_id.clone())
        .collect::<Vec<_>>();
    if ready_login_ids.is_empty() {
        return Ok(None);
    }

    let current_active = state.accounts().active_account();
    let active_auth_login_id = auth_states
        .iter()
        .find(|state| state.active && state.minecraft_account.is_some())
        .map(|state| state.login_id.clone())
        .or_else(|| ready_login_ids.first().cloned());
    let login_id_to_select = match current_active.as_ref() {
        None => active_auth_login_id,
        Some(account)
            if account.kind == LauncherAccountKind::Microsoft
                && account.login_id.as_deref().is_some_and(|login_id| {
                    ready_login_ids.iter().any(|ready| ready == login_id)
                }) =>
        {
            None
        }
        Some(account) if account.kind == LauncherAccountKind::Microsoft => active_auth_login_id,
        Some(_) => None,
    };

    let mut repaired_active = None;
    for auth_state in auth_states {
        let Some(account) = auth_state.minecraft_account.as_ref() else {
            continue;
        };
        let select = login_id_to_select.as_deref() == Some(auth_state.login_id.as_str());
        let record = state
            .accounts()
            .sync_microsoft_account(
                &account.login_id,
                &account.profile.id,
                &account.profile.name,
                select,
            )
            .await
            .map_err(account_store_error)?;
        if select {
            repaired_active = Some(record);
        }
    }

    Ok(repaired_active)
}

fn auth_state_map(states: Vec<AuthLoginAccountState>) -> HashMap<String, AuthLoginAccountState> {
    states
        .into_iter()
        .map(|state| (state.login_id.clone(), state))
        .collect()
}

async fn activate_account_auth(
    state: &AppState,
    account: &LauncherAccountRecord,
) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    if let LauncherAccountKind::Microsoft = account.kind {
        let Some(login_id) = account.login_id.as_deref() else {
            return Err(account_auth_missing_error());
        };
        match state.auth_logins().switch_active_account(login_id).await {
            Ok(true) => {}
            Ok(false) => return Err(account_auth_missing_error()),
            Err(_) => return Err(account_selection_failed_error()),
        }
    }
    Ok(())
}

fn account_missing_error() -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({
            "error": "Account is not available.",
            "status": "account_not_found",
        })),
    )
}

fn account_auth_missing_error() -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::PRECONDITION_FAILED,
        Json(serde_json::json!({
            "error": "Microsoft account credentials are not available. Sign in again.",
            "status": "auth_refresh_required",
        })),
    )
}

fn account_selection_failed_error() -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({
            "error": "Could not select account. Restart Axial and try again.",
            "status": "account_selection_failed",
        })),
    )
}

fn account_remove_failed_error() -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({
            "error": "Could not remove account credentials. Restart Axial and try again.",
            "status": "auth_persistence_failed",
        })),
    )
}

fn account_store_error(error: std::io::Error) -> (StatusCode, Json<serde_json::Value>) {
    if error.kind() == std::io::ErrorKind::InvalidInput {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": error.to_string(),
                "status": "account_persistence_failed",
            })),
        );
    }
    account_persistence_failed_response()
}

fn invalid_account_input(message: String) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidInput, message)
}

fn config_error(_: ConfigStoreError) -> (StatusCode, Json<serde_json::Value>) {
    account_persistence_failed_response()
}

pub(super) fn account_persistence_failed_response() -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({
            "error": ACCOUNT_SAVE_ERROR_MESSAGE,
            "status": "account_persistence_failed",
        })),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{
        AppStateInit, AuthLoginMinecraftProfile, AuthLoginMinecraftSkin, InstallStore,
        NewAuthLoginMinecraftAccount, NewAuthLoginMsaToken, SessionStore,
    };
    use axial_config::{AppConfig, AppPaths, ConfigStore, InstanceRegistrySnapshot, InstanceStore};
    use axial_performance::PerformanceManager;
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::Arc,
        time::{SystemTime, UNIX_EPOCH},
    };

    #[tokio::test]
    async fn account_list_repairs_active_microsoft_account_from_auth_store() {
        let fixture = TestFixture::new("repair-active-microsoft");
        let (_token, minecraft_account) = fixture
            .state
            .auth_logins()
            .replace_with_msa_and_minecraft_account(
                test_msa_token("msa-access-token"),
                test_minecraft_account("profile-1", "mateoltd"),
            )
            .await
            .expect("insert authenticated account");

        assert_eq!(fixture.state.accounts().list(), Vec::new());

        let response = account_list_response(&fixture.state)
            .await
            .expect("account list");

        assert_eq!(response.accounts.len(), 1);
        let account = &response.accounts[0];
        assert_eq!(account.kind, LauncherAccountKind::Microsoft);
        assert_eq!(
            account.login_id.as_deref(),
            Some(minecraft_account.login_id.as_str())
        );
        assert_eq!(account.display_name, "mateoltd");
        assert!(account.active);
        assert!(account.minecraft_profile_ready);
        assert!(account.minecraft_ownership_verified);
        assert!(account.minecraft_profile.is_some());
        assert_eq!(
            response.active_account_id.as_deref(),
            Some(account.account_id.as_str())
        );
        assert_eq!(
            fixture.state.config().current().launch_auth_mode,
            LAUNCH_AUTH_MODE_ONLINE
        );
        assert_eq!(fixture.state.config().current().username, "mateoltd");
    }

    #[tokio::test]
    async fn account_list_repairs_microsoft_accounts_without_stealing_offline_selection() {
        let fixture = TestFixture::new("repair-preserve-offline");
        let offline = fixture
            .state
            .accounts()
            .create_offline_account("LocalUser")
            .await
            .expect("create offline account");
        let (_token, minecraft_account) = fixture
            .state
            .auth_logins()
            .replace_with_msa_and_minecraft_account(
                test_msa_token("msa-access-token"),
                test_minecraft_account("profile-1", "mateoltd"),
            )
            .await
            .expect("insert authenticated account");

        let response = account_list_response(&fixture.state)
            .await
            .expect("account list");

        assert_eq!(
            response.active_account_id.as_deref(),
            Some(offline.account_id.as_str())
        );
        let offline_response = response
            .accounts
            .iter()
            .find(|account| account.account_id == offline.account_id)
            .expect("offline account response");
        assert!(offline_response.active);
        let microsoft_response = response
            .accounts
            .iter()
            .find(|account| {
                account.login_id.as_deref() == Some(minecraft_account.login_id.as_str())
            })
            .expect("microsoft account response");
        assert!(!microsoft_response.active);
        assert_eq!(microsoft_response.display_name, "mateoltd");
        assert!(microsoft_response.minecraft_profile_ready);
    }

    #[tokio::test]
    async fn account_select_missing_microsoft_auth_does_not_change_active_account() {
        let fixture = TestFixture::new("select-missing-auth-keeps-active");
        let offline = fixture
            .state
            .accounts()
            .create_offline_account("LocalUser")
            .await
            .expect("create offline account");
        let microsoft = fixture
            .state
            .accounts()
            .sync_microsoft_account("missing-login", "profile-1", "mateoltd", false)
            .await
            .expect("sync microsoft account");

        let result = select_account(&fixture.state, &microsoft.account_id).await;

        let (status, Json(body)) = result.expect_err("select should fail without auth token");
        assert_eq!(status, StatusCode::PRECONDITION_FAILED);
        assert_eq!(body["status"], "auth_refresh_required");
        assert_eq!(
            fixture.state.accounts().active_account().as_ref(),
            Some(&offline)
        );
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

    fn test_msa_token(access_token: &str) -> NewAuthLoginMsaToken {
        NewAuthLoginMsaToken {
            access_token: access_token.to_string(),
            refresh_token: Some(format!("{access_token}-refresh")),
            id_token: None,
            token_type: "Bearer".to_string(),
            expires_in: 3600,
            scope: Some("XboxLive.signin offline_access".to_string()),
        }
    }

    fn test_minecraft_account(
        profile_id: &str,
        profile_name: &str,
    ) -> NewAuthLoginMinecraftAccount {
        NewAuthLoginMinecraftAccount {
            access_token: "minecraft-access-token".to_string(),
            token_type: Some("Bearer".to_string()),
            expires_in: 3600,
            profile: AuthLoginMinecraftProfile {
                id: profile_id.to_string(),
                name: profile_name.to_string(),
                skins: vec![AuthLoginMinecraftSkin {
                    id: "skin-1".to_string(),
                    state: "ACTIVE".to_string(),
                    url: "https://textures.minecraft.net/texture/profile-skin".to_string(),
                    variant: "CLASSIC".to_string(),
                }],
                capes: Vec::new(),
            },
            owns_minecraft_java: true,
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
            "axial-account-routes-{name}-{}-{nonce}",
            std::process::id()
        ))
    }
}
