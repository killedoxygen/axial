//! Public account projections contain identity and readiness, never secrets.

use super::{
    credential_store::CredentialError,
    credentials::Credentials,
    microsoft::MinecraftProfile,
    model::{AccountId, AccountKind, AccountRecord, LaunchAuthMode},
    session::{AuthError, AuthService, now_seconds},
};
use crate::{
    media::SkinVariant,
    skins::delivery::{active_cape, current_skin},
};
use serde::Serialize;
use ts_rs::TS;

const STORAGE_UNAVAILABLE: &str = "Secure credential storage is unavailable.";
const OWNERSHIP_UNVERIFIED: &str =
    "The selected Microsoft account has not verified Minecraft Java ownership.";

#[derive(Clone, Debug, Serialize, TS)]
pub struct AccountActionState {
    pub state_id: &'static str,
    pub label: &'static str,
    pub enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub disabled_reason: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub detail: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub success_summary: Option<&'static str>,
}

#[derive(Clone, Debug, Serialize, TS)]
pub struct AccountReadiness {
    pub msa_authenticated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional, type = "number")]
    pub msa_token_expires_in: Option<u64>,
    pub msa_refresh_available: bool,
    pub minecraft_profile_ready: bool,
    pub minecraft_ownership_verified: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional, type = "number")]
    pub minecraft_token_expires_in: Option<u64>,
    pub online_mode_ready: bool,
    pub online_action: AccountActionState,
    pub refresh_action: AccountActionState,
    pub profile_sync_action: AccountActionState,
    /// Changing the Minecraft profile's skin or cape.
    pub skin_action: AccountActionState,
}

/// The skin a Minecraft profile wears right now.
#[derive(Clone, Debug, Serialize, TS)]
pub struct ProfileSkin {
    pub id: String,
    pub url: String,
    pub variant: SkinVariant,
}

#[derive(Clone, Debug, Serialize, TS)]
pub struct AccountView {
    #[serde(flatten)]
    pub identity: AccountRecord,
    pub active: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub profile_skin: Option<ProfileSkin>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub profile_cape_id: Option<String>,
    #[serde(flatten)]
    pub readiness: AccountReadiness,
    pub view_model: AccountViewModel,
}

#[derive(Clone, Debug, Serialize, TS)]
pub struct AccountViewModel {
    pub detail: &'static str,
}

#[derive(Clone, Debug, Serialize, TS)]
pub struct AccountListResponse {
    #[ts(type = "number")]
    pub revision: u64,
    #[ts(type = "number")]
    pub selection_revision: u64,
    pub active_account_id: Option<AccountId>,
    pub launch_auth_mode: LaunchAuthMode,
    /// Whether this build can open the Microsoft sign-in window.
    pub sign_in_available: bool,
    pub accounts: Vec<AccountView>,
}

