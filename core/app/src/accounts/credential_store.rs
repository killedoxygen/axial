//! Revisioned keyring publication. The application owns one isolated profile.
//!
//! Begin the change before an OAuth refresh request. This records a durable
//! pending marker so a restart cannot reuse a potentially consumed refresh token.
//! Publish account metadata only after `save` or `delete` returns its receipt.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::tasks::TaskOwner;

use super::credentials::{Credentials, MAX_CREDENTIAL_BYTES};

/// Never use the legacy `axial-auth` service, including in development.
pub const KEYRING_SERVICE: &str = "com.mateoltd.axial.rewrite.credentials";
const CHUNK_BYTES: usize = 900;
const MAX_HEAD_BYTES: usize = 900;

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CredentialError {
    #[error("Secure credential storage is unavailable.")]
    Unavailable,
    #[error("The secure credential record is malformed.")]
    Malformed,
    #[error("The secure credential write could not be confirmed.")]
    Ambiguous,
    #[error("The account credentials changed before this operation completed.")]
    Stale,
    #[error("An interrupted credential change requires sign-in again.")]
    Unresolved,
    #[error("Secure credential cleanup remains pending.")]
    CleanupPending,
    #[error("The credential revision limit was reached.")]
    RevisionExhausted,
    #[error("The account identity is invalid.")]
    InvalidAccount,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CredentialState {
    Absent,
    Pending,
    Ready,
    Deleted,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CredentialStatus {
    pub revision: u64,
    pub state: CredentialState,
    pub cleanup_pending: bool,
}

/// This is an in-process fence, not a deserializable caller-authored capability.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CredentialFence {
    profile: Uuid,
    account: Uuid,
    revision: u64,
    operation: Uuid,
}

impl CredentialFence {
    pub fn revision(&self) -> u64 {
        self.revision
    }
}

/// The head was read back from the secure store before issuing this receipt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CredentialReceipt {
    account_id: String,
    revision: u64,
    cleanup_pending: bool,
}

impl CredentialReceipt {
    pub fn account_id(&self) -> &str {
        &self.account_id
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn cleanup_pending(&self) -> bool {
        self.cleanup_pending
    }
}

#[derive(Clone, Debug)]
pub struct StoredCredentials {
    revision: u64,
    credentials: Credentials,
}

impl StoredCredentials {
    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn credentials(&self) -> &Credentials {
        &self.credentials
    }
}

#[derive(Clone)]
pub struct CredentialStore {
    profile: Uuid,
    keyring: Arc<dyn SecureEntries>,
    gate: Arc<Mutex<()>>,
    tasks: TaskOwner,
}

impl std::fmt::Debug for CredentialStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CredentialStore")
            .field("profile", &self.profile)
            .finish()
    }
}

impl CredentialStore {
    /// Construction performs no keyring I/O. All instances for a profile share
    /// their process gate; composition must additionally own that profile across
    /// processes, just as it owns the profile's metadata and mutable files.
    pub fn open(profile: Uuid) -> Self {
        Self::with_task_owner(
            profile,
            TaskOwner::new(64).expect("Nonzero credential capacity"),
        )
    }

    /// Production composition supplies its authoritative shutdown owner here.
    pub fn with_task_owner(profile: Uuid, tasks: TaskOwner) -> Self {
        static GATES: OnceLock<Mutex<HashMap<Uuid, Arc<Mutex<()>>>>> = OnceLock::new();
        let mut gates = GATES
            .get_or_init(Mutex::default)
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        Self {
            profile,
            keyring: Arc::new(OsKeyring),
            gate: gates.entry(profile).or_default().clone(),
            tasks,
        }
    }

