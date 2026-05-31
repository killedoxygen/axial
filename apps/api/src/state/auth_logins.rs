use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::{Mutex, MutexGuard, RwLock};

#[cfg(not(test))]
use super::auth_persistence::SecureAuthSnapshotPersistence;
use super::auth_persistence::{
    AuthPersistenceError, AuthSnapshotPersistence, AuthSnapshotRejection, PersistedAuthSnapshot,
};

#[derive(Clone, Eq, PartialEq)]
pub struct AuthLoginSession {
    pub login_id: String,
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub expires_in: u64,
    pub interval: u64,
    pub message: Option<String>,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

impl fmt::Debug for AuthLoginSession {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AuthLoginSession")
            .field("login_id", &self.login_id)
            .field("device_code", &"[redacted]")
            .field("user_code", &self.user_code)
            .field("verification_uri", &self.verification_uri)
            .field("expires_in", &self.expires_in)
            .field("interval", &self.interval)
            .field("message", &self.message)
            .field("created_at", &self.created_at)
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

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

#[derive(Clone, Eq, PartialEq)]
pub struct NewAuthLoginSession {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub expires_in: u64,
    pub interval: u64,
    pub message: Option<String>,
}

impl fmt::Debug for NewAuthLoginSession {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NewAuthLoginSession")
            .field("device_code", &"[redacted]")
            .field("user_code", &self.user_code)
            .field("verification_uri", &self.verification_uri)
            .field("expires_in", &self.expires_in)
            .field("interval", &self.interval)
            .field("message", &self.message)
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

pub struct AuthLoginStore {
    sessions: RwLock<HashMap<String, AuthLoginSession>>,
    active_msa_token: RwLock<Option<AuthLoginMsaToken>>,
    active_minecraft_account: RwLock<Option<AuthLoginMinecraftAccount>>,
    active_auth_refresh: Mutex<()>,
    active_auth_generation: AtomicU64,
    persistence: Option<Arc<dyn AuthSnapshotPersistence>>,
    next_id: AtomicU64,
}

impl AuthLoginStore {
    pub fn new() -> Self {
        Self {
            sessions: RwLock::new(HashMap::new()),
            active_msa_token: RwLock::new(None),
            active_minecraft_account: RwLock::new(None),
            active_auth_refresh: Mutex::new(()),
            active_auth_generation: AtomicU64::new(0),
            persistence: None,
            next_id: AtomicU64::new(1),
        }
    }

    pub fn load_from_secure_store() -> Self {
        #[cfg(test)]
        {
            Self::new()
        }

        #[cfg(not(test))]
        {
            Self::with_persistence(Arc::new(SecureAuthSnapshotPersistence::new()))
        }
    }

    pub(crate) fn with_persistence(persistence: Arc<dyn AuthSnapshotPersistence>) -> Self {
        let (active_msa_token, active_minecraft_account) =
            load_active_snapshot(persistence.as_ref());

        Self {
            sessions: RwLock::new(HashMap::new()),
            active_msa_token: RwLock::new(active_msa_token),
            active_minecraft_account: RwLock::new(active_minecraft_account),
            active_auth_refresh: Mutex::new(()),
            active_auth_generation: AtomicU64::new(0),
            persistence: Some(persistence),
            next_id: AtomicU64::new(1),
        }
    }

    pub async fn insert(&self, new_session: NewAuthLoginSession) -> AuthLoginSession {
        let created_at = Utc::now();
        let expires_at =
            created_at + chrono::Duration::seconds(saturating_u64_to_i64(new_session.expires_in));
        let session = AuthLoginSession {
            login_id: self.next_login_id(),
            device_code: new_session.device_code,
            user_code: new_session.user_code,
            verification_uri: new_session.verification_uri,
            expires_in: new_session.expires_in,
            interval: new_session.interval,
            message: new_session.message,
            created_at,
            expires_at,
        };

        let mut sessions = self.sessions.write().await;
        sessions.retain(|_, session| session.expires_at > created_at);
        sessions.insert(session.login_id.clone(), session.clone());
        session
    }

    pub async fn get(&self, login_id: &str) -> Option<AuthLoginSession> {
        let now = Utc::now();
        self.sessions
            .read()
            .await
            .get(login_id)
            .filter(|session| session.expires_at > now)
            .cloned()
    }

