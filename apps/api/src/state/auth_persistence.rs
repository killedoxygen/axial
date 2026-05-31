use super::auth_logins::{AuthLoginMinecraftAccount, AuthLoginMinecraftProfile, AuthLoginMsaToken};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;

const AUTH_SNAPSHOT_SCHEMA_VERSION: u8 = 1;
#[cfg(not(test))]
const AUTH_SNAPSHOT_SERVICE: &str = "croopor-auth";
#[cfg(not(test))]
const AUTH_SNAPSHOT_USER: &str = "minecraft-auth";

pub(crate) trait AuthSnapshotPersistence: Send + Sync {
    fn load_snapshot(&self) -> Result<Option<PersistedAuthSnapshot>, AuthPersistenceError>;
    fn save_snapshot(&self, snapshot: &PersistedAuthSnapshot) -> Result<(), AuthPersistenceError>;
    fn delete_snapshot(&self) -> Result<(), AuthPersistenceError>;
}

#[derive(Debug, thiserror::Error)]
#[cfg_attr(test, allow(dead_code))]
pub(crate) enum AuthPersistenceError {
    #[error("secure auth storage is unavailable: {0}")]
    Unavailable(String),
    #[error("secure auth snapshot is malformed: {0}")]
    Malformed(String),
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PersistedAuthSnapshot {
    version: u8,
    active_msa_token: PersistedMsaToken,
    active_minecraft_account: Option<PersistedMinecraftAccount>,
}

impl PersistedAuthSnapshot {
    pub(crate) fn from_active(
        active_msa_token: &AuthLoginMsaToken,
        active_minecraft_account: Option<&AuthLoginMinecraftAccount>,
    ) -> Self {
        Self {
            version: AUTH_SNAPSHOT_SCHEMA_VERSION,
            active_msa_token: PersistedMsaToken::from(active_msa_token),
            active_minecraft_account: active_minecraft_account.map(PersistedMinecraftAccount::from),
        }
    }

    pub(crate) fn into_active(
        self,
        now: DateTime<Utc>,
    ) -> Result<(AuthLoginMsaToken, Option<AuthLoginMinecraftAccount>), AuthSnapshotRejection> {
        if self.version != AUTH_SNAPSHOT_SCHEMA_VERSION {
            return Err(AuthSnapshotRejection::Malformed);
        }

        let active_msa_token = self.active_msa_token.into_active()?;
        if active_msa_token.expires_at <= now {
            return Err(AuthSnapshotRejection::Expired);
        }

        let active_minecraft_account = self
            .active_minecraft_account
            .map(PersistedMinecraftAccount::into_active)
            .transpose()?;
        if active_minecraft_account
            .as_ref()
            .is_some_and(|account| account.login_id != active_msa_token.login_id)
        {
            return Err(AuthSnapshotRejection::Malformed);
        }
        if active_minecraft_account
            .as_ref()
            .is_some_and(|account| account.expires_at <= now)
        {
            return Err(AuthSnapshotRejection::Expired);
        }

        Ok((active_msa_token, active_minecraft_account))
    }
}

impl fmt::Debug for PersistedAuthSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PersistedAuthSnapshot")
            .field("version", &self.version)
            .field("active_msa_token", &self.active_msa_token)
            .field("active_minecraft_account", &self.active_minecraft_account)
            .finish()
    }
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedMsaToken {
    login_id: String,
    access_token: String,
    refresh_token: Option<String>,
    id_token: Option<String>,
    token_type: String,
    expires_in: u64,
    scope: Option<String>,
    authenticated_at_millis: i64,
    expires_at_millis: i64,
}

impl PersistedMsaToken {
    fn into_active(self) -> Result<AuthLoginMsaToken, AuthSnapshotRejection> {
        if is_blank(&self.login_id) || is_blank(&self.access_token) || is_blank(&self.token_type) {
            return Err(AuthSnapshotRejection::Malformed);
        }

        Ok(AuthLoginMsaToken {
            login_id: self.login_id,
            access_token: self.access_token,
            refresh_token: self.refresh_token,
            id_token: self.id_token,
            token_type: self.token_type,
            expires_in: self.expires_in,
            scope: self.scope,
            authenticated_at: from_timestamp_millis(self.authenticated_at_millis)?,
            expires_at: from_timestamp_millis(self.expires_at_millis)?,
        })
    }
}

impl From<&AuthLoginMsaToken> for PersistedMsaToken {
    fn from(value: &AuthLoginMsaToken) -> Self {
        Self {
            login_id: value.login_id.clone(),
            access_token: value.access_token.clone(),
            refresh_token: value.refresh_token.clone(),
            id_token: value.id_token.clone(),
            token_type: value.token_type.clone(),
            expires_in: value.expires_in,
            scope: value.scope.clone(),
            authenticated_at_millis: value.authenticated_at.timestamp_millis(),
            expires_at_millis: value.expires_at.timestamp_millis(),
        }
    }
}

impl fmt::Debug for PersistedMsaToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PersistedMsaToken")
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
            .field("authenticated_at_millis", &self.authenticated_at_millis)
            .field("expires_at_millis", &self.expires_at_millis)
            .finish()
    }
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedMinecraftAccount {
    login_id: String,
    access_token: String,
    token_type: Option<String>,
    expires_in: u64,
    profile: AuthLoginMinecraftProfile,
    owns_minecraft_java: bool,
    authenticated_at_millis: i64,
    expires_at_millis: i64,
}

