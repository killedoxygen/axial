use super::auth_logins::{AuthLoginMinecraftAccount, AuthLoginMinecraftProfile, AuthLoginMsaToken};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fmt;

const AUTH_SNAPSHOT_SCHEMA_VERSION: u8 = 2;
// Keep each credential value comfortably under small OS keyring per-secret limits.
const AUTH_SNAPSHOT_CHUNK_BYTES: usize = 900;
const AUTH_SNAPSHOT_MAX_CHUNKS: usize = 128;
#[cfg(not(test))]
const AUTH_SNAPSHOT_SERVICE: &str = "croopor-auth";
#[cfg(not(test))]
const AUTH_SNAPSHOT_PREVIOUS_USER: &str = "minecraft-auth";
#[cfg(not(test))]
const AUTH_SNAPSHOT_CHUNK_INDEX_USER: &str = "minecraft-auth-v2-index";
#[cfg(not(test))]
const AUTH_SNAPSHOT_CHUNK_USER_PREFIX: &str = "minecraft-auth-v2";
#[cfg(not(test))]
const AUTH_SNAPSHOT_CHUNK_INDEX_SCHEMA: &str = "croopor.auth.snapshot.chunks";
#[cfg(not(test))]
const AUTH_SNAPSHOT_CHUNK_INDEX_VERSION: u8 = 1;
#[cfg(not(test))]
const AUTH_SNAPSHOT_CHUNK_SLOT_COUNT: usize = 2;

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
    active_login_id: Option<String>,
    msa_tokens: Vec<PersistedMsaToken>,
    minecraft_accounts: Vec<PersistedMinecraftAccount>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PersistedAuthState {
    pub active_login_id: Option<String>,
    pub msa_tokens: Vec<AuthLoginMsaToken>,
    pub minecraft_accounts: Vec<AuthLoginMinecraftAccount>,
}

#[cfg(not(test))]
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedAuthSnapshotChunkIndex {
    schema: String,
    version: u8,
    active_slot: usize,
    chunks: usize,
}

impl PersistedAuthSnapshot {
    pub(crate) fn from_state(
        active_login_id: Option<&str>,
        msa_tokens: &[AuthLoginMsaToken],
        minecraft_accounts: &[AuthLoginMinecraftAccount],
    ) -> Self {
        Self {
            version: AUTH_SNAPSHOT_SCHEMA_VERSION,
            active_login_id: active_login_id.map(ToOwned::to_owned),
            msa_tokens: msa_tokens.iter().map(PersistedMsaToken::from).collect(),
            minecraft_accounts: minecraft_accounts
                .iter()
                .map(PersistedMinecraftAccount::from)
                .collect(),
        }
    }

    pub(crate) fn into_state(
        self,
        now: DateTime<Utc>,
    ) -> Result<PersistedAuthState, AuthSnapshotRejection> {
        if self.version != AUTH_SNAPSHOT_SCHEMA_VERSION {
            return Err(AuthSnapshotRejection::Malformed);
        }

        let mut login_ids = HashSet::new();
        let mut msa_tokens = Vec::with_capacity(self.msa_tokens.len());
        for token in self.msa_tokens {
            let token = token.into_active()?;
            if !login_ids.insert(token.login_id.clone()) {
                return Err(AuthSnapshotRejection::Malformed);
            }
            if token.expires_at <= now && !has_nonblank_refresh_token(&token) {
                continue;
            }
            msa_tokens.push(token);
        }

        if msa_tokens.is_empty() {
            return Err(AuthSnapshotRejection::Expired);
        }
        let valid_login_ids: HashSet<String> = msa_tokens
            .iter()
            .map(|token| token.login_id.clone())
            .collect();

        let mut minecraft_login_ids = HashSet::new();
        let mut minecraft_accounts = Vec::with_capacity(self.minecraft_accounts.len());
        for account in self.minecraft_accounts {
            let account = account.into_active()?;
            if !valid_login_ids.contains(&account.login_id) || account.expires_at <= now {
                continue;
            }
            if !minecraft_login_ids.insert(account.login_id.clone()) {
                return Err(AuthSnapshotRejection::Malformed);
            }
            minecraft_accounts.push(account);
        }

        let active_login_id = self
            .active_login_id
            .map(|login_id| login_id.trim().to_string())
            .filter(|login_id| !login_id.is_empty());
        if active_login_id
            .as_ref()
            .is_some_and(|login_id| !valid_login_ids.contains(login_id))
        {
            return Err(AuthSnapshotRejection::Malformed);
        }

        Ok(PersistedAuthState {
            active_login_id,
            msa_tokens,
            minecraft_accounts,
        })
    }
}