    pub async fn complete_with_msa_token(
        &self,
        login_id: &str,
        new_token: NewAuthLoginMsaToken,
    ) -> Option<AuthLoginMsaToken> {
        let now = Utc::now();
        let session = self.sessions.write().await.remove(login_id);
        if !session.is_some_and(|session| session.expires_at > now) {
            return None;
        }

        let token = AuthLoginMsaToken {
            login_id: login_id.to_string(),
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

        *self.active_msa_token.write().await = Some(token.clone());
        *self.active_minecraft_account.write().await = None;
        self.bump_active_auth_generation();
        self.save_active_snapshot(&token, None);
        Some(token)
    }

    pub async fn complete_with_msa_and_minecraft_account(
        &self,
        login_id: &str,
        new_token: NewAuthLoginMsaToken,
        new_account: NewAuthLoginMinecraftAccount,
    ) -> Option<(AuthLoginMsaToken, AuthLoginMinecraftAccount)> {
        let now = Utc::now();
        let session = self.sessions.write().await.remove(login_id);
        if !session.is_some_and(|session| session.expires_at > now) {
            return None;
        }

        let token = AuthLoginMsaToken {
            login_id: login_id.to_string(),
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
            login_id: login_id.to_string(),
            access_token: new_account.access_token,
            token_type: new_account.token_type,
            expires_in: new_account.expires_in,
            profile: new_account.profile,
            owns_minecraft_java: new_account.owns_minecraft_java,
            authenticated_at: now,
            expires_at: now
                + chrono::Duration::seconds(saturating_u64_to_i64(new_account.expires_in)),
        };

        *self.active_msa_token.write().await = Some(token.clone());
        *self.active_minecraft_account.write().await = Some(account.clone());
        self.bump_active_auth_generation();
        self.save_active_snapshot(&token, Some(&account));
        Some((token, account))
    }

    pub async fn refresh_with_msa_and_minecraft_account(
        &self,
        new_token: NewAuthLoginMsaToken,
        new_account: NewAuthLoginMinecraftAccount,
        fallback_refresh_token: &str,
    ) -> Option<(AuthLoginMsaToken, AuthLoginMinecraftAccount)> {
        let now = Utc::now();
        let login_id = self
            .active_msa_token
            .read()
            .await
            .as_ref()?
            .login_id
            .clone();
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
            login_id,
            access_token: new_account.access_token,
            token_type: new_account.token_type,
            expires_in: new_account.expires_in,
            profile: new_account.profile,
            owns_minecraft_java: new_account.owns_minecraft_java,
            authenticated_at: now,
            expires_at: now
                + chrono::Duration::seconds(saturating_u64_to_i64(new_account.expires_in)),
        };

        *self.active_msa_token.write().await = Some(token.clone());
        *self.active_minecraft_account.write().await = Some(account.clone());
        self.bump_active_auth_generation();
        self.save_active_snapshot(&token, Some(&account));
        Some((token, account))
    }

    pub async fn refresh_with_msa_token(
        &self,
        new_token: NewAuthLoginMsaToken,
        fallback_refresh_token: &str,
    ) -> Option<AuthLoginMsaToken> {
        let now = Utc::now();
        let login_id = self
            .active_msa_token
            .read()
            .await
            .as_ref()?
            .login_id
            .clone();
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
        let preserved_account = {
            let mut account = self.active_minecraft_account.write().await;
            match account.as_ref() {
                Some(active) if active.login_id == login_id && active.expires_at > now => {
                    account.clone()
                }
                Some(_) => {
                    *account = None;
                    None
                }
                None => None,
            }
        };

        *self.active_msa_token.write().await = Some(token.clone());
        self.bump_active_auth_generation();
        self.save_active_snapshot(&token, preserved_account.as_ref());
        Some(token)
    }

    pub async fn increase_interval(&self, login_id: &str, additional_seconds: u64) -> Option<u64> {
        let now = Utc::now();
        let mut sessions = self.sessions.write().await;
        let session = sessions
            .get_mut(login_id)
            .filter(|session| session.expires_at > now)?;
        session.interval = session.interval.saturating_add(additional_seconds);
        Some(session.interval)
    }

    pub async fn remove(&self, login_id: &str) -> bool {
        self.sessions.write().await.remove(login_id).is_some()
    }

    pub async fn has_active_msa_auth(&self) -> bool {
        self.active_msa_auth_remaining_seconds().await.is_some()
    }

    pub async fn active_msa_auth_remaining_seconds(&self) -> Option<u64> {
        let mut token = self.active_msa_token.write().await;
        let (expires_at, expires_in, has_refresh_token) = {
            let active = token.as_ref()?;
            (
                active.expires_at,
                active.expires_in,
                active
                    .refresh_token
                    .as_deref()
                    .is_some_and(|refresh_token| !refresh_token.trim().is_empty()),
            )
        };
        let remaining = (expires_at - Utc::now()).num_milliseconds();
        if remaining <= 0 {
            if !has_refresh_token {
                *token = None;
            }
            return None;
        }

        Some((((remaining as u64) + 999) / 1000).min(expires_in))
    }

