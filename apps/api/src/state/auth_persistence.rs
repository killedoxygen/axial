use super::auth_logins::{AuthLoginMinecraftAccount, AuthLoginMinecraftProfile, AuthLoginMsaToken};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fmt;
use std::sync::{Arc, Mutex};

const AUTH_SNAPSHOT_SCHEMA_VERSION: u8 = 2;
// Keep each credential value comfortably under small OS keyring per-secret limits.
const AUTH_SNAPSHOT_CHUNK_BYTES: usize = 900;
const AUTH_SNAPSHOT_MAX_CHUNKS: usize = 128;
#[cfg(not(test))]
const AUTH_SNAPSHOT_SERVICE: &str = "axial-auth";
const AUTH_SNAPSHOT_HEAD_USER: &str = "minecraft-auth-v3-head";
const AUTH_SNAPSHOT_CHUNK_USER_PREFIX: &str = "minecraft-auth-v3";
const AUTH_SNAPSHOT_HEAD_SCHEMA: &str = "axial.auth.snapshot.head";
const AUTH_SNAPSHOT_HEAD_VERSION: u8 = 1;
const AUTH_SNAPSHOT_CHUNK_SLOT_COUNT: usize = 2;
const AUTH_PERSISTENCE_LOCK_INVARIANT: &str =
    "secure auth cleanup lock poisoned; persisted credential cleanup ownership may diverge";