impl fmt::Debug for PersistedAuthSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PersistedAuthSnapshot")
            .field("version", &self.version)
            .field("active_login_id", &self.active_login_id)
            .field("msa_tokens", &self.msa_tokens)
            .field("minecraft_accounts", &self.minecraft_accounts)
            .finish()
    }
}

fn encode_snapshot_chunks(
    snapshot: &PersistedAuthSnapshot,
) -> Result<Vec<String>, AuthPersistenceError> {
    let value = serde_json::to_vec(snapshot)
        .map_err(|error| AuthPersistenceError::Malformed(error.to_string()))?;
    let encoded = BASE64_STANDARD.encode(value);
    let chunks = encoded
        .as_bytes()
        .chunks(AUTH_SNAPSHOT_CHUNK_BYTES)
        .map(|chunk| {
            std::str::from_utf8(chunk)
                .map(str::to_string)
                .map_err(|error| AuthPersistenceError::Malformed(error.to_string()))
        })
        .collect::<Result<Vec<_>, _>>()?;

    validate_snapshot_chunk_count(chunks.len())?;
    Ok(chunks)
}

fn decode_snapshot_chunks(
    chunks: &[String],
) -> Result<PersistedAuthSnapshot, AuthPersistenceError> {
    validate_snapshot_chunk_count(chunks.len())?;

    let encoded_len = chunks.iter().map(String::len).sum();
    let mut encoded = String::with_capacity(encoded_len);
    for chunk in chunks {
        if chunk.is_empty() {
            return Err(AuthPersistenceError::Malformed(
                "secure auth snapshot chunk is empty".to_string(),
            ));
        }
        encoded.push_str(chunk);
    }

    let value = BASE64_STANDARD
        .decode(encoded.as_bytes())
        .map_err(|error| AuthPersistenceError::Malformed(error.to_string()))?;
    serde_json::from_slice(&value)
        .map_err(|error| AuthPersistenceError::Malformed(error.to_string()))
}