    pub async fn active_msa_refresh_token(&self) -> Option<String> {
        self.active_msa_token
            .read()
            .await
            .as_ref()
            .and_then(|token| token.refresh_token.as_deref())
            .map(str::trim)
            .filter(|refresh_token| !refresh_token.is_empty())
            .map(ToOwned::to_owned)
    }

    pub async fn active_msa_token_state(&self) -> Option<ActiveMsaTokenState> {
        let mut token = self.active_msa_token.write().await;
        let (expires_at, expires_in, has_refresh_token) = {
            let active = token.as_ref()?;
            (
                active.expires_at,
                active.expires_in,
                active
                    .refresh_token
                    .as_deref()
                    .is_some_and(|refresh_token| !refresh_token.trim().is_empty()),
            )
        };
        let remaining = (expires_at - Utc::now()).num_milliseconds();
        if remaining <= 0 {
            if !has_refresh_token {
                *token = None;
                self.bump_active_auth_generation();
            }
            return None;
        }

        Some(ActiveMsaTokenState {
            token: token.as_ref()?.clone(),
            token_expires_in: (((remaining as u64) + 999) / 1000).min(expires_in),
        })
    }

    pub async fn active_minecraft_account_state(&self) -> Option<ActiveMinecraftAccountState> {
        let mut account = self.active_minecraft_account.write().await;
        let (expires_at, expires_in) = {
            let active = account.as_ref()?;
            (active.expires_at, active.expires_in)
        };
        let remaining = (expires_at - Utc::now()).num_milliseconds();
        if remaining <= 0 {
            *account = None;
            self.bump_active_auth_generation();
            return None;
        }

        Some(ActiveMinecraftAccountState {
            account: account.as_ref()?.clone(),
            token_expires_in: (((remaining as u64) + 999) / 1000).min(expires_in),
        })
    }

    pub async fn active_current_minecraft_account_state(
        &self,
    ) -> Option<ActiveMinecraftAccountState> {
        let msa_state = self.active_msa_token_state().await?;
        let minecraft_state = self.active_minecraft_account_state().await?;
        if minecraft_state.account.login_id != msa_state.token.login_id
            || minecraft_state.account.authenticated_at < msa_state.token.authenticated_at
        {
            return None;
        }

        Some(minecraft_state)
    }

    pub async fn clear_all(&self) -> (usize, bool) {
        let cleared_pending_logins = {
            let mut sessions = self.sessions.write().await;
            let len = sessions.len();
            sessions.clear();
            len
        };
        let had_msa_auth = self.active_msa_token.write().await.take().is_some();
        *self.active_minecraft_account.write().await = None;
        self.bump_active_auth_generation();
        self.delete_active_snapshot();

        (cleared_pending_logins, had_msa_auth)
    }

    pub async fn clear_active_auth(&self) -> bool {
        let had_msa_auth = self.active_msa_token.write().await.take().is_some();
        let had_minecraft_account = self.active_minecraft_account.write().await.take().is_some();
        if had_msa_auth || had_minecraft_account {
            self.bump_active_auth_generation();
        }
        self.delete_active_snapshot();
        had_msa_auth || had_minecraft_account
    }

    pub(crate) async fn active_auth_refresh_guard(&self) -> MutexGuard<'_, ()> {
        self.active_auth_refresh.lock().await
    }