pub(crate) trait AuthSnapshotPersistence: Send + Sync {
    fn load_snapshot(&self) -> Result<Option<PersistedAuthSnapshot>, AuthPersistenceError>;
    fn save_snapshot(&self, snapshot: &PersistedAuthSnapshot) -> Result<(), AuthPersistenceError>;
    fn delete_snapshot(&self) -> Result<(), AuthPersistenceError>;
    fn flush(&self) -> Result<(), AuthPersistenceError>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[cfg_attr(test, allow(dead_code))]
pub(crate) enum AuthPersistenceError {
    #[error("secure auth storage is unavailable")]
    Unavailable,
    #[error("secure auth snapshot is malformed")]
    Malformed,
    #[error("secure auth commit state is ambiguous")]
    Ambiguous,
    #[error("secure auth cleanup is incomplete")]
    CleanupPending,
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

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedAuthSnapshotHead {
    schema: String,
    version: u8,
    generation: u64,
    kind: PersistedAuthSnapshotHeadKind,
    active_slot: Option<usize>,
    chunks: usize,
    digest: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PersistedAuthSnapshotHeadKind {
    Live,
    Deleted,
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
    let value = serde_json::to_vec(snapshot).map_err(|_| AuthPersistenceError::Malformed)?;
    let encoded = BASE64_STANDARD.encode(value);
    let chunks = encoded
        .as_bytes()
        .chunks(AUTH_SNAPSHOT_CHUNK_BYTES)
        .map(|chunk| {
            std::str::from_utf8(chunk)
                .map(str::to_string)
                .map_err(|_| AuthPersistenceError::Malformed)
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
            return Err(AuthPersistenceError::Malformed);
        }
        encoded.push_str(chunk);
    }

    let value = BASE64_STANDARD
        .decode(encoded.as_bytes())
        .map_err(|_| AuthPersistenceError::Malformed)?;
    serde_json::from_slice(&value).map_err(|_| AuthPersistenceError::Malformed)
}

fn validate_snapshot_chunk_count(count: usize) -> Result<(), AuthPersistenceError> {
    if count == 0 || count > AUTH_SNAPSHOT_MAX_CHUNKS {
        return Err(AuthPersistenceError::Malformed);
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

trait CredentialEntryBackend: Send + Sync {
    fn get(&self, user: &str) -> Result<Option<String>, AuthPersistenceError>;
    fn set(&self, user: &str, value: &str) -> Result<(), AuthPersistenceError>;
    fn delete(&self, user: &str) -> Result<(), AuthPersistenceError>;
}

#[cfg(not(test))]
struct KeyringCredentialEntryBackend;

#[cfg(not(test))]
impl CredentialEntryBackend for KeyringCredentialEntryBackend {
    fn get(&self, user: &str) -> Result<Option<String>, AuthPersistenceError> {
        #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
        {
            let _ = user;
            Err(AuthPersistenceError::Unavailable)
        }

        #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
        {
            let entry = keyring::Entry::new(AUTH_SNAPSHOT_SERVICE, user)
                .map_err(|_| AuthPersistenceError::Unavailable)?;
            match entry.get_password() {
                Ok(value) => Ok(Some(value)),
                Err(keyring::Error::NoEntry) => Ok(None),
                Err(_) => Err(AuthPersistenceError::Unavailable),
            }
        }
    }

    fn set(&self, user: &str, value: &str) -> Result<(), AuthPersistenceError> {
        #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
        {
            let _ = (user, value);
            Err(AuthPersistenceError::Unavailable)
        }

        #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
        {
            keyring::Entry::new(AUTH_SNAPSHOT_SERVICE, user)
                .and_then(|entry| entry.set_password(value))
                .map_err(|_| AuthPersistenceError::Unavailable)
        }
    }

    fn delete(&self, user: &str) -> Result<(), AuthPersistenceError> {
        #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
        {
            let _ = user;
            Err(AuthPersistenceError::Unavailable)
        }

        #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
        {
            let entry = keyring::Entry::new(AUTH_SNAPSHOT_SERVICE, user)
                .map_err(|_| AuthPersistenceError::Unavailable)?;
            match entry.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
                Err(_) => Err(AuthPersistenceError::Unavailable),
            }
        }
    }
}

pub(crate) struct SecureAuthSnapshotPersistence {
    backend: Arc<dyn CredentialEntryBackend>,
    cleanup: Mutex<HashSet<String>>,
}

impl SecureAuthSnapshotPersistence {
    #[cfg(not(test))]
    pub(crate) fn new() -> Self {
        Self::with_backend(Arc::new(KeyringCredentialEntryBackend))
    }

    fn with_backend(backend: Arc<dyn CredentialEntryBackend>) -> Self {
        Self {
            backend,
            cleanup: Mutex::new(HashSet::new()),
        }
    }

    fn chunk_user(slot: usize, chunk: usize) -> String {
        format!("{AUTH_SNAPSHOT_CHUNK_USER_PREFIX}-slot-{slot}-chunk-{chunk}")
    }

    fn read_head(
        &self,
    ) -> Result<Option<(PersistedAuthSnapshotHead, String)>, AuthPersistenceError> {
        let Some(raw) = self.backend.get(AUTH_SNAPSHOT_HEAD_USER)? else {
            return Ok(None);
        };
        let head: PersistedAuthSnapshotHead =
            serde_json::from_str(&raw).map_err(|_| AuthPersistenceError::Malformed)?;
        validate_snapshot_head(&head)?;
        Ok(Some((head, raw)))
    }

    fn write_entry_verified(&self, user: &str, value: &str) -> Result<(), AuthPersistenceError> {
        let write = self.backend.set(user, value);
        match self.backend.get(user) {
            Ok(Some(observed)) if observed == value => Ok(()),
            _ => Err(write.err().unwrap_or(AuthPersistenceError::Unavailable)),
        }
    }

    fn write_head_verified(
        &self,
        value: &str,
        previous: Option<&str>,
    ) -> Result<(), AuthPersistenceError> {
        let write = self.backend.set(AUTH_SNAPSHOT_HEAD_USER, value);
        match self.backend.get(AUTH_SNAPSHOT_HEAD_USER) {
            Ok(Some(observed)) if observed == value => Ok(()),
            Ok(observed) if write.is_err() && observed.as_deref() == previous => {
                Err(AuthPersistenceError::Unavailable)
            }
            _ => Err(AuthPersistenceError::Ambiguous),
        }
    }

    fn schedule_cleanup<I>(&self, users: I)
    where
        I: IntoIterator<Item = String>,
    {
        self.cleanup
            .lock()
            .expect(AUTH_PERSISTENCE_LOCK_INVARIANT)
            .extend(users);
    }

    fn schedule_slot_range(&self, slot: usize, start: usize, end: usize) {
        self.schedule_cleanup(
            (start..end.min(AUTH_SNAPSHOT_MAX_CHUNKS)).map(|chunk| Self::chunk_user(slot, chunk)),
        );
    }

    fn schedule_all_chunks(&self) {
        for slot in 0..AUTH_SNAPSHOT_CHUNK_SLOT_COUNT {
            self.schedule_slot_range(slot, 0, AUTH_SNAPSHOT_MAX_CHUNKS);
        }
    }

    fn run_cleanup(&self) -> Result<(), AuthPersistenceError> {
        let users = self
            .cleanup
            .lock()
            .expect(AUTH_PERSISTENCE_LOCK_INVARIANT)
            .iter()
            .cloned()
            .collect::<Vec<_>>();
        let mut failed = false;
        for user in users {
            if self.backend.delete(&user).is_ok() {
                self.cleanup
                    .lock()
                    .expect(AUTH_PERSISTENCE_LOCK_INVARIANT)
                    .remove(&user);
            } else {
                failed = true;
            }
        }
        if failed {
            Err(AuthPersistenceError::CleanupPending)
        } else {
            Ok(())
        }
    }
}

impl AuthSnapshotPersistence for SecureAuthSnapshotPersistence {
    fn load_snapshot(&self) -> Result<Option<PersistedAuthSnapshot>, AuthPersistenceError> {
        let Some((head, _)) = self.read_head()? else {
            return Ok(None);
        };
        if head.kind == PersistedAuthSnapshotHeadKind::Deleted {
            self.schedule_all_chunks();
            let _ = self.run_cleanup();
            return Ok(None);
        }

        let slot = head.active_slot.ok_or(AuthPersistenceError::Malformed)?;
        let mut chunks = Vec::with_capacity(head.chunks);
        for chunk in 0..head.chunks {
            let value = self
                .backend
                .get(&Self::chunk_user(slot, chunk))?
                .ok_or(AuthPersistenceError::Malformed)?;
            chunks.push(value);
        }
        let digest = snapshot_chunks_digest(&chunks);
        if head.digest.as_deref() != Some(digest.as_str()) {
            return Err(AuthPersistenceError::Malformed);
        }
        let snapshot = decode_snapshot_chunks(&chunks)?;
        for inactive_slot in 0..AUTH_SNAPSHOT_CHUNK_SLOT_COUNT {
            if inactive_slot != slot {
                self.schedule_slot_range(inactive_slot, 0, AUTH_SNAPSHOT_MAX_CHUNKS);
            }
        }
        self.schedule_slot_range(slot, head.chunks, AUTH_SNAPSHOT_MAX_CHUNKS);
        let _ = self.run_cleanup();
        Ok(Some(snapshot))
    }

    fn save_snapshot(&self, snapshot: &PersistedAuthSnapshot) -> Result<(), AuthPersistenceError> {
        self.flush()?;
        let previous = self.read_head()?;
        let (generation, next_slot) = match previous.as_ref().map(|(head, _)| head) {
            Some(head) => (
                head.generation
                    .checked_add(1)
                    .ok_or(AuthPersistenceError::Malformed)?,
                head.active_slot
                    .map(|slot| (slot + 1) % AUTH_SNAPSHOT_CHUNK_SLOT_COUNT)
                    .unwrap_or((head.generation as usize) % AUTH_SNAPSHOT_CHUNK_SLOT_COUNT),
            ),
            None => (1, 0),
        };
        let chunks = encode_snapshot_chunks(snapshot)?;
        for (chunk, value) in chunks.iter().enumerate() {
            if let Err(error) =
                self.write_entry_verified(&Self::chunk_user(next_slot, chunk), value)
            {
                self.schedule_slot_range(next_slot, 0, AUTH_SNAPSHOT_MAX_CHUNKS);
                let _ = self.run_cleanup();
                return Err(error);
            }
        }

        let head = PersistedAuthSnapshotHead {
            schema: AUTH_SNAPSHOT_HEAD_SCHEMA.to_string(),
            version: AUTH_SNAPSHOT_HEAD_VERSION,
            generation,
            kind: PersistedAuthSnapshotHeadKind::Live,
            active_slot: Some(next_slot),
            chunks: chunks.len(),
            digest: Some(snapshot_chunks_digest(&chunks)),
        };
        let raw = serde_json::to_string(&head).map_err(|_| AuthPersistenceError::Malformed)?;
        self.write_head_verified(&raw, previous.as_ref().map(|(_, raw)| raw.as_str()))?;

        self.schedule_slot_range(next_slot, chunks.len(), AUTH_SNAPSHOT_MAX_CHUNKS);
        if let Some((previous, _)) = previous
            && let Some(previous_slot) = previous.active_slot
            && previous_slot != next_slot
        {
            self.schedule_slot_range(previous_slot, 0, previous.chunks);
        }
        let _ = self.run_cleanup();
        Ok(())
    }

    fn delete_snapshot(&self) -> Result<(), AuthPersistenceError> {
        let previous = self.read_head()?;
        if previous
            .as_ref()
            .is_some_and(|(head, _)| head.kind == PersistedAuthSnapshotHeadKind::Deleted)
        {
            self.schedule_all_chunks();
            let _ = self.run_cleanup();
            return Ok(());
        }
        let generation = previous
            .as_ref()
            .map(|(head, _)| head.generation)
            .unwrap_or(0)
            .checked_add(1)
            .ok_or(AuthPersistenceError::Malformed)?;
        let tombstone = PersistedAuthSnapshotHead {
            schema: AUTH_SNAPSHOT_HEAD_SCHEMA.to_string(),
            version: AUTH_SNAPSHOT_HEAD_VERSION,
            generation,
            kind: PersistedAuthSnapshotHeadKind::Deleted,
            active_slot: None,
            chunks: 0,
            digest: None,
        };
        let raw = serde_json::to_string(&tombstone).map_err(|_| AuthPersistenceError::Malformed)?;
        self.write_head_verified(&raw, previous.as_ref().map(|(_, raw)| raw.as_str()))?;
        self.schedule_all_chunks();
        let _ = self.run_cleanup();
        Ok(())
    }

    fn flush(&self) -> Result<(), AuthPersistenceError> {
        self.run_cleanup()
    }
}

fn validate_snapshot_head(head: &PersistedAuthSnapshotHead) -> Result<(), AuthPersistenceError> {
    if head.schema != AUTH_SNAPSHOT_HEAD_SCHEMA
        || head.version != AUTH_SNAPSHOT_HEAD_VERSION
        || head.generation == 0
    {
        return Err(AuthPersistenceError::Malformed);
    }
    match head.kind {
        PersistedAuthSnapshotHeadKind::Live => {
            let slot = head.active_slot.ok_or(AuthPersistenceError::Malformed)?;
            if slot >= AUTH_SNAPSHOT_CHUNK_SLOT_COUNT {
                return Err(AuthPersistenceError::Malformed);
            }
            validate_snapshot_chunk_count(head.chunks)?;
            let digest = head
                .digest
                .as_deref()
                .ok_or(AuthPersistenceError::Malformed)?;
            if digest.len() != 64
                || !digest
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            {
                return Err(AuthPersistenceError::Malformed);
            }
        }
        PersistedAuthSnapshotHeadKind::Deleted => {
            if head.active_slot.is_some() || head.chunks != 0 || head.digest.is_some() {
                return Err(AuthPersistenceError::Malformed);
            }
        }
    }
    Ok(())
}

fn snapshot_chunks_digest(chunks: &[String]) -> String {
    let mut digest = Sha256::new();
    for chunk in chunks {
        digest.update(chunk.as_bytes());
    }
    digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
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
    use std::collections::HashMap;

    #[derive(Clone)]
    enum SetFault {
        Before,
        After,
        ReplaceThenFail(String),
    }

    #[derive(Default)]
    struct MockCredentialBackend {
        state: Mutex<MockCredentialBackendState>,
    }

    #[derive(Default)]
    struct MockCredentialBackendState {
        entries: HashMap<String, String>,
        set_faults: HashMap<String, SetFault>,
        fail_deletes: bool,
    }

    impl MockCredentialBackend {
        fn entry(&self, user: &str) -> Option<String> {
            self.state
                .lock()
                .expect("backend state")
                .entries
                .get(user)
                .cloned()
        }

        fn put(&self, user: &str, value: impl Into<String>) {
            self.state
                .lock()
                .expect("backend state")
                .entries
                .insert(user.to_string(), value.into());
        }

        fn fail_next_set(&self, user: &str, fault: SetFault) {
            self.state
                .lock()
                .expect("backend state")
                .set_faults
                .insert(user.to_string(), fault);
        }

        fn set_delete_failure(&self, fail: bool) {
            self.state.lock().expect("backend state").fail_deletes = fail;
        }
    }

    impl CredentialEntryBackend for MockCredentialBackend {
        fn get(&self, user: &str) -> Result<Option<String>, AuthPersistenceError> {
            Ok(self.entry(user))
        }

        fn set(&self, user: &str, value: &str) -> Result<(), AuthPersistenceError> {
            let mut state = self.state.lock().expect("backend state");
            match state.set_faults.remove(user) {
                Some(SetFault::Before) => Err(AuthPersistenceError::Unavailable),
                Some(SetFault::After) => {
                    state.entries.insert(user.to_string(), value.to_string());
                    Err(AuthPersistenceError::Unavailable)
                }
                Some(SetFault::ReplaceThenFail(replacement)) => {
                    state.entries.insert(user.to_string(), replacement);
                    Err(AuthPersistenceError::Unavailable)
                }
                None => {
                    state.entries.insert(user.to_string(), value.to_string());
                    Ok(())
                }
            }
        }

        fn delete(&self, user: &str) -> Result<(), AuthPersistenceError> {
            let mut state = self.state.lock().expect("backend state");
            if state.fail_deletes {
                return Err(AuthPersistenceError::Unavailable);
            }
            state.entries.remove(user);
            Ok(())
        }
    }

    fn secure_store(backend: Arc<MockCredentialBackend>) -> SecureAuthSnapshotPersistence {
        SecureAuthSnapshotPersistence::with_backend(backend)
    }

    fn snapshot(access_token: &str) -> PersistedAuthSnapshot {
        let now = DateTime::from_timestamp_millis(Utc::now().timestamp_millis())
            .expect("valid current timestamp");
        let token = AuthLoginMsaToken {
            login_id: "login-test".to_string(),
            access_token: access_token.to_string(),
            refresh_token: Some("refresh-token".to_string()),
            id_token: None,
            token_type: "Bearer".to_string(),
            expires_in: 3600,
            scope: Some("XboxLive.signin offline_access".to_string()),
            authenticated_at: now,
            expires_at: now + chrono::Duration::seconds(3600),
        };
        PersistedAuthSnapshot::from_state(Some("login-test"), &[token], &[])
    }

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

        assert_eq!(error, AuthPersistenceError::Malformed);
    }

    #[test]
    fn secure_auth_commit_switches_slots_and_loads_exact_snapshot() {
        let backend = Arc::new(MockCredentialBackend::default());
        let store = secure_store(backend.clone());
        let first = snapshot("first-access-token");
        let second = snapshot("second-access-token");

        store.save_snapshot(&first).expect("save first snapshot");
        let first_head: PersistedAuthSnapshotHead =
            serde_json::from_str(&backend.entry(AUTH_SNAPSHOT_HEAD_USER).expect("first head"))
                .expect("decode first head");
        store.save_snapshot(&second).expect("save second snapshot");
        let second_head: PersistedAuthSnapshotHead =
            serde_json::from_str(&backend.entry(AUTH_SNAPSHOT_HEAD_USER).expect("second head"))
                .expect("decode second head");

        assert_eq!(first_head.generation, 1);
        assert_eq!(first_head.active_slot, Some(0));
        assert_eq!(second_head.generation, 2);
        assert_eq!(second_head.active_slot, Some(1));
        assert_eq!(store.load_snapshot().expect("load snapshot"), Some(second));
    }

    #[test]
    fn secure_auth_restart_reconstructs_live_snapshot_cleanup() {
        let backend = Arc::new(MockCredentialBackend::default());
        let writer = secure_store(backend.clone());
        let committed = snapshot("committed-access-token");
        writer
            .save_snapshot(&committed)
            .expect("save committed snapshot");
        let head: PersistedAuthSnapshotHead =
            serde_json::from_str(&backend.entry(AUTH_SNAPSHOT_HEAD_USER).expect("head"))
                .expect("decode head");
        let active_slot = head.active_slot.expect("live slot");
        let inactive_slot = (active_slot + 1) % AUTH_SNAPSHOT_CHUNK_SLOT_COUNT;
        let active_tail = SecureAuthSnapshotPersistence::chunk_user(active_slot, head.chunks);
        let inactive_chunk = SecureAuthSnapshotPersistence::chunk_user(inactive_slot, 0);
        backend.put(&active_tail, "stale-active-tail-secret");
        backend.put(&inactive_chunk, "stale-inactive-secret");
        backend.set_delete_failure(true);

        let restarted = secure_store(backend.clone());
        assert_eq!(
            restarted.load_snapshot().expect("load live snapshot"),
            Some(committed.clone())
        );
        for chunk in 0..head.chunks {
            assert!(
                backend
                    .entry(&SecureAuthSnapshotPersistence::chunk_user(
                        active_slot,
                        chunk
                    ))
                    .is_some(),
                "active chunk {chunk} must remain readable"
            );
        }
        assert_eq!(restarted.flush(), Err(AuthPersistenceError::CleanupPending));

        backend.set_delete_failure(false);
        restarted.flush().expect("retry reconstructed cleanup");
        assert!(backend.entry(&active_tail).is_none());
        assert!(backend.entry(&inactive_chunk).is_none());
        assert_eq!(
            restarted.load_snapshot().expect("reload live snapshot"),
            Some(committed)
        );
    }

    #[test]
    fn poisoned_secure_auth_cleanup_lock_panics_before_deleting_credentials() {
        let backend = Arc::new(MockCredentialBackend::default());
        let store = secure_store(backend.clone());
        let stale_user = SecureAuthSnapshotPersistence::chunk_user(1, 0);
        backend.put(&stale_user, "stale-secret");
        store.schedule_cleanup([stale_user.clone()]);
        let poison = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _cleanup = store.cleanup.lock().expect("cleanup lock");
            panic!("inject secure auth cleanup lock poison");
        }));
        assert!(poison.is_err());

        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| store.flush()))
            .expect_err("poisoned cleanup must panic");
        let message = panic
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| panic.downcast_ref::<&str>().copied())
            .expect("string panic payload");
        assert!(message.contains(AUTH_PERSISTENCE_LOCK_INVARIANT));
        assert_eq!(backend.entry(&stale_user).as_deref(), Some("stale-secret"));
    }

    #[test]
    fn secure_auth_accepts_head_write_error_when_exact_commit_reads_back() {
        let backend = Arc::new(MockCredentialBackend::default());
        let store = secure_store(backend.clone());
        let candidate = snapshot("committed-after-error");
        backend.fail_next_set(AUTH_SNAPSHOT_HEAD_USER, SetFault::After);

        store
            .save_snapshot(&candidate)
            .expect("exact readback proves commit");

        assert_eq!(
            store.load_snapshot().expect("load committed"),
            Some(candidate)
        );
    }

    #[test]
    fn secure_auth_rejects_uncommitted_head_write_and_preserves_previous_snapshot() {
        let backend = Arc::new(MockCredentialBackend::default());
        let store = secure_store(backend.clone());
        let previous = snapshot("previous-access-token");
        store
            .save_snapshot(&previous)
            .expect("save previous snapshot");
        backend.fail_next_set(AUTH_SNAPSHOT_HEAD_USER, SetFault::Before);

        assert_eq!(
            store.save_snapshot(&snapshot("uncommitted-access-token")),
            Err(AuthPersistenceError::Unavailable)
        );
        assert_eq!(
            store.load_snapshot().expect("load previous"),
            Some(previous)
        );
    }

    #[test]
    fn secure_auth_latches_unexpected_head_after_uncertain_write() {
        let backend = Arc::new(MockCredentialBackend::default());
        let store = secure_store(backend.clone());
        backend.fail_next_set(
            AUTH_SNAPSHOT_HEAD_USER,
            SetFault::ReplaceThenFail("foreign-head".to_string()),
        );

        assert_eq!(
            store.save_snapshot(&snapshot("candidate-access-token")),
            Err(AuthPersistenceError::Ambiguous)
        );
        assert_eq!(
            backend.entry(AUTH_SNAPSHOT_HEAD_USER).as_deref(),
            Some("foreign-head")
        );
    }

    #[test]
    fn secure_auth_accepts_chunk_write_error_when_exact_chunk_reads_back() {
        let backend = Arc::new(MockCredentialBackend::default());
        let store = secure_store(backend.clone());
        backend.fail_next_set(
            &SecureAuthSnapshotPersistence::chunk_user(0, 0),
            SetFault::After,
        );

        store
            .save_snapshot(&snapshot("chunk-readback-access-token"))
            .expect("exact chunk readback permits head commit");
    }

    #[test]
    fn secure_auth_cleans_inactive_slot_after_partial_staging_failure() {
        let backend = Arc::new(MockCredentialBackend::default());
        let store = secure_store(backend.clone());
        let candidate = snapshot(&"large-access-token".repeat(AUTH_SNAPSHOT_CHUNK_BYTES));
        assert!(
            encode_snapshot_chunks(&candidate)
                .expect("encode multi-chunk candidate")
                .len()
                > 1
        );
        backend.fail_next_set(
            &SecureAuthSnapshotPersistence::chunk_user(0, 1),
            SetFault::Before,
        );
        backend.set_delete_failure(true);

        assert_eq!(
            store.save_snapshot(&candidate),
            Err(AuthPersistenceError::Unavailable)
        );
        assert!(backend.entry(AUTH_SNAPSHOT_HEAD_USER).is_none());
        assert_eq!(
            store.load_snapshot().expect("missing head stays empty"),
            None
        );
        assert!(
            backend
                .entry(&SecureAuthSnapshotPersistence::chunk_user(0, 0))
                .is_some()
        );
        assert_eq!(store.flush(), Err(AuthPersistenceError::CleanupPending));

        backend.set_delete_failure(false);
        store.flush().expect("retry inactive-slot cleanup");
        for chunk in 0..AUTH_SNAPSHOT_MAX_CHUNKS {
            assert!(
                backend
                    .entry(&SecureAuthSnapshotPersistence::chunk_user(0, chunk))
                    .is_none()
            );
        }
    }

    #[test]
    fn secure_auth_rejects_digest_mismatch_without_falling_back() {
        let backend = Arc::new(MockCredentialBackend::default());
        let store = secure_store(backend.clone());
        let first = snapshot("first-access-token");
        store.save_snapshot(&first).expect("save first");
        let first_head: PersistedAuthSnapshotHead =
            serde_json::from_str(&backend.entry(AUTH_SNAPSHOT_HEAD_USER).expect("first head"))
                .expect("decode first head");
        let first_chunk = backend
            .entry(&SecureAuthSnapshotPersistence::chunk_user(0, 0))
            .expect("first chunk");

        store
            .save_snapshot(&snapshot("second-access-token"))
            .expect("save second");
        backend.put(
            &SecureAuthSnapshotPersistence::chunk_user(0, 0),
            first_chunk,
        );
        assert_eq!(first_head.active_slot, Some(0));
        backend.put(
            &SecureAuthSnapshotPersistence::chunk_user(1, 0),
            "tampered-active-chunk",
        );

        assert_eq!(store.load_snapshot(), Err(AuthPersistenceError::Malformed));
    }

    #[test]
    fn secure_auth_refuses_to_rewrite_malformed_head() {
        let backend = Arc::new(MockCredentialBackend::default());
        backend.put(AUTH_SNAPSHOT_HEAD_USER, r#"{"schema":"unknown"}"#);
        let store = secure_store(backend.clone());

        assert_eq!(
            store.save_snapshot(&snapshot("must-not-write")),
            Err(AuthPersistenceError::Malformed)
        );
        assert_eq!(
            backend.entry(AUTH_SNAPSHOT_HEAD_USER).as_deref(),
            Some(r#"{"schema":"unknown"}"#)
        );
    }

    #[test]
    fn secure_auth_tombstone_is_authoritative_while_cleanup_retries() {
        let backend = Arc::new(MockCredentialBackend::default());
        let store = secure_store(backend.clone());
        store
            .save_snapshot(&snapshot("deleted-access-token"))
            .expect("save snapshot");
        backend.set_delete_failure(true);

        store
            .delete_snapshot()
            .expect("tombstone commit is the delete boundary");

        let head: PersistedAuthSnapshotHead = serde_json::from_str(
            &backend
                .entry(AUTH_SNAPSHOT_HEAD_USER)
                .expect("tombstone head"),
        )
        .expect("decode tombstone");
        assert_eq!(head.kind, PersistedAuthSnapshotHeadKind::Deleted);
        assert_eq!(store.load_snapshot().expect("tombstone loads empty"), None);
        assert_eq!(store.flush(), Err(AuthPersistenceError::CleanupPending));

        backend.set_delete_failure(false);
        store.flush().expect("retry stale credential cleanup");
        for slot in 0..AUTH_SNAPSHOT_CHUNK_SLOT_COUNT {
            for chunk in 0..AUTH_SNAPSHOT_MAX_CHUNKS {
                assert!(
                    backend
                        .entry(&SecureAuthSnapshotPersistence::chunk_user(slot, chunk))
                        .is_none()
                );
            }
        }
        assert!(backend.entry(AUTH_SNAPSHOT_HEAD_USER).is_some());
    }

    #[test]
    fn secure_auth_rejects_live_head_with_missing_chunk() {
        let backend = Arc::new(MockCredentialBackend::default());
        let store = secure_store(backend.clone());
        let chunks = encode_snapshot_chunks(&snapshot("missing-chunk")).expect("encode snapshot");
        let head = PersistedAuthSnapshotHead {
            schema: AUTH_SNAPSHOT_HEAD_SCHEMA.to_string(),
            version: AUTH_SNAPSHOT_HEAD_VERSION,
            generation: 1,
            kind: PersistedAuthSnapshotHeadKind::Live,
            active_slot: Some(0),
            chunks: chunks.len(),
            digest: Some(snapshot_chunks_digest(&chunks)),
        };
        backend.put(
            AUTH_SNAPSHOT_HEAD_USER,
            serde_json::to_string(&head).expect("encode head"),
        );

        assert_eq!(store.load_snapshot(), Err(AuthPersistenceError::Malformed));
    }
}