    pub fn task_owner(&self) -> &TaskOwner {
        &self.tasks
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn isolated_for_tests() -> Self {
        Self {
            profile: Uuid::new_v4(),
            keyring: Arc::new(memory::MemoryEntries::default()),
            gate: Arc::default(),
            tasks: TaskOwner::new(64).unwrap(),
        }
    }

    pub async fn status(&self, account_id: &str) -> Result<CredentialStatus, CredentialError> {
        let account = parse_account(account_id)?;
        self.run(move |store| {
            Ok(match store.head(account)? {
                None => CredentialStatus {
                    revision: 0,
                    state: CredentialState::Absent,
                    cleanup_pending: false,
                },
                Some(head) => CredentialStatus {
                    revision: head.revision,
                    state: match head.state {
                        HeadState::Live { .. } => CredentialState::Ready,
                        HeadState::Pending { .. } => CredentialState::Pending,
                        HeadState::Deleted => CredentialState::Deleted,
                    },
                    cleanup_pending: !head.retired.is_empty(),
                },
            })
        })
        .await
    }

    pub async fn load(
        &self,
        account_id: &str,
    ) -> Result<Option<StoredCredentials>, CredentialError> {
        let account = parse_account(account_id)?;
        self.run(move |store| {
            let Some(head) = store.head(account)? else {
                return Ok(None);
            };
            match head.state {
                HeadState::Live { blob } => Ok(Some(StoredCredentials {
                    revision: head.revision,
                    credentials: store.read_blob(account, &blob)?,
                })),
                HeadState::Pending { .. } => Err(CredentialError::Unresolved),
                HeadState::Deleted => Ok(None),
            }
        })
        .await
    }

    /// This durable marker must precede any provider call that can rotate tokens.
    pub async fn begin_change(
        &self,
        account_id: &str,
        expected_revision: u64,
    ) -> Result<CredentialFence, CredentialError> {
        let account = parse_account(account_id)?;
        self.run(move |store| {
            let mut current = store.head(account)?;
            if current.as_ref().map_or(0, |head| head.revision) != expected_revision {
                return Err(CredentialError::Stale);
            }
            if current
                .as_ref()
                .is_some_and(|head| matches!(head.state, HeadState::Pending { .. }))
            {
                return Err(CredentialError::Unresolved);
            }
            if let Some(head) = &mut current {
                store.clean_retired(account, head)?;
            }
            let previous = current.and_then(|head| match head.state {
                HeadState::Live { blob } => Some(blob),
                _ => None,
            });
            let revision = expected_revision
                .checked_add(1)
                .ok_or(CredentialError::RevisionExhausted)?;
            let operation = Uuid::new_v4();
            let pending = Head {
                schema: 1,
                revision,
                operation,
                state: HeadState::Pending {
                    previous,
                    candidate: None,
                },
                retired: Vec::new(),
            };
            store.write_head(account, &pending)?;
            Ok(CredentialFence {
                profile: store.profile,
                account,
                revision,
                operation,
            })
        })
        .await
    }

    /// Saving uses fresh keyring entries. No committed record is overwritten.
    /// Retrying this fence is allowed only with the identical credential bundle.
    pub async fn save(
        &self,
        fence: &CredentialFence,
        credentials: Credentials,
    ) -> Result<CredentialReceipt, CredentialError> {
        self.check_profile(fence)?;
        let fence = fence.clone();
        self.run(move |store| {
            let bytes = credentials.encode_for_keyring()?;
            let mut head = store.fenced_head(&fence)?;
            let layout = match &head.state {
                HeadState::Live { blob }
                | HeadState::Pending {
                    candidate: Some(blob),
                    ..
                } => blob.layout,
                _ if cfg!(target_os = "macos") => BlobLayout::Single,
                _ => BlobLayout::Chunks,
            };
            let blob = Blob {
                id: fence.operation,
                chunks: bytes.len().div_ceil(layout.chunk_bytes()),
                digest: hex::encode(Sha256::digest(&bytes)),
                layout,
            };
            let previous = match &head.state {
                HeadState::Live { blob: committed } if committed == &blob => {
                    // Idempotent recovery also validates the actual secret bytes.
                    if store.read_blob(fence.account, committed)? != credentials {
                        return Err(CredentialError::Malformed);
                    }
                    let pending = store.clean_retired(fence.account, &mut head).is_err();
                    return Ok(receipt(fence.account, head.revision, pending));
                }
                HeadState::Pending {
                    previous,
                    candidate,
                } => {
                    if candidate.as_ref().is_some_and(|existing| existing != &blob) {
                        return Err(CredentialError::Stale);
                    }
                    previous.clone()
                }
                _ => return Err(CredentialError::Stale),
            };
            // Record every candidate entry before writing it so interrupted
            // publication and logout can delete exactly the owned secrets.
            head.state = HeadState::Pending {
                previous: previous.clone(),
                candidate: Some(blob.clone()),
            };
            store.write_head(fence.account, &head)?;
            for (index, chunk) in bytes.chunks(layout.chunk_bytes()).enumerate() {
                store.write_entry(&store.chunk_key(fence.account, blob.id, index), chunk)?;
            }
            if store.read_blob(fence.account, &blob)? != credentials {
                return Err(CredentialError::Malformed);
            }
            head.state = HeadState::Live { blob };
            head.retired = previous.into_iter().collect();
            store.write_head(fence.account, &head)?;
            let cleanup_pending = store.clean_retired(fence.account, &mut head).is_err();
            Ok(receipt(fence.account, head.revision, cleanup_pending))
        })
        .await
    }

    /// Read the result of an uncertain save; never repeat provider refresh to
    /// discover whether local persistence completed.
    pub async fn reconcile(
        &self,
        fence: &CredentialFence,
    ) -> Result<CredentialReceipt, CredentialError> {
        self.check_profile(fence)?;
        let fence = fence.clone();
        self.run(move |store| {
            let mut head = store.fenced_head(&fence)?;
            let HeadState::Live { blob } = &head.state else {
                return Err(CredentialError::Unresolved);
            };
            store.read_blob(fence.account, blob)?;
            let pending = store.clean_retired(fence.account, &mut head).is_err();
            Ok(receipt(fence.account, head.revision, pending))
        })
        .await
    }

    /// The tombstone is acknowledged before secret cleanup. It remains in the
    /// keyring so logout/relogin cannot recreate an earlier revision.
    pub async fn delete(
        &self,
        account_id: &str,
        expected_revision: Option<u64>,
    ) -> Result<CredentialReceipt, CredentialError> {
        let account = parse_account(account_id)?;
        self.run(move |store| {
            let current = store.head(account)?;
            let revision = current.as_ref().map_or(0, |head| head.revision);
            if expected_revision.is_some_and(|expected| expected != revision) {
                return Err(CredentialError::Stale);
            }
            if let Some(mut head) = current.clone() {
                if matches!(head.state, HeadState::Deleted) {
                    let pending = store.clean_retired(account, &mut head).is_err();
                    return Ok(receipt(account, head.revision, pending));
                }
            }
            let mut retired = Vec::new();
            if let Some(head) = current {
                retired.extend(head.retired);
                match head.state {
                    HeadState::Live { blob } => retired.push(blob),
                    HeadState::Pending {
                        previous,
                        candidate,
                    } => {
                        retired.extend(previous);
                        retired.extend(candidate);
                    }
                    HeadState::Deleted => {}
                }
            }
            let mut deleted = Head {
                schema: 1,
                revision: revision
                    .checked_add(1)
                    .ok_or(CredentialError::RevisionExhausted)?,
                operation: Uuid::new_v4(),
                state: HeadState::Deleted,
                retired,
            };
            store.write_head(account, &deleted)?;
            let pending = store.clean_retired(account, &mut deleted).is_err();
            Ok(receipt(account, deleted.revision, pending))
        })
        .await
    }

    pub async fn flush_cleanup(&self, account_id: &str) -> Result<(), CredentialError> {
        let account = parse_account(account_id)?;
        self.run(move |store| {
            if let Some(mut head) = store.head(account)? {
                store.clean_retired(account, &mut head)?;
            }
            Ok(())
        })
        .await
    }

    async fn run<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&Self) -> Result<T, CredentialError> + Send + 'static,
    ) -> Result<T, CredentialError> {
        let store = self.clone();
        // Admission and shutdown observe this operation before blocking I/O is
        // spawned. Dropping the HTTP waiter cannot cancel secure publication.
        // Cancellation cannot interrupt an OS-keyring call; its owner joins it.
        self.tasks
            .try_spawn((), move |_cancellation| async move {
                match tokio::task::spawn_blocking(move || {
                    let _guard = store
                        .gate
                        .lock()
                        .map_err(|_| CredentialError::Unavailable)?;
                    operation(&store)
                })
                .await
                {
                    Ok(result) => result,
                    Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
                    Err(_) => Err(CredentialError::Unavailable),
                }
            })
            .map_err(|_| CredentialError::Unavailable)?
            .join()
            .await
            .map_err(|_| CredentialError::Unavailable)?
    }

    fn check_profile(&self, fence: &CredentialFence) -> Result<(), CredentialError> {
        if fence.profile != self.profile {
            Err(CredentialError::Stale)
        } else {
            Ok(())
        }
    }

    fn fenced_head(&self, fence: &CredentialFence) -> Result<Head, CredentialError> {
        let head = self.head(fence.account)?.ok_or(CredentialError::Stale)?;
        if head.revision != fence.revision || head.operation != fence.operation {
            return Err(CredentialError::Stale);
        }
        Ok(head)
    }

    fn head_key(&self, account: Uuid) -> String {
        format!("{}:{account}:head-v1", self.profile)
    }

    fn chunk_key(&self, account: Uuid, blob: Uuid, index: usize) -> String {
        format!("{}:{account}:{blob}:{index}", self.profile)
    }

    fn head(&self, account: Uuid) -> Result<Option<Head>, CredentialError> {
        let Some(bytes) = self.keyring.get(&self.head_key(account))? else {
            return Ok(None);
        };
        if bytes.len() > MAX_HEAD_BYTES {
            return Err(CredentialError::Malformed);
        }
        let head: Head = serde_json::from_slice(&bytes).map_err(|_| CredentialError::Malformed)?;
        head.validate()?;
        Ok(Some(head))
    }

    fn write_head(&self, account: Uuid, head: &Head) -> Result<(), CredentialError> {
        head.validate()?;
        let bytes = serde_json::to_vec(head).map_err(|_| CredentialError::Malformed)?;
        if bytes.len() > MAX_HEAD_BYTES {
            return Err(CredentialError::Malformed);
        }
        self.write_entry(&self.head_key(account), &bytes)
    }

    fn write_entry(&self, key: &str, bytes: &[u8]) -> Result<(), CredentialError> {
        // Some OS APIs may commit and still report failure. Exact readback is
        // the only acknowledgment; an error is never treated as "not written".
        let _write_result = self.keyring.set(key, bytes);
        match self.keyring.get(key) {
            Ok(Some(actual)) if actual == bytes => Ok(()),
            _ => Err(CredentialError::Ambiguous),
        }
    }

    fn read_blob(&self, account: Uuid, blob: &Blob) -> Result<Credentials, CredentialError> {
        blob.validate()?;
        let chunk_bytes = blob.layout.chunk_bytes();
        let mut bytes = Vec::with_capacity(blob.chunks * chunk_bytes);
        for index in 0..blob.chunks {
            let chunk = self
                .keyring
                .get(&self.chunk_key(account, blob.id, index))?
                .ok_or(CredentialError::Malformed)?;
            if chunk.is_empty()
                || chunk.len() > chunk_bytes
                || (index + 1 < blob.chunks && chunk.len() != chunk_bytes)
            {
                return Err(CredentialError::Malformed);
            }
            bytes.extend_from_slice(&chunk);
        }
        if hex::encode(Sha256::digest(&bytes)) != blob.digest {
            return Err(CredentialError::Malformed);
        }
        Credentials::decode_from_keyring(&bytes)
    }

    fn clean_retired(&self, account: Uuid, head: &mut Head) -> Result<(), CredentialError> {
        if head.retired.is_empty() {
            return Ok(());
        }
        for blob in &head.retired {
            for index in 0..blob.chunks {
                let key = self.chunk_key(account, blob.id, index);
                let _delete_result = self.keyring.delete(&key);
                if !matches!(self.keyring.get(&key), Ok(None)) {
                    return Err(CredentialError::CleanupPending);
                }
            }
        }
        let mut cleaned = head.clone();
        cleaned.retired.clear();
        self.write_head(account, &cleaned)
            .map_err(|_| CredentialError::CleanupPending)?;
        *head = cleaned;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Head {
    schema: u8,
    revision: u64,
    operation: Uuid,
    state: HeadState,
    retired: Vec<Blob>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum HeadState {
    Live {
        blob: Blob,
    },
    Pending {
        previous: Option<Blob>,
        candidate: Option<Blob>,
    },
    Deleted,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Blob {
    id: Uuid,
    chunks: usize,
    digest: String,
    #[serde(default)]
    layout: BlobLayout,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum BlobLayout {
    #[default]
    Chunks,
    Single,
}

impl BlobLayout {
    fn chunk_bytes(self) -> usize {
        match self {
            Self::Chunks => CHUNK_BYTES,
            Self::Single => MAX_CREDENTIAL_BYTES,
        }
    }
}

impl Head {
    fn validate(&self) -> Result<(), CredentialError> {
        if self.schema != 1
            || self.revision == 0
            || self.operation.is_nil()
            || self.retired.len() > 2
        {
            return Err(CredentialError::Malformed);
        }
        let live = match &self.state {
            HeadState::Live { blob } => {
                if blob.id != self.operation || self.retired.len() > 1 {
                    return Err(CredentialError::Malformed);
                }
                blob.validate()?;
                Some(blob.id)
            }
            HeadState::Pending {
                previous,
                candidate,
            } => {
                if !self.retired.is_empty() {
                    return Err(CredentialError::Malformed);
                }
                if let Some(blob) = previous {
                    blob.validate()?;
                }
                if let Some(blob) = candidate {
                    blob.validate()?;
                    if blob.id != self.operation
                        || previous
                            .as_ref()
                            .is_some_and(|previous| previous.id == blob.id)
                    {
                        return Err(CredentialError::Malformed);
                    }
                }
                None
            }
            HeadState::Deleted => None,
        };
        let mut ids = Vec::new();
        for blob in &self.retired {
            blob.validate()?;
            if live == Some(blob.id) || ids.contains(&blob.id) {
                return Err(CredentialError::Malformed);
            }
            ids.push(blob.id);
        }
        Ok(())
    }
}

impl Blob {
    fn validate(&self) -> Result<(), CredentialError> {
        if self.id.is_nil()
            || self.chunks == 0
            || self.chunks > MAX_CREDENTIAL_BYTES.div_ceil(self.layout.chunk_bytes())
            || self.digest.len() != 64
            || !self.digest.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(CredentialError::Malformed);
        }
        Ok(())
    }
}

fn parse_account(account_id: &str) -> Result<Uuid, CredentialError> {
    Uuid::parse_str(account_id)
        .ok()
        .filter(|id| !id.is_nil())
        .ok_or(CredentialError::InvalidAccount)
}

fn receipt(account: Uuid, revision: u64, cleanup_pending: bool) -> CredentialReceipt {
    CredentialReceipt {
        account_id: account.to_string(),
        revision,
        cleanup_pending,
    }
}

trait SecureEntries: Send + Sync {
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>, CredentialError>;
    fn set(&self, key: &str, bytes: &[u8]) -> Result<(), CredentialError>;
    fn delete(&self, key: &str) -> Result<(), CredentialError>;
}

struct OsKeyring;

impl SecureEntries for OsKeyring {
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>, CredentialError> {
        let entry =
            keyring::Entry::new(KEYRING_SERVICE, key).map_err(|_| CredentialError::Unavailable)?;
        match entry.get_secret() {
            Ok(bytes) => Ok(Some(bytes)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(_) => Err(CredentialError::Unavailable),
        }
    }

    fn set(&self, key: &str, bytes: &[u8]) -> Result<(), CredentialError> {
        keyring::Entry::new(KEYRING_SERVICE, key)
            .map_err(|_| CredentialError::Unavailable)?
            .set_secret(bytes)
            .map_err(|_| CredentialError::Unavailable)
    }

    fn delete(&self, key: &str) -> Result<(), CredentialError> {
        let entry =
            keyring::Entry::new(KEYRING_SERVICE, key).map_err(|_| CredentialError::Unavailable)?;
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(_) => Err(CredentialError::Unavailable),
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
mod memory {
    use super::*;
    use std::sync::Condvar;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    #[derive(Default)]
    pub(super) struct Faults {
        pub(super) unavailable: bool,
        pub(super) reject_chunks: bool,
        pub(super) reject_deletes: bool,
        pub(super) write_then_error: bool,
        pub(super) lose_live_ack: bool,
        pub(super) fail_next_get: bool,
    }

    #[derive(Default)]
    pub(super) struct MemoryEntries {
        pub(super) values: Mutex<HashMap<String, Vec<u8>>>,
        pub(super) faults: Mutex<Faults>,
        pub(super) block_next_write: Mutex<Option<Arc<WriteBlock>>>,
        pub(super) reads: AtomicUsize,
    }

    #[derive(Default)]
    pub(super) struct WriteBlock {
        pub(super) entered: AtomicBool,
        pub(super) proceed: Mutex<bool>,
        pub(super) released: Condvar,
    }

    impl SecureEntries for MemoryEntries {
        fn get(&self, key: &str) -> Result<Option<Vec<u8>>, CredentialError> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            let mut faults = self.faults.lock().unwrap();
            if faults.unavailable || std::mem::take(&mut faults.fail_next_get) {
                return Err(CredentialError::Unavailable);
            }
            Ok(self.values.lock().unwrap().get(key).cloned())
        }

        fn set(&self, key: &str, bytes: &[u8]) -> Result<(), CredentialError> {
            if let Some(block) = self.block_next_write.lock().unwrap().take() {
                block.entered.store(true, Ordering::SeqCst);
                let mut proceed = block.proceed.lock().unwrap();
                while !*proceed {
                    proceed = block.released.wait(proceed).unwrap();
                }
            }
            let mut faults = self.faults.lock().unwrap();
            if faults.unavailable || (faults.reject_chunks && !key.ends_with(":head-v1")) {
                return Err(CredentialError::Unavailable);
            }
            self.values.lock().unwrap().insert(key.into(), bytes.into());
            if faults.lose_live_ack
                && serde_json::from_slice::<Head>(bytes)
                    .ok()
                    .is_some_and(|head| matches!(head.state, HeadState::Live { .. }))
            {
                faults.lose_live_ack = false;
                faults.fail_next_get = true;
            }
            if faults.write_then_error {
                Err(CredentialError::Unavailable)
            } else {
                Ok(())
            }
        }

        fn delete(&self, key: &str) -> Result<(), CredentialError> {
            let faults = self.faults.lock().unwrap();
            if faults.unavailable || faults.reject_deletes {
                return Err(CredentialError::Unavailable);
            }
            self.values.lock().unwrap().remove(key);
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::memory::{MemoryEntries, WriteBlock};
    use super::*;
    use std::sync::atomic::Ordering;

    const ACCOUNT: &str = "4f7d5a11-d427-48e8-8953-d0d2f3829e9c";
    const OTHER_ACCOUNT: &str = "f92369e6-9c0a-48e7-a238-8ad1e11cc2e4";

    fn fixture() -> (CredentialStore, Arc<MemoryEntries>) {
        let keyring = Arc::new(MemoryEntries::default());
        let store = CredentialStore {
            profile: Uuid::parse_str("d0507f34-686a-411b-852e-1151a9656e67").unwrap(),
            keyring: keyring.clone(),
            gate: Arc::default(),
            tasks: TaskOwner::new(64).unwrap(),
        };
        (store, keyring)
    }

    fn restart(store: &CredentialStore) -> CredentialStore {
        CredentialStore {
            profile: store.profile,
            keyring: store.keyring.clone(),
            gate: Arc::default(),
            tasks: TaskOwner::new(64).unwrap(),
        }
    }

    fn credentials(label: &str) -> Credentials {
        Credentials::new(
            format!("msa-{label}"),
            Some(format!("refresh-{label}")),
            100,
            format!("game-{label}"),
            200,
        )
        .unwrap()
    }

    async fn save_first(store: &CredentialStore, label: &str) -> CredentialReceipt {
        let fence = store.begin_change(ACCOUNT, 0).await.unwrap();
        store.save(&fence, credentials(label)).await.unwrap()
    }

    #[tokio::test]
    async fn secure_save_load_and_delete_acknowledge_revision_across_restart() {
        let (store, keyring) = fixture();
        assert_eq!(
            store.status(ACCOUNT).await.unwrap().state,
            CredentialState::Absent
        );
        assert_eq!(store.status(ACCOUNT).await.unwrap().revision, 0);
        let saved = save_first(&store, "first").await;
        assert_eq!(saved.account_id(), ACCOUNT);
        assert_eq!(saved.revision(), 1);
        assert!(!saved.cleanup_pending());
        let restarted = restart(&store);
        let loaded = restarted.load(ACCOUNT).await.unwrap().unwrap();
        assert_eq!(loaded.revision(), 1);
        assert_eq!(loaded.credentials(), &credentials("first"));
        let deleted = restarted.delete(ACCOUNT, Some(1)).await.unwrap();
        assert_eq!(deleted.revision(), 2);
        assert!(!deleted.cleanup_pending());
        assert!(restart(&store).load(ACCOUNT).await.unwrap().is_none());
        assert_eq!(
            store.status(ACCOUNT).await.unwrap().state,
            CredentialState::Deleted
        );
        assert_eq!(store.status(ACCOUNT).await.unwrap().revision, 2);
        assert_eq!(
            keyring.values.lock().unwrap().len(),
            1,
            "Only the nonsecret tombstone remains"
        );
    }

    #[tokio::test]
    async fn refresh_pending_after_restart_never_reuses_old_refresh_token() {
        let (store, _) = fixture();
        save_first(&store, "old").await;
        let refresh = store.begin_change(ACCOUNT, 1).await.unwrap();
        assert_eq!(refresh.revision(), 2);
        let restarted = restart(&store);
        assert_eq!(
            restarted.load(ACCOUNT).await.unwrap_err(),
            CredentialError::Unresolved
        );
        assert_eq!(
            restarted.begin_change(ACCOUNT, 2).await.unwrap_err(),
            CredentialError::Unresolved
        );
        assert_eq!(
            restarted.reconcile(&refresh).await.unwrap_err(),
            CredentialError::Unresolved
        );
        restarted.delete(ACCOUNT, Some(2)).await.unwrap();
        assert_eq!(
            store.save(&refresh, credentials("late")).await.unwrap_err(),
            CredentialError::Stale
        );
        assert!(store.load(ACCOUNT).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn logout_and_relogin_fence_old_provider_completions_and_cleanup() {
        let (store, _) = fixture();
        save_first(&store, "first").await;
        let refresh = store.begin_change(ACCOUNT, 1).await.unwrap();
        let deleted = store.delete(ACCOUNT, None).await.unwrap();
        assert_eq!(deleted.revision(), 3);
        let login = store
            .begin_change(ACCOUNT, deleted.revision())
            .await
            .unwrap();
        let saved = store.save(&login, credentials("new-login")).await.unwrap();
        assert_eq!(saved.revision(), 4);
        assert_eq!(
            store
                .save(&refresh, credentials("stale-refresh"))
                .await
                .unwrap_err(),
            CredentialError::Stale
        );
        assert_eq!(
            store
                .delete(ACCOUNT, Some(refresh.revision()))
                .await
                .unwrap_err(),
            CredentialError::Stale
        );
        assert_eq!(
            store.load(ACCOUNT).await.unwrap().unwrap().credentials(),
            &credentials("new-login")
        );
    }

    #[tokio::test]
    async fn failed_write_acknowledged_only_when_exact_secret_can_be_read_back() {
        let (store, keyring) = fixture();
        keyring.faults.lock().unwrap().write_then_error = true;
        let saved = save_first(&store, "committed-despite-error").await;
        assert_eq!(saved.revision(), 1);
        assert_eq!(
            store.load(ACCOUNT).await.unwrap().unwrap().credentials(),
            &credentials("committed-despite-error")
        );
    }

    #[tokio::test]
    async fn unavailable_keyring_has_no_volatile_or_plaintext_success_path() {
        let (store, keyring) = fixture();
        keyring.faults.lock().unwrap().unavailable = true;
        assert_eq!(
            store.status(ACCOUNT).await.unwrap_err(),
            CredentialError::Unavailable
        );
        assert_eq!(
            store.begin_change(ACCOUNT, 0).await.unwrap_err(),
            CredentialError::Unavailable
        );
        assert_eq!(
            store.delete(ACCOUNT, None).await.unwrap_err(),
            CredentialError::Unavailable
        );
        assert!(keyring.values.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn ambiguous_publication_can_be_reconciled_without_repeating_provider_call() {
        let (store, keyring) = fixture();
        let fence = store.begin_change(ACCOUNT, 0).await.unwrap();
        keyring.faults.lock().unwrap().lose_live_ack = true;
        assert_eq!(
            store
                .save(&fence, credentials("rotated"))
                .await
                .unwrap_err(),
            CredentialError::Ambiguous
        );
        let receipt = restart(&store).reconcile(&fence).await.unwrap();
        assert_eq!(receipt.revision(), 1);
        assert_eq!(
            store.load(ACCOUNT).await.unwrap().unwrap().credentials(),
            &credentials("rotated")
        );
        assert_eq!(
            store
                .save(&fence, credentials("different-response"))
                .await
                .unwrap_err(),
            CredentialError::Stale
        );
        assert_eq!(
            store
                .save(&fence, credentials("rotated"))
                .await
                .unwrap()
                .revision(),
            1
        );
    }

    #[tokio::test]
    async fn interrupted_chunk_write_preserves_pending_and_exact_cleanup_obligations() {
        let (store, keyring) = fixture();
        save_first(&store, "old").await;
        let fence = store.begin_change(ACCOUNT, 1).await.unwrap();
        keyring.faults.lock().unwrap().reject_chunks = true;
        assert_eq!(
            store.save(&fence, credentials("new")).await.unwrap_err(),
            CredentialError::Ambiguous
        );
        assert_eq!(
            restart(&store).load(ACCOUNT).await.unwrap_err(),
            CredentialError::Unresolved
        );
        keyring.faults.lock().unwrap().reject_chunks = false;
        store.delete(ACCOUNT, Some(2)).await.unwrap();
        assert_eq!(keyring.values.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn acknowledged_logout_retains_retryable_cleanup_after_failure() {
        let (store, keyring) = fixture();
        save_first(&store, "first").await;
        keyring.faults.lock().unwrap().reject_deletes = true;
        let deleted = store.delete(ACCOUNT, Some(1)).await.unwrap();
        assert_eq!(deleted.revision(), 2);
        assert!(deleted.cleanup_pending());
        assert!(restart(&store).load(ACCOUNT).await.unwrap().is_none());
        assert!(
            restart(&store)
                .status(ACCOUNT)
                .await
                .unwrap()
                .cleanup_pending
        );
        assert_eq!(
            store.begin_change(ACCOUNT, 2).await.unwrap_err(),
            CredentialError::CleanupPending
        );
        keyring.faults.lock().unwrap().reject_deletes = false;
        restart(&store).flush_cleanup(ACCOUNT).await.unwrap();
        assert!(!store.status(ACCOUNT).await.unwrap().cleanup_pending);
        assert_eq!(keyring.values.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn rotation_cleanup_and_logout_preserve_other_accounts_and_profiles() {
        let (store, keyring) = fixture();
        save_first(&store, "first").await;
        let second = store.begin_change(OTHER_ACCOUNT, 0).await.unwrap();
        store
            .save(&second, credentials("other-account"))
            .await
            .unwrap();
        let other_profile = CredentialStore {
            profile: Uuid::new_v4(),
            keyring: keyring.clone(),
            gate: Arc::default(),
            tasks: TaskOwner::new(64).unwrap(),
        };
        save_first(&other_profile, "other-profile").await;
        keyring
            .values
            .lock()
            .unwrap()
            .insert("legacy-axial-auth-canary".into(), b"unchanged".to_vec());
        let refresh = store.begin_change(ACCOUNT, 1).await.unwrap();
        assert_eq!(
            other_profile
                .save(&refresh, credentials("wrong-profile"))
                .await
                .unwrap_err(),
            CredentialError::Stale
        );
        store.save(&refresh, credentials("rotated")).await.unwrap();
        store.delete(ACCOUNT, None).await.unwrap();
        assert_eq!(
            store
                .load(OTHER_ACCOUNT)
                .await
                .unwrap()
                .unwrap()
                .credentials(),
            &credentials("other-account")
        );
        assert_eq!(
            other_profile
                .load(ACCOUNT)
                .await
                .unwrap()
                .unwrap()
                .credentials(),
            &credentials("other-profile")
        );
        assert_eq!(
            keyring
                .values
                .lock()
                .unwrap()
                .get("legacy-axial-auth-canary")
                .unwrap(),
            b"unchanged"
        );
        assert_ne!(KEYRING_SERVICE, "axial-auth");
    }

    #[tokio::test]
    async fn malformed_secret_is_preserved_without_falling_back_to_previous_tokens() {
        let (store, keyring) = fixture();
        save_first(&store, "first").await;
        let account = parse_account(ACCOUNT).unwrap();
        let head = store.head(account).unwrap().unwrap();
        let HeadState::Live { blob } = head.state else {
            panic!("Expected live record")
        };
        let key = store.chunk_key(account, blob.id, 0);
        keyring
            .values
            .lock()
            .unwrap()
            .insert(key.clone(), b"invalid".to_vec());
        assert_eq!(
            store.load(ACCOUNT).await.unwrap_err(),
            CredentialError::Malformed
        );
        assert_eq!(
            keyring.values.lock().unwrap().get(&key).unwrap(),
            b"invalid"
        );
        assert_eq!(
            store.status("../../unrelated").await.unwrap_err(),
            CredentialError::InvalidAccount
        );
        assert_eq!(
            store.status(&Uuid::nil().to_string()).await.unwrap_err(),
            CredentialError::InvalidAccount
        );
    }

    #[tokio::test]
    async fn large_tokens_use_bounded_chunks_and_cleanup_every_owned_entry() {
        let (store, keyring) = fixture();
        let secrets = Credentials::new(
            "a".repeat(16_000),
            Some("b".repeat(16_000)),
            1,
            "c".repeat(16_000),
            2,
        )
        .unwrap();
        let fence = store.begin_change(ACCOUNT, 0).await.unwrap();
        store.save(&fence, secrets.clone()).await.unwrap();
        let item_limit = if cfg!(target_os = "macos") {
            65_536
        } else {
            900
        };
        assert!(
            keyring
                .values
                .lock()
                .unwrap()
                .values()
                .all(|value| value.len() <= item_limit)
        );
        assert_eq!(
            store.load(ACCOUNT).await.unwrap().unwrap().credentials(),
            &secrets
        );
        store.delete(ACCOUNT, Some(1)).await.unwrap();
        assert_eq!(keyring.values.lock().unwrap().len(), 1);
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn macos_large_credentials_reopen_with_two_secure_reads() {
        let (store, keyring) = fixture();
        let secrets = credentials(&"a".repeat(16_000));
        let fence = store.begin_change(ACCOUNT, 0).await.unwrap();
        store.save(&fence, secrets.clone()).await.unwrap();
        let before = keyring.reads.load(Ordering::SeqCst);
        let loaded = restart(&store).load(ACCOUNT).await.unwrap().unwrap();
        assert_eq!(loaded.revision(), 1);
        assert_eq!(loaded.credentials(), &secrets);
        assert_eq!(keyring.reads.load(Ordering::SeqCst) - before, 2);
    }

    #[tokio::test]
    async fn current_chunked_records_reopen_retry_and_retire_their_exact_items() {
        for pending in [false, true] {
            let (store, keyring) = fixture();
            let account = parse_account(ACCOUNT).unwrap();
            let operation = Uuid::new_v4();
            let secrets = credentials(&"a".repeat(1_000));
            let bytes = secrets.encode_for_keyring().unwrap();
            let chunks = bytes.chunks(900).collect::<Vec<_>>();
            let blob = serde_json::json!({
                "id": operation,
                "chunks": chunks.len(),
                "digest": hex::encode(Sha256::digest(&bytes)),
            });
            let state = if pending {
                serde_json::json!({ "kind": "pending", "previous": null, "candidate": blob })
            } else {
                serde_json::json!({ "kind": "live", "blob": blob })
            };
            let head = serde_json::to_vec(&serde_json::json!({
                "schema": 1, "revision": 1, "operation": operation,
                "state": state, "retired": [],
            }))
            .unwrap();
            {
                let mut entries = keyring.values.lock().unwrap();
                entries.insert(store.head_key(account), head);
                for (index, chunk) in chunks.iter().enumerate() {
                    entries.insert(store.chunk_key(account, operation, index), chunk.to_vec());
                }
            }
            let reopened = restart(&store);
            if pending {
                assert_eq!(
                    reopened.load(ACCOUNT).await.unwrap_err(),
                    CredentialError::Unresolved
                );
            } else {
                assert_eq!(
                    reopened.load(ACCOUNT).await.unwrap().unwrap().credentials(),
                    &secrets
                );
            }
            let fence = CredentialFence {
                profile: store.profile,
                account,
                revision: 1,
                operation,
            };
            assert_eq!(
                reopened
                    .save(&fence, secrets.clone())
                    .await
                    .unwrap()
                    .revision(),
                1
            );
            assert_eq!(keyring.values.lock().unwrap().len(), chunks.len() + 1);
            let next = reopened.begin_change(ACCOUNT, 1).await.unwrap();
            reopened.save(&next, credentials("next")).await.unwrap();
            assert_eq!(keyring.values.lock().unwrap().len(), 2);
            assert_eq!(
                restart(&store)
                    .load(ACCOUNT)
                    .await
                    .unwrap()
                    .unwrap()
                    .credentials(),
                &credentials("next")
            );
            reopened.delete(ACCOUNT, Some(2)).await.unwrap();
            assert_eq!(keyring.values.lock().unwrap().len(), 1);
        }
    }

    #[tokio::test]
    async fn invalid_payload_layouts_refuse_before_reading_any_payload() {
        let (store, keyring) = fixture();
        save_first(&store, "first").await;
        let key = store.head_key(parse_account(ACCOUNT).unwrap());
        let original: serde_json::Value =
            serde_json::from_slice(keyring.values.lock().unwrap().get(&key).unwrap()).unwrap();
        for (layout, chunks) in [("single", 2), ("future", 1)] {
            let mut value = original.clone();
            value["state"]["blob"]["layout"] = layout.into();
            value["state"]["blob"]["chunks"] = chunks.into();
            let bytes = serde_json::to_vec(&value).unwrap();
            keyring
                .values
                .lock()
                .unwrap()
                .insert(key.clone(), bytes.clone());
            let before = keyring.reads.load(Ordering::SeqCst);
            assert_eq!(
                store.load(ACCOUNT).await.unwrap_err(),
                CredentialError::Malformed
            );
            assert_eq!(keyring.reads.load(Ordering::SeqCst) - before, 1);
            assert_eq!(keyring.values.lock().unwrap().get(&key).unwrap(), &bytes);
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "Requires a locally signed executable and an unlocked macOS test keychain"]
    fn macos_keyring_accepts_a_complete_bounded_payload() {
        let key = format!("native-capacity-test:{}", Uuid::new_v4());
        let bytes = vec![0xa5; MAX_CREDENTIAL_BYTES];
        let written = OsKeyring.set(&key, &bytes);
        let observed = OsKeyring.get(&key);
        let removed = OsKeyring.delete(&key);
        let absent = OsKeyring.get(&key);
        assert!(written.is_ok(), "Synthetic payload write failed");
        assert!(
            matches!(observed, Ok(Some(value)) if value == bytes),
            "Synthetic payload readback failed"
        );
        assert!(removed.is_ok(), "Synthetic item cleanup failed");
        assert!(matches!(absent, Ok(None)), "Synthetic item remains");
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    #[ignore = "Requires a locally signed executable and an unlocked macOS test keychain"]
    async fn macos_saved_credentials_reopen_in_a_signed_process() {
        const CHILD_PROFILE: &str = "AXIAL_TEST_KEYCHAIN_REOPEN_PROFILE";
        let child_profile = std::env::var(CHILD_PROFILE).ok();
        let profile = child_profile
            .as_deref()
            .map(|value| Uuid::parse_str(value).unwrap())
            .unwrap_or_else(Uuid::new_v4);
        let store = CredentialStore::open(profile);
        let secrets = credentials(&"a".repeat(16_000));
        if child_profile.is_some() {
            let loaded = store.load(ACCOUNT).await.unwrap().unwrap();
            assert_eq!(loaded.revision(), 1);
            assert_eq!(loaded.credentials(), &secrets);
            return;
        }
        let outcome = async {
            let fence = store.begin_change(ACCOUNT, 0).await.map_err(|_| "Publication")?;
            store.save(&fence, secrets).await.map_err(|_| "Publication")?;
            let mut child = tokio::process::Command::new(std::env::current_exe().map_err(|_| "Executable")?)
            .args([
                "--ignored", "--exact",
                "accounts::credential_store::tests::macos_saved_credentials_reopen_in_a_signed_process",
                "--test-threads=1",
            ])
            .env(CHILD_PROFILE, profile.to_string())
            .stdout(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|_| "Spawn")?;
            match tokio::time::timeout(std::time::Duration::from_secs(10), child.wait()).await {
                Ok(Ok(status)) if status.success() => Ok(()),
                Ok(Ok(_)) => Err("Reopen"),
                _ => {
                    if !matches!(tokio::time::timeout(std::time::Duration::from_secs(3), child.kill()).await, Ok(Ok(()))) {
                        return Err("Unsettled child");
                    }
                    Err("Reopen")
                }
            }
        }.await;
        assert_ne!(
            outcome,
            Err("Unsettled child"),
            "Retained synthetic profile {profile}"
        );
        let cleanup = async {
            let deleted = store.delete(ACCOUNT, None).await?;
            if deleted.cleanup_pending() {
                return Err(CredentialError::CleanupPending);
            }
            let key = store.head_key(parse_account(ACCOUNT)?);
            OsKeyring.delete(&key)?;
            match OsKeyring.get(&key)? {
                None => Ok(()),
                Some(_) => Err(CredentialError::CleanupPending),
            }
        }
        .await;
        assert!(cleanup.is_ok(), "Retained synthetic profile {profile}");
        assert!(outcome.is_ok(), "Signed process reopen failed: {outcome:?}");
    }

    #[tokio::test]
    async fn dropped_waiter_cannot_cancel_accepted_keyring_change() {
        let (store, keyring) = fixture();
        let block = Arc::new(WriteBlock::default());
        *keyring.block_next_write.lock().unwrap() = Some(block.clone());
        let requester = {
            let store = store.clone();
            tokio::spawn(async move { store.begin_change(ACCOUNT, 0).await })
        };
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while !block.entered.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        requester.abort();
        *block.proceed.lock().unwrap() = true;
        block.released.notify_all();
        let status = store.status(ACCOUNT).await.unwrap();
        assert_eq!(status.revision, 1);
        assert_eq!(status.state, CredentialState::Pending);
        assert_eq!(
            store.load(ACCOUNT).await.unwrap_err(),
            CredentialError::Unresolved
        );
    }
}
