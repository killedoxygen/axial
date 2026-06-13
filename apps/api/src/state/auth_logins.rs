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
    PersistedAuthState,
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

pub struct AuthLoginStore {
    msa_tokens: RwLock<HashMap<String, AuthLoginMsaToken>>,
    minecraft_accounts: RwLock<HashMap<String, AuthLoginMinecraftAccount>>,
    active_login_id: RwLock<Option<String>>,
    active_auth_refresh: Mutex<()>,
    active_auth_generation: AtomicU64,
    persistence: Option<Arc<dyn AuthSnapshotPersistence>>,
    next_id: AtomicU64,
}

impl AuthLoginStore {
    pub fn new() -> Self {
        Self {
            msa_tokens: RwLock::new(HashMap::new()),
            minecraft_accounts: RwLock::new(HashMap::new()),
            active_login_id: RwLock::new(None),
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
        let state = load_active_snapshot(persistence.as_ref());

        Self {
            msa_tokens: RwLock::new(
                state
                    .msa_tokens
                    .into_iter()
                    .map(|token| (token.login_id.clone(), token))
                    .collect(),
            ),
            minecraft_accounts: RwLock::new(
                state
                    .minecraft_accounts
                    .into_iter()
                    .map(|account| (account.login_id.clone(), account))
                    .collect(),
            ),
            active_login_id: RwLock::new(state.active_login_id),
            active_auth_refresh: Mutex::new(()),
            active_auth_generation: AtomicU64::new(0),
            persistence: Some(persistence),
            next_id: AtomicU64::new(1),
        }
    }

    pub async fn replace_with_msa_token(
        &self,
        new_token: NewAuthLoginMsaToken,
    ) -> AuthLoginMsaToken {
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

        self.msa_tokens
            .write()
            .await
            .insert(token.login_id.clone(), token.clone());
        self.minecraft_accounts.write().await.remove(&login_id);
        *self.active_login_id.write().await = Some(token.login_id.clone());
        self.bump_active_auth_generation();
        self.save_auth_snapshot().await;
        token
    }

    pub async fn replace_with_msa_and_minecraft_account(
        &self,
        new_token: NewAuthLoginMsaToken,
        new_account: NewAuthLoginMinecraftAccount,
    ) -> (AuthLoginMsaToken, AuthLoginMinecraftAccount) {
        let (login_id, token, account) = self
            .new_msa_and_minecraft_account_pair(new_token, new_account, Utc::now())
            .await;

        self.msa_tokens
            .write()
            .await
            .insert(token.login_id.clone(), token.clone());
        self.minecraft_accounts
            .write()
            .await
            .insert(account.login_id.clone(), account.clone());
        *self.active_login_id.write().await = Some(login_id);
        self.bump_active_auth_generation();
        self.save_auth_snapshot().await;
        (token, account)
    }

    pub(crate) async fn replace_with_msa_and_minecraft_account_durable(
        &self,
        new_token: NewAuthLoginMsaToken,
        new_account: NewAuthLoginMinecraftAccount,
    ) -> Result<(AuthLoginMsaToken, AuthLoginMinecraftAccount), AuthPersistenceError> {
        let (login_id, token, account) = self
            .new_msa_and_minecraft_account_pair(new_token, new_account, Utc::now())
            .await;
        let mut next_msa_tokens = self.msa_tokens.read().await.clone();
        let mut next_minecraft_accounts = self.minecraft_accounts.read().await.clone();
        next_msa_tokens.insert(token.login_id.clone(), token.clone());
        next_minecraft_accounts.insert(account.login_id.clone(), account.clone());
        let next_msa_values = next_msa_tokens.values().cloned().collect::<Vec<_>>();
        let next_minecraft_values = next_minecraft_accounts
            .values()
            .cloned()
            .collect::<Vec<_>>();

        self.persist_auth_snapshot(Some(&login_id), &next_msa_values, &next_minecraft_values)?;

        *self.msa_tokens.write().await = next_msa_tokens;
        *self.minecraft_accounts.write().await = next_minecraft_accounts;
        *self.active_login_id.write().await = Some(login_id);
        self.bump_active_auth_generation();
        Ok((token, account))
    }

    async fn new_msa_and_minecraft_account_pair(
        &self,
        new_token: NewAuthLoginMsaToken,
        new_account: NewAuthLoginMinecraftAccount,
        now: DateTime<Utc>,
    ) -> (String, AuthLoginMsaToken, AuthLoginMinecraftAccount) {
        let profile_id = new_account.profile.id.trim().to_string();
        let login_id = self
            .minecraft_accounts
            .read()
            .await
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

    pub async fn refresh_with_msa_and_minecraft_account(
        &self,
        new_token: NewAuthLoginMsaToken,
        new_account: NewAuthLoginMinecraftAccount,
        fallback_refresh_token: &str,
    ) -> Option<(AuthLoginMsaToken, AuthLoginMinecraftAccount)> {
        let login_id = self.active_login_id.read().await.clone()?;
        self.refresh_login_with_msa_and_minecraft_account(
            &login_id,
            new_token,
            new_account,
            fallback_refresh_token,
        )
        .await
    }

    pub async fn refresh_login_with_msa_and_minecraft_account(
        &self,
        login_id: &str,
        new_token: NewAuthLoginMsaToken,
        new_account: NewAuthLoginMinecraftAccount,
        fallback_refresh_token: &str,
    ) -> Option<(AuthLoginMsaToken, AuthLoginMinecraftAccount)> {
        let now = Utc::now();
        let login_id = login_id.trim().to_string();
        if login_id.is_empty() {
            return None;
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

        {
            let mut msa_tokens = self.msa_tokens.write().await;
            if !msa_tokens.contains_key(&login_id) {
                return None;
            }
            let mut minecraft_accounts = self.minecraft_accounts.write().await;
            msa_tokens.insert(token.login_id.clone(), token.clone());
            minecraft_accounts.insert(account.login_id.clone(), account.clone());
        }
        self.bump_active_auth_generation();
        self.save_auth_snapshot().await;
        Some((token, account))
    }

    pub async fn refresh_with_msa_token(
        &self,
        new_token: NewAuthLoginMsaToken,
        fallback_refresh_token: &str,
    ) -> Option<AuthLoginMsaToken> {
        let login_id = self.active_login_id.read().await.clone()?;
        self.refresh_login_with_msa_token(&login_id, new_token, fallback_refresh_token)
            .await
    }

    pub async fn refresh_login_with_msa_token(
        &self,
        login_id: &str,
        new_token: NewAuthLoginMsaToken,
        fallback_refresh_token: &str,
    ) -> Option<AuthLoginMsaToken> {
        let now = Utc::now();
        let login_id = login_id.trim().to_string();
        if login_id.is_empty() {
            return None;
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

        {
            let mut msa_tokens = self.msa_tokens.write().await;
            if !msa_tokens.contains_key(&login_id) {
                return None;
            }
            let mut minecraft_accounts = self.minecraft_accounts.write().await;
            if minecraft_accounts
                .get(&login_id)
                .map_or(true, |account| account.expires_at <= now)
            {
                minecraft_accounts.remove(&login_id);
            }
            msa_tokens.insert(token.login_id.clone(), token.clone());
        }

        self.bump_active_auth_generation();
        self.save_auth_snapshot().await;
        Some(token)
    }

    pub async fn has_active_msa_auth(&self) -> bool {
        self.active_msa_auth_remaining_seconds().await.is_some()
    }

    pub async fn active_msa_auth_remaining_seconds(&self) -> Option<u64> {
        let login_id = self.active_login_id.read().await.clone()?;
        let token = self.msa_tokens.read().await.get(&login_id).cloned()?;
        let has_refresh_token = has_nonblank_refresh_token(&token);
        let expires_at = token.expires_at;
        let expires_in = token.expires_in;
        let remaining = (expires_at - Utc::now()).num_milliseconds();
        if remaining <= 0 {
            if !has_refresh_token {
                self.remove_login_from_memory(&login_id).await;
                self.save_auth_snapshot().await;
            }
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
        let login_id = self.active_login_id.read().await.clone()?;
        let refresh_token = self
            .msa_tokens
            .read()
            .await
            .get(&login_id)
            .and_then(|token| token.refresh_token.as_deref())
            .map(str::trim)
            .filter(|refresh_token| !refresh_token.is_empty())
            .map(ToOwned::to_owned)?;
        Some((login_id, refresh_token))
    }

    pub async fn account_states(&self) -> Vec<AuthLoginAccountState> {
        let now = Utc::now();
        let active_login_id = self.active_login_id.read().await.clone();
        let tokens = self.msa_tokens.read().await.clone();
        let accounts = self.minecraft_accounts.read().await.clone();
        let mut states = tokens
            .into_values()
            .map(|token| {
                let token_remaining =
                    remaining_seconds_option(token.expires_at, token.expires_in, now);
                let refresh_available = has_nonblank_refresh_token(&token);
                let account = accounts.get(&token.login_id).cloned().filter(|account| {
                    (token_remaining.is_some() || refresh_available)
                        && account.expires_at > now
                        && account.authenticated_at >= token.authenticated_at
                });
                let minecraft_token_expires_in = account.as_ref().and_then(|account| {
                    remaining_seconds_option(account.expires_at, account.expires_in, now)
                });
                AuthLoginAccountState {
                    login_id: token.login_id.clone(),
                    active: active_login_id.as_deref() == Some(token.login_id.as_str()),
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
        let login_id = self.active_login_id.read().await.clone()?;
        let token = self.msa_tokens.read().await.get(&login_id).cloned()?;
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
        let login_id = self.active_login_id.read().await.clone()?;
        let account = self
            .minecraft_accounts
            .read()
            .await
            .get(&login_id)
            .cloned()?;
        let expires_at = account.expires_at;
        let expires_in = account.expires_in;
        let remaining = (expires_at - Utc::now()).num_milliseconds();
        if remaining <= 0 {
            self.minecraft_accounts.write().await.remove(&login_id);
            self.bump_active_auth_generation();
            self.save_auth_snapshot().await;
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
        let login_id = self.active_login_id.read().await.clone()?;
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
        let token = self.msa_tokens.read().await.get(login_id).cloned()?;
        if token.expires_at <= now {
            return None;
        }
        let account = self
            .minecraft_accounts
            .read()
            .await
            .get(login_id)
            .cloned()?;
        if account.expires_at <= now {
            self.minecraft_accounts.write().await.remove(login_id);
            self.bump_active_auth_generation();
            self.save_auth_snapshot().await;
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

    pub async fn update_active_current_minecraft_profile(
        &self,
        login_id: &str,
        profile: AuthLoginMinecraftProfile,
    ) -> bool {
        self.update_active_current_minecraft_profile_and_ownership(login_id, profile, None)
            .await
            .is_some()
    }

    pub async fn update_active_current_minecraft_profile_and_ownership(
        &self,
        login_id: &str,
        profile: AuthLoginMinecraftProfile,
        owns_minecraft_java: Option<bool>,
    ) -> Option<ActiveMinecraftAccountState> {
        let now = Utc::now();
        let token = self.msa_tokens.read().await.get(login_id).cloned()?;
        if token.expires_at <= now {
            return None;
        }

        let updated_account = {
            let mut active_accounts = self.minecraft_accounts.write().await;
            let account = active_accounts.get_mut(login_id)?;
            if account.login_id != login_id
                || account.login_id != token.login_id
                || account.authenticated_at < token.authenticated_at
            {
                return None;
            }
            if account.expires_at <= now {
                active_accounts.remove(login_id);
                self.bump_active_auth_generation();
                return None;
            }

            account.profile = profile;
            if let Some(owns_minecraft_java) = owns_minecraft_java {
                account.owns_minecraft_java = owns_minecraft_java;
            }
            account.clone()
        };

        self.bump_active_auth_generation();
        self.save_auth_snapshot().await;
        Some(ActiveMinecraftAccountState {
            token_expires_in: remaining_seconds(
                updated_account.expires_at,
                updated_account.expires_in,
            ),
            account: updated_account,
        })
    }

    pub(crate) async fn clear_all(&self) -> Result<bool, AuthPersistenceError> {
        self.delete_active_snapshot()?;

        let had_msa_tokens = {
            let mut msa_tokens = self.msa_tokens.write().await;
            let had_msa_tokens = !msa_tokens.is_empty();
            msa_tokens.clear();
            had_msa_tokens
        };
        let had_minecraft_accounts = {
            let mut minecraft_accounts = self.minecraft_accounts.write().await;
            let had_minecraft_accounts = !minecraft_accounts.is_empty();
            minecraft_accounts.clear();
            had_minecraft_accounts
        };
        *self.active_login_id.write().await = None;
        self.bump_active_auth_generation();

        Ok(had_msa_tokens || had_minecraft_accounts)
    }

    pub(crate) async fn switch_active_account(
        &self,
        login_id: &str,
    ) -> Result<bool, AuthPersistenceError> {
        let msa_tokens = self.msa_tokens.read().await;
        if !msa_tokens.contains_key(login_id) {
            return Ok(false);
        }
        *self.active_login_id.write().await = Some(login_id.to_string());
        self.bump_active_auth_generation();

        let msa_values = msa_tokens.values().cloned().collect::<Vec<_>>();
        let minecraft_values = self
            .minecraft_accounts
            .read()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        if let Err(error) =
            self.persist_auth_snapshot(Some(login_id), &msa_values, &minecraft_values)
        {
            tracing::warn!(
                "auth snapshot persistence failed while switching active Microsoft account: {error}"
            );
        }
        Ok(true)
    }

    pub(crate) async fn remove_account(
        &self,
        login_id: &str,
    ) -> Result<bool, AuthPersistenceError> {
        let mut msa_tokens = self.msa_tokens.write().await;
        let mut minecraft_accounts = self.minecraft_accounts.write().await;
        let mut active_login_id = self.active_login_id.write().await;

        let mut next_msa_tokens = msa_tokens.clone();
        let mut next_minecraft_accounts = minecraft_accounts.clone();
        let removed_msa = next_msa_tokens.remove(login_id).is_some();
        let removed_minecraft = next_minecraft_accounts.remove(login_id).is_some();
        let removed = removed_msa || removed_minecraft;
        if !removed {
            return Ok(false);
        }

        let next_active_login_id = if active_login_id.as_deref() == Some(login_id) {
            None
        } else {
            active_login_id.clone()
        };
        let next_msa_values = next_msa_tokens.values().cloned().collect::<Vec<_>>();
        let next_minecraft_values = next_minecraft_accounts
            .values()
            .cloned()
            .collect::<Vec<_>>();
        self.persist_auth_snapshot(
            next_active_login_id.as_deref(),
            &next_msa_values,
            &next_minecraft_values,
        )?;

        *msa_tokens = next_msa_tokens;
        *minecraft_accounts = next_minecraft_accounts;
        *active_login_id = next_active_login_id;
        self.bump_active_auth_generation();
        Ok(true)
    }

    pub(crate) async fn active_auth_refresh_guard(&self) -> MutexGuard<'_, ()> {
        self.active_auth_refresh.lock().await
    }

    pub(crate) fn active_auth_generation(&self) -> u64 {
        self.active_auth_generation.load(Ordering::Acquire)
    }

    pub(crate) async fn active_minecraft_login_id(&self) -> Option<String> {
        self.active_login_id.read().await.clone()
    }

    #[cfg(test)]
    pub async fn active_msa_token(&self) -> Option<AuthLoginMsaToken> {
        let login_id = self.active_login_id.read().await.clone()?;
        self.msa_tokens.read().await.get(&login_id).cloned()
    }

    #[cfg(test)]
    pub async fn active_minecraft_account(&self) -> Option<AuthLoginMinecraftAccount> {
        let login_id = self.active_login_id.read().await.clone()?;
        self.minecraft_accounts.read().await.get(&login_id).cloned()
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

    async fn save_auth_snapshot(&self) {
        let active_login_id = self.active_login_id.read().await.clone();
        let msa_tokens = self
            .msa_tokens
            .read()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let minecraft_accounts = self
            .minecraft_accounts
            .read()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        if let Err(error) =
            self.persist_auth_snapshot(active_login_id.as_deref(), &msa_tokens, &minecraft_accounts)
        {
            tracing::warn!("auth snapshot persistence save failed: {error}");
        }
    }

    fn persist_auth_snapshot(
        &self,
        active_login_id: Option<&str>,
        msa_tokens: &[AuthLoginMsaToken],
        minecraft_accounts: &[AuthLoginMinecraftAccount],
    ) -> Result<(), AuthPersistenceError> {
        let Some(persistence) = &self.persistence else {
            return Ok(());
        };

        if msa_tokens.is_empty() {
            return persistence.delete_snapshot();
        }

        let snapshot =
            PersistedAuthSnapshot::from_state(active_login_id, msa_tokens, minecraft_accounts);
        persistence.save_snapshot(&snapshot)
    }

    fn delete_active_snapshot(&self) -> Result<(), AuthPersistenceError> {
        let Some(persistence) = &self.persistence else {
            return Ok(());
        };

        persistence.delete_snapshot()
    }

    async fn remove_login_from_memory(&self, login_id: &str) -> bool {
        let removed_msa = self.msa_tokens.write().await.remove(login_id).is_some();
        let removed_minecraft = self
            .minecraft_accounts
            .write()
            .await
            .remove(login_id)
            .is_some();
        let mut active_login_id = self.active_login_id.write().await;
        if active_login_id.as_deref() == Some(login_id) {
            *active_login_id = None;
        }
        removed_msa || removed_minecraft
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

fn load_active_snapshot(persistence: &dyn AuthSnapshotPersistence) -> PersistedAuthState {
    let snapshot = match persistence.load_snapshot() {
        Ok(Some(snapshot)) => snapshot,
        Ok(None) => return empty_persisted_auth_state(),
        Err(AuthPersistenceError::Malformed(error)) => {
            tracing::warn!(
                "stored auth snapshot is malformed; clearing secure auth entry: {error}"
            );
            clear_rejected_snapshot(persistence);
            return empty_persisted_auth_state();
        }
        Err(AuthPersistenceError::Unavailable(error)) => {
            tracing::warn!(
                "secure auth storage is unavailable; starting without restored auth: {error}"
            );
            return empty_persisted_auth_state();
        }
    };

    match snapshot.into_state(Utc::now()) {
        Ok(state) => state,
        Err(AuthSnapshotRejection::Expired) => {
            tracing::warn!("stored auth snapshot is expired; clearing secure auth entry");
            clear_rejected_snapshot(persistence);
            empty_persisted_auth_state()
        }
        Err(AuthSnapshotRejection::Malformed) => {
            tracing::warn!("stored auth snapshot is malformed; clearing secure auth entry");
            clear_rejected_snapshot(persistence);
            empty_persisted_auth_state()
        }
    }
}

fn empty_persisted_auth_state() -> PersistedAuthState {
    PersistedAuthState {
        active_login_id: None,
        msa_tokens: Vec::new(),
        minecraft_accounts: Vec::new(),
    }
}

fn clear_rejected_snapshot(persistence: &dyn AuthSnapshotPersistence) {
    if let Err(error) = persistence.delete_snapshot() {
        tracing::warn!("auth snapshot persistence cleanup failed: {error}");
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
        fail_saves: AtomicBool,
        fail_deletes: AtomicBool,
    }

    impl MockAuthSnapshotPersistence {
        fn with_snapshot(snapshot: PersistedAuthSnapshot) -> Self {
            Self {
                snapshot: Mutex::new(Some(snapshot)),
                saves: AtomicU64::new(0),
                deletes: AtomicU64::new(0),
                fail_saves: AtomicBool::new(false),
                fail_deletes: AtomicBool::new(false),
            }
        }

        fn with_failing_deletes(snapshot: PersistedAuthSnapshot) -> Self {
            Self {
                snapshot: Mutex::new(Some(snapshot)),
                saves: AtomicU64::new(0),
                deletes: AtomicU64::new(0),
                fail_saves: AtomicBool::new(false),
                fail_deletes: AtomicBool::new(true),
            }
        }

        fn fail_saves(&self) {
            self.fail_saves.store(true, Ordering::Relaxed);
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
            if self.fail_saves.load(Ordering::Relaxed) {
                return Err(AuthPersistenceError::Unavailable("save failed".to_string()));
            }

            *self.snapshot.lock().expect("snapshot lock") = Some(snapshot.clone());
            self.saves.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }

        fn delete_snapshot(&self) -> Result<(), AuthPersistenceError> {
            self.deletes.fetch_add(1, Ordering::Relaxed);
            if self.fail_deletes.load(Ordering::Relaxed) {
                return Err(AuthPersistenceError::Unavailable(
                    "delete failed".to_string(),
                ));
            }

            *self.snapshot.lock().expect("snapshot lock") = None;
            Ok(())
        }
    }

    #[tokio::test]
    async fn auth_login_store_keeps_multiple_msa_tokens_and_marks_newest_active() {
        let store = AuthLoginStore::new();
        let first = store
            .replace_with_msa_token(new_msa_token("first-access-token", None, 3600))
            .await;
        let second = store
            .replace_with_msa_token(new_msa_token(
                "second-access-token",
                Some("second-refresh-token"),
                3600,
            ))
            .await;

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
    async fn auth_login_store_switches_active_account_when_switch_persistence_fails() {
        let persistence = Arc::new(MockAuthSnapshotPersistence::default());
        let store = AuthLoginStore::with_persistence(persistence.clone());
        let first = store
            .replace_with_msa_token(new_msa_token(
                "first-access-token",
                Some("first-refresh-token"),
                3600,
            ))
            .await;
        let second = store
            .replace_with_msa_token(new_msa_token(
                "second-access-token",
                Some("second-refresh-token"),
                3600,
            ))
            .await;
        persistence.fail_saves();

        let switched = store
            .switch_active_account(&first.login_id)
            .await
            .expect("switch should not fail on secure-save failure");

        assert!(switched);
        let active = store.active_msa_token().await.expect("active token");
        assert_eq!(active.login_id, first.login_id);
        assert_eq!(active.access_token, "first-access-token");
        assert_ne!(active.login_id, second.login_id);
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
            .await;

        let had_auth = store.clear_all().await.expect("clear auth");

        assert!(had_auth);
        assert_eq!(store.active_msa_token().await, None);
        assert!(store.account_states().await.is_empty());
        assert!(!store.clear_all().await.expect("clear empty auth"));
    }

    #[tokio::test]
    async fn auth_login_store_drops_expired_active_msa_auth() {
        let store = AuthLoginStore::new();
        store
            .replace_with_msa_token(new_msa_token("msa-access-token", None, 0))
            .await;

        assert_eq!(store.active_msa_auth_remaining_seconds().await, None);
        assert_eq!(store.active_msa_token().await, None);
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
            .await;

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
            .await;

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

        let store = AuthLoginStore::with_persistence(persistence.clone());

        assert_eq!(store.active_msa_token().await, Some(valid_token));
        assert_eq!(store.active_minecraft_account().await, Some(valid_account));
        assert_eq!(store.account_states().await.len(), 1);
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
            test_persisted_snapshot_with_version(&token, None, 1),
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
            &["msa_tokens", "0", "access_token"],
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
            &["msa_tokens", "0", "refresh_token"],
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
                &["minecraft_accounts", "0", "access_token"],
                "   ",
            ),
            &["minecraft_accounts", "0", "profile", "name"],
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

        let store = AuthLoginStore::with_persistence(persistence.clone());

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
        let store = AuthLoginStore::with_persistence(persistence.clone());

        store
            .replace_with_msa_and_minecraft_account(
                new_msa_token("msa-access-token", Some("msa-refresh-token"), 3600),
                new_minecraft_account("PersistedPlayer", 3600, true),
            )
            .await;

        assert_eq!(persistence.saves(), 1);
        assert!(persistence.snapshot().is_some());
    }

    #[tokio::test]
    async fn auth_login_store_durable_completion_fails_before_mutating_when_secure_save_fails() {
        let persistence = Arc::new(MockAuthSnapshotPersistence::default());
        let store = AuthLoginStore::with_persistence(persistence.clone());
        persistence.fail_saves();

        let error = store
            .replace_with_msa_and_minecraft_account_durable(
                new_msa_token("msa-access-token", Some("msa-refresh-token"), 3600),
                new_minecraft_account("PersistedPlayer", 3600, true),
            )
            .await
            .expect_err("durable completion should fail");

        assert!(matches!(error, AuthPersistenceError::Unavailable(_)));
        assert_eq!(store.active_msa_token().await, None);
        assert_eq!(store.active_minecraft_account().await, None);
        assert!(store.account_states().await.is_empty());
        assert_eq!(persistence.saves(), 0);
        assert_eq!(persistence.snapshot(), None);
    }

    #[tokio::test]
    async fn auth_login_store_persists_rotated_refresh_snapshot() {
        let persistence = Arc::new(MockAuthSnapshotPersistence::default());
        let store = AuthLoginStore::with_persistence(persistence.clone());
        store
            .replace_with_msa_token(new_msa_token(
                "old-msa-access-token",
                Some("old-msa-refresh-token"),
                0,
            ))
            .await;

        store
            .refresh_with_msa_and_minecraft_account(
                new_msa_token("new-msa-access-token", Some("new-msa-refresh-token"), 3600),
                new_minecraft_account("RotatedRefresh", 3600, true),
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
            .await;
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
            .expect("refresh captured first login");

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
        let store = AuthLoginStore::with_persistence(persistence.clone());
        store
            .replace_with_msa_token(new_msa_token(
                "old-msa-access-token",
                Some("old-msa-refresh-token"),
                0,
            ))
            .await;

        store
            .refresh_with_msa_token(
                new_msa_token("new-msa-access-token", Some("new-msa-refresh-token"), 3600),
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
        store
            .replace_with_msa_and_minecraft_account(
                new_msa_token("old-msa-access-token", Some("old-msa-refresh-token"), 3600),
                new_minecraft_account("PreservedPlayer", 3600, true),
            )
            .await;

        store
            .refresh_with_msa_token(
                new_msa_token("new-msa-access-token", Some("new-msa-refresh-token"), 3600),
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
        store
            .replace_with_msa_token(new_msa_token("msa-access-token", None, 3600))
            .await;

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
        let store = AuthLoginStore::with_persistence(persistence.clone());

        let error = store
            .clear_all()
            .await
            .expect_err("secure delete failure should be visible");

        assert!(matches!(error, AuthPersistenceError::Unavailable(_)));
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