    pub(crate) fn active_auth_generation(&self) -> u64 {
        self.active_auth_generation.load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub async fn active_msa_token(&self) -> Option<AuthLoginMsaToken> {
        self.active_msa_token.read().await.clone()
    }

    #[cfg(test)]
    pub async fn active_minecraft_account(&self) -> Option<AuthLoginMinecraftAccount> {
        self.active_minecraft_account.read().await.clone()
    }

    pub async fn remove_expired(&self, login_id: &str) -> bool {
        let now = Utc::now();
        let session = {
            let sessions = self.sessions.read().await;
            sessions.get(login_id).cloned()
        };

        match session {
            Some(session) if session.expires_at > now => false,
            Some(_) => {
                self.sessions.write().await.remove(login_id);
                true
            }
            None => false,
        }
    }

    pub async fn len(&self) -> usize {
        self.sessions.read().await.len()
    }

    fn next_login_id(&self) -> String {
        let sequence = self.next_id.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or_default();
        format!("msa-{nanos:x}-{sequence:x}")
    }

    fn bump_active_auth_generation(&self) {
        self.active_auth_generation.fetch_add(1, Ordering::AcqRel);
    }

    fn save_active_snapshot(
        &self,
        token: &AuthLoginMsaToken,
        account: Option<&AuthLoginMinecraftAccount>,
    ) {
        let Some(persistence) = &self.persistence else {
            return;
        };

        let snapshot = PersistedAuthSnapshot::from_active(token, account);
        if let Err(error) = persistence.save_snapshot(&snapshot) {
            tracing::warn!("auth snapshot persistence save failed: {error}");
        }
    }

    fn delete_active_snapshot(&self) {
        let Some(persistence) = &self.persistence else {
            return;
        };

        if let Err(error) = persistence.delete_snapshot() {
            tracing::warn!("auth snapshot persistence delete failed: {error}");
        }
    }
}

impl Default for AuthLoginStore {
    fn default() -> Self {
        Self::new()
    }
}

fn saturating_u64_to_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn load_active_snapshot(
    persistence: &dyn AuthSnapshotPersistence,
) -> (Option<AuthLoginMsaToken>, Option<AuthLoginMinecraftAccount>) {
    let snapshot = match persistence.load_snapshot() {
        Ok(Some(snapshot)) => snapshot,
        Ok(None) => return (None, None),
        Err(AuthPersistenceError::Malformed(error)) => {
            tracing::warn!(
                "stored auth snapshot is malformed; clearing secure auth entry: {error}"
            );
            clear_rejected_snapshot(persistence);
            return (None, None);
        }
        Err(AuthPersistenceError::Unavailable(error)) => {
            tracing::warn!(
                "secure auth storage is unavailable; starting without restored auth: {error}"
            );
            return (None, None);
        }
    };

    match snapshot.into_active(Utc::now()) {
        Ok((token, account)) => (Some(token), account),
        Err(AuthSnapshotRejection::Expired) => {
            tracing::warn!("stored auth snapshot is expired; clearing secure auth entry");
            clear_rejected_snapshot(persistence);
            (None, None)
        }
        Err(AuthSnapshotRejection::Malformed) => {
            tracing::warn!("stored auth snapshot is malformed; clearing secure auth entry");
            clear_rejected_snapshot(persistence);
            (None, None)
        }
    }
}

fn clear_rejected_snapshot(persistence: &dyn AuthSnapshotPersistence) {
    if let Err(error) = persistence.delete_snapshot() {
        tracing::warn!("auth snapshot persistence cleanup failed: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::auth_persistence::{
        test_persisted_snapshot, test_persisted_snapshot_with_version,
    };
    use std::sync::Mutex;

    #[derive(Default)]
    struct MockAuthSnapshotPersistence {
        snapshot: Mutex<Option<PersistedAuthSnapshot>>,
        saves: AtomicU64,
        deletes: AtomicU64,
    }

    impl MockAuthSnapshotPersistence {
        fn with_snapshot(snapshot: PersistedAuthSnapshot) -> Self {
            Self {
                snapshot: Mutex::new(Some(snapshot)),
                saves: AtomicU64::new(0),
                deletes: AtomicU64::new(0),
            }
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
    }

    impl AuthSnapshotPersistence for MockAuthSnapshotPersistence {
        fn load_snapshot(&self) -> Result<Option<PersistedAuthSnapshot>, AuthPersistenceError> {
            Ok(self.snapshot())
        }

        fn save_snapshot(
            &self,
            snapshot: &PersistedAuthSnapshot,
        ) -> Result<(), AuthPersistenceError> {
            *self.snapshot.lock().expect("snapshot lock") = Some(snapshot.clone());
            self.saves.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }

        fn delete_snapshot(&self) -> Result<(), AuthPersistenceError> {
            *self.snapshot.lock().expect("snapshot lock") = None;
            self.deletes.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
    }

    #[tokio::test]
    async fn auth_login_store_keeps_raw_device_code_server_side() {
        let store = AuthLoginStore::new();

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

        assert!(session.login_id.starts_with("msa-"));
        assert_eq!(session.device_code, "raw-device-code");
        assert_eq!(session.user_code, "ABCD-EFGH");
        assert_eq!(session.expires_in, 900);
        assert_eq!(session.interval, 5);
        assert!(session.expires_at > session.created_at);
        assert_eq!(store.get(&session.login_id).await, Some(session));
    }

    #[tokio::test]
    async fn auth_login_store_prunes_expired_sessions_on_insert() {
        let store = AuthLoginStore::new();

        let expired = store
            .insert(NewAuthLoginSession {
                device_code: "expired-device-code".to_string(),
                user_code: "OLD-CODE".to_string(),
                verification_uri: "https://www.microsoft.com/link".to_string(),
                expires_in: 0,
                interval: 5,
                message: None,
            })
            .await;
        let active = store
            .insert(NewAuthLoginSession {
                device_code: "active-device-code".to_string(),
                user_code: "NEW-CODE".to_string(),
                verification_uri: "https://www.microsoft.com/link".to_string(),
                expires_in: 900,
                interval: 5,
                message: None,
            })
            .await;

        assert_eq!(store.get(&expired.login_id).await, None);
        assert_eq!(store.get(&active.login_id).await, Some(active));
        assert_eq!(store.len().await, 1);
    }

    #[tokio::test]
    async fn auth_login_store_removes_expired_known_session() {
        let store = AuthLoginStore::new();

        let expired = store
            .insert(NewAuthLoginSession {
                device_code: "expired-device-code".to_string(),
                user_code: "OLD-CODE".to_string(),
                verification_uri: "https://www.microsoft.com/link".to_string(),
                expires_in: 0,
                interval: 5,
                message: None,
            })
            .await;

        assert!(store.remove_expired(&expired.login_id).await);
        assert!(!store.remove_expired(&expired.login_id).await);
        assert_eq!(store.len().await, 0);
    }

    #[tokio::test]
    async fn auth_login_store_does_not_remove_pending_session() {
        let store = AuthLoginStore::new();

        let active = store
            .insert(NewAuthLoginSession {
                device_code: "active-device-code".to_string(),
                user_code: "NEW-CODE".to_string(),
                verification_uri: "https://www.microsoft.com/link".to_string(),
                expires_in: 900,
                interval: 5,
                message: None,
            })
            .await;

        assert!(!store.remove_expired(&active.login_id).await);
        assert_eq!(store.get(&active.login_id).await, Some(active));
    }

    #[tokio::test]
    async fn auth_login_store_keeps_only_one_active_msa_token() {
        let store = AuthLoginStore::new();
        let first = store
            .insert(NewAuthLoginSession {
                device_code: "first-device-code".to_string(),
                user_code: "FIRST".to_string(),
                verification_uri: "https://www.microsoft.com/link".to_string(),
                expires_in: 900,
                interval: 5,
                message: None,
            })
            .await;
        let second = store
            .insert(NewAuthLoginSession {
                device_code: "second-device-code".to_string(),
                user_code: "SECOND".to_string(),
                verification_uri: "https://www.microsoft.com/link".to_string(),
                expires_in: 900,
                interval: 5,
                message: None,
            })
            .await;

        store
            .complete_with_msa_token(
                &first.login_id,
                NewAuthLoginMsaToken {
                    access_token: "first-access-token".to_string(),
                    refresh_token: None,
                    id_token: None,
                    token_type: "Bearer".to_string(),
                    expires_in: 3600,
                    scope: None,
                },
            )
            .await
            .expect("first token");
        store
            .complete_with_msa_token(
                &second.login_id,
                NewAuthLoginMsaToken {
                    access_token: "second-access-token".to_string(),
                    refresh_token: Some("second-refresh-token".to_string()),
                    id_token: None,
                    token_type: "Bearer".to_string(),
                    expires_in: 3600,
                    scope: Some("XboxLive.signin offline_access".to_string()),
                },
            )
            .await
            .expect("second token");

        let active = store.active_msa_token().await.expect("active token");
        assert_eq!(active.login_id, second.login_id);
        assert_eq!(active.access_token, "second-access-token");
        assert_eq!(
            active.refresh_token,
            Some("second-refresh-token".to_string())
        );
        assert_eq!(store.get(&first.login_id).await, None);
        assert_eq!(store.get(&second.login_id).await, None);
    }

    #[tokio::test]
    async fn auth_login_store_clear_all_removes_pending_and_active_msa_auth() {
        let store = AuthLoginStore::new();
        let session = store
            .insert(NewAuthLoginSession {
                device_code: "raw-device-code".to_string(),
                user_code: "ABCD-EFGH".to_string(),
                verification_uri: "https://www.microsoft.com/link".to_string(),
                expires_in: 900,
                interval: 5,
                message: None,
            })
            .await;
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
        let pending = store
            .insert(NewAuthLoginSession {
                device_code: "pending-device-code".to_string(),
                user_code: "PENDING".to_string(),
                verification_uri: "https://www.microsoft.com/link".to_string(),
                expires_in: 900,
                interval: 5,
                message: None,
            })
            .await;

        let summary = store.clear_all().await;

        assert_eq!(summary, (1, true));
        assert_eq!(store.get(&pending.login_id).await, None);
        assert_eq!(store.active_msa_token().await, None);

        assert_eq!(store.clear_all().await, (0, false));
    }

    #[tokio::test]
    async fn auth_login_store_drops_expired_active_msa_auth() {
        let store = AuthLoginStore::new();
        let session = store
            .insert(NewAuthLoginSession {
                device_code: "raw-device-code".to_string(),
                user_code: "ABCD-EFGH".to_string(),
                verification_uri: "https://www.microsoft.com/link".to_string(),
                expires_in: 900,
                interval: 5,
                message: None,
            })
            .await;
        store
            .complete_with_msa_token(
                &session.login_id,
                NewAuthLoginMsaToken {
                    access_token: "msa-access-token".to_string(),
                    refresh_token: None,
                    id_token: None,
                    token_type: "Bearer".to_string(),
                    expires_in: 0,
                    scope: None,
                },
            )
            .await
            .expect("complete login");

        assert_eq!(store.active_msa_auth_remaining_seconds().await, None);
        assert_eq!(store.active_msa_token().await, None);
        assert!(!store.has_active_msa_auth().await);
    }

    #[tokio::test]
    async fn auth_login_store_keeps_expired_msa_refresh_material() {
        let store = AuthLoginStore::new();
        let session = store
            .insert(NewAuthLoginSession {
                device_code: "raw-device-code".to_string(),
                user_code: "ABCD-EFGH".to_string(),
                verification_uri: "https://www.microsoft.com/link".to_string(),
                expires_in: 900,
                interval: 5,
                message: None,
            })
            .await;
        store
            .complete_with_msa_token(
                &session.login_id,
                NewAuthLoginMsaToken {
                    access_token: "msa-access-token".to_string(),
                    refresh_token: Some("msa-refresh-token".to_string()),
                    id_token: None,
                    token_type: "Bearer".to_string(),
                    expires_in: 0,
                    scope: None,
                },
            )
            .await
            .expect("complete login");

        assert_eq!(store.active_msa_auth_remaining_seconds().await, None);
        assert_eq!(
            store.active_msa_refresh_token().await,
            Some("msa-refresh-token".to_string())
        );
        assert!(store.active_msa_token().await.is_some());
        assert!(!store.has_active_msa_auth().await);
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

        let store = AuthLoginStore::with_persistence(persistence.clone());

        assert_eq!(store.active_msa_token().await, Some(token));
        assert_eq!(store.active_minecraft_account().await, Some(account));
        assert_eq!(persistence.deletes(), 0);
    }

    #[tokio::test]
    async fn auth_login_store_clears_expired_secure_auth_snapshot_on_restore() {
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

        let store = AuthLoginStore::with_persistence(persistence.clone());

        assert_eq!(store.active_msa_token().await, None);
        assert_eq!(persistence.snapshot(), None);
        assert_eq!(persistence.deletes(), 1);
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

        let store = AuthLoginStore::with_persistence(persistence.clone());

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
    async fn auth_login_store_clears_wrong_schema_secure_auth_snapshot_on_restore() {
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
        let persistence = Arc::new(MockAuthSnapshotPersistence::with_snapshot(
            test_persisted_snapshot_with_version(&token, None, 2),
        ));

        let store = AuthLoginStore::with_persistence(persistence.clone());

        assert_eq!(store.active_msa_token().await, None);
        assert_eq!(persistence.snapshot(), None);
        assert_eq!(persistence.deletes(), 1);
    }

    #[tokio::test]
    async fn auth_login_store_clears_blank_msa_token_secure_auth_snapshot_on_restore() {
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
            &["active_msa_token", "access_token"],
            "   ",
        );
        let persistence = Arc::new(MockAuthSnapshotPersistence::with_snapshot(snapshot));

        let store = AuthLoginStore::with_persistence(persistence.clone());

        assert_eq!(store.active_msa_token().await, None);
        assert_eq!(persistence.snapshot(), None);
        assert_eq!(persistence.deletes(), 1);
    }

    #[tokio::test]
    async fn auth_login_store_clears_blank_msa_refresh_token_secure_auth_snapshot_on_restore() {
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
            &["active_msa_token", "refresh_token"],
            "   ",
        );
        let persistence = Arc::new(MockAuthSnapshotPersistence::with_snapshot(snapshot));

        let store = AuthLoginStore::with_persistence(persistence.clone());

        assert_eq!(store.active_msa_token().await, None);
        assert_eq!(persistence.snapshot(), None);
        assert_eq!(persistence.deletes(), 1);
    }

    #[tokio::test]
    async fn auth_login_store_clears_blank_minecraft_snapshot_on_restore() {
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
                &["active_minecraft_account", "access_token"],
                "   ",
            ),
            &["active_minecraft_account", "profile", "name"],
            "   ",
        );
        let persistence = Arc::new(MockAuthSnapshotPersistence::with_snapshot(snapshot));

        let store = AuthLoginStore::with_persistence(persistence.clone());

        assert_eq!(store.active_msa_token().await, None);
        assert_eq!(store.active_minecraft_account().await, None);
        assert_eq!(persistence.snapshot(), None);
        assert_eq!(persistence.deletes(), 1);
    }

    #[tokio::test]
    async fn auth_login_store_clears_mismatched_login_ids_secure_auth_snapshot_on_restore() {
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

        let store = AuthLoginStore::with_persistence(persistence.clone());

        assert_eq!(store.active_msa_token().await, None);
        assert_eq!(store.active_minecraft_account().await, None);
        assert_eq!(persistence.snapshot(), None);
        assert_eq!(persistence.deletes(), 1);
    }

    #[tokio::test]
    async fn auth_login_store_persists_secure_auth_snapshot_on_completion() {
        let persistence = Arc::new(MockAuthSnapshotPersistence::default());
        let store = AuthLoginStore::with_persistence(persistence.clone());
        let session = store
            .insert(NewAuthLoginSession {
                device_code: "raw-device-code".to_string(),
                user_code: "ABCD-EFGH".to_string(),
                verification_uri: "https://www.microsoft.com/link".to_string(),
                expires_in: 900,
                interval: 5,
                message: None,
            })
            .await;

        store
            .complete_with_msa_and_minecraft_account(
                &session.login_id,
                NewAuthLoginMsaToken {
                    access_token: "msa-access-token".to_string(),
                    refresh_token: Some("msa-refresh-token".to_string()),
                    id_token: Some("msa-id-token".to_string()),
                    token_type: "Bearer".to_string(),
                    expires_in: 3600,
                    scope: Some("XboxLive.signin offline_access".to_string()),
                },
                NewAuthLoginMinecraftAccount {
                    access_token: "minecraft-access-token".to_string(),
                    token_type: Some("Bearer".to_string()),
                    expires_in: 3600,
                    profile: test_profile("PersistedPlayer"),
                    owns_minecraft_java: true,
                },
            )
            .await
            .expect("complete login");

        assert_eq!(persistence.saves(), 1);
        assert!(persistence.snapshot().is_some());
    }

    #[tokio::test]
    async fn auth_login_store_persists_rotated_refresh_snapshot() {
        let persistence = Arc::new(MockAuthSnapshotPersistence::default());
        let store = AuthLoginStore::with_persistence(persistence.clone());
        let session = store
            .insert(NewAuthLoginSession {
                device_code: "raw-device-code".to_string(),
                user_code: "ABCD-EFGH".to_string(),
                verification_uri: "https://www.microsoft.com/link".to_string(),
                expires_in: 900,
                interval: 5,
                message: None,
            })
            .await;
        store
            .complete_with_msa_token(
                &session.login_id,
                NewAuthLoginMsaToken {
                    access_token: "old-msa-access-token".to_string(),
                    refresh_token: Some("old-msa-refresh-token".to_string()),
                    id_token: None,
                    token_type: "Bearer".to_string(),
                    expires_in: 0,
                    scope: Some("XboxLive.signin offline_access".to_string()),
                },
            )
            .await
            .expect("complete login");

        store
            .refresh_with_msa_and_minecraft_account(
                NewAuthLoginMsaToken {
                    access_token: "new-msa-access-token".to_string(),
                    refresh_token: Some("new-msa-refresh-token".to_string()),
                    id_token: None,
                    token_type: "Bearer".to_string(),
                    expires_in: 3600,
                    scope: Some("XboxLive.signin offline_access".to_string()),
                },
                NewAuthLoginMinecraftAccount {
                    access_token: "minecraft-access-token".to_string(),
                    token_type: Some("Bearer".to_string()),
                    expires_in: 3600,
                    profile: test_profile("RotatedRefresh"),
                    owns_minecraft_java: true,
                },
                "old-msa-refresh-token",
            )
            .await
            .expect("refresh active auth");

        let restored = AuthLoginStore::with_persistence(persistence.clone());
        let active = restored.active_msa_token().await.expect("restored token");
        assert_eq!(active.access_token, "new-msa-access-token");
        assert_eq!(
            active.refresh_token,
            Some("new-msa-refresh-token".to_string())
        );
        assert_eq!(persistence.saves(), 2);
    }

    #[tokio::test]
    async fn auth_login_store_persists_msa_only_rotated_refresh_snapshot() {
        let persistence = Arc::new(MockAuthSnapshotPersistence::default());
        let store = AuthLoginStore::with_persistence(persistence.clone());
        let session = store
            .insert(NewAuthLoginSession {
                device_code: "raw-device-code".to_string(),
                user_code: "ABCD-EFGH".to_string(),
                verification_uri: "https://www.microsoft.com/link".to_string(),
                expires_in: 900,
                interval: 5,
                message: None,
            })
            .await;
        store
            .complete_with_msa_token(
                &session.login_id,
                NewAuthLoginMsaToken {
                    access_token: "old-msa-access-token".to_string(),
                    refresh_token: Some("old-msa-refresh-token".to_string()),
                    id_token: None,
                    token_type: "Bearer".to_string(),
                    expires_in: 0,
                    scope: Some("XboxLive.signin offline_access".to_string()),
                },
            )
            .await
            .expect("complete login");

        store
            .refresh_with_msa_token(
                NewAuthLoginMsaToken {
                    access_token: "new-msa-access-token".to_string(),
                    refresh_token: Some("new-msa-refresh-token".to_string()),
                    id_token: None,
                    token_type: "Bearer".to_string(),
                    expires_in: 3600,
                    scope: Some("XboxLive.signin offline_access".to_string()),
                },
                "old-msa-refresh-token",
            )
            .await
            .expect("refresh msa token");

        let restored = AuthLoginStore::with_persistence(persistence.clone());
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
        let session = store
            .insert(NewAuthLoginSession {
                device_code: "raw-device-code".to_string(),
                user_code: "ABCD-EFGH".to_string(),
                verification_uri: "https://www.microsoft.com/link".to_string(),
                expires_in: 900,
                interval: 5,
                message: None,
            })
            .await;
        store
            .complete_with_msa_and_minecraft_account(
                &session.login_id,
                NewAuthLoginMsaToken {
                    access_token: "old-msa-access-token".to_string(),
                    refresh_token: Some("old-msa-refresh-token".to_string()),
                    id_token: None,
                    token_type: "Bearer".to_string(),
                    expires_in: 3600,
                    scope: Some("XboxLive.signin offline_access".to_string()),
                },
                NewAuthLoginMinecraftAccount {
                    access_token: "old-minecraft-access-token".to_string(),
                    token_type: Some("Bearer".to_string()),
                    expires_in: 3600,
                    profile: test_profile("PreservedPlayer"),
                    owns_minecraft_java: true,
                },
            )
            .await
            .expect("complete login");

        store
            .refresh_with_msa_token(
                NewAuthLoginMsaToken {
                    access_token: "new-msa-access-token".to_string(),
                    refresh_token: Some("new-msa-refresh-token".to_string()),
                    id_token: None,
                    token_type: "Bearer".to_string(),
                    expires_in: 3600,
                    scope: Some("XboxLive.signin offline_access".to_string()),
                },
                "old-msa-refresh-token",
            )
            .await
            .expect("refresh msa token");

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
        let store = AuthLoginStore::with_persistence(persistence.clone());
        let session = store
            .insert(NewAuthLoginSession {
                device_code: "raw-device-code".to_string(),
                user_code: "ABCD-EFGH".to_string(),
                verification_uri: "https://www.microsoft.com/link".to_string(),
                expires_in: 900,
                interval: 5,
                message: None,
            })
            .await;
        store
            .complete_with_msa_token(
                &session.login_id,
                NewAuthLoginMsaToken {
                    access_token: "msa-access-token".to_string(),
                    refresh_token: None,
                    id_token: None,
                    token_type: "Bearer".to_string(),
                    expires_in: 3600,
                    scope: None,
                },
            )
            .await
            .expect("complete login");

        assert!(store.clear_active_auth().await);

        assert_eq!(persistence.snapshot(), None);
        assert_eq!(persistence.deletes(), 1);
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
            cursor = cursor
                .get_mut(*segment)
                .unwrap_or_else(|| panic!("snapshot field {segment}"));
        }

        *cursor
            .get_mut(path[path.len() - 1])
            .unwrap_or_else(|| panic!("snapshot field {}", path[path.len() - 1])) =
            serde_json::Value::String(value.to_string());

        serde_json::from_value(serialized).expect("deserialize persisted snapshot")
    }
}
