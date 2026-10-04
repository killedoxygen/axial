//! Account-bound online operations. Only explicit commands touch credentials;
//! construction never opens the OS keyring or calls a provider.

use std::{
    future::Future,
    sync::{
        Arc, RwLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::Mutex;
use uuid::Uuid;

use super::{
    credential_store::{CredentialError, CredentialFence, CredentialState, CredentialStore},
    credentials::Credentials,
    directory::AccountDirectory,
    microsoft::{
        self, MicrosoftAuthError, MicrosoftLoginFlow, MicrosoftMinecraftSession, MinecraftProfile,
    },
    model::{
        AccountError, AccountKind, AccountPreconditions, AccountSnapshot, MicrosoftIdentity,
        microsoft_account_id,
    },
    selection::CapturedAccount,
};
use crate::tasks::TaskOwner;

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error(transparent)]
    Account(#[from] AccountError),
    #[error(transparent)]
    Credentials(#[from] CredentialError),
    #[error(transparent)]
    Provider(#[from] MicrosoftAuthError),
    #[error("Sign in with Microsoft again to continue.")]
    SignInRequired,
    #[error("This Microsoft account does not own Minecraft Java.")]
    OwnershipMissing,
    #[error("Microsoft sign-in expired. Start sign-in again.")]
    LoginExpired,
    #[error("Account work is unavailable while the application is closing.")]
    Unavailable,
}

/// Kept by the native OAuth window owner, never deserialized from HTTP input.
pub struct PendingLogin {
    service: Uuid,
    selection_revision: u64,
    login_generation: u64,
    expires: Instant,
    flow: MicrosoftLoginFlow,
}

impl PendingLogin {
    pub fn auth_request_uri(&self) -> &str {
        self.flow.auth_request_uri()
    }
}

#[derive(Clone)]
pub struct AuthService {
    directory: Arc<AccountDirectory>,
    credentials: Arc<CredentialStore>,
    tasks: TaskOwner,
    // This gate joins each keyring acknowledgement to its metadata commit.
    // Provider network calls happen outside it; logout can fence a refresh.
    mutation: Arc<Mutex<()>>,
    service: Uuid,
    login_generation: Arc<AtomicU64>,
    account_removed: Arc<RwLock<Option<Arc<AccountRemovedObserver>>>>,
    profile_client: microsoft::ProfileClient,
}

type AccountRemovedObserver = dyn Fn(&str) + Send + Sync;

impl AuthService {
    pub fn new(
        directory: Arc<AccountDirectory>,
        credentials: Arc<CredentialStore>,
        tasks: TaskOwner,
    ) -> Self {
        Self {
            directory,
            credentials,
            tasks,
            mutation: Arc::default(),
            service: Uuid::new_v4(),
            login_generation: Arc::default(),
            account_removed: Arc::default(),
            profile_client: microsoft::ProfileClient::default(),
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn with_profile_endpoints_for_tests(mut self, base: &str) -> Self {
        self.profile_client = microsoft::ProfileClient::fixture(base);
        self
    }

    pub fn directory(&self) -> &Arc<AccountDirectory> {
        &self.directory
    }
    pub fn task_owner(&self) -> &TaskOwner {
        &self.tasks
    }

    /// Composition supplies an infallible, in-memory invalidation callback.
    /// Capture the consumer weakly to avoid an auth/profile-media owner cycle.
    /// Durable account references must instead cascade in the metadata commit.
    pub fn set_account_removed_observer(&self, observer: Arc<AccountRemovedObserver>) {
        *self
            .account_removed
            .write()
            .unwrap_or_else(|error| error.into_inner()) = Some(observer);
    }

    fn notify_account_removed(&self, account_id: &str) {
        let observer = self
            .account_removed
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        if let Some(observer) = observer {
            observer(account_id);
        }
    }

    pub async fn begin_login(&self) -> Result<PendingLogin, AuthError> {
        let service = self.clone();
        self.retain(async move {
            let revision = service.directory.selection_revision()?;
            let login_generation = service.login_generation.load(Ordering::SeqCst);
            let flow = microsoft::begin_login().await?;
            service.validate_login(revision, login_generation)?;
            Ok(PendingLogin {
                service: service.service,
                selection_revision: revision,
                login_generation,
                expires: Instant::now() + Duration::from_secs(600),
                flow,
            })
        })
        .await
    }

    pub async fn finish_login(
        &self,
        pending: PendingLogin,
        callback: url::Url,
    ) -> Result<CapturedAccount, AuthError> {
        if pending.service != self.service || pending.expires <= Instant::now() {
            return Err(AuthError::LoginExpired);
        }
        let service = self.clone();
        self.retain(async move {
            service.validate_login(pending.selection_revision, pending.login_generation)?;
            let result = microsoft::finish_login(pending.flow, &callback).await?;
            service
                .commit_login(pending.selection_revision, pending.login_generation, result)
                .await
        })
        .await
    }

    /// Closing the native OAuth window invalidates even a callback whose
    /// provider request has already started. No credentials are touched here.
    pub fn cancel_login(&self) {
        self.login_generation.fetch_add(1, Ordering::SeqCst);
    }

    fn validate_login(&self, revision: u64, generation: u64) -> Result<(), AuthError> {
        if self.directory.selection_revision()? != revision
            || self.login_generation.load(Ordering::SeqCst) != generation
        {
            return Err(AccountError::StaleCapture.into());
        }
        Ok(())
    }

    async fn commit_login(
        &self,
        revision: u64,
        generation: u64,
        result: MicrosoftMinecraftSession,
    ) -> Result<CapturedAccount, AuthError> {
        let _gate = self.mutation.lock().await;
        self.validate_login(revision, generation)?;
        microsoft::validate_profile(&result.profile)?;
        let account_id = microsoft_account_id(&result.profile.id)?;
        let mut status = self.credentials.status(&account_id).await?;
        // Explicit reauthentication is also the recovery for a refresh that
        // may have consumed a token before a prior process stopped.
        if status.state == CredentialState::Pending {
            let receipt = self
                .credentials
                .delete(&account_id, Some(status.revision))
                .await?;
            status.revision = receipt.revision();
        }
        let fence = self
            .credentials
            .begin_change(&account_id, status.revision)
            .await?;
        let (credentials, profile) = split_session(result)?;
        let receipt = self.credentials.save(&fence, credentials).await?;
        let identity = identity(
            profile,
            Uuid::new_v4().to_string(),
            receipt.revision(),
            true,
        );
        let committed = self.validate_login(revision, generation).and_then(|()| {
            self.directory
                .commit_microsoft(revision, identity)
                .map_err(Into::into)
        });
        match committed {
            Ok(capture) => Ok(capture),
            Err(error) => {
                // The selection may change during OS I/O. Revoke only the
                // acknowledged candidate, never a newer login's credentials.
                let _ = self
                    .credentials
                    .delete(&account_id, Some(receipt.revision()))
                    .await;
                Err(error)
            }
        }
    }

    /// Secret output is private, non-serializable, and bound to the exact
    /// metadata credential revision. Reading never refreshes implicitly.
    pub async fn credentials(&self, capture: &CapturedAccount) -> Result<Credentials, AuthError> {
        let _gate = self.mutation.lock().await;
        self.credentials_inner(capture).await
    }

    async fn credentials_inner(&self, capture: &CapturedAccount) -> Result<Credentials, AuthError> {
        if capture.kind() != AccountKind::Microsoft {
            return Err(AccountError::NotMicrosoft.into());
        }
        self.directory.validate_account_capture(capture)?;
        if capture.credential_revision() == 0 {
            return Err(AuthError::SignInRequired);
        }
        let stored = self
            .credentials
            .load(capture.account_id())
            .await?
            .ok_or(AuthError::SignInRequired)?;
        if stored.revision() != capture.credential_revision() {
            return Err(AuthError::SignInRequired);
        }
        self.directory.validate_account_capture(capture)?;
        Ok(stored.credentials().clone())
    }

    pub async fn launch_credentials(
        &self,
        capture: &CapturedAccount,
    ) -> Result<Credentials, AuthError> {
        self.directory.validate_capture(capture)?;
        let credentials = self.credentials(capture).await?;
        if !capture.owns_minecraft_java() {
            return Err(AuthError::OwnershipMissing);
        }
        if credentials.minecraft_expires_at() <= now_seconds().saturating_add(30) {
            return Err(AuthError::SignInRequired);
        }
        self.directory.validate_capture(capture)?;
        Ok(credentials)
    }

    pub async fn refresh_selected(&self) -> Result<CapturedAccount, AuthError> {
        let capture = self.directory.capture_selected()?;
        self.refresh(capture).await
    }

    pub async fn refresh(&self, capture: CapturedAccount) -> Result<CapturedAccount, AuthError> {
        let service = self.clone();
        self.retain(async move {
            match service.launch_credentials(&capture).await {
                Ok(_) => return Ok(capture),
                Err(AuthError::SignInRequired | AuthError::OwnershipMissing) => {}
                Err(error) => return Err(error),
            }
            let (fence, refresh_token) = service.begin_refresh(&capture).await?;
            // Failure leaves the pending marker intact: the old refresh token
            // may already have been rotated by the provider.
            let result = microsoft::refresh_login(&refresh_token).await?;
            service.finish_refresh(&capture, &fence, result).await
        })
        .await
    }

    async fn begin_refresh(
        &self,
        capture: &CapturedAccount,
    ) -> Result<(CredentialFence, String), AuthError> {
        let _gate = self.mutation.lock().await;
        self.directory.validate_capture(capture)?;
        let credentials = self.credentials_inner(capture).await?;
        let refresh = credentials
            .microsoft_refresh_token()
            .ok_or(AuthError::SignInRequired)?
            .to_owned();
        let fence = self
            .credentials
            .begin_change(capture.account_id(), capture.credential_revision())
            .await?;
        Ok((fence, refresh))
    }

    async fn finish_refresh(
        &self,
        capture: &CapturedAccount,
        fence: &CredentialFence,
        result: MicrosoftMinecraftSession,
    ) -> Result<CapturedAccount, AuthError> {
        let _gate = self.mutation.lock().await;
        self.directory.validate_capture(capture)?;
        if microsoft_account_id(&result.profile.id)? != capture.account_id() {
            return Err(AccountError::StaleCapture.into());
        }
        let (credentials, profile) = split_session(result)?;
        let receipt = self.credentials.save(fence, credentials).await?;
        let identity = identity(
            profile,
            capture
                .login_id()
                .ok_or(AuthError::SignInRequired)?
                .to_owned(),
            receipt.revision(),
            true,
        );
        self.directory
            .refresh_microsoft(capture, identity)
            .map_err(Into::into)
    }

    pub async fn sync_selected_profile(&self) -> Result<CapturedAccount, AuthError> {
        let capture = self.directory.capture_selected()?;
        let service = self.clone();
        self.retain(async move {
            let credentials = service.credentials(&capture).await?;
            if credentials.minecraft_expires_at() <= now_seconds() {
                return Err(AuthError::SignInRequired);
            }
            let synced = service
                .profile_client
                .sync(credentials.minecraft_access_token())
                .await?;
            let _gate = service.mutation.lock().await;
            service.credentials_inner(&capture).await?;
            let identity = identity(
                synced.profile,
                capture
                    .login_id()
                    .ok_or(AuthError::SignInRequired)?
                    .to_owned(),
                capture.credential_revision(),
                synced.owns_minecraft_java,
            );
            service
                .directory
                .refresh_microsoft(&capture, identity)
                .map_err(Into::into)
        })
        .await
    }

    /// Used after an acknowledged skin/cape change and a provider profile read.
    pub async fn commit_profile(
        &self,
        capture: &CapturedAccount,
        profile: MinecraftProfile,
    ) -> Result<CapturedAccount, AuthError> {
        let _gate = self.mutation.lock().await;
        self.credentials_inner(capture).await?;
        microsoft::validate_profile(&profile)?;
        let identity = identity(
            profile,
            capture
                .login_id()
                .ok_or(AuthError::SignInRequired)?
                .to_owned(),
            capture.credential_revision(),
            capture.owns_minecraft_java(),
        );
        self.directory
            .refresh_account_microsoft(capture, identity)
            .map_err(Into::into)
    }

    /// The callback performs only short synchronous local metadata work. It is
    /// fenced against refresh, logout and profile commits for this service.
    pub async fn with_account_capture<T>(
        &self,
        capture: &CapturedAccount,
        apply: impl FnOnce() -> Result<T, AuthError>,
    ) -> Result<T, AuthError> {
        let _gate = self.mutation.lock().await;
        self.credentials_inner(capture).await?;
        apply()
    }

    pub async fn select_account(
        &self,
        account_id: String,
        expected: AccountPreconditions,
    ) -> Result<AccountSnapshot, AuthError> {
        let service = self.clone();
        self.retain(async move {
            let _gate = service.mutation.lock().await;
            let capture = service.directory.capture(&account_id)?;
            check_expected(&capture, expected)?;
            if capture.kind() == AccountKind::Microsoft {
                let credentials = service.credentials_inner(&capture).await?;
                if credentials.microsoft_expires_at() <= now_seconds()
                    && credentials.microsoft_refresh_token().is_none()
                {
                    return Err(AuthError::SignInRequired);
                }
            }
            service
                .directory
                .select_with_preconditions(&account_id, expected)
                .map_err(Into::into)
        })
        .await
    }

    pub async fn remove_account(
        &self,
        account_id: String,
        expected: AccountPreconditions,
    ) -> Result<AccountSnapshot, AuthError> {
        let service = self.clone();
        self.retain(async move {
            let _gate = service.mutation.lock().await;
            let capture = service.directory.capture(&account_id)?;
            check_expected(&capture, expected)?;
            let snapshot = if capture.kind() == AccountKind::Offline {
                service
                    .directory
                    .remove_offline_with_preconditions(&account_id, expected)?
            } else {
                // Delete also revokes an in-flight refresh's pending fence.
                let receipt = service.credentials.delete(&account_id, None).await?;
                if receipt.cleanup_pending() {
                    return Err(CredentialError::CleanupPending.into());
                }
                service.directory.remove_microsoft(&capture)?
            };
            service.notify_account_removed(&account_id);
            service.select_fallback(snapshot).await
        })
        .await
    }

    async fn select_fallback(
        &self,
        snapshot: AccountSnapshot,
    ) -> Result<AccountSnapshot, AuthError> {
        if snapshot.active_account_id.is_some() {
            return Ok(snapshot);
        }
        for account in &snapshot.accounts {
            if account.kind != AccountKind::Microsoft {
                continue;
            }
            let capture = CapturedAccount::new(&snapshot, account.account_id.as_str())?;
            let Ok(credentials) = self.credentials_inner(&capture).await else {
                continue;
            };
            if credentials.microsoft_expires_at() > now_seconds()
                || credentials.microsoft_refresh_token().is_some()
            {
                return self
                    .directory
                    .select_with_preconditions(
                        capture.account_id(),
                        AccountPreconditions {
                            expected_selection_revision: Some(snapshot.selection_revision),
                            expected_account_revision: Some(capture.account_revision()),
                        },
                    )
                    .map_err(Into::into);
            }
        }
        Ok(snapshot)
    }

    pub async fn logout_selected(&self) -> Result<AccountSnapshot, AuthError> {
        let snapshot = self.directory.snapshot()?;
        let Some(account) = snapshot.active_account() else {
            return Ok(snapshot);
        };
        if account.kind != AccountKind::Microsoft {
            return Ok(snapshot);
        }
        self.remove_account(
            account.account_id.to_string(),
            AccountPreconditions {
                expected_selection_revision: Some(snapshot.selection_revision),
                expected_account_revision: Some(account.account_revision),
            },
        )
        .await
    }

    /// Retained /auth/logout semantics clear all Microsoft identities while
    /// leaving offline identities available, including when one is selected.
    pub async fn logout(&self) -> Result<AccountSnapshot, AuthError> {
        self.cancel_login();
        let service = self.clone();
        self.retain(async move {
            let _gate = service.mutation.lock().await;
            let snapshot = service.directory.snapshot()?;
            for account in snapshot
                .accounts
                .iter()
                .filter(|account| account.kind == AccountKind::Microsoft)
            {
                let capture = service.directory.capture(account.account_id.as_str())?;
                let receipt = service
                    .credentials
                    .delete(capture.account_id(), None)
                    .await?;
                if receipt.cleanup_pending() {
                    return Err(CredentialError::CleanupPending.into());
                }
                service.directory.remove_microsoft(&capture)?;
                service.notify_account_removed(capture.account_id());
            }
            service.directory.snapshot().map_err(Into::into)
        })
        .await
    }

    pub(crate) async fn retain<T: Send + 'static>(
        &self,
        operation: impl Future<Output = Result<T, AuthError>> + Send + 'static,
    ) -> Result<T, AuthError> {
        self.tasks
            .try_spawn((), move |_| operation)
            .map_err(|_| AuthError::Unavailable)?
            .join()
            .await
            .map_err(|_| AuthError::Unavailable)?
    }
}

fn split_session(
    session: MicrosoftMinecraftSession,
) -> Result<(Credentials, MinecraftProfile), AuthError> {
    microsoft::validate_profile(&session.profile)?;
    let credentials = Credentials::new(
        session.microsoft_access_token,
        session.microsoft_refresh_token,
        session.microsoft_expires_at,
        session.minecraft_access_token,
        session.minecraft_expires_at,
    )?;
    Ok((credentials, session.profile))
}

fn identity(
    profile: MinecraftProfile,
    login_id: String,
    credential_revision: u64,
    owns_minecraft_java: bool,
) -> MicrosoftIdentity {
    MicrosoftIdentity {
        login_id,
        profile_id: profile.id.clone(),
        display_name: profile.name.clone(),
        credential_revision,
        profile,
        owns_minecraft_java,
    }
}

fn check_expected(
    capture: &CapturedAccount,
    expected: AccountPreconditions,
) -> Result<(), AccountError> {
    if expected
        .expected_selection_revision
        .is_some_and(|revision| revision != capture.selection_revision())
        || expected
            .expected_account_revision
            .is_some_and(|revision| revision != capture.account_revision())
    {
        return Err(AccountError::StaleCapture);
    }
    Ok(())
}

pub(crate) fn now_seconds() -> u64 {
    chrono::Utc::now().timestamp().max(0) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::MetadataStore;

    fn service() -> AuthService {
        AuthService::new(
            Arc::new(AccountDirectory::new(Arc::new(MetadataStore::in_memory().unwrap())).unwrap()),
            Arc::new(CredentialStore::isolated_for_tests()),
            TaskOwner::new(32).unwrap(),
        )
    }

    fn provider_result(id: &str, name: &str, secret: &str) -> MicrosoftMinecraftSession {
        MicrosoftMinecraftSession {
            microsoft_access_token: format!("msa-{secret}"),
            microsoft_refresh_token: Some(format!("refresh-{secret}")),
            microsoft_expires_at: now_seconds() + 3600,
            minecraft_access_token: format!("game-{secret}"),
            minecraft_expires_at: now_seconds() + 3600,
            profile: MinecraftProfile {
                id: id.into(),
                name: name.into(),
                skins: vec![],
                capes: vec![],
            },
        }
    }

    async fn login(service: &AuthService, id: &str, name: &str) -> CapturedAccount {
        service
            .commit_login(
                service.directory.selection_revision().unwrap(),
                service.login_generation.load(Ordering::SeqCst),
                provider_result(id, name, name),
            )
            .await
            .unwrap()
    }

    const FIRST: &str = "12345678123442348234123456789abc";
    const SECOND: &str = "22345678123442348234123456789abc";

    #[tokio::test]
    async fn provider_result_commits_credentials_before_metadata_and_projects_no_tokens() {
        let service = service();
        let capture = login(&service, FIRST, "PlayerOne").await;
        assert_eq!(capture.credential_revision(), 1);
        assert_eq!(
            service
                .launch_credentials(&capture)
                .await
                .unwrap()
                .minecraft_access_token(),
            "game-PlayerOne"
        );
        let status = service.status(false).await.unwrap();
        assert!(status.readiness.online_mode_ready);
        let json = serde_json::to_string(&status).unwrap();
        for secret in [
            "msa-PlayerOne",
            "refresh-PlayerOne",
            "game-PlayerOne",
            "access_token",
            "refresh_token",
        ] {
            assert!(!json.contains(secret));
        }
    }

    #[tokio::test]
    async fn ready_selected_refresh_without_refresh_token_preserves_capture_and_credentials() {
        let service = service();
        let mut result = provider_result(FIRST, "PlayerOne", "ready");
        result.microsoft_refresh_token = None;
        let original = service
            .commit_login(
                service.directory.selection_revision().unwrap(),
                service.login_generation.load(Ordering::SeqCst),
                result,
            )
            .await
            .unwrap();
        let snapshot = service.directory.snapshot().unwrap();
        let credentials = service.launch_credentials(&original).await.unwrap();
        assert!(credentials.microsoft_refresh_token().is_none());
        let credential_status = service
            .credentials
            .status(original.account_id())
            .await
            .unwrap();
        assert_eq!(credential_status.state, CredentialState::Ready);

        let refreshed = service.refresh_selected().await.unwrap();

        assert_eq!(refreshed.account_id(), original.account_id());
        assert_eq!(refreshed.login_id(), original.login_id());
        assert_eq!(refreshed.profile(), original.profile());
        assert_eq!(
            (
                refreshed.selection_revision(),
                refreshed.account_revision(),
                refreshed.profile_revision(),
                refreshed.credential_revision(),
            ),
            (
                original.selection_revision(),
                original.account_revision(),
                original.profile_revision(),
                original.credential_revision(),
            )
        );
        assert_eq!(service.directory.snapshot().unwrap(), snapshot);
        assert_eq!(
            service.launch_credentials(&refreshed).await.unwrap(),
            credentials
        );
        assert_eq!(
            service
                .credentials
                .status(refreshed.account_id())
                .await
                .unwrap(),
            credential_status
        );
    }

    #[tokio::test]
    async fn selected_refresh_requires_refresh_credentials_for_expired_or_near_expiry_tokens() {
        for remaining_seconds in [0, 15] {
            let service = service();
            let mut result = provider_result(FIRST, "PlayerOne", "expiring");
            result.microsoft_refresh_token = None;
            result.minecraft_expires_at = now_seconds() + remaining_seconds;
            let capture = service
                .commit_login(
                    service.directory.selection_revision().unwrap(),
                    service.login_generation.load(Ordering::SeqCst),
                    result,
                )
                .await
                .unwrap();
            let snapshot = service.directory.snapshot().unwrap();
            let credentials = service.credentials(&capture).await.unwrap();
            assert!(matches!(
                service.launch_credentials(&capture).await,
                Err(AuthError::SignInRequired)
            ));

            assert!(matches!(
                service.refresh_selected().await,
                Err(AuthError::SignInRequired)
            ));

            assert_eq!(service.directory.snapshot().unwrap(), snapshot);
            assert_eq!(service.credentials(&capture).await.unwrap(), credentials);
        }
    }

    #[tokio::test]
    async fn ready_refresh_preserves_selection_kind_and_shutdown_refusals() {
        let service = service();
        let mut result = provider_result(FIRST, "PlayerOne", "ready");
        result.microsoft_refresh_token = None;
        let capture = service
            .commit_login(
                service.directory.selection_revision().unwrap(),
                service.login_generation.load(Ordering::SeqCst),
                result,
            )
            .await
            .unwrap();
        service.directory.create_offline_account("Steve").unwrap();
        assert!(matches!(
            service.refresh(capture.clone()).await,
            Err(AuthError::Account(AccountError::StaleCapture))
        ));
        assert!(matches!(
            service.refresh_selected().await,
            Err(AuthError::Account(AccountError::NotMicrosoft))
        ));
        let selected = service
            .select_account(capture.account_id().into(), AccountPreconditions::default())
            .await
            .unwrap();
        assert_eq!(
            selected.active_account_id.as_ref(),
            Some(capture.identity())
        );
        service.tasks.close_admission();
        assert!(matches!(
            service.refresh_selected().await,
            Err(AuthError::Unavailable)
        ));
    }

    #[tokio::test]
    async fn account_switch_and_cancel_reject_late_login_without_keyring_publication() {
        let service = service();
        let revision = service.directory.selection_revision().unwrap();
        service.directory.create_offline_account("Steve").unwrap();
        assert!(matches!(
            service
                .commit_login(revision, 0, provider_result(FIRST, "PlayerOne", "late"))
                .await,
            Err(AuthError::Account(AccountError::StaleCapture))
        ));
        assert_eq!(
            service.credentials.status(FIRST).await.unwrap().state,
            CredentialState::Absent
        );
        let revision = service.directory.selection_revision().unwrap();
        service.cancel_login();
        assert!(matches!(
            service
                .commit_login(revision, 0, provider_result(FIRST, "PlayerOne", "late"))
                .await,
            Err(AuthError::Account(AccountError::StaleCapture))
        ));
    }

    #[tokio::test]
    async fn logout_revokes_refresh_and_old_completion_cannot_resurrect_relogin() {
        let service = service();
        let original = login(&service, FIRST, "PlayerOne").await;
        let (fence, _) = service.begin_refresh(&original).await.unwrap();
        service.logout().await.unwrap();
        assert_eq!(
            service.credentials.status(FIRST).await.unwrap().state,
            CredentialState::Deleted
        );
        let replacement = login(&service, FIRST, "PlayerOne").await;
        assert_ne!(original.login_id(), replacement.login_id());
        assert!(
            service
                .finish_refresh(
                    &original,
                    &fence,
                    provider_result(FIRST, "PlayerOne", "obsolete")
                )
                .await
                .is_err()
        );
        assert_eq!(
            service
                .credentials(&replacement)
                .await
                .unwrap()
                .minecraft_access_token(),
            "game-PlayerOne"
        );
        assert!(
            service
                .with_account_capture(&original, || -> Result<(), AuthError> {
                    panic!("stale callback must not execute")
                })
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn interrupted_refresh_requires_explicit_reauthentication() {
        let service = service();
        let capture = login(&service, FIRST, "PlayerOne").await;
        let _interrupted = service.begin_refresh(&capture).await.unwrap();
        assert!(matches!(
            service.credentials(&capture).await,
            Err(AuthError::Credentials(CredentialError::Unresolved))
        ));
        assert!(matches!(
            service.refresh_selected().await,
            Err(AuthError::Credentials(CredentialError::Unresolved))
        ));
        assert!(
            !service
                .status(false)
                .await
                .unwrap()
                .readiness
                .online_mode_ready
        );
        let recovered = login(&service, FIRST, "PlayerOne").await;
        assert!(recovered.credential_revision() > capture.credential_revision());
        assert!(service.launch_credentials(&recovered).await.is_ok());
    }

    #[tokio::test]
    async fn logout_clears_all_microsoft_accounts_even_with_offline_selection() {
        let service = service();
        login(&service, FIRST, "PlayerOne").await;
        login(&service, SECOND, "PlayerTwo").await;
        service.directory.create_offline_account("Steve").unwrap();
        let result = service.logout().await.unwrap();
        assert_eq!(result.accounts.len(), 1);
        assert_eq!(result.active_account().unwrap().display_name, "Steve");
        for id in [FIRST, SECOND] {
            assert!(service.credentials.load(id).await.unwrap().is_none());
        }
    }

    #[tokio::test]
    async fn profile_completion_is_bound_to_identity_and_revisions() {
        let service = service();
        let capture = login(&service, FIRST, "PlayerOne").await;
        service.directory.create_offline_account("Steve").unwrap();
        let mut profile = capture.profile().unwrap().clone();
        profile.name = "NewName".into();
        let next = service
            .commit_profile(&capture, profile.clone())
            .await
            .unwrap();
        assert!(next.profile_revision() > capture.profile_revision());
        assert_eq!(
            service.directory.capture_selected().unwrap().display_name(),
            "Steve"
        );
        assert!(service.commit_profile(&capture, profile).await.is_err());
    }

    #[tokio::test]
    async fn profile_only_commit_preserves_negative_ownership_and_explicit_refresh_can_recheck() {
        let service = service();
        let original = login(&service, FIRST, "PlayerOne").await;
        let credentials = service.credentials(&original).await.unwrap();
        let unowned = service
            .directory
            .refresh_microsoft(
                &original,
                identity(
                    original.profile().unwrap().clone(),
                    original.login_id().unwrap().into(),
                    original.credential_revision(),
                    false,
                ),
            )
            .unwrap();
        let mut profile = unowned.profile().unwrap().clone();
        profile.name = "UpdatedPlayer".into();
        let updated = service.commit_profile(&unowned, profile).await.unwrap();
        assert!(!updated.owns_minecraft_java());
        assert_eq!(
            updated.credential_revision(),
            original.credential_revision()
        );
        assert_eq!(service.credentials(&updated).await.unwrap(), credentials);
        assert!(matches!(
            service.launch_credentials(&updated).await,
            Err(AuthError::OwnershipMissing)
        ));
        let status = service.status(false).await.unwrap();
        assert!(!status.readiness.online_mode_ready);
        assert!(
            status.readiness.refresh_action.enabled && status.readiness.profile_sync_action.enabled
        );
        assert!(
            status
                .readiness
                .online_action
                .detail
                .unwrap()
                .contains("ownership")
        );
        let (fence, _) = service.begin_refresh(&updated).await.unwrap();
        let refreshed = service
            .finish_refresh(
                &updated,
                &fence,
                provider_result(FIRST, "UpdatedPlayer", "refreshed"),
            )
            .await
            .unwrap();
        assert!(refreshed.owns_minecraft_java());
        assert!(service.launch_credentials(&refreshed).await.is_ok());
    }

    #[tokio::test]
    async fn removal_notifies_only_committed_identities_and_logout_notifies_each_account() {
        let service = service();
        let first = login(&service, FIRST, "PlayerOne").await;
        login(&service, SECOND, "PlayerTwo").await;
        let observed = Arc::new(std::sync::Mutex::new(Vec::new()));
        let observer_output = observed.clone();
        let directory = Arc::downgrade(service.directory());
        service.set_account_removed_observer(Arc::new(move |id| {
            let snapshot = directory.upgrade().unwrap().snapshot().unwrap();
            assert!(
                snapshot
                    .accounts
                    .iter()
                    .all(|account| account.account_id.as_str() != id)
            );
            observer_output.lock().unwrap().push(id.to_owned());
        }));
        assert!(
            service
                .remove_account(
                    first.account_id().into(),
                    AccountPreconditions {
                        expected_selection_revision: Some(first.selection_revision()),
                        expected_account_revision: None,
                    }
                )
                .await
                .is_err()
        );
        assert!(observed.lock().unwrap().is_empty());
        service
            .remove_account(first.account_id().into(), AccountPreconditions::default())
            .await
            .unwrap();
        assert_eq!(observed.lock().unwrap().as_slice(), [first.account_id()]);
        service.logout().await.unwrap();
        let removed = observed.lock().unwrap();
        assert_eq!(removed.len(), 2);
        assert_eq!(removed[1], microsoft_account_id(SECOND).unwrap());
    }
}