impl PersistedMinecraftAccount {
    fn into_active(self) -> Result<AuthLoginMinecraftAccount, AuthSnapshotRejection> {
        if is_blank(&self.login_id)
            || is_blank(&self.access_token)
            || is_blank(&self.profile.id)
            || is_blank(&self.profile.name)
        {
            return Err(AuthSnapshotRejection::Malformed);
        }

        Ok(AuthLoginMinecraftAccount {
            login_id: self.login_id,
            access_token: self.access_token,
            token_type: self.token_type,
            expires_in: self.expires_in,
            profile: self.profile,
            owns_minecraft_java: self.owns_minecraft_java,
            authenticated_at: from_timestamp_millis(self.authenticated_at_millis)?,
            expires_at: from_timestamp_millis(self.expires_at_millis)?,
        })
    }
}

impl From<&AuthLoginMinecraftAccount> for PersistedMinecraftAccount {
    fn from(value: &AuthLoginMinecraftAccount) -> Self {
        Self {
            login_id: value.login_id.clone(),
            access_token: value.access_token.clone(),
            token_type: value.token_type.clone(),
            expires_in: value.expires_in,
            profile: value.profile.clone(),
            owns_minecraft_java: value.owns_minecraft_java,
            authenticated_at_millis: value.authenticated_at.timestamp_millis(),
            expires_at_millis: value.expires_at.timestamp_millis(),
        }
    }
}

impl fmt::Debug for PersistedMinecraftAccount {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PersistedMinecraftAccount")
            .field("login_id", &self.login_id)
            .field("access_token", &"[redacted]")
            .field("token_type", &self.token_type)
            .field("expires_in", &self.expires_in)
            .field("profile", &self.profile)
            .field("owns_minecraft_java", &self.owns_minecraft_java)
            .field("authenticated_at_millis", &self.authenticated_at_millis)
            .field("expires_at_millis", &self.expires_at_millis)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AuthSnapshotRejection {
    Expired,
    Malformed,
}

#[cfg(not(test))]
pub(crate) struct SecureAuthSnapshotPersistence;

#[cfg(not(test))]
impl SecureAuthSnapshotPersistence {
    pub(crate) fn new() -> Self {
        Self
    }

    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
    fn entry(&self) -> Result<keyring::Entry, AuthPersistenceError> {
        keyring::Entry::new(AUTH_SNAPSHOT_SERVICE, AUTH_SNAPSHOT_USER)
            .map_err(|error| AuthPersistenceError::Unavailable(error.to_string()))
    }
}

#[cfg(not(test))]
impl AuthSnapshotPersistence for SecureAuthSnapshotPersistence {
    fn load_snapshot(&self) -> Result<Option<PersistedAuthSnapshot>, AuthPersistenceError> {
        #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
        {
            return Err(AuthPersistenceError::Unavailable(
                "OS secure auth storage is not configured for this platform".to_string(),
            ));
        }

        #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
        {
            let entry = self.entry()?;
            let password = match entry.get_password() {
                Ok(password) => password,
                Err(keyring::Error::NoEntry) => return Ok(None),
                Err(error) => return Err(AuthPersistenceError::Unavailable(error.to_string())),
            };

            serde_json::from_str(&password)
                .map(Some)
                .map_err(|error| AuthPersistenceError::Malformed(error.to_string()))
        }
    }

    fn save_snapshot(&self, snapshot: &PersistedAuthSnapshot) -> Result<(), AuthPersistenceError> {
        #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
        {
            let _ = snapshot;
            return Err(AuthPersistenceError::Unavailable(
                "OS secure auth storage is not configured for this platform".to_string(),
            ));
        }

        #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
        {
            let value = serde_json::to_string(snapshot)
                .map_err(|error| AuthPersistenceError::Malformed(error.to_string()))?;
            self.entry()?
                .set_password(&value)
                .map_err(|error| AuthPersistenceError::Unavailable(error.to_string()))
        }
    }

    fn delete_snapshot(&self) -> Result<(), AuthPersistenceError> {
        #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
        {
            return Err(AuthPersistenceError::Unavailable(
                "OS secure auth storage is not configured for this platform".to_string(),
            ));
        }

        #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
        {
            let entry = self.entry()?;
            match entry.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
                Err(error) => Err(AuthPersistenceError::Unavailable(error.to_string())),
            }
        }
    }
}

fn from_timestamp_millis(value: i64) -> Result<DateTime<Utc>, AuthSnapshotRejection> {
    DateTime::from_timestamp_millis(value).ok_or(AuthSnapshotRejection::Malformed)
}

fn is_blank(value: &str) -> bool {
    value.trim().is_empty()
}

#[cfg(test)]
pub(crate) fn test_persisted_snapshot(
    active_msa_token: &AuthLoginMsaToken,
    active_minecraft_account: Option<&AuthLoginMinecraftAccount>,
) -> PersistedAuthSnapshot {
    PersistedAuthSnapshot::from_active(active_msa_token, active_minecraft_account)
}

#[cfg(test)]
pub(crate) fn test_persisted_snapshot_with_version(
    active_msa_token: &AuthLoginMsaToken,
    active_minecraft_account: Option<&AuthLoginMinecraftAccount>,
    version: u8,
) -> PersistedAuthSnapshot {
    let mut snapshot =
        PersistedAuthSnapshot::from_active(active_msa_token, active_minecraft_account);
    snapshot.version = version;
    snapshot
}