impl AuthService {
    pub async fn account_list(
        &self,
        sign_in_available: bool,
    ) -> Result<AccountListResponse, AuthError> {
        let snapshot = self.directory().snapshot()?;
        let mut accounts = Vec::with_capacity(snapshot.accounts.len());
        for account in &snapshot.accounts {
            let mut storage_unavailable = false;
            let credentials = if account.kind == AccountKind::Microsoft {
                let capture =
                    super::selection::CapturedAccount::new(&snapshot, account.account_id.as_str())?;
                match self.credentials(&capture).await {
                    Ok(credentials) => Some(credentials),
                    Err(AuthError::Account(error)) => return Err(error.into()),
                    Err(AuthError::Credentials(CredentialError::Unavailable)) => {
                        storage_unavailable = true;
                        None
                    }
                    Err(_) => None,
                }
            } else {
                None
            };
            let readiness = readiness(account, credentials.as_ref(), storage_unavailable);
            let detail = match account.kind {
                AccountKind::Offline => "Offline identity",
                AccountKind::Microsoft if storage_unavailable => STORAGE_UNAVAILABLE,
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
            let profile = account.minecraft_profile.as_ref();
            accounts.push(AccountView {
                identity: account.clone(),
                active: snapshot.active_account_id.as_ref() == Some(&account.account_id),
                profile_skin: profile.and_then(profile_skin),
                profile_cape_id: profile.and_then(active_cape).map(str::to_owned),
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
            sign_in_available,
            accounts,
        })
    }
}

fn profile_skin(profile: &MinecraftProfile) -> Option<ProfileSkin> {
    current_skin(profile).map(|skin| ProfileSkin {
        id: skin.id.clone(),
        url: skin.url.clone(),
        variant: SkinVariant::from_provider(&skin.variant),
    })
}

fn readiness(
    account: &AccountRecord,
    credentials: Option<&Credentials>,
    storage_unavailable: bool,
) -> AccountReadiness {
    let microsoft = account.kind == AccountKind::Microsoft;
    let now = now_seconds();
    let msa_expiry = credentials.map(|c| c.microsoft_expires_at().saturating_sub(now));
    let game_expiry = credentials.map(|c| c.minecraft_expires_at().saturating_sub(now));
    let refresh = credentials.is_some_and(|c| c.microsoft_refresh_token().is_some());
    let owned = account.owns_minecraft_java;
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
    } else if storage_unavailable {
        action(
            "online_credentials_unavailable",
            "Credentials unavailable",
            false,
            STORAGE_UNAVAILABLE,
            "",
        )
    } else {
        action(
            "online_sign_in_required",
            "Sign in required",
            false,
            if microsoft && !owned {
                OWNERSHIP_UNVERIFIED
            } else {
                "Sign in with Microsoft to use Online mode."
            },
            "",
        )
    };
    if microsoft && !owned && !storage_unavailable {
        online_action.detail = Some(OWNERSHIP_UNVERIFIED);
    }
    AccountReadiness {
        msa_authenticated: msa_expiry.is_some_and(|expiry| expiry > 0),
        msa_token_expires_in: msa_expiry,
        msa_refresh_available: refresh,
        minecraft_profile_ready: account.minecraft_profile.is_some(),
        minecraft_ownership_verified: owned && credentials.is_some(),
        minecraft_token_expires_in: game_expiry,
        online_mode_ready: ready,
        online_action,
        refresh_action: action(
            "refresh_available",
            "Refresh sign-in",
            refresh,
            if storage_unavailable {
                STORAGE_UNAVAILABLE
            } else {
                "Sign in with Microsoft again to refresh this account."
            },
            "Microsoft sign-in refreshed.",
        ),
        profile_sync_action: action(
            "profile_sync_available",
            "Sync profile",
            microsoft && game_expiry.is_some_and(|expiry| expiry > 0),
            if storage_unavailable {
                STORAGE_UNAVAILABLE
            } else {
                "Refresh Microsoft sign-in before syncing this profile."
            },
            "Minecraft profile synced.",
        ),
        skin_action: action(
            "online_profile_ready",
            "Online profile ready",
            ready,
            if storage_unavailable {
                STORAGE_UNAVAILABLE
            } else if microsoft && !owned {
                OWNERSHIP_UNVERIFIED
            } else {
                "A verified Microsoft account is required for online profile actions."
            },
            "Minecraft profile updated.",
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
    async fn offline_list_exposes_identity_without_credentials() {
        let directory =
            Arc::new(AccountDirectory::new(Arc::new(MetadataStore::in_memory().unwrap())).unwrap());
        directory.create_offline_account("Steve").unwrap();
        let service = AuthService::new(
            directory,
            Arc::new(CredentialStore::isolated_for_tests()),
            TaskOwner::new(8).unwrap(),
        );
        let list = service.account_list(false).await.unwrap();
        let active = &list.accounts[0];
        assert_eq!(
            active.identity.minecraft_uuid(),
            "5627dd98e6be3c21b8a8e92344183641"
        );
        assert!(!active.readiness.online_mode_ready);
        assert!(!active.readiness.msa_refresh_available);
        assert!(!list.sign_in_available);
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
        assert!(
            service.account_list(true).await.unwrap().accounts[0]
                .readiness
                .online_mode_ready
        );

        deny_reads();
        let (list, exact, launch) = tokio::time::timeout(Duration::from_secs(2), async {
            tokio::join!(
                service.account_list(true),
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
        assert_eq!(directory.snapshot().unwrap(), before);
        assert_eq!(list.accounts.len(), 1);
        assert_eq!(list.accounts[0].identity, before.accounts[0]);
        assert_eq!(
            serde_json::to_value(&list).unwrap()["accounts"][0]["view_model"]["detail"],
            "Secure credential storage is unavailable."
        );
        assert!(list.accounts[0].active);
        assert_eq!(list.active_account_id, before.active_account_id);
        assert_eq!(list.launch_auth_mode, LaunchAuthMode::Online);
        assert_eq!(list.accounts[0].identity.display_name, "SavedPlayer");
        assert_eq!(list.accounts[0].identity.minecraft_uuid(), PROFILE);
        assert_eq!(list.accounts[0].identity.minecraft_profile, Some(profile));
        assert!(list.sign_in_available);
        assert!(!list.accounts[0].readiness.skin_action.enabled);
        {
            let response = serde_json::to_value(&list).unwrap()["accounts"][0].clone();
            assert_eq!(
                response["online_action"]["state_id"],
                "online_credentials_unavailable"
            );
            for field in ["online_action", "refresh_action", "profile_sync_action"] {
                assert_eq!(
                    response[field]["disabled_reason"],
                    "Secure credential storage is unavailable."
                );
            }
        }
        assert_eq!(
            serde_json::to_value(&list).unwrap()["accounts"][0]["skin_action"]["disabled_reason"],
            "Secure credential storage is unavailable."
        );
        {
            let readiness = &list.accounts[0].readiness;
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