fn validate_snapshot_chunk_count(count: usize) -> Result<(), AuthPersistenceError> {
    if count == 0 || count > AUTH_SNAPSHOT_MAX_CHUNKS {
        return Err(AuthPersistenceError::Malformed(
            "secure auth snapshot chunk count is invalid".to_string(),
        ));
    }
    Ok(())
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
        if self
            .refresh_token
            .as_ref()
            .is_some_and(|refresh_token| is_blank(refresh_token))
        {
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
    fn entry(&self, user: &str) -> Result<keyring::Entry, AuthPersistenceError> {
        keyring::Entry::new(AUTH_SNAPSHOT_SERVICE, user)
            .map_err(|error| AuthPersistenceError::Unavailable(error.to_string()))
    }

    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
    fn chunk_user(slot: usize, chunk: usize) -> String {
        format!("{AUTH_SNAPSHOT_CHUNK_USER_PREFIX}-slot-{slot}-chunk-{chunk}")
    }

    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
    fn read_chunk_index(
        &self,
    ) -> Result<Option<PersistedAuthSnapshotChunkIndex>, AuthPersistenceError> {
        let entry = self.entry(AUTH_SNAPSHOT_CHUNK_INDEX_USER)?;
        let password = match entry.get_password() {
            Ok(password) => password,
            Err(keyring::Error::NoEntry) => return Ok(None),
            Err(error) => return Err(AuthPersistenceError::Unavailable(error.to_string())),
        };

        serde_json::from_str(&password)
            .map(Some)
            .map_err(|error| AuthPersistenceError::Malformed(error.to_string()))
    }

    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
    fn load_chunked_snapshot(&self) -> Result<Option<PersistedAuthSnapshot>, AuthPersistenceError> {
        let Some(index) = self.read_chunk_index()? else {
            return Ok(None);
        };
        validate_chunk_index(&index)?;

        let mut chunks = Vec::with_capacity(index.chunks);
        for chunk_index in 0..index.chunks {
            let user = Self::chunk_user(index.active_slot, chunk_index);
            let entry = self.entry(&user)?;
            let password = match entry.get_password() {
                Ok(password) => password,
                Err(keyring::Error::NoEntry) => {
                    return Err(AuthPersistenceError::Malformed(
                        "secure auth snapshot chunk is missing".to_string(),
                    ));
                }
                Err(error) => return Err(AuthPersistenceError::Unavailable(error.to_string())),
            };
            chunks.push(password);
        }

        decode_snapshot_chunks(&chunks).map(Some)
    }

    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
    fn delete_entry(&self, user: &str) -> Result<(), AuthPersistenceError> {
        let entry = self.entry(user)?;
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(AuthPersistenceError::Unavailable(error.to_string())),
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
    fn delete_entry_best_effort(&self, user: &str) {
        let _ = self.delete_entry(user);
    }

    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
    fn delete_chunk_range_best_effort(&self, slot: usize, start: usize, end: usize) {
        for chunk_index in start..end.min(AUTH_SNAPSHOT_MAX_CHUNKS) {
            let user = Self::chunk_user(slot, chunk_index);
            self.delete_entry_best_effort(&user);
        }
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
            self.load_chunked_snapshot()
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
            let previous_index = match self.read_chunk_index() {
                Ok(index) => index,
                Err(AuthPersistenceError::Malformed(_)) => None,
                Err(error) => return Err(error),
            };
            let next_slot = previous_index
                .as_ref()
                .filter(|index| index.active_slot < AUTH_SNAPSHOT_CHUNK_SLOT_COUNT)
                .map(|index| (index.active_slot + 1) % AUTH_SNAPSHOT_CHUNK_SLOT_COUNT)
                .unwrap_or(0);
            let chunks = encode_snapshot_chunks(snapshot)?;

            for (chunk_index, chunk) in chunks.iter().enumerate() {
                let user = Self::chunk_user(next_slot, chunk_index);
                self.entry(&user)?
                    .set_password(chunk)
                    .map_err(|error| AuthPersistenceError::Unavailable(error.to_string()))?;
            }

            let index = PersistedAuthSnapshotChunkIndex {
                schema: AUTH_SNAPSHOT_CHUNK_INDEX_SCHEMA.to_string(),
                version: AUTH_SNAPSHOT_CHUNK_INDEX_VERSION,
                active_slot: next_slot,
                chunks: chunks.len(),
            };
            let value = serde_json::to_string(&index)
                .map_err(|error| AuthPersistenceError::Malformed(error.to_string()))?;
            self.entry(AUTH_SNAPSHOT_CHUNK_INDEX_USER)?
                .set_password(&value)
                .map_err(|error| AuthPersistenceError::Unavailable(error.to_string()))?;

            self.delete_entry_best_effort(AUTH_SNAPSHOT_PREVIOUS_USER);
            self.delete_chunk_range_best_effort(next_slot, chunks.len(), AUTH_SNAPSHOT_MAX_CHUNKS);
            if let Some(index) = previous_index
                && index.active_slot < AUTH_SNAPSHOT_CHUNK_SLOT_COUNT
                && index.active_slot != next_slot
            {
                self.delete_chunk_range_best_effort(index.active_slot, 0, index.chunks);
            }

            Ok(())
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
            let mut first_error = None;
            self.delete_entry_best_effort(AUTH_SNAPSHOT_PREVIOUS_USER);
            if let Err(error) = self.delete_entry(AUTH_SNAPSHOT_CHUNK_INDEX_USER) {
                first_error.get_or_insert_with(|| error.to_string());
            }
            for slot in 0..AUTH_SNAPSHOT_CHUNK_SLOT_COUNT {
                for chunk_index in 0..AUTH_SNAPSHOT_MAX_CHUNKS {
                    let user = Self::chunk_user(slot, chunk_index);
                    if let Err(error) = self.delete_entry(&user) {
                        first_error.get_or_insert_with(|| error.to_string());
                    }
                }
            }

            if let Some(error) = first_error {
                Err(AuthPersistenceError::Unavailable(error))
            } else {
                Ok(())
            }
        }
    }
}

#[cfg(not(test))]
fn validate_chunk_index(
    index: &PersistedAuthSnapshotChunkIndex,
) -> Result<(), AuthPersistenceError> {
    if index.schema != AUTH_SNAPSHOT_CHUNK_INDEX_SCHEMA
        || index.version != AUTH_SNAPSHOT_CHUNK_INDEX_VERSION
        || index.active_slot >= AUTH_SNAPSHOT_CHUNK_SLOT_COUNT
    {
        return Err(AuthPersistenceError::Malformed(
            "secure auth snapshot chunk index is invalid".to_string(),
        ));
    }
    validate_snapshot_chunk_count(index.chunks)
}

fn from_timestamp_millis(value: i64) -> Result<DateTime<Utc>, AuthSnapshotRejection> {
    DateTime::from_timestamp_millis(value).ok_or(AuthSnapshotRejection::Malformed)
}

fn is_blank(value: &str) -> bool {
    value.trim().is_empty()
}

fn has_nonblank_refresh_token(token: &AuthLoginMsaToken) -> bool {
    token
        .refresh_token
        .as_deref()
        .is_some_and(|refresh_token| !is_blank(refresh_token))
}

#[cfg(test)]
pub(crate) fn test_persisted_snapshot(
    active_msa_token: &AuthLoginMsaToken,
    active_minecraft_account: Option<&AuthLoginMinecraftAccount>,
) -> PersistedAuthSnapshot {
    let minecraft_accounts = active_minecraft_account
        .cloned()
        .into_iter()
        .collect::<Vec<_>>();
    PersistedAuthSnapshot::from_state(
        Some(&active_msa_token.login_id),
        std::slice::from_ref(active_msa_token),
        &minecraft_accounts,
    )
}

#[cfg(test)]
pub(crate) fn test_persisted_snapshot_with_version(
    active_msa_token: &AuthLoginMsaToken,
    active_minecraft_account: Option<&AuthLoginMinecraftAccount>,
    version: u8,
) -> PersistedAuthSnapshot {
    let mut snapshot = test_persisted_snapshot(active_msa_token, active_minecraft_account);
    snapshot.version = version;
    snapshot
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_snapshot_chunks_round_trip_large_snapshot() {
        let now = DateTime::from_timestamp_millis(Utc::now().timestamp_millis())
            .expect("valid current timestamp");
        let token = AuthLoginMsaToken {
            login_id: "login-large".to_string(),
            access_token: "msa-access-token".repeat(AUTH_SNAPSHOT_CHUNK_BYTES),
            refresh_token: Some("msa-refresh-token".repeat(AUTH_SNAPSHOT_CHUNK_BYTES)),
            id_token: Some("msa-id-token".repeat(AUTH_SNAPSHOT_CHUNK_BYTES)),
            token_type: "Bearer".to_string(),
            expires_in: 3600,
            scope: Some("XboxLive.signin offline_access".to_string()),
            authenticated_at: now,
            expires_at: now + chrono::Duration::seconds(3600),
        };
        let account = AuthLoginMinecraftAccount {
            login_id: "login-large".to_string(),
            access_token: "minecraft-access-token".repeat(AUTH_SNAPSHOT_CHUNK_BYTES),
            token_type: Some("Bearer".to_string()),
            expires_in: 3600,
            profile: AuthLoginMinecraftProfile {
                id: "profile-id".to_string(),
                name: "ChunkedPlayer".to_string(),
                skins: Vec::new(),
                capes: Vec::new(),
            },
            owns_minecraft_java: true,
            authenticated_at: now,
            expires_at: now + chrono::Duration::seconds(3600),
        };
        let snapshot = PersistedAuthSnapshot::from_state(Some("login-large"), &[token], &[account]);

        let chunks = encode_snapshot_chunks(&snapshot).expect("chunk large auth snapshot");

        assert!(chunks.len() > 1);
        assert!(
            chunks
                .iter()
                .all(|chunk| !chunk.is_empty() && chunk.len() <= AUTH_SNAPSHOT_CHUNK_BYTES)
        );
        assert_eq!(
            decode_snapshot_chunks(&chunks).expect("decode chunked auth snapshot"),
            snapshot
        );
    }

    #[test]
    fn auth_snapshot_chunks_reject_empty_chunk_sets() {
        let error = decode_snapshot_chunks(&[]).expect_err("empty chunks should be invalid");

        assert!(matches!(error, AuthPersistenceError::Malformed(_)));
    }
}
