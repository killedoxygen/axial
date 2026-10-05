//! Public account projections contain identity and readiness, never secrets.

use super::{
    credentials::Credentials,
    model::{AccountId, AccountKind, AccountRecord, LaunchAuthMode},
    session::{AuthError, AuthService, now_seconds},
};
use serde::Serialize;

#[derive(Clone, Debug, Serialize)]
pub struct AccountActionState {
    pub state_id: &'static str,
    pub label: &'static str,
    pub enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disabled_reason: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub success_summary: Option<&'static str>,
}

#[derive(Clone, Debug, Serialize)]
pub struct AccountReadiness {
    pub msa_authenticated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub msa_token_expires_in: Option<u64>,
    pub msa_refresh_available: bool,
    pub minecraft_profile_ready: bool,
    pub minecraft_ownership_verified: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub minecraft_token_expires_in: Option<u64>,
    pub online_mode_ready: bool,
    pub online_action: AccountActionState,
    pub refresh_action: AccountActionState,
    pub profile_sync_action: AccountActionState,
}

#[derive(Clone, Debug, Serialize)]
pub struct AccountView {
    #[serde(flatten)]
    pub identity: AccountRecord,
    pub active: bool,
    #[serde(flatten)]
    pub readiness: AccountReadiness,
    pub view_model: AccountViewModel,
}

#[derive(Clone, Debug, Serialize)]
pub struct AccountViewModel {
    pub detail: &'static str,
}

#[derive(Clone, Debug, Serialize)]
pub struct AccountListResponse {
    pub revision: u64,
    pub selection_revision: u64,
    pub active_account_id: Option<AccountId>,
    pub launch_auth_mode: LaunchAuthMode,
    pub accounts: Vec<AccountView>,
}

#[derive(Clone, Debug, Serialize)]
pub struct AuthStatusResponse {
    pub selection_revision: u64,
    pub launch_auth_mode: LaunchAuthMode,
    pub mode: &'static str,
    pub username: String,
    pub uuid: String,
    pub provider: &'static str,
    pub verified: bool,
    pub skin_source: &'static str,
    pub login_available: bool,
    pub login_reason: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub msa_provider: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub minecraft_profile: Option<super::microsoft::MinecraftProfile>,
    #[serde(flatten)]
    pub readiness: AccountReadiness,
    pub skin_action: AccountActionState,
}

impl AuthService {
    pub async fn account_list(&self) -> Result<AccountListResponse, AuthError> {
        let snapshot = self.directory().snapshot()?;
        let mut accounts = Vec::with_capacity(snapshot.accounts.len());
        for account in &snapshot.accounts {
            let credentials = if account.kind == AccountKind::Microsoft {
                let capture =
                    super::selection::CapturedAccount::new(&snapshot, account.account_id.as_str())?;
                match self.credentials(&capture).await {
                    Ok(credentials) => Some(credentials),
                    Err(AuthError::Account(error)) => return Err(error.into()),
                    // A keyring outage cannot turn into a fabricated online
                    // session. Preserve the identity with disabled actions.
                    Err(_) => None,
                }
            } else {
                None
            };
            let readiness = readiness(Some(account), credentials.as_ref());
            let detail = match account.kind {
                AccountKind::Offline => "Offline identity",
                AccountKind::Microsoft if readiness.online_mode_ready => {
                    "Microsoft account ready for online play"
                }
                AccountKind::Microsoft if !account.owns_minecraft_java => {
                    "This Microsoft account does not own Minecraft Java."
                }
                AccountKind::Microsoft if readiness.msa_refresh_available => {
                    "Refresh Microsoft sign-in before playing online"
                }
                AccountKind::Microsoft => "Sign in again to use this Microsoft account",
            };
            accounts.push(AccountView {
                identity: account.clone(),
                active: snapshot.active_account_id.as_ref() == Some(&account.account_id),
                readiness,
                view_model: AccountViewModel { detail },
            });
        }
        if self.directory().selection_revision()? != snapshot.selection_revision {
            return Err(super::model::AccountError::StaleCapture.into());
        }
        Ok(AccountListResponse {
            revision: snapshot.revision,
            selection_revision: snapshot.selection_revision,
            active_account_id: snapshot.active_account_id,
            launch_auth_mode: snapshot.launch_auth_mode,
            accounts,
        })
    }

