use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as SyncMutex, RwLock as SyncRwLock};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::{Mutex as AsyncMutex, MutexGuard, OwnedMutexGuard};

use super::auth_persistence::SecureAuthSnapshotPersistence;
use super::auth_persistence::{
    AuthPersistenceError, AuthSnapshotPersistence, AuthSnapshotRejection, PersistedAuthSnapshot,
    PersistedAuthState, SecureAuthPersistenceMode,
};

#[derive(Clone, Eq, PartialEq)]
pub struct AuthLoginMsaToken {
    pub login_id: String,
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub id_token: Option<String>,
    pub token_type: String,
    pub expires_in: u64,
    pub scope: Option<String>,
    pub authenticated_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

impl fmt::Debug for AuthLoginMsaToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AuthLoginMsaToken")
            .field("login_id", &self.login_id)
            .field("access_token", &"[redacted]")
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "[redacted]"),
            )
            .field("id_token", &self.id_token.as_ref().map(|_| "[redacted]"))
            .field("token_type", &self.token_type)
            .field("expires_in", &self.expires_in)
            .field("scope", &self.scope)
            .field("authenticated_at", &self.authenticated_at)
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct AuthLoginMinecraftAccount {
    pub login_id: String,
    pub access_token: String,
    pub token_type: Option<String>,
    pub expires_in: u64,
    pub profile: AuthLoginMinecraftProfile,
    pub owns_minecraft_java: bool,
    pub authenticated_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

impl fmt::Debug for AuthLoginMinecraftAccount {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AuthLoginMinecraftAccount")
            .field("login_id", &self.login_id)
            .field("access_token", &"[redacted]")
            .field("token_type", &self.token_type)
            .field("expires_in", &self.expires_in)
            .field("profile", &self.profile)
            .field("owns_minecraft_java", &self.owns_minecraft_java)
            .field("authenticated_at", &self.authenticated_at)
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthLoginMinecraftProfile {
    pub id: String,
    pub name: String,
    pub skins: Vec<AuthLoginMinecraftSkin>,
    pub capes: Vec<AuthLoginMinecraftCape>,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthLoginMinecraftSkin {
    pub id: String,
    pub state: String,
    pub url: String,
    pub variant: String,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthLoginMinecraftCape {
    pub id: String,
    pub state: String,
    pub url: String,
}

#[derive(Clone, Eq, PartialEq)]
pub struct NewAuthLoginMsaToken {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub id_token: Option<String>,
    pub token_type: String,
    pub expires_in: u64,
    pub scope: Option<String>,
}

impl fmt::Debug for NewAuthLoginMsaToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NewAuthLoginMsaToken")
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

#[derive(Clone, Eq, PartialEq)]
pub struct NewAuthLoginMinecraftAccount {
    pub access_token: String,
    pub token_type: Option<String>,
    pub expires_in: u64,
    pub profile: AuthLoginMinecraftProfile,
    pub owns_minecraft_java: bool,
}

impl fmt::Debug for NewAuthLoginMinecraftAccount {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NewAuthLoginMinecraftAccount")
            .field("access_token", &"[redacted]")
            .field("token_type", &self.token_type)
            .field("expires_in", &self.expires_in)
            .field("profile", &self.profile)
            .field("owns_minecraft_java", &self.owns_minecraft_java)
            .finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActiveMinecraftAccountState {
    pub account: AuthLoginMinecraftAccount,
    pub token_expires_in: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActiveMsaTokenState {
    pub token: AuthLoginMsaToken,
    pub token_expires_in: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthLoginAccountState {
    pub login_id: String,
    pub active: bool,
    pub msa_authenticated: bool,
    pub msa_token_expires_in: Option<u64>,
    pub msa_refresh_available: bool,
    pub minecraft_account: Option<AuthLoginMinecraftAccount>,
    pub minecraft_token_expires_in: Option<u64>,
}

const AUTH_STORE_LOCK_INVARIANT: &str =
    "auth login store lock poisoned; committed and persisted credentials may diverge";

#[derive(Clone, Default)]
struct AuthLoginCommittedState {
    msa_tokens: HashMap<String, AuthLoginMsaToken>,
    minecraft_accounts: HashMap<String, AuthLoginMinecraftAccount>,
    active_login_id: Option<String>,
}

#[derive(Clone)]
struct PendingAuthCommit {
    candidate: AuthLoginCommittedState,
    bump_generation: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AuthLoginStoreLifecycle {
    Open,
    Closed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AuthLoginStartupState {
    Ready,
    Rejected,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum AuthLoginStoreError {
    #[error("secure auth persistence failed")]
    Persistence(#[source] AuthPersistenceError),
    #[error("secure auth persistence task stopped")]
    BlockingTask,
    #[error("secure auth state is unavailable after startup rejection")]
    MutationLatched,
    #[error("secure auth store is closed")]
    Closed,
}

impl From<AuthPersistenceError> for AuthLoginStoreError {
    fn from(error: AuthPersistenceError) -> Self {
        Self::Persistence(error)
    }
}

pub struct AuthLoginStore {
    committed: Arc<SyncRwLock<AuthLoginCommittedState>>,
    mutation_gate: Arc<AsyncMutex<()>>,
    pending_commit: Arc<SyncMutex<Option<PendingAuthCommit>>>,
    lifecycle: Arc<SyncMutex<AuthLoginStoreLifecycle>>,
    startup_state: AuthLoginStartupState,
    mutation_latched: Arc<AtomicBool>,
    load_issue_count: usize,
    active_auth_refresh: AsyncMutex<()>,
    active_auth_generation: Arc<AtomicU64>,
    persistence: Option<Arc<dyn AuthSnapshotPersistence>>,
    persistence_mode: SecureAuthPersistenceMode,
    next_id: AtomicU64,
}

impl AuthLoginStore {
    #[cfg(test)]
    pub fn new() -> Self {
        Self {
            committed: Arc::new(SyncRwLock::new(AuthLoginCommittedState::default())),
            mutation_gate: Arc::new(AsyncMutex::new(())),
            pending_commit: Arc::new(SyncMutex::new(None)),
            lifecycle: Arc::new(SyncMutex::new(AuthLoginStoreLifecycle::Open)),
            startup_state: AuthLoginStartupState::Ready,
            mutation_latched: Arc::new(AtomicBool::new(false)),
            load_issue_count: 0,
            active_auth_refresh: AsyncMutex::new(()),
            active_auth_generation: Arc::new(AtomicU64::new(0)),
            persistence: None,
            persistence_mode: SecureAuthPersistenceMode::OsKeyring,
            next_id: AtomicU64::new(1),
        }
    }

    pub async fn load_from_secure_store() -> std::io::Result<Self> {
        #[cfg(test)]
        {
            Ok(Self::new())
        }

        #[cfg(not(test))]
        {
            let persistence =
                tokio::task::spawn_blocking(SecureAuthSnapshotPersistence::for_app_startup)
                    .await
                    .map_err(|_| {
                        std::io::Error::other("secure auth persistence startup task stopped")
                    })??;
            Ok(Self::with_persistence(Arc::new(persistence)).await)
        }
    }

    pub(crate) async fn with_persistence(persistence: Arc<dyn AuthSnapshotPersistence>) -> Self {
        let persistence_mode = persistence.mode();
        let load_persistence = persistence.clone();
        let loaded = tokio::task::spawn_blocking(move || load_persistence.load_snapshot()).await;
        let (state, load_issue_count, startup_state) = match loaded {
            Ok(Ok(Some(snapshot))) => match snapshot.into_state(Utc::now()) {
                Ok(state) => (state, 0, AuthLoginStartupState::Ready),
                Err(AuthSnapshotRejection::Expired) => (
                    empty_persisted_auth_state(),
                    1,
                    AuthLoginStartupState::Ready,
                ),
                Err(AuthSnapshotRejection::Malformed) => (
                    empty_persisted_auth_state(),
                    1,
                    AuthLoginStartupState::Rejected,
                ),
            },
            Ok(Ok(None)) => (
                empty_persisted_auth_state(),
                0,
                AuthLoginStartupState::Ready,
            ),
            Ok(Err(_)) | Err(_) => (
                empty_persisted_auth_state(),
                1,
                AuthLoginStartupState::Rejected,
            ),
        };
        Self {
            committed: Arc::new(SyncRwLock::new(AuthLoginCommittedState {
                msa_tokens: state
                    .msa_tokens
                    .into_iter()
                    .map(|token| (token.login_id.clone(), token))
                    .collect(),
                minecraft_accounts: state
                    .minecraft_accounts
                    .into_iter()
                    .map(|account| (account.login_id.clone(), account))
                    .collect(),
                active_login_id: state.active_login_id,
            })),
            mutation_gate: Arc::new(AsyncMutex::new(())),
            pending_commit: Arc::new(SyncMutex::new(None)),
            lifecycle: Arc::new(SyncMutex::new(AuthLoginStoreLifecycle::Open)),
            startup_state,
            mutation_latched: Arc::new(AtomicBool::new(false)),
            load_issue_count,
            active_auth_refresh: AsyncMutex::new(()),
            active_auth_generation: Arc::new(AtomicU64::new(0)),
            persistence: Some(persistence),
            persistence_mode,
            next_id: AtomicU64::new(1),
        }
    }

    pub fn load_issue_count(&self) -> usize {
        self.load_issue_count
    }

    pub(crate) fn secure_auth_persistence_mode(&self) -> &'static str {
        self.persistence_mode.as_str()
    }

    #[cfg(test)]
    pub(crate) async fn isolated_volatile_for_test() -> Self {
        Self::with_persistence(Arc::new(
            SecureAuthSnapshotPersistence::isolated_volatile_for_test(),
        ))
        .await
    }

    async fn begin_mutation(
        &self,
    ) -> Result<(OwnedMutexGuard<()>, AuthLoginCommittedState), AuthLoginStoreError> {
        let mutation = self.mutation_gate.clone().lock_owned().await;
        let mutation = self.prepare_mutation_gate(mutation).await?;
        let state = self
            .committed
            .read()
            .expect(AUTH_STORE_LOCK_INVARIANT)
            .clone();
        Ok((mutation, state))
    }

    async fn prepare_mutation_gate(
        &self,
        mut mutation: OwnedMutexGuard<()>,
    ) -> Result<OwnedMutexGuard<()>, AuthLoginStoreError> {
        self.ensure_mutation_allowed()?;
        let pending = self
            .pending_commit
            .lock()
            .expect(AUTH_STORE_LOCK_INVARIANT)
            .clone();
        if let Some(pending) = pending {
            mutation = self
                .commit_candidate_holding_gate(pending.candidate, pending.bump_generation, mutation)
                .await?;
        }
        Ok(mutation)
    }

    async fn commit_candidate(
        &self,
        candidate: AuthLoginCommittedState,
        mutation: OwnedMutexGuard<()>,
    ) -> Result<(), AuthLoginStoreError> {
        let mutation = self
            .commit_candidate_holding_gate(candidate, true, mutation)
            .await?;
        drop(mutation);
        Ok(())
    }

    async fn commit_candidate_holding_gate(
        &self,
        candidate: AuthLoginCommittedState,
        bump_generation: bool,
        mutation: OwnedMutexGuard<()>,
    ) -> Result<OwnedMutexGuard<()>, AuthLoginStoreError> {
        let committed = self.committed.clone();
        let pending_commit = self.pending_commit.clone();
        let persistence = self.persistence.clone();
        let active_auth_generation = self.active_auth_generation.clone();
        let task_mutation_latched = self.mutation_latched.clone();
        let observer_mutation_latched = self.mutation_latched.clone();
        let task_candidate = candidate.clone();
        let (completed_tx, completed_rx) = tokio::sync::oneshot::channel();
        drop(tokio::spawn(async move {
            let persisted = persist_auth_state_blocking(persistence, task_candidate).await;
            let result = match persisted {
                Ok(()) => {
                    *committed.write().expect(AUTH_STORE_LOCK_INVARIANT) = candidate;
                    *pending_commit.lock().expect(AUTH_STORE_LOCK_INVARIANT) = None;
                    if bump_generation {
                        active_auth_generation.fetch_add(1, Ordering::AcqRel);
                    }
                    Ok(())
                }
                Err(error) => {
                    if matches!(
                        error,
                        AuthLoginStoreError::Persistence(
                            AuthPersistenceError::Unavailable
                                | AuthPersistenceError::CleanupPending
                        )
                    ) {
                        *pending_commit.lock().expect(AUTH_STORE_LOCK_INVARIANT) =
                            Some(PendingAuthCommit {
                                candidate,
                                bump_generation,
                            });
                    } else {
                        task_mutation_latched.store(true, Ordering::Release);
                        *pending_commit.lock().expect(AUTH_STORE_LOCK_INVARIANT) = None;
                    }
                    Err(error)
                }
            };
            let _ = completed_tx.send((result, mutation));
        }));
        let (result, mutation) = match completed_rx.await {
            Ok(completed) => completed,
            Err(_) => {
                observer_mutation_latched.store(true, Ordering::Release);
                return Err(AuthLoginStoreError::BlockingTask);
            }
        };
        result?;
        Ok(mutation)
    }

    fn ensure_mutation_allowed(&self) -> Result<(), AuthLoginStoreError> {
        if *self.lifecycle.lock().expect(AUTH_STORE_LOCK_INVARIANT)
            == AuthLoginStoreLifecycle::Closed
        {
            return Err(AuthLoginStoreError::Closed);
        }
        if self.startup_state == AuthLoginStartupState::Rejected
            || self.mutation_latched.load(Ordering::Acquire)
        {
            return Err(AuthLoginStoreError::MutationLatched);
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) async fn replace_with_msa_token(
        &self,
        new_token: NewAuthLoginMsaToken,
    ) -> Result<AuthLoginMsaToken, AuthLoginStoreError> {
        let (mutation, mut candidate) = self.begin_mutation().await?;
        let now = Utc::now();
        let login_id = self.next_login_id();

        let token = AuthLoginMsaToken {
            login_id: login_id.clone(),
            access_token: new_token.access_token,
            refresh_token: new_token.refresh_token,
            id_token: new_token.id_token,
            token_type: new_token.token_type,
            expires_in: new_token.expires_in,
            scope: new_token.scope,
            authenticated_at: now,
            expires_at: now
                + chrono::Duration::seconds(saturating_u64_to_i64(new_token.expires_in)),
        };

        candidate
            .msa_tokens
            .insert(token.login_id.clone(), token.clone());
        candidate.minecraft_accounts.remove(&login_id);
        candidate.active_login_id = Some(token.login_id.clone());
        self.commit_candidate(candidate, mutation).await?;
        Ok(token)
    }

    pub(crate) async fn replace_with_msa_and_minecraft_account(
        &self,
        new_token: NewAuthLoginMsaToken,
        new_account: NewAuthLoginMinecraftAccount,
    ) -> Result<(AuthLoginMsaToken, AuthLoginMinecraftAccount), AuthLoginStoreError> {
        let (mutation, mut candidate) = self.begin_mutation().await?;
        let (login_id, token, account) =
            self.new_msa_and_minecraft_account_pair(&candidate, new_token, new_account, Utc::now());
        candidate
            .msa_tokens
            .insert(token.login_id.clone(), token.clone());
        candidate
            .minecraft_accounts
            .insert(account.login_id.clone(), account.clone());
        candidate.active_login_id = Some(login_id);
        self.commit_candidate(candidate, mutation).await?;
        Ok((token, account))
    }

    fn new_msa_and_minecraft_account_pair(
        &self,
        state: &AuthLoginCommittedState,
        new_token: NewAuthLoginMsaToken,
        new_account: NewAuthLoginMinecraftAccount,
        now: DateTime<Utc>,
    ) -> (String, AuthLoginMsaToken, AuthLoginMinecraftAccount) {
        let profile_id = new_account.profile.id.trim().to_string();
        let login_id = state
            .minecraft_accounts
            .values()
            .find(|account| {
                !profile_id.is_empty() && account.profile.id.eq_ignore_ascii_case(&profile_id)
            })
            .map(|account| account.login_id.clone())
            .unwrap_or_else(|| self.next_login_id());
        let token = AuthLoginMsaToken {
            login_id: login_id.clone(),
            access_token: new_token.access_token,
            refresh_token: new_token.refresh_token,
            id_token: new_token.id_token,
            token_type: new_token.token_type,
            expires_in: new_token.expires_in,
            scope: new_token.scope,
            authenticated_at: now,
            expires_at: now
                + chrono::Duration::seconds(saturating_u64_to_i64(new_token.expires_in)),
        };
        let account = AuthLoginMinecraftAccount {
            login_id: login_id.clone(),
            access_token: new_account.access_token,
            token_type: new_account.token_type,
            expires_in: new_account.expires_in,
            profile: new_account.profile,
            owns_minecraft_java: new_account.owns_minecraft_java,
            authenticated_at: now,
            expires_at: now
                + chrono::Duration::seconds(saturating_u64_to_i64(new_account.expires_in)),
        };

        (login_id, token, account)
    }

    #[cfg(test)]
    pub(crate) async fn refresh_with_msa_and_minecraft_account(
        &self,
        new_token: NewAuthLoginMsaToken,
        new_account: NewAuthLoginMinecraftAccount,
        fallback_refresh_token: &str,
    ) -> Result<Option<(AuthLoginMsaToken, AuthLoginMinecraftAccount)>, AuthLoginStoreError> {
        let login_id = self
            .committed
            .read()
            .expect(AUTH_STORE_LOCK_INVARIANT)
            .active_login_id
            .clone();
        let Some(login_id) = login_id else {
            return Ok(None);
        };
        self.refresh_login_with_msa_and_minecraft_account(
            &login_id,
            new_token,
            new_account,
            fallback_refresh_token,
        )
        .await
    }

    pub(crate) async fn refresh_login_with_msa_and_minecraft_account(
        &self,
        login_id: &str,
        new_token: NewAuthLoginMsaToken,
        new_account: NewAuthLoginMinecraftAccount,
        fallback_refresh_token: &str,
    ) -> Result<Option<(AuthLoginMsaToken, AuthLoginMinecraftAccount)>, AuthLoginStoreError> {
        let (mutation, mut candidate) = self.begin_mutation().await?;
        let now = Utc::now();
        let login_id = login_id.trim().to_string();
        if login_id.is_empty() {
            return Ok(None);
        }
        let refresh_token = new_token
            .refresh_token
            .filter(|refresh_token| !refresh_token.trim().is_empty())
            .or_else(|| Some(fallback_refresh_token.to_string()));
        let token = AuthLoginMsaToken {
            login_id: login_id.clone(),
            access_token: new_token.access_token,
            refresh_token,
            id_token: new_token.id_token,
            token_type: new_token.token_type,
            expires_in: new_token.expires_in,
            scope: new_token.scope,
            authenticated_at: now,
            expires_at: now
                + chrono::Duration::seconds(saturating_u64_to_i64(new_token.expires_in)),
        };
        let account = AuthLoginMinecraftAccount {
            login_id: login_id.clone(),
            access_token: new_account.access_token,
            token_type: new_account.token_type,
            expires_in: new_account.expires_in,
            profile: new_account.profile,
            owns_minecraft_java: new_account.owns_minecraft_java,
            authenticated_at: now,
            expires_at: now
                + chrono::Duration::seconds(saturating_u64_to_i64(new_account.expires_in)),
        };

        if !candidate.msa_tokens.contains_key(&login_id) {
            return Ok(None);
        }
        candidate
            .msa_tokens
            .insert(token.login_id.clone(), token.clone());
        candidate
            .minecraft_accounts
            .insert(account.login_id.clone(), account.clone());
        self.commit_candidate(candidate, mutation).await?;
        Ok(Some((token, account)))
    }

    #[cfg(test)]
    pub(crate) async fn refresh_with_msa_token(
        &self,
        new_token: NewAuthLoginMsaToken,
        fallback_refresh_token: &str,
    ) -> Result<Option<AuthLoginMsaToken>, AuthLoginStoreError> {
        let login_id = self
            .committed
            .read()
            .expect(AUTH_STORE_LOCK_INVARIANT)
            .active_login_id
            .clone();
        let Some(login_id) = login_id else {
            return Ok(None);
        };
        self.refresh_login_with_msa_token(&login_id, new_token, fallback_refresh_token)
            .await
    }

    #[cfg(test)]
    pub(crate) async fn refresh_login_with_msa_token(
        &self,
        login_id: &str,
        new_token: NewAuthLoginMsaToken,
        fallback_refresh_token: &str,
    ) -> Result<Option<AuthLoginMsaToken>, AuthLoginStoreError> {
        let (mutation, mut candidate) = self.begin_mutation().await?;
        let now = Utc::now();
        let login_id = login_id.trim().to_string();
        if login_id.is_empty() {
            return Ok(None);
        }
        let refresh_token = new_token
            .refresh_token
            .filter(|refresh_token| !refresh_token.trim().is_empty())
            .or_else(|| Some(fallback_refresh_token.to_string()));
        let token = AuthLoginMsaToken {
            login_id: login_id.clone(),
            access_token: new_token.access_token,
            refresh_token,
            id_token: new_token.id_token,
            token_type: new_token.token_type,
            expires_in: new_token.expires_in,
            scope: new_token.scope,
            authenticated_at: now,
            expires_at: now
                + chrono::Duration::seconds(saturating_u64_to_i64(new_token.expires_in)),
        };

        if !candidate.msa_tokens.contains_key(&login_id) {
            return Ok(None);
        }
        if candidate
            .minecraft_accounts
            .get(&login_id)
            .is_none_or(|account| account.expires_at <= now)
        {
            candidate.minecraft_accounts.remove(&login_id);
        }
        candidate
            .msa_tokens
            .insert(token.login_id.clone(), token.clone());
        self.commit_candidate(candidate, mutation).await?;
        Ok(Some(token))
    }

    pub async fn has_active_msa_auth(&self) -> bool {
        self.active_msa_auth_remaining_seconds().await.is_some()
    }

    pub async fn active_msa_auth_remaining_seconds(&self) -> Option<u64> {
        let state = self.committed.read().expect(AUTH_STORE_LOCK_INVARIANT);
        let login_id = state.active_login_id.as_ref()?;
        let token = state.msa_tokens.get(login_id)?;
        let expires_at = token.expires_at;
        let expires_in = token.expires_in;
        let remaining = (expires_at - Utc::now()).num_milliseconds();
        if remaining <= 0 {
            return None;
        }

        Some((remaining as u64).div_ceil(1000).min(expires_in))
    }

    pub async fn active_msa_refresh_token(&self) -> Option<String> {
        self.active_msa_refresh_login()
            .await
            .map(|(_, token)| token)
    }

    pub async fn active_msa_refresh_login(&self) -> Option<(String, String)> {
        let state = self.committed.read().expect(AUTH_STORE_LOCK_INVARIANT);
        let login_id = state.active_login_id.clone()?;
        let refresh_token = state
            .msa_tokens
            .get(&login_id)
            .and_then(|token| token.refresh_token.as_deref())
            .map(str::trim)
            .filter(|refresh_token| !refresh_token.is_empty())
            .map(ToOwned::to_owned)?;
        Some((login_id, refresh_token))
    }

    pub async fn account_states(&self) -> Vec<AuthLoginAccountState> {
        let now = Utc::now();
        let state = self
            .committed
            .read()
            .expect(AUTH_STORE_LOCK_INVARIANT)
            .clone();
        let mut states = state
            .msa_tokens
            .into_values()
            .map(|token| {
                let token_remaining =
                    remaining_seconds_option(token.expires_at, token.expires_in, now);
                let refresh_available = has_nonblank_refresh_token(&token);
                let account = state
                    .minecraft_accounts
                    .get(&token.login_id)
                    .cloned()
                    .filter(|account| {
                        (token_remaining.is_some() || refresh_available)
                            && account.expires_at > now
                            && account.authenticated_at >= token.authenticated_at
                    });
                let minecraft_token_expires_in = account.as_ref().and_then(|account| {
                    remaining_seconds_option(account.expires_at, account.expires_in, now)
                });
                AuthLoginAccountState {
                    login_id: token.login_id.clone(),
                    active: state.active_login_id.as_deref() == Some(token.login_id.as_str()),
                    msa_authenticated: token_remaining.is_some(),
                    msa_token_expires_in: token_remaining,
                    msa_refresh_available: refresh_available,
                    minecraft_account: account,
                    minecraft_token_expires_in,
                }
            })
            .collect::<Vec<_>>();
        states.sort_by(|left, right| {
            right
                .active
                .cmp(&left.active)
                .then_with(|| {
                    right
                        .minecraft_account
                        .as_ref()
                        .map(|account| account.authenticated_at)
                        .cmp(
                            &left
                                .minecraft_account
                                .as_ref()
                                .map(|account| account.authenticated_at),
                        )
                })
                .then_with(|| left.login_id.cmp(&right.login_id))
        });
        states
    }

    pub async fn active_msa_token_state(&self) -> Option<ActiveMsaTokenState> {
        let state = self.committed.read().expect(AUTH_STORE_LOCK_INVARIANT);
        let login_id = state.active_login_id.as_ref()?;
        let token = state.msa_tokens.get(login_id).cloned()?;
        let expires_at = token.expires_at;
        let expires_in = token.expires_in;
        let remaining = (expires_at - Utc::now()).num_milliseconds();
        if remaining <= 0 {
            return None;
        }

        Some(ActiveMsaTokenState {
            token,
            token_expires_in: (remaining as u64).div_ceil(1000).min(expires_in),
        })
    }

    pub async fn active_minecraft_account_state(&self) -> Option<ActiveMinecraftAccountState> {
        let state = self.committed.read().expect(AUTH_STORE_LOCK_INVARIANT);
        let login_id = state.active_login_id.as_ref()?;
        let account = state.minecraft_accounts.get(login_id).cloned()?;
        let expires_at = account.expires_at;
        let expires_in = account.expires_in;
        let remaining = (expires_at - Utc::now()).num_milliseconds();
        if remaining <= 0 {
            return None;
        }

        Some(ActiveMinecraftAccountState {
            account,
            token_expires_in: (remaining as u64).div_ceil(1000).min(expires_in),
        })
    }

    pub async fn active_current_minecraft_account_state(
        &self,
    ) -> Option<ActiveMinecraftAccountState> {
        let login_id = self
            .committed
            .read()
            .expect(AUTH_STORE_LOCK_INVARIANT)
            .active_login_id
            .clone()?;
        self.current_minecraft_account_state_for_login(&login_id)
            .await
    }

    pub async fn current_minecraft_account_state_for_login(
        &self,
        login_id: &str,
    ) -> Option<ActiveMinecraftAccountState> {
        let login_id = login_id.trim();
        if login_id.is_empty() {
            return None;
        }
        let now = Utc::now();
        let state = self.committed.read().expect(AUTH_STORE_LOCK_INVARIANT);
        let token = state.msa_tokens.get(login_id).cloned()?;
        if token.expires_at <= now {
            return None;
        }
        let account = state.minecraft_accounts.get(login_id).cloned()?;
        if account.expires_at <= now {
            return None;
        }
        if account.login_id != token.login_id || account.authenticated_at < token.authenticated_at {
            return None;
        }

        Some(ActiveMinecraftAccountState {
            token_expires_in: remaining_seconds(account.expires_at, account.expires_in),
            account,
        })
    }

    pub(crate) async fn update_active_current_minecraft_profile(
        &self,
        login_id: &str,
        profile: AuthLoginMinecraftProfile,
    ) -> Result<bool, AuthLoginStoreError> {
        self.update_active_current_minecraft_profile_and_ownership(login_id, profile, None)
            .await
            .map(|account| account.is_some())
    }

    pub(crate) async fn update_active_current_minecraft_profile_and_ownership(
        &self,
        login_id: &str,
        profile: AuthLoginMinecraftProfile,
        owns_minecraft_java: Option<bool>,
    ) -> Result<Option<ActiveMinecraftAccountState>, AuthLoginStoreError> {
        let (mutation, mut candidate) = self.begin_mutation().await?;
        let now = Utc::now();
        let Some(token) = candidate.msa_tokens.get(login_id).cloned() else {
            return Ok(None);
        };
        if token.expires_at <= now {
            return Ok(None);
        }

        let updated_account = {
            let Some(account) = candidate.minecraft_accounts.get_mut(login_id) else {
                return Ok(None);
            };
            if account.login_id != login_id
                || account.login_id != token.login_id
                || account.authenticated_at < token.authenticated_at
            {
                return Ok(None);
            }
            if account.expires_at <= now {
                return Ok(None);
            }

            account.profile = profile;
            if let Some(owns_minecraft_java) = owns_minecraft_java {
                account.owns_minecraft_java = owns_minecraft_java;
            }
            account.clone()
        };

        let result = ActiveMinecraftAccountState {
            token_expires_in: remaining_seconds(
                updated_account.expires_at,
                updated_account.expires_in,
            ),
            account: updated_account,
        };
        self.commit_candidate(candidate, mutation).await?;
        Ok(Some(result))
    }

    pub(crate) async fn clear_all(&self) -> Result<bool, AuthLoginStoreError> {
        let (mutation, candidate) = self.begin_mutation().await?;
        let had_auth = !candidate.msa_tokens.is_empty() || !candidate.minecraft_accounts.is_empty();
        let mutation = self
            .commit_candidate_holding_gate(AuthLoginCommittedState::default(), true, mutation)
            .await?;
        let mutation = self.flush_persistence_holding_gate(mutation).await?;
        drop(mutation);
        Ok(had_auth)
    }

    pub(crate) async fn switch_active_account(
        &self,
        login_id: &str,
    ) -> Result<bool, AuthLoginStoreError> {
        let (mutation, mut candidate) = self.begin_mutation().await?;
        if !candidate.msa_tokens.contains_key(login_id) {
            return Ok(false);
        }
        if candidate.active_login_id.as_deref() == Some(login_id) {
            return Ok(true);
        }
        candidate.active_login_id = Some(login_id.to_string());
        self.commit_candidate(candidate, mutation).await?;
        Ok(true)
    }

    pub(crate) async fn remove_account(&self, login_id: &str) -> Result<bool, AuthLoginStoreError> {
        let (mutation, mut candidate) = self.begin_mutation().await?;
        let removed_msa = candidate.msa_tokens.remove(login_id).is_some();
        let removed_minecraft = candidate.minecraft_accounts.remove(login_id).is_some();
        let removed = removed_msa || removed_minecraft;
        if !removed {
            return Ok(false);
        }
        if candidate.active_login_id.as_deref() == Some(login_id) {
            candidate.active_login_id = None;
        }
        self.commit_candidate(candidate, mutation).await?;
        Ok(true)
    }

    pub(crate) async fn active_auth_refresh_guard(&self) -> MutexGuard<'_, ()> {
        self.active_auth_refresh.lock().await
    }

    pub(crate) fn active_auth_generation(&self) -> u64 {
        self.active_auth_generation.load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub(crate) async fn flush(&self) -> Result<(), AuthLoginStoreError> {
        if *self.lifecycle.lock().expect(AUTH_STORE_LOCK_INVARIANT)
            == AuthLoginStoreLifecycle::Closed
        {
            return Ok(());
        }
        let (mutation, _) = self.begin_mutation().await?;
        let mutation = self.flush_persistence_holding_gate(mutation).await?;
        drop(mutation);
        Ok(())
    }

    pub(crate) async fn close(&self) -> Result<(), AuthLoginStoreError> {
        let mutation = self.mutation_gate.clone().lock_owned().await;
        if *self.lifecycle.lock().expect(AUTH_STORE_LOCK_INVARIANT)
            == AuthLoginStoreLifecycle::Closed
        {
            return Ok(());
        }
        if self.startup_state == AuthLoginStartupState::Rejected {
            *self.lifecycle.lock().expect(AUTH_STORE_LOCK_INVARIANT) =
                AuthLoginStoreLifecycle::Closed;
            return Ok(());
        }
        let mutation = self.prepare_mutation_gate(mutation).await?;
        let mutation = self.flush_persistence_holding_gate(mutation).await?;
        *self.lifecycle.lock().expect(AUTH_STORE_LOCK_INVARIANT) = AuthLoginStoreLifecycle::Closed;
        drop(mutation);
        Ok(())
    }

    async fn flush_persistence_holding_gate(
        &self,
        mutation: OwnedMutexGuard<()>,
    ) -> Result<OwnedMutexGuard<()>, AuthLoginStoreError> {
        let persistence = self.persistence.clone();
        let (completed_tx, completed_rx) = tokio::sync::oneshot::channel();
        drop(tokio::spawn(async move {
            let result = match persistence {
                Some(persistence) => tokio::task::spawn_blocking(move || persistence.flush())
                    .await
                    .map_err(|_| AuthLoginStoreError::BlockingTask)
                    .and_then(|result| result.map_err(AuthLoginStoreError::Persistence)),
                None => Ok(()),
            };
            let _ = completed_tx.send((result, mutation));
        }));
        let (result, mutation) = completed_rx
            .await
            .map_err(|_| AuthLoginStoreError::BlockingTask)?;
        result?;
        Ok(mutation)
    }

    #[cfg(test)]
    pub async fn active_msa_token(&self) -> Option<AuthLoginMsaToken> {
        let state = self.committed.read().expect(AUTH_STORE_LOCK_INVARIANT);
        let login_id = state.active_login_id.as_ref()?;
        state.msa_tokens.get(login_id).cloned()
    }

    #[cfg(test)]
    pub async fn active_minecraft_account(&self) -> Option<AuthLoginMinecraftAccount> {
        let state = self.committed.read().expect(AUTH_STORE_LOCK_INVARIANT);
        let login_id = state.active_login_id.as_ref()?;
        state.minecraft_accounts.get(login_id).cloned()
    }

    fn next_login_id(&self) -> String {
        let sequence = self.next_id.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or_default();
        format!("msa-{nanos:x}-{sequence:x}")
    }
}

#[cfg(test)]
impl Default for AuthLoginStore {
    fn default() -> Self {
        Self::new()
    }
}

fn saturating_u64_to_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn remaining_seconds(expires_at: DateTime<Utc>, expires_in: u64) -> u64 {
    let remaining = (expires_at - Utc::now()).num_milliseconds();
    if remaining <= 0 {
        return 0;
    }

    (remaining as u64).div_ceil(1000).min(expires_in)
}

fn remaining_seconds_option(
    expires_at: DateTime<Utc>,
    expires_in: u64,
    now: DateTime<Utc>,
) -> Option<u64> {
    let remaining = (expires_at - now).num_milliseconds();
    if remaining <= 0 {
        return None;
    }
    Some((remaining as u64).div_ceil(1000).min(expires_in))
}

async fn persist_auth_state_blocking(
    persistence: Option<Arc<dyn AuthSnapshotPersistence>>,
    state: AuthLoginCommittedState,
) -> Result<(), AuthLoginStoreError> {
    let Some(persistence) = persistence else {
        return Ok(());
    };
    tokio::task::spawn_blocking(move || {
        if state.msa_tokens.is_empty() {
            return persistence.delete_snapshot();
        }
        let mut msa_tokens = state.msa_tokens.into_values().collect::<Vec<_>>();
        msa_tokens.sort_by(|left, right| left.login_id.cmp(&right.login_id));
        let mut minecraft_accounts = state.minecraft_accounts.into_values().collect::<Vec<_>>();
        minecraft_accounts.sort_by(|left, right| left.login_id.cmp(&right.login_id));
        let snapshot = PersistedAuthSnapshot::from_state(
            state.active_login_id.as_deref(),
            &msa_tokens,
            &minecraft_accounts,
        );
        persistence.save_snapshot(&snapshot)
    })
    .await
    .map_err(|_| AuthLoginStoreError::BlockingTask)?
    .map_err(AuthLoginStoreError::Persistence)
}

fn empty_persisted_auth_state() -> PersistedAuthState {
    PersistedAuthState {
        active_login_id: None,
        msa_tokens: Vec::new(),
        minecraft_accounts: Vec::new(),
    }
}

fn has_nonblank_refresh_token(token: &AuthLoginMsaToken) -> bool {
    token
        .refresh_token
        .as_deref()
        .is_some_and(|refresh_token| !refresh_token.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::auth_persistence::{
        test_persisted_snapshot, test_persisted_snapshot_with_version,
    };
    use std::sync::Mutex;
    use std::sync::atomic::AtomicBool;

    #[derive(Default)]
    struct MockAuthSnapshotPersistence {
        snapshot: Mutex<Option<PersistedAuthSnapshot>>,
        saves: AtomicU64,
        deletes: AtomicU64,
        flushes: AtomicU64,
        fail_loads: AtomicBool,
        fail_saves: AtomicBool,
        ambiguous_saves: AtomicBool,
        fail_deletes: AtomicBool,
        fail_flushes: AtomicBool,
        block_saves: AtomicBool,
        save_started: AtomicBool,
        release_saves: AtomicBool,
    }

    impl MockAuthSnapshotPersistence {
        fn with_snapshot(snapshot: PersistedAuthSnapshot) -> Self {
            Self {
                snapshot: Mutex::new(Some(snapshot)),
                ..Self::default()
            }
        }

        fn with_failing_deletes(snapshot: PersistedAuthSnapshot) -> Self {
            let persistence = Self::with_snapshot(snapshot);
            persistence.fail_deletes.store(true, Ordering::Relaxed);
            persistence
        }

        fn with_unavailable_load() -> Self {
            let persistence = Self::default();
            persistence.fail_loads.store(true, Ordering::Relaxed);
            persistence
        }

        fn fail_saves(&self) {
            self.fail_saves.store(true, Ordering::Relaxed);
        }

        fn fail_saves_ambiguously(&self) {
            self.ambiguous_saves.store(true, Ordering::Relaxed);
        }

        fn allow_saves(&self) {
            self.fail_saves.store(false, Ordering::Release);
        }

        fn fail_flushes(&self) {
            self.fail_flushes.store(true, Ordering::Release);
        }

        fn allow_flushes(&self) {
            self.fail_flushes.store(false, Ordering::Release);
        }

        fn block_saves(&self) {
            self.save_started.store(false, Ordering::Release);
            self.release_saves.store(false, Ordering::Release);
            self.block_saves.store(true, Ordering::Release);
        }

        fn release_saves(&self) {
            self.release_saves.store(true, Ordering::Release);
        }

        fn snapshot(&self) -> Option<PersistedAuthSnapshot> {
            self.snapshot.lock().expect("snapshot lock").clone()
        }

        fn saves(&self) -> u64 {
            self.saves.load(Ordering::Relaxed)
        }

        fn deletes(&self) -> u64 {
            self.deletes.load(Ordering::Relaxed)
        }

        fn flushes(&self) -> u64 {
            self.flushes.load(Ordering::Relaxed)
        }
    }

    impl AuthSnapshotPersistence for MockAuthSnapshotPersistence {
        fn load_snapshot(&self) -> Result<Option<PersistedAuthSnapshot>, AuthPersistenceError> {
            if self.fail_loads.load(Ordering::Relaxed) {
                return Err(AuthPersistenceError::Unavailable);
            }
            Ok(self.snapshot())
        }

        fn save_snapshot(
            &self,
            snapshot: &PersistedAuthSnapshot,
        ) -> Result<(), AuthPersistenceError> {
            self.save_started.store(true, Ordering::Release);
            while self.block_saves.load(Ordering::Acquire)
                && !self.release_saves.load(Ordering::Acquire)
            {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            if self.ambiguous_saves.load(Ordering::Relaxed) {
                return Err(AuthPersistenceError::Ambiguous);
            }
            if self.fail_saves.load(Ordering::Relaxed) {
                return Err(AuthPersistenceError::Unavailable);
            }

            *self.snapshot.lock().expect("snapshot lock") = Some(snapshot.clone());
            self.saves.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }

        fn delete_snapshot(&self) -> Result<(), AuthPersistenceError> {
            self.deletes.fetch_add(1, Ordering::Relaxed);
            if self.fail_deletes.load(Ordering::Relaxed) {
                return Err(AuthPersistenceError::Unavailable);
            }

            *self.snapshot.lock().expect("snapshot lock") = None;
            Ok(())
        }

        fn flush(&self) -> Result<(), AuthPersistenceError> {
            self.flushes.fetch_add(1, Ordering::Relaxed);
            if self.fail_flushes.load(Ordering::Acquire) {
                Err(AuthPersistenceError::CleanupPending)
            } else {
                Ok(())
            }
        }
    }

    #[tokio::test]
    async fn auth_login_store_keeps_multiple_msa_tokens_and_marks_newest_active() {
        let store = AuthLoginStore::new();
        let first = store
            .replace_with_msa_token(new_msa_token("first-access-token", None, 3600))
            .await
            .expect("persist auth");
        let second = store
            .replace_with_msa_token(new_msa_token(
                "second-access-token",
                Some("second-refresh-token"),
                3600,
            ))
            .await
            .expect("persist auth");

        let active = store.active_msa_token().await.expect("active token");
        assert_eq!(active.login_id, second.login_id);
        assert_eq!(active.access_token, "second-access-token");
        assert_eq!(
            active.refresh_token,
            Some("second-refresh-token".to_string())
        );
        let account_states = store.account_states().await;
        assert_eq!(account_states.len(), 2);
        assert_eq!(account_states[0].login_id, second.login_id);
        assert!(account_states[0].active);
        assert_eq!(account_states[1].login_id, first.login_id);
        assert!(!account_states[1].active);
    }

    #[tokio::test]
    async fn auth_login_store_does_not_publish_switch_when_persistence_fails() {
        let persistence = Arc::new(MockAuthSnapshotPersistence::default());
        let store = AuthLoginStore::with_persistence(persistence.clone()).await;
        let first = store
            .replace_with_msa_token(new_msa_token(
                "first-access-token",
                Some("first-refresh-token"),
                3600,
            ))
            .await
            .expect("persist auth");
        let second = store
            .replace_with_msa_token(new_msa_token(
                "second-access-token",
                Some("second-refresh-token"),
                3600,
            ))
            .await
            .expect("persist auth");
        persistence.fail_saves();

        let error = store
            .switch_active_account(&first.login_id)
            .await
            .expect_err("switch persistence failure is visible");

        assert!(matches!(
            error,
            AuthLoginStoreError::Persistence(AuthPersistenceError::Unavailable)
        ));
        let active = store.active_msa_token().await.expect("active token");
        assert_eq!(active.login_id, second.login_id);
        assert_eq!(active.access_token, "second-access-token");
    }

    #[tokio::test]
    async fn auth_login_store_clear_all_removes_active_msa_auth() {
        let store = AuthLoginStore::new();
        store
            .replace_with_msa_token(new_msa_token(
                "msa-access-token",
                Some("msa-refresh-token"),
                3600,
            ))
            .await
            .expect("persist auth");

        let had_auth = store.clear_all().await.expect("clear auth");

        assert!(had_auth);
        assert_eq!(store.active_msa_token().await, None);
        assert!(store.account_states().await.is_empty());
        assert!(!store.clear_all().await.expect("clear empty auth"));
    }

    #[tokio::test]
    async fn auth_login_store_expiry_reads_do_not_mutate_committed_auth() {
        let store = AuthLoginStore::new();
        store
            .replace_with_msa_token(new_msa_token("msa-access-token", None, 0))
            .await
            .expect("persist auth");

        assert_eq!(store.active_msa_auth_remaining_seconds().await, None);
        assert!(store.active_msa_token().await.is_some());
        assert!(!store.has_active_msa_auth().await);
    }

    #[tokio::test]
    async fn auth_login_store_keeps_expired_msa_refresh_material() {
        let store = AuthLoginStore::new();
        store
            .replace_with_msa_token(new_msa_token(
                "msa-access-token",
                Some("msa-refresh-token"),
                0,
            ))
            .await
            .expect("persist auth");

        assert_eq!(store.active_msa_auth_remaining_seconds().await, None);
        assert_eq!(
            store.active_msa_refresh_token().await,
            Some("msa-refresh-token".to_string())
        );
        assert!(store.active_msa_token().await.is_some());
        assert!(!store.has_active_msa_auth().await);
    }

    #[tokio::test]
    async fn auth_login_store_hides_minecraft_readiness_without_live_msa_or_refresh() {
        let store = AuthLoginStore::new();
        store
            .replace_with_msa_and_minecraft_account(
                new_msa_token("expired-msa-access-token", None, 0),
                new_minecraft_account("ExpiredMsaProfile", 3600, true),
            )
            .await
            .expect("persist auth");

        let states = store.account_states().await;
        assert_eq!(states.len(), 1);
        assert!(!states[0].msa_authenticated);
        assert!(!states[0].msa_refresh_available);
        assert_eq!(states[0].msa_token_expires_in, None);
        assert_eq!(states[0].minecraft_account, None);
        assert_eq!(states[0].minecraft_token_expires_in, None);
    }

    #[tokio::test]
    async fn auth_login_store_restores_valid_secure_auth_snapshot() {
        let now = DateTime::from_timestamp_millis(Utc::now().timestamp_millis())
            .expect("valid current timestamp");
        let token = AuthLoginMsaToken {
            login_id: "msa-valid".to_string(),
            access_token: "msa-access-token".to_string(),
            refresh_token: Some("msa-refresh-token".to_string()),
            id_token: Some("msa-id-token".to_string()),
            token_type: "Bearer".to_string(),
            expires_in: 3600,
            scope: Some("XboxLive.signin offline_access".to_string()),
            authenticated_at: now,
            expires_at: now + chrono::Duration::seconds(3600),
        };
        let account = AuthLoginMinecraftAccount {
            login_id: "msa-valid".to_string(),
            access_token: "minecraft-access-token".to_string(),
            token_type: Some("Bearer".to_string()),
            expires_in: 3600,
            profile: test_profile("RestoredPlayer"),
            owns_minecraft_java: true,
            authenticated_at: now,
            expires_at: now + chrono::Duration::seconds(3600),
        };
        let persistence = Arc::new(MockAuthSnapshotPersistence::with_snapshot(
            test_persisted_snapshot(&token, Some(&account)),
        ));

        let store = AuthLoginStore::with_persistence(persistence.clone()).await;

        assert_eq!(store.active_msa_token().await, Some(token));
        assert_eq!(store.active_minecraft_account().await, Some(account));
        assert_eq!(persistence.deletes(), 0);
    }

    #[tokio::test]
    async fn auth_login_store_does_not_rewrite_expired_snapshot_on_restore() {
        let now = Utc::now();
        let token = AuthLoginMsaToken {
            login_id: "msa-expired".to_string(),
            access_token: "msa-access-token".to_string(),
            refresh_token: None,
            id_token: None,
            token_type: "Bearer".to_string(),
            expires_in: 3600,
            scope: None,
            authenticated_at: now - chrono::Duration::seconds(7200),
            expires_at: now - chrono::Duration::seconds(3600),
        };
        let persistence = Arc::new(MockAuthSnapshotPersistence::with_snapshot(
            test_persisted_snapshot(&token, None),
        ));

        let store = AuthLoginStore::with_persistence(persistence.clone()).await;

        assert_eq!(store.active_msa_token().await, None);
        assert!(persistence.snapshot().is_some());
        assert_eq!(persistence.deletes(), 0);
        assert_eq!(store.load_issue_count(), 1);
    }

    #[tokio::test]
    async fn auth_login_store_restores_expired_msa_refresh_material_without_expired_minecraft() {
        let now = DateTime::from_timestamp_millis(Utc::now().timestamp_millis())
            .expect("valid current timestamp");
        let token = AuthLoginMsaToken {
            login_id: "msa-refresh-capable".to_string(),
            access_token: "msa-access-token".to_string(),
            refresh_token: Some("msa-refresh-token".to_string()),
            id_token: None,
            token_type: "Bearer".to_string(),
            expires_in: 3600,
            scope: Some("XboxLive.signin offline_access".to_string()),
            authenticated_at: now - chrono::Duration::seconds(7200),
            expires_at: now - chrono::Duration::seconds(3600),
        };
        let account = AuthLoginMinecraftAccount {
            login_id: "msa-refresh-capable".to_string(),
            access_token: "minecraft-access-token".to_string(),
            token_type: Some("Bearer".to_string()),
            expires_in: 3600,
            profile: test_profile("ExpiredMinecraft"),
            owns_minecraft_java: true,
            authenticated_at: now - chrono::Duration::seconds(7200),
            expires_at: now - chrono::Duration::seconds(3600),
        };
        let persistence = Arc::new(MockAuthSnapshotPersistence::with_snapshot(
            test_persisted_snapshot(&token, Some(&account)),
        ));

        let store = AuthLoginStore::with_persistence(persistence.clone()).await;

        assert_eq!(store.active_msa_token().await, Some(token));
        assert_eq!(
            store.active_msa_refresh_token().await,
            Some("msa-refresh-token".to_string())
        );
        assert_eq!(store.active_msa_auth_remaining_seconds().await, None);
        assert_eq!(store.active_minecraft_account().await, None);
        assert_eq!(persistence.deletes(), 0);
    }

    #[tokio::test]
    async fn auth_login_store_drops_minecraft_snapshot_for_skipped_expired_msa_token() {
        let now = DateTime::from_timestamp_millis(Utc::now().timestamp_millis())
            .expect("valid current timestamp");
        let expired_token = AuthLoginMsaToken {
            login_id: "msa-expired-without-refresh".to_string(),
            access_token: "expired-msa-access-token".to_string(),
            refresh_token: None,
            id_token: None,
            token_type: "Bearer".to_string(),
            expires_in: 3600,
            scope: None,
            authenticated_at: now - chrono::Duration::seconds(7200),
            expires_at: now - chrono::Duration::seconds(3600),
        };
        let orphaned_account = AuthLoginMinecraftAccount {
            login_id: expired_token.login_id.clone(),
            access_token: "orphaned-minecraft-access-token".to_string(),
            token_type: Some("Bearer".to_string()),
            expires_in: 3600,
            profile: test_profile("OrphanedMinecraft"),
            owns_minecraft_java: true,
            authenticated_at: now - chrono::Duration::seconds(1200),
            expires_at: now + chrono::Duration::seconds(2400),
        };
        let valid_token = AuthLoginMsaToken {
            login_id: "msa-valid".to_string(),
            access_token: "valid-msa-access-token".to_string(),
            refresh_token: Some("valid-refresh-token".to_string()),
            id_token: None,
            token_type: "Bearer".to_string(),
            expires_in: 3600,
            scope: Some("XboxLive.signin offline_access".to_string()),
            authenticated_at: now,
            expires_at: now + chrono::Duration::seconds(3600),
        };
        let valid_account = AuthLoginMinecraftAccount {
            login_id: valid_token.login_id.clone(),
            access_token: "valid-minecraft-access-token".to_string(),
            token_type: Some("Bearer".to_string()),
            expires_in: 3600,
            profile: test_profile("ValidMinecraft"),
            owns_minecraft_java: true,
            authenticated_at: now,
            expires_at: now + chrono::Duration::seconds(3600),
        };
        let snapshot = PersistedAuthSnapshot::from_state(
            Some(&valid_token.login_id),
            &[expired_token, valid_token.clone()],
            &[orphaned_account, valid_account.clone()],
        );
        let persistence = Arc::new(MockAuthSnapshotPersistence::with_snapshot(snapshot));

        let store = AuthLoginStore::with_persistence(persistence.clone()).await;

        assert_eq!(store.active_msa_token().await, Some(valid_token));
        assert_eq!(store.active_minecraft_account().await, Some(valid_account));
        assert_eq!(store.account_states().await.len(), 1);
        assert_eq!(persistence.deletes(), 0);
    }

    #[tokio::test]
    async fn auth_login_store_closes_malformed_startup_rejection_without_rewrite() {
        let now = Utc::now();
        let token = AuthLoginMsaToken {
            login_id: "msa-wrong-schema".to_string(),
            access_token: "msa-access-token".to_string(),
            refresh_token: None,
            id_token: None,
            token_type: "Bearer".to_string(),
            expires_in: 3600,
            scope: None,
            authenticated_at: now,
            expires_at: now + chrono::Duration::seconds(3600),
        };
        let snapshot = test_persisted_snapshot_with_version(&token, None, 1);
        let persistence = Arc::new(MockAuthSnapshotPersistence::with_snapshot(snapshot.clone()));

        let store = AuthLoginStore::with_persistence(persistence.clone()).await;

        assert_eq!(store.active_msa_token().await, None);
        assert_eq!(persistence.snapshot(), Some(snapshot.clone()));
        assert_eq!(store.load_issue_count(), 1);
        assert!(matches!(
            store
                .replace_with_msa_token(new_msa_token("blocked", None, 3600))
                .await,
            Err(AuthLoginStoreError::MutationLatched)
        ));
        store
            .close()
            .await
            .expect("close rejected secure auth store");
        store.close().await.expect("close remains idempotent");
        assert_eq!(persistence.snapshot(), Some(snapshot));
        assert_eq!(persistence.saves(), 0);
        assert_eq!(persistence.deletes(), 0);
        assert_eq!(persistence.flushes(), 0);
        assert!(matches!(
            store
                .replace_with_msa_token(new_msa_token("closed", None, 3600))
                .await,
            Err(AuthLoginStoreError::Closed)
        ));
    }

    #[tokio::test]
    async fn auth_login_store_closes_unavailable_startup_rejection_without_persistence_access() {
        let persistence = Arc::new(MockAuthSnapshotPersistence::with_unavailable_load());
        let store = AuthLoginStore::with_persistence(persistence.clone()).await;

        assert_eq!(store.load_issue_count(), 1);
        assert!(matches!(
            store
                .replace_with_msa_token(new_msa_token("blocked", None, 3600))
                .await,
            Err(AuthLoginStoreError::MutationLatched)
        ));
        store
            .close()
            .await
            .expect("close rejected secure auth store");
        store.close().await.expect("close remains idempotent");
        assert_eq!(persistence.snapshot(), None);
        assert_eq!(persistence.saves(), 0);
        assert_eq!(persistence.deletes(), 0);
        assert_eq!(persistence.flushes(), 0);
        assert!(matches!(
            store
                .replace_with_msa_token(new_msa_token("closed", None, 3600))
                .await,
            Err(AuthLoginStoreError::Closed)
        ));
    }

    #[tokio::test]
    async fn auth_login_store_post_start_ambiguous_mutation_still_blocks_close() {
        let persistence = Arc::new(MockAuthSnapshotPersistence::default());
        let store = AuthLoginStore::with_persistence(persistence.clone()).await;
        persistence.fail_saves_ambiguously();

        assert!(matches!(
            store
                .replace_with_msa_token(new_msa_token("ambiguous", None, 3600))
                .await,
            Err(AuthLoginStoreError::Persistence(
                AuthPersistenceError::Ambiguous
            ))
        ));
        assert!(matches!(
            store.close().await,
            Err(AuthLoginStoreError::MutationLatched)
        ));
        assert_eq!(persistence.snapshot(), None);
        assert_eq!(persistence.saves(), 0);
        assert_eq!(persistence.deletes(), 0);
        assert_eq!(persistence.flushes(), 0);
        assert!(matches!(
            store
                .replace_with_msa_token(new_msa_token("still-blocked", None, 3600))
                .await,
            Err(AuthLoginStoreError::MutationLatched)
        ));
    }

    #[tokio::test]
    async fn auth_login_store_rejects_blank_msa_token_without_rewrite() {
        let now = Utc::now();
        let token = AuthLoginMsaToken {
            login_id: "msa-blank-token".to_string(),
            access_token: "msa-access-token".to_string(),
            refresh_token: None,
            id_token: None,
            token_type: "Bearer".to_string(),
            expires_in: 3600,
            scope: None,
            authenticated_at: now,
            expires_at: now + chrono::Duration::seconds(3600),
        };
        let snapshot = persisted_snapshot_with_string_field(
            test_persisted_snapshot(&token, None),
            &["msa_tokens", "0", "access_token"],
            "   ",
        );
        let persistence = Arc::new(MockAuthSnapshotPersistence::with_snapshot(snapshot));

        let store = AuthLoginStore::with_persistence(persistence.clone()).await;

        assert_eq!(store.active_msa_token().await, None);
        assert!(persistence.snapshot().is_some());
        assert_eq!(persistence.deletes(), 0);
        assert_eq!(store.load_issue_count(), 1);
    }

    #[tokio::test]
    async fn auth_login_store_rejects_blank_refresh_token_without_rewrite() {
        let now = Utc::now();
        let token = AuthLoginMsaToken {
            login_id: "msa-blank-refresh-token".to_string(),
            access_token: "msa-access-token".to_string(),
            refresh_token: Some("msa-refresh-token".to_string()),
            id_token: None,
            token_type: "Bearer".to_string(),
            expires_in: 3600,
            scope: None,
            authenticated_at: now,
            expires_at: now + chrono::Duration::seconds(3600),
        };
        let snapshot = persisted_snapshot_with_string_field(
            test_persisted_snapshot(&token, None),
            &["msa_tokens", "0", "refresh_token"],
            "   ",
        );
        let persistence = Arc::new(MockAuthSnapshotPersistence::with_snapshot(snapshot));

        let store = AuthLoginStore::with_persistence(persistence.clone()).await;

        assert_eq!(store.active_msa_token().await, None);
        assert!(persistence.snapshot().is_some());
        assert_eq!(persistence.deletes(), 0);
        assert_eq!(store.load_issue_count(), 1);
    }

    #[tokio::test]
    async fn auth_login_store_rejects_blank_minecraft_snapshot_without_rewrite() {
        let now = Utc::now();
        let token = AuthLoginMsaToken {
            login_id: "msa-blank-minecraft".to_string(),
            access_token: "msa-access-token".to_string(),
            refresh_token: None,
            id_token: None,
            token_type: "Bearer".to_string(),
            expires_in: 3600,
            scope: None,
            authenticated_at: now,
            expires_at: now + chrono::Duration::seconds(3600),
        };
        let account = AuthLoginMinecraftAccount {
            login_id: "msa-blank-minecraft".to_string(),
            access_token: "minecraft-access-token".to_string(),
            token_type: Some("Bearer".to_string()),
            expires_in: 3600,
            profile: test_profile("BlankMinecraft"),
            owns_minecraft_java: true,
            authenticated_at: now,
            expires_at: now + chrono::Duration::seconds(3600),
        };
        let snapshot = persisted_snapshot_with_string_field(
            persisted_snapshot_with_string_field(
                test_persisted_snapshot(&token, Some(&account)),
                &["minecraft_accounts", "0", "access_token"],
                "   ",
            ),
            &["minecraft_accounts", "0", "profile", "name"],
            "   ",
        );
        let persistence = Arc::new(MockAuthSnapshotPersistence::with_snapshot(snapshot));

        let store = AuthLoginStore::with_persistence(persistence.clone()).await;

        assert_eq!(store.active_msa_token().await, None);
        assert_eq!(store.active_minecraft_account().await, None);
        assert!(persistence.snapshot().is_some());
        assert_eq!(persistence.deletes(), 0);
        assert_eq!(store.load_issue_count(), 1);
    }

    #[tokio::test]
    async fn auth_login_store_drops_mismatched_minecraft_snapshot_on_restore() {
        let now = Utc::now();
        let token = AuthLoginMsaToken {
            login_id: "msa-login".to_string(),
            access_token: "msa-access-token".to_string(),
            refresh_token: None,
            id_token: None,
            token_type: "Bearer".to_string(),
            expires_in: 3600,
            scope: None,
            authenticated_at: now,
            expires_at: now + chrono::Duration::seconds(3600),
        };
        let account = AuthLoginMinecraftAccount {
            login_id: "minecraft-login".to_string(),
            access_token: "minecraft-access-token".to_string(),
            token_type: Some("Bearer".to_string()),
            expires_in: 3600,
            profile: test_profile("MismatchedLogin"),
            owns_minecraft_java: true,
            authenticated_at: now,
            expires_at: now + chrono::Duration::seconds(3600),
        };
        let persistence = Arc::new(MockAuthSnapshotPersistence::with_snapshot(
            test_persisted_snapshot(&token, Some(&account)),
        ));

        let store = AuthLoginStore::with_persistence(persistence.clone()).await;

        let restored = store.active_msa_token().await.expect("restored token");
        assert_eq!(restored.login_id, token.login_id);
        assert_eq!(restored.access_token, token.access_token);
        assert_eq!(restored.refresh_token, token.refresh_token);
        assert_eq!(store.active_minecraft_account().await, None);
        assert_eq!(persistence.deletes(), 0);
    }

    #[tokio::test]
    async fn auth_login_store_persists_secure_auth_snapshot_on_completion() {
        let persistence = Arc::new(MockAuthSnapshotPersistence::default());
        let store = AuthLoginStore::with_persistence(persistence.clone()).await;

        store
            .replace_with_msa_and_minecraft_account(
                new_msa_token("msa-access-token", Some("msa-refresh-token"), 3600),
                new_minecraft_account("PersistedPlayer", 3600, true),
            )
            .await
            .expect("persist auth");

        assert_eq!(persistence.saves(), 1);
        assert!(persistence.snapshot().is_some());
    }

    #[tokio::test]
    async fn auth_login_store_persist_failure_does_not_publish() {
        let persistence = Arc::new(MockAuthSnapshotPersistence::default());
        let store = AuthLoginStore::with_persistence(persistence.clone()).await;
        persistence.fail_saves();

        let error = store
            .replace_with_msa_and_minecraft_account(
                new_msa_token("msa-access-token", Some("msa-refresh-token"), 3600),
                new_minecraft_account("PersistedPlayer", 3600, true),
            )
            .await
            .expect_err("credential commit should fail");

        assert!(matches!(
            error,
            AuthLoginStoreError::Persistence(AuthPersistenceError::Unavailable)
        ));
        assert_eq!(store.active_msa_token().await, None);
        assert_eq!(store.active_minecraft_account().await, None);
        assert!(store.account_states().await.is_empty());
        assert_eq!(persistence.saves(), 0);
        assert_eq!(persistence.snapshot(), None);
    }

    #[tokio::test]
    async fn cancelled_failed_commit_retries_exact_state_before_flush_and_close() {
        let persistence = Arc::new(MockAuthSnapshotPersistence::default());
        persistence.fail_saves();
        persistence.block_saves();
        let store = Arc::new(AuthLoginStore::with_persistence(persistence.clone()).await);
        let task_store = store.clone();
        let task = tokio::spawn(async move {
            task_store
                .replace_with_msa_token(new_msa_token(
                    "cancelled-access-token",
                    Some("cancelled-refresh-token"),
                    3600,
                ))
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !persistence.save_started.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("save attempt starts");
        task.abort();
        assert!(task.await.expect_err("caller aborted").is_cancelled());
        persistence.release_saves();

        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while store
                .pending_commit
                .lock()
                .expect(AUTH_STORE_LOCK_INVARIANT)
                .is_none()
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("detached owner records exact retry");
        assert_eq!(store.active_msa_token().await, None);

        persistence.allow_saves();
        store.flush().await.expect("retry exact state and flush");
        assert_eq!(
            store
                .active_msa_token()
                .await
                .expect("retried token")
                .access_token,
            "cancelled-access-token"
        );
        assert!(persistence.snapshot().is_some());
        assert_eq!(persistence.saves(), 1);
        assert_eq!(persistence.flushes(), 1);
        store.close().await.expect("close store");
        assert_eq!(persistence.flushes(), 2);
        assert!(matches!(
            store
                .replace_with_msa_token(new_msa_token("closed", None, 3600))
                .await,
            Err(AuthLoginStoreError::Closed)
        ));
    }

    #[tokio::test]
    async fn auth_login_store_close_reports_cleanup_and_remains_retryable() {
        let persistence = Arc::new(MockAuthSnapshotPersistence::default());
        let store = AuthLoginStore::with_persistence(persistence.clone()).await;
        persistence.fail_flushes();

        assert!(matches!(
            store.close().await,
            Err(AuthLoginStoreError::Persistence(
                AuthPersistenceError::CleanupPending
            ))
        ));
        assert_eq!(persistence.flushes(), 1);

        persistence.allow_flushes();
        store.close().await.expect("retry secure auth close");
        assert_eq!(persistence.flushes(), 2);
        assert!(matches!(
            store
                .replace_with_msa_token(new_msa_token("closed", None, 3600))
                .await,
            Err(AuthLoginStoreError::Closed)
        ));
    }

    #[tokio::test]
    async fn auth_login_store_persists_rotated_refresh_snapshot() {
        let persistence = Arc::new(MockAuthSnapshotPersistence::default());
        let store = AuthLoginStore::with_persistence(persistence.clone()).await;
        store
            .replace_with_msa_token(new_msa_token(
                "old-msa-access-token",
                Some("old-msa-refresh-token"),
                0,
            ))
            .await
            .expect("persist auth");

        store
            .refresh_with_msa_and_minecraft_account(
                new_msa_token("new-msa-access-token", Some("new-msa-refresh-token"), 3600),
                new_minecraft_account("RotatedRefresh", 3600, true),
                "old-msa-refresh-token",
            )
            .await
            .expect("persist refresh")
            .expect("active login");

        let restored = AuthLoginStore::with_persistence(persistence.clone()).await;
        let active = restored.active_msa_token().await.expect("restored token");
        assert_eq!(active.access_token, "new-msa-access-token");
        assert_eq!(
            active.refresh_token,
            Some("new-msa-refresh-token".to_string())
        );
        assert_eq!(persistence.saves(), 2);
    }

    #[tokio::test]
    async fn auth_login_store_refreshes_captured_login_when_active_account_changes() {
        let store = AuthLoginStore::new();
        let (first_token, _) = store
            .replace_with_msa_and_minecraft_account(
                new_msa_token(
                    "first-old-msa-access-token",
                    Some("first-old-refresh-token"),
                    3600,
                ),
                new_minecraft_account("FirstOldProfile", 3600, true),
            )
            .await
            .expect("persist auth");
        let second_token = store
            .replace_with_msa_and_minecraft_account(
                new_msa_token(
                    "second-msa-access-token",
                    Some("second-refresh-token"),
                    3600,
                ),
                new_minecraft_account("SecondProfile", 3600, true),
            )
            .await
            .expect("persist auth")
            .0;

        store
            .refresh_login_with_msa_and_minecraft_account(
                &first_token.login_id,
                new_msa_token(
                    "first-new-msa-access-token",
                    Some("first-new-refresh-token"),
                    3600,
                ),
                new_minecraft_account("FirstNewProfile", 3600, true),
                "first-old-refresh-token",
            )
            .await
            .expect("persist refresh")
            .expect("captured login");

        let states = store.account_states().await;
        let first = states
            .iter()
            .find(|state| state.login_id == first_token.login_id)
            .expect("first account state");
        let second = states
            .iter()
            .find(|state| state.login_id == second_token.login_id)
            .expect("second account state");

        assert!(!first.active);
        assert_eq!(
            first
                .minecraft_account
                .as_ref()
                .expect("first minecraft account")
                .profile
                .name,
            "FirstNewProfile"
        );
        assert_eq!(
            first
                .minecraft_account
                .as_ref()
                .expect("first minecraft account")
                .access_token,
            "minecraft-access-token"
        );
        assert!(second.active);
        assert_eq!(
            second
                .minecraft_account
                .as_ref()
                .expect("second minecraft account")
                .profile
                .name,
            "SecondProfile"
        );
        assert_eq!(
            store
                .active_msa_token()
                .await
                .expect("active second msa token")
                .access_token,
            "second-msa-access-token"
        );
    }

    #[tokio::test]
    async fn auth_login_store_persists_msa_only_rotated_refresh_snapshot() {
        let persistence = Arc::new(MockAuthSnapshotPersistence::default());
        let store = AuthLoginStore::with_persistence(persistence.clone()).await;
        store
            .replace_with_msa_token(new_msa_token(
                "old-msa-access-token",
                Some("old-msa-refresh-token"),
                0,
            ))
            .await
            .expect("persist auth");

        store
            .refresh_with_msa_token(
                new_msa_token("new-msa-access-token", Some("new-msa-refresh-token"), 3600),
                "old-msa-refresh-token",
            )
            .await
            .expect("persist refresh")
            .expect("active login");

        let restored = AuthLoginStore::with_persistence(persistence.clone()).await;
        let active = restored.active_msa_token().await.expect("restored token");
        assert_eq!(active.access_token, "new-msa-access-token");
        assert_eq!(
            active.refresh_token,
            Some("new-msa-refresh-token".to_string())
        );
        assert_eq!(restored.active_minecraft_account().await, None);
        assert_eq!(persistence.saves(), 2);
    }

    #[tokio::test]
    async fn current_minecraft_account_ignores_account_older_than_active_msa_token() {
        let store = AuthLoginStore::new();
        store
            .replace_with_msa_and_minecraft_account(
                new_msa_token("old-msa-access-token", Some("old-msa-refresh-token"), 3600),
                new_minecraft_account("PreservedPlayer", 3600, true),
            )
            .await
            .expect("persist auth");

        store
            .refresh_with_msa_token(
                new_msa_token("new-msa-access-token", Some("new-msa-refresh-token"), 3600),
                "old-msa-refresh-token",
            )
            .await
            .expect("persist refresh")
            .expect("active login");

        assert_eq!(
            store
                .active_minecraft_account_state()
                .await
                .expect("raw minecraft account")
                .account
                .profile
                .name,
            "PreservedPlayer"
        );
        assert_eq!(store.active_current_minecraft_account_state().await, None);
    }

    #[tokio::test]
    async fn auth_login_store_deletes_secure_auth_snapshot_on_clear() {
        let persistence = Arc::new(MockAuthSnapshotPersistence::default());
        let store = AuthLoginStore::with_persistence(persistence.clone()).await;
        store
            .replace_with_msa_token(new_msa_token("msa-access-token", None, 3600))
            .await
            .expect("persist auth");

        assert!(store.clear_all().await.expect("clear auth"));

        assert_eq!(persistence.snapshot(), None);
        assert_eq!(persistence.deletes(), 1);
    }

    #[tokio::test]
    async fn auth_login_store_keeps_auth_when_secure_clear_fails() {
        let now = DateTime::from_timestamp_millis(Utc::now().timestamp_millis())
            .expect("valid current timestamp");
        let token = AuthLoginMsaToken {
            login_id: "msa-delete-fails".to_string(),
            access_token: "msa-access-token".to_string(),
            refresh_token: Some("msa-refresh-token".to_string()),
            id_token: None,
            token_type: "Bearer".to_string(),
            expires_in: 3600,
            scope: Some("XboxLive.signin offline_access".to_string()),
            authenticated_at: now,
            expires_at: now + chrono::Duration::seconds(3600),
        };
        let persistence = Arc::new(MockAuthSnapshotPersistence::with_failing_deletes(
            test_persisted_snapshot(&token, None),
        ));
        let store = AuthLoginStore::with_persistence(persistence.clone()).await;

        let error = store
            .clear_all()
            .await
            .expect_err("secure delete failure should be visible");

        assert!(matches!(
            error,
            AuthLoginStoreError::Persistence(AuthPersistenceError::Unavailable)
        ));
        assert_eq!(store.active_msa_token().await, Some(token));
        assert!(persistence.snapshot().is_some());
        assert_eq!(persistence.deletes(), 1);
    }

    fn new_msa_token(
        access_token: &str,
        refresh_token: Option<&str>,
        expires_in: u64,
    ) -> NewAuthLoginMsaToken {
        NewAuthLoginMsaToken {
            access_token: access_token.to_string(),
            refresh_token: refresh_token.map(ToOwned::to_owned),
            id_token: None,
            token_type: "Bearer".to_string(),
            expires_in,
            scope: Some("XboxLive.signin offline_access".to_string()),
        }
    }

    fn new_minecraft_account(
        profile_name: &str,
        expires_in: u64,
        owns_minecraft_java: bool,
    ) -> NewAuthLoginMinecraftAccount {
        NewAuthLoginMinecraftAccount {
            access_token: "minecraft-access-token".to_string(),
            token_type: Some("Bearer".to_string()),
            expires_in,
            profile: test_profile(profile_name),
            owns_minecraft_java,
        }
    }

    fn test_profile(name: &str) -> AuthLoginMinecraftProfile {
        AuthLoginMinecraftProfile {
            id: format!("{name}-id"),
            name: name.to_string(),
            skins: vec![AuthLoginMinecraftSkin {
                id: format!("{name}-skin"),
                state: "ACTIVE".to_string(),
                url: "https://textures.minecraft.net/texture/example".to_string(),
                variant: "classic".to_string(),
            }],
            capes: vec![AuthLoginMinecraftCape {
                id: format!("{name}-cape"),
                state: "ACTIVE".to_string(),
                url: "https://textures.minecraft.net/texture/cape".to_string(),
            }],
        }
    }

    fn persisted_snapshot_with_string_field(
        snapshot: PersistedAuthSnapshot,
        path: &[&str],
        value: &str,
    ) -> PersistedAuthSnapshot {
        let mut serialized = serde_json::to_value(snapshot).expect("serialize persisted snapshot");
        let mut cursor = &mut serialized;

        for segment in &path[..path.len() - 1] {
            cursor = get_snapshot_value_mut(cursor, segment);
        }

        *get_snapshot_value_mut(cursor, path[path.len() - 1]) =
            serde_json::Value::String(value.to_string());

        serde_json::from_value(serialized).expect("deserialize persisted snapshot")
    }

    fn get_snapshot_value_mut<'a>(
        value: &'a mut serde_json::Value,
        segment: &str,
    ) -> &'a mut serde_json::Value {
        if let Ok(index) = segment.parse::<usize>() {
            return value
                .get_mut(index)
                .unwrap_or_else(|| panic!("snapshot array index {segment}"));
        }

        value
            .get_mut(segment)
            .unwrap_or_else(|| panic!("snapshot field {segment}"))
    }
}