    pub async fn status(&self, login_available: bool) -> Result<AuthStatusResponse, AuthError> {
        let list = self.account_list().await?;
        let active = list.accounts.into_iter().find(|account| account.active);
        let readiness = active
            .as_ref()
            .map(|account| account.readiness.clone())
            .unwrap_or_else(|| readiness(None, None));
        let microsoft = active
            .as_ref()
            .is_some_and(|account| account.identity.kind == AccountKind::Microsoft);
        let online = microsoft
            && active
                .as_ref()
                .is_some_and(|account| account.identity.owns_minecraft_java);
        let skin_action = action(
            "online_profile_ready",
            "Online profile ready",
            readiness.online_mode_ready,
            if microsoft && !online {
                "The selected Microsoft account has not verified Minecraft Java ownership."
            } else {
                "A verified Microsoft account is required for online profile actions."
            },
            "Minecraft profile updated.",
        );
        Ok(AuthStatusResponse {
            selection_revision: list.selection_revision,
            launch_auth_mode: list.launch_auth_mode,
            mode: if online { "online" } else { "offline" },
            username: active
                .as_ref()
                .map(|a| a.identity.display_name.clone())
                .unwrap_or_default(),
            uuid: active
                .as_ref()
                .map(|a| a.identity.minecraft_uuid().to_owned())
                .unwrap_or_default(),
            provider: if online { "microsoft" } else { "offline" },
            verified: readiness.online_mode_ready,
            skin_source: if readiness.minecraft_profile_ready {
                "minecraft_profile"
            } else {
                "default"
            },
            login_available,
            login_reason: if login_available {
                ""
            } else {
                "Microsoft sign-in is available in the desktop app"
            },
            msa_provider: microsoft.then_some("microsoft"),
            minecraft_profile: active.and_then(|a| a.identity.minecraft_profile),
            readiness,
            skin_action,
        })
    }
}

fn readiness(
    account: Option<&AccountRecord>,
    credentials: Option<&Credentials>,
) -> AccountReadiness {
    let microsoft = account.is_some_and(|a| a.kind == AccountKind::Microsoft);
    let now = now_seconds();
    let msa_expiry = credentials.map(|c| c.microsoft_expires_at().saturating_sub(now));
    let game_expiry = credentials.map(|c| c.minecraft_expires_at().saturating_sub(now));
    let refresh = credentials.is_some_and(|c| c.microsoft_refresh_token().is_some());
    let owned = account.is_some_and(|a| a.owns_minecraft_java);
    let ready = microsoft && owned && game_expiry.is_some_and(|expiry| expiry > 30);
    let mut online_action = if ready {
        action(
            "online_ready",
            "Online ready",
            true,
            "",
            "Microsoft account verified. Online launch is ready.",
        )
    } else if refresh {
        action(
            "online_refresh_available",
            "Refresh available",
            true,
            "",
            "Microsoft sign-in can be refreshed for Online mode.",
        )
    } else {
        action(
            "online_sign_in_required",
            "Sign in required",
            false,
            if microsoft && !owned {
                "The selected Microsoft account has not verified Minecraft Java ownership."
            } else {
                "Sign in with Microsoft to use Online mode."
            },
            "",
        )
    };
    if microsoft && !owned {
        online_action.detail =
            Some("The selected Microsoft account has not verified Minecraft Java ownership.");
    }
    AccountReadiness {
        msa_authenticated: msa_expiry.is_some_and(|expiry| expiry > 0),
        msa_token_expires_in: msa_expiry,
        msa_refresh_available: refresh,
        minecraft_profile_ready: account.is_some_and(|a| a.minecraft_profile.is_some()),
        minecraft_ownership_verified: owned && credentials.is_some(),
        minecraft_token_expires_in: game_expiry,
        online_mode_ready: ready,
        online_action,
        refresh_action: action(
            "refresh_available",
            "Refresh sign-in",
            refresh,
            "Sign in with Microsoft again to refresh this account.",
            "Microsoft sign-in refreshed.",
        ),
        profile_sync_action: action(
            "profile_sync_available",
            "Sync profile",
            microsoft && game_expiry.is_some_and(|expiry| expiry > 0),
            "Refresh Microsoft sign-in before syncing this profile.",
            "Minecraft profile synced.",
        ),
    }
}

fn action(
    state: &'static str,
    label: &'static str,
    enabled: bool,
    reason: &'static str,
    success: &'static str,
) -> AccountActionState {
    AccountActionState {
        state_id: state,
        label,
        enabled,
        disabled_reason: (!enabled).then_some(reason),
        detail: (!enabled).then_some(reason),
        success_summary: enabled.then_some(success),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        accounts::{
            credential_store::{CredentialError, CredentialStore},
            directory::AccountDirectory,
            microsoft::MinecraftProfile,
            model::{MicrosoftIdentity, microsoft_account_id},
        },
        storage::MetadataStore,
        tasks::TaskOwner,
    };
    use std::{sync::Arc, time::Duration};

    #[tokio::test]
    async fn offline_status_and_list_expose_coherent_revisions_without_credentials() {
        let directory =
            Arc::new(AccountDirectory::new(Arc::new(MetadataStore::in_memory().unwrap())).unwrap());
        directory.create_offline_account("Steve").unwrap();
        let service = AuthService::new(
            directory,
            Arc::new(CredentialStore::isolated_for_tests()),
            TaskOwner::new(8).unwrap(),
        );
        let list = service.account_list().await.unwrap();
        let status = service.status(false).await.unwrap();
        assert_eq!(list.selection_revision, status.selection_revision);
        assert_eq!(status.uuid, "5627dd98e6be3c21b8a8e92344183641");
        assert!(!status.readiness.online_mode_ready);
        assert!(!status.readiness.msa_refresh_available);
        assert!(!status.login_available);
        let json = serde_json::to_value(list).unwrap();
        assert_eq!(json["accounts"][0]["active"], true);
        assert_eq!(json["accounts"][0]["minecraft_profile_ready"], false);
        assert!(!json.to_string().contains("access_token"));
    }

    #[tokio::test]
    async fn denied_credentials_preserve_identity_and_allow_shutdown() {
        const PROFILE: &str = "12345678123442348234123456789abc";
        let directory =
            Arc::new(AccountDirectory::new(Arc::new(MetadataStore::in_memory().unwrap())).unwrap());
        let tasks = TaskOwner::new(8).unwrap();
        let (credentials, deny_reads) = CredentialStore::with_read_denial_for_tests(tasks.clone());
        let bundle = Credentials::new(
            "fixture-msa".into(),
            Some("fixture-refresh".into()),
            now_seconds() + 3600,
            "fixture-minecraft".into(),
            now_seconds() + 3600,
        )
        .unwrap();
        let fence = credentials
            .begin_change(&microsoft_account_id(PROFILE).unwrap(), 0)
            .await
            .unwrap();
        let receipt = credentials.save(&fence, bundle.clone()).await.unwrap();
        let profile = MinecraftProfile {
            id: PROFILE.into(),
            name: "SavedPlayer".into(),
            skins: vec![],
            capes: vec![],
        };
        let capture = directory
            .commit_microsoft(
                directory.selection_revision().unwrap(),
                MicrosoftIdentity {
                    login_id: uuid::Uuid::new_v4().to_string(),
                    profile_id: PROFILE.into(),
                    display_name: profile.name.clone(),
                    credential_revision: receipt.revision(),
                    profile: profile.clone(),
                    owns_minecraft_java: true,
                },
            )
            .unwrap();
        let before = directory.snapshot().unwrap();
        let service = AuthService::new(directory.clone(), Arc::new(credentials), tasks.clone());
        assert_eq!(service.launch_credentials(&capture).await.unwrap(), bundle);
        assert!(service.status(true).await.unwrap().verified);

        deny_reads();
        let (list, status, exact, launch) = tokio::time::timeout(Duration::from_secs(2), async {
            tokio::join!(
                service.account_list(),
                service.status(true),
                service.credentials(&capture),
                service.launch_credentials(&capture),
            )
        })
        .await
        .expect("denied secure storage must return without interaction");
        tasks.try_close_idle().unwrap();
        tasks.shutdown(Duration::from_secs(2)).await.unwrap();
        assert!(tasks.shutdown_receipt().unwrap().belongs_to(&tasks));

        assert!(matches!(
            exact,
            Err(AuthError::Credentials(CredentialError::Unavailable))
        ));
        assert!(matches!(
            launch,
            Err(AuthError::Credentials(CredentialError::Unavailable))
        ));
        let list = list.unwrap();
        let status = status.unwrap();
        assert_eq!(directory.snapshot().unwrap(), before);
        assert_eq!(list.accounts.len(), 1);
        assert_eq!(list.accounts[0].identity, before.accounts[0]);
        assert!(list.accounts[0].active);
        assert_eq!(list.active_account_id, before.active_account_id);
        assert_eq!(list.selection_revision, status.selection_revision);
        assert_eq!(status.launch_auth_mode, LaunchAuthMode::Online);
        assert_eq!((status.mode, status.provider), ("online", "microsoft"));
        assert_eq!(status.username, "SavedPlayer");
        assert_eq!(status.uuid, PROFILE);
        assert_eq!(status.minecraft_profile, Some(profile));
        assert!(status.login_available);
        assert!(!status.verified && !status.skin_action.enabled);
        for readiness in [&list.accounts[0].readiness, &status.readiness] {
            assert!(readiness.minecraft_profile_ready);
            assert!(!readiness.msa_authenticated);
            assert!(!readiness.msa_refresh_available);
            assert!(!readiness.minecraft_ownership_verified);
            assert!(!readiness.online_mode_ready);
            assert_eq!(readiness.msa_token_expires_in, None);
            assert_eq!(readiness.minecraft_token_expires_in, None);
            assert!(!readiness.online_action.enabled);
            assert!(!readiness.refresh_action.enabled);
            assert!(!readiness.profile_sync_action.enabled);
        }
    }
}
