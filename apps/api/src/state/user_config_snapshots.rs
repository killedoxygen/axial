use crate::execution::persistence::{
    AcceptedWrite, AtomicSnapshotWriter, PersistenceCoordinator, PersistenceError,
    PersistenceOwnerLease, WriteUrgency,
};
use crate::execution::user_owned_state::{UserConfigCapture, classify_user_config_leaf};
use crate::state::contracts::{
    OperationId, ReconciliationIncarnationFingerprint, ReconciliationInventoryFingerprint,
    TargetDescriptor, valid_reconciliation_fingerprint,
};
use crate::state::ownership::{CurrentArtifact, classify_current_artifact};
use axial_config::{AppPaths, Instance, is_canonical_instance_id};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{self, Read as _};
use std::path::Path;
use std::sync::{Arc, Mutex};
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

const USER_CONFIG_SNAPSHOT_FILE: &str = "guardian-user-config-snapshots.json";
const USER_CONFIG_SNAPSHOT_SCHEMA: &str = "axial.guardian_user_config_snapshots";
const USER_CONFIG_SNAPSHOT_SCHEMA_VERSION: u32 = 1;
const USER_CONFIG_SNAPSHOT_MAX_ENCODED_BYTES: usize = 6 * 1024 * 1024;
const USER_CONFIG_SNAPSHOT_MAX_RECORDS: usize = 8;
const USER_CONFIG_SNAPSHOT_MAX_FILES: usize = 64;
const USER_CONFIG_SNAPSHOT_MAX_ENTRIES: usize = USER_CONFIG_SNAPSHOT_MAX_FILES + 1;
const USER_CONFIG_SNAPSHOT_MAX_FILE_BYTES: u64 = 64 * 1024;
const USER_CONFIG_SNAPSHOT_MAX_RAW_BYTES: u64 = 512 * 1024;
const USER_CONFIG_SNAPSHOT_ID_DOMAIN: &[u8] = b"axial.guardian.user-config-snapshot.v1\0";
const USER_CONFIG_SNAPSHOT_LOCK_INVARIANT: &str =
    "user config snapshot lock poisoned; committed and persisted state may diverge";

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct UserConfigSnapshotFile {
    schema: String,
    schema_version: u32,
    snapshots: Vec<UserConfigSnapshotRecord>,
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct UserConfigSnapshotRecord {
    snapshot_id: String,
    instance_id: String,
    instance_created_at: String,
    incarnation_fingerprint: String,
    inventory_fingerprint: String,
    r3_operation_id: String,
    captured_at: String,
    entries: Vec<UserConfigSnapshotEntry>,
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
enum UserConfigSnapshotEntry {
    Absent {
        slot: String,
    },
    File {
        slot: String,
        size: u64,
        sha256: String,
        bytes: String,
    },
}

impl UserConfigSnapshotFile {
    fn empty() -> Self {
        Self {
            schema: USER_CONFIG_SNAPSHOT_SCHEMA.to_string(),
            schema_version: USER_CONFIG_SNAPSHOT_SCHEMA_VERSION,
            snapshots: Vec::new(),
        }
    }
}

struct UserConfigSnapshotState {
    visible: UserConfigSnapshotFile,
    retry_candidate: Option<(u64, UserConfigSnapshotFile)>,
    pending_discards: Vec<UserConfigSnapshotSelector>,
    in_flight: Vec<UserConfigSnapshotSelector>,
    pending_retirements: BTreeSet<String>,
    startup_cleanup_pending: bool,
}

struct UserConfigSnapshotPersistence {
    owner: PersistenceOwnerLease,
    writer: AtomicSnapshotWriter,
}

struct PendingUserConfigSnapshotCommit {
    ticket: AcceptedWrite,
    revision: u64,
    candidate: UserConfigSnapshotFile,
}

pub(super) struct UserConfigSnapshotStore {
    state: Arc<Mutex<UserConfigSnapshotState>>,
    mutation_gate: Arc<AsyncMutex<()>>,
    persistence: UserConfigSnapshotPersistence,
}

#[must_use]
pub(super) struct UserConfigSnapshotReceipt {
    selector: UserConfigSnapshotSelector,
    armed: bool,
    state: Arc<Mutex<UserConfigSnapshotState>>,
}

#[derive(Clone, Eq, PartialEq)]
struct UserConfigSnapshotSelector {
    snapshot_id: String,
    instance_id: String,
    instance_created_at: String,
    incarnation_fingerprint: String,
    inventory_fingerprint: String,
    r3_operation_id: String,
}

pub(super) struct UserConfigSnapshotIdentity {
    pub(super) instance_id: String,
    pub(super) instance_created_at: String,
    pub(super) incarnation_fingerprint: ReconciliationIncarnationFingerprint,
    pub(super) inventory_fingerprint: ReconciliationInventoryFingerprint,
    pub(super) r3_operation_id: OperationId,
    pub(super) captured_at: chrono::DateTime<chrono::FixedOffset>,
}

impl UserConfigSnapshotStore {
    pub(super) fn claim(
        paths: &AppPaths,
        registered: &[Instance],
        registry_authoritative: bool,
    ) -> io::Result<Self> {
        Self::claim_with_coordinator(
            paths,
            registered,
            registry_authoritative,
            PersistenceCoordinator::global(),
        )
    }

    fn claim_with_coordinator(
        paths: &AppPaths,
        registered: &[Instance],
        registry_authoritative: bool,
        persistence: PersistenceCoordinator,
    ) -> io::Result<Self> {
        let path = paths.config_dir.join(USER_CONFIG_SNAPSHOT_FILE);
        let owner = persistence.claim_owner(&path).map_err(io::Error::from)?;
        let writer = owner
            .writer(&path, user_config_snapshot_target())
            .map_err(io::Error::from)?;
        let loaded = load_user_config_snapshot_file(&path)?;
        let registered = registered
            .iter()
            .map(|instance| (instance.id.as_str(), instance.created_at.as_str()))
            .collect::<BTreeMap<_, _>>();
        let mut visible = loaded.clone();
        if registry_authoritative {
            visible.snapshots.retain(|record| {
                registered
                    .get(record.instance_id.as_str())
                    .is_some_and(|created_at| *created_at == record.instance_created_at)
            });
        }
        let startup_cleanup_pending = visible != loaded;
        Ok(Self {
            state: Arc::new(Mutex::new(UserConfigSnapshotState {
                visible,
                retry_candidate: None,
                pending_discards: Vec::new(),
                in_flight: Vec::new(),
                pending_retirements: BTreeSet::new(),
                startup_cleanup_pending,
            })),
            mutation_gate: Arc::new(AsyncMutex::new(())),
            persistence: UserConfigSnapshotPersistence { owner, writer },
        })
    }

    pub(super) async fn persist_before_core(
        &self,
        identity: UserConfigSnapshotIdentity,
        capture: UserConfigCapture,
    ) -> io::Result<UserConfigSnapshotReceipt> {
        let mutation = self.mutation_gate.clone().lock_owned().await;
        let mutation = self.reconcile_obligations_holding_gate(mutation).await?;
        let record = user_config_snapshot_record(&identity, capture)?;
        let selector = UserConfigSnapshotSelector::from_record(&record);
        let mut candidate = self
            .state
            .lock()
            .expect(USER_CONFIG_SNAPSHOT_LOCK_INVARIANT)
            .visible
            .clone();
        candidate
            .snapshots
            .retain(|existing| existing.instance_id != record.instance_id);
        candidate.snapshots.push(record);
        candidate.snapshots.sort_by(snapshot_record_order);
        while candidate.snapshots.len() > USER_CONFIG_SNAPSHOT_MAX_RECORDS {
            candidate.snapshots.remove(0);
        }
        {
            let mut state = self
                .state
                .lock()
                .expect(USER_CONFIG_SNAPSHOT_LOCK_INVARIANT);
            if !state.pending_discards.contains(&selector) {
                state.pending_discards.push(selector.clone());
            }
        }
        match self.commit_holding_gate(candidate, mutation).await {
            Ok(mutation) => {
                let receipt = self.arm_in_flight(selector);
                drop(mutation);
                Ok(receipt)
            }
            Err(error) => Err(error),
        }
    }

    pub(super) async fn reconcile_pending_removals(&self) -> io::Result<()> {
        let mutation = self.mutation_gate.clone().lock_owned().await;
        self.reconcile_obligations_holding_gate(mutation)
            .await
            .map(drop)
    }

    pub(super) async fn retire_instance(&self, instance_id: String) -> io::Result<()> {
        let mutation = self.mutation_gate.clone().lock_owned().await;
        self.state
            .lock()
            .expect(USER_CONFIG_SNAPSHOT_LOCK_INVARIANT)
            .pending_retirements
            .insert(instance_id);
        self.reconcile_obligations_holding_gate(mutation)
            .await
            .map(drop)
    }

    pub(super) async fn retain_successful_incarnations(
        &self,
        successful: &BTreeSet<(String, String, String, String, String)>,
    ) -> io::Result<()> {
        let mutation = self.mutation_gate.clone().lock_owned().await;
        let mutation = self.reconcile_obligations_holding_gate(mutation).await?;
        let (mut candidate, startup_cleanup_pending) = {
            let state = self
                .state
                .lock()
                .expect(USER_CONFIG_SNAPSHOT_LOCK_INVARIANT);
            (state.visible.clone(), state.startup_cleanup_pending)
        };
        let before = candidate.snapshots.len();
        candidate.snapshots.retain(|record| {
            successful.contains(&(
                record.instance_id.clone(),
                record.instance_created_at.clone(),
                record.incarnation_fingerprint.clone(),
                record.inventory_fingerprint.clone(),
                record.r3_operation_id.clone(),
            ))
        });
        if candidate.snapshots.len() == before && !startup_cleanup_pending {
            drop(mutation);
            return Ok(());
        }
        self.commit_holding_gate(candidate, mutation)
            .await
            .map(drop)
    }

    pub(super) async fn close(&self) -> io::Result<()> {
        let mutation = self.mutation_gate.clone().lock_owned().await;
        let mutation = self.reconcile_obligations_holding_gate(mutation).await?;
        if !self
            .state
            .lock()
            .expect(USER_CONFIG_SNAPSHOT_LOCK_INVARIANT)
            .in_flight
            .is_empty()
        {
            drop(mutation);
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "user config snapshot receipts remain in flight",
            ));
        }
        let cleanup = {
            let state = self
                .state
                .lock()
                .expect(USER_CONFIG_SNAPSHOT_LOCK_INVARIANT);
            state.startup_cleanup_pending.then(|| state.visible.clone())
        };
        let mutation = match cleanup {
            Some(candidate) => self.commit_holding_gate(candidate, mutation).await?,
            None => mutation,
        };
        self.persistence
            .owner
            .close()
            .await
            .map_err(io::Error::from)?;
        drop(mutation);
        Ok(())
    }

    async fn reconcile_retry_holding_gate(
        &self,
        mutation: OwnedMutexGuard<()>,
    ) -> io::Result<OwnedMutexGuard<()>> {
        let retained = self
            .state
            .lock()
            .expect(USER_CONFIG_SNAPSHOT_LOCK_INVARIANT)
            .retry_candidate
            .clone();
        let Some((candidate_revision, candidate)) = retained else {
            return Ok(mutation);
        };
        let ticket = self.persistence.writer.retry().map_err(io::Error::from)?;
        let revision = ticket.revision().get();
        assert_eq!(
            revision, candidate_revision,
            "user config snapshot retry diverged from its exact candidate"
        );
        self.await_commit_holding_gate(
            PendingUserConfigSnapshotCommit {
                ticket,
                revision,
                candidate,
            },
            mutation,
        )
        .await
    }

    async fn reconcile_obligations_holding_gate(
        &self,
        mutation: OwnedMutexGuard<()>,
    ) -> io::Result<OwnedMutexGuard<()>> {
        let mutation = self.reconcile_retry_holding_gate(mutation).await?;
        let (pending_discards, pending_retirements, mut candidate) = {
            let state = self
                .state
                .lock()
                .expect(USER_CONFIG_SNAPSHOT_LOCK_INVARIANT);
            (
                state.pending_discards.clone(),
                state.pending_retirements.clone(),
                state.visible.clone(),
            )
        };
        if pending_discards.is_empty() && pending_retirements.is_empty() {
            return Ok(mutation);
        }
        let before = candidate.snapshots.len();
        candidate.snapshots.retain(|record| {
            !pending_retirements.contains(&record.instance_id)
                && !pending_discards
                    .iter()
                    .any(|discard| discard.matches(record))
        });
        if candidate.snapshots.len() == before {
            self.clear_pending_removals(&pending_discards, &pending_retirements);
            return Ok(mutation);
        }
        let mutation = self.commit_holding_gate(candidate, mutation).await?;
        self.clear_pending_removals(&pending_discards, &pending_retirements);
        Ok(mutation)
    }

    fn arm_in_flight(&self, selector: UserConfigSnapshotSelector) -> UserConfigSnapshotReceipt {
        let mut state = self
            .state
            .lock()
            .expect(USER_CONFIG_SNAPSHOT_LOCK_INVARIANT);
        state
            .pending_discards
            .retain(|pending| pending != &selector);
        assert!(
            !state.in_flight.contains(&selector),
            "user config snapshot selector cannot be armed twice"
        );
        state.in_flight.push(selector.clone());
        drop(state);
        UserConfigSnapshotReceipt {
            selector,
            armed: true,
            state: self.state.clone(),
        }
    }

    fn clear_pending_removals(
        &self,
        completed_discards: &[UserConfigSnapshotSelector],
        completed_retirements: &BTreeSet<String>,
    ) {
        let mut state = self
            .state
            .lock()
            .expect(USER_CONFIG_SNAPSHOT_LOCK_INVARIANT);
        state
            .pending_discards
            .retain(|pending| !completed_discards.contains(pending));
        state
            .pending_retirements
            .retain(|pending| !completed_retirements.contains(pending));
    }

    async fn commit_holding_gate(
        &self,
        candidate: UserConfigSnapshotFile,
        mutation: OwnedMutexGuard<()>,
    ) -> io::Result<OwnedMutexGuard<()>> {
        let encoded = encode_user_config_snapshot_file(candidate.clone())?;
        let ticket = self
            .persistence
            .writer
            .accept(encoded, WriteUrgency::Immediate, Ok)
            .map_err(io::Error::from)?;
        let revision = ticket.revision().get();
        self.await_commit_holding_gate(
            PendingUserConfigSnapshotCommit {
                ticket,
                revision,
                candidate,
            },
            mutation,
        )
        .await
    }

    async fn await_commit_holding_gate(
        &self,
        commit: PendingUserConfigSnapshotCommit,
        mutation: OwnedMutexGuard<()>,
    ) -> io::Result<OwnedMutexGuard<()>> {
        let state = self.state.clone();
        let (completed_tx, completed_rx) = tokio::sync::oneshot::channel();
        commit.ticket.observe(move |result| {
            let result = match result {
                Ok(_) => {
                    let mut state = state.lock().expect(USER_CONFIG_SNAPSHOT_LOCK_INVARIANT);
                    state.visible = commit.candidate;
                    state.startup_cleanup_pending = false;
                    if state
                        .retry_candidate
                        .as_ref()
                        .is_some_and(|(revision, _)| *revision == commit.revision)
                    {
                        state.retry_candidate = None;
                    }
                    Ok(())
                }
                Err(error) => {
                    if matches!(&error, PersistenceError::Write { .. }) {
                        state
                            .lock()
                            .expect(USER_CONFIG_SNAPSHOT_LOCK_INVARIANT)
                            .retry_candidate = Some((commit.revision, commit.candidate));
                    }
                    Err(io::Error::from(error))
                }
            };
            let _ = completed_tx.send((result, mutation));
        });
        let (result, mutation) = completed_rx
            .await
            .map_err(|_| io::Error::other("user config snapshot commit observer stopped"))?;
        result?;
        Ok(mutation)
    }
}

impl UserConfigSnapshotReceipt {
    fn arm_pending_discard(&mut self) {
        if !self.armed {
            return;
        }
        let mut state = self
            .state
            .lock()
            .expect(USER_CONFIG_SNAPSHOT_LOCK_INVARIANT);
        state.in_flight.retain(|pending| pending != &self.selector);
        if !state.pending_discards.contains(&self.selector) {
            state.pending_discards.push(self.selector.clone());
        }
        self.armed = false;
    }

    pub(super) fn confirm(mut self) -> Self {
        if self.armed {
            self.state
                .lock()
                .expect(USER_CONFIG_SNAPSHOT_LOCK_INVARIANT)
                .in_flight
                .retain(|pending| pending != &self.selector);
            self.armed = false;
        }
        self
    }

    #[cfg(test)]
    fn matches(&self, record: &UserConfigSnapshotRecord) -> bool {
        self.selector.matches(record)
    }
}

impl Drop for UserConfigSnapshotReceipt {
    fn drop(&mut self) {
        self.arm_pending_discard();
    }
}

impl UserConfigSnapshotSelector {
    fn from_record(record: &UserConfigSnapshotRecord) -> Self {
        Self {
            snapshot_id: record.snapshot_id.clone(),
            instance_id: record.instance_id.clone(),
            instance_created_at: record.instance_created_at.clone(),
            incarnation_fingerprint: record.incarnation_fingerprint.clone(),
            inventory_fingerprint: record.inventory_fingerprint.clone(),
            r3_operation_id: record.r3_operation_id.clone(),
        }
    }

    fn matches(&self, record: &UserConfigSnapshotRecord) -> bool {
        self.snapshot_id == record.snapshot_id
            && self.instance_id == record.instance_id
            && self.instance_created_at == record.instance_created_at
            && self.incarnation_fingerprint == record.incarnation_fingerprint
            && self.inventory_fingerprint == record.inventory_fingerprint
            && self.r3_operation_id == record.r3_operation_id
    }
}

fn load_user_config_snapshot_file(path: &Path) -> io::Result<UserConfigSnapshotFile> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(UserConfigSnapshotFile::empty());
        }
        Err(error) => return Err(error),
    };
    let mut encoded = Vec::new();
    file.by_ref()
        .take((USER_CONFIG_SNAPSHOT_MAX_ENCODED_BYTES + 1) as u64)
        .read_to_end(&mut encoded)?;
    if encoded.len() > USER_CONFIG_SNAPSHOT_MAX_ENCODED_BYTES {
        return Err(invalid_snapshot_store(
            "snapshot store exceeds its byte bound",
        ));
    }
    let snapshot = serde_json::from_slice::<UserConfigSnapshotFile>(&encoded)
        .map_err(|_| invalid_snapshot_store("snapshot store is not strict current-v1 JSON"))?;
    validate_user_config_snapshot_file(&snapshot)?;
    Ok(snapshot)
}

fn encode_user_config_snapshot_file(snapshot: UserConfigSnapshotFile) -> io::Result<Vec<u8>> {
    validate_user_config_snapshot_file(&snapshot)?;
    let encoded = serde_json::to_vec_pretty(&snapshot)
        .map_err(|_| invalid_snapshot_store("snapshot store encoding failed"))?;
    if encoded.len() > USER_CONFIG_SNAPSHOT_MAX_ENCODED_BYTES {
        return Err(invalid_snapshot_store(
            "snapshot store exceeds its byte bound",
        ));
    }
    Ok(encoded)
}

fn validate_user_config_snapshot_file(snapshot: &UserConfigSnapshotFile) -> io::Result<()> {
    if snapshot.schema != USER_CONFIG_SNAPSHOT_SCHEMA
        || snapshot.schema_version != USER_CONFIG_SNAPSHOT_SCHEMA_VERSION
        || snapshot.snapshots.len() > USER_CONFIG_SNAPSHOT_MAX_RECORDS
        || snapshot
            .snapshots
            .windows(2)
            .any(|pair| snapshot_record_order(&pair[0], &pair[1]) != std::cmp::Ordering::Less)
    {
        return Err(invalid_snapshot_store("snapshot store is not canonical"));
    }
    let mut instances = std::collections::BTreeSet::new();
    for record in &snapshot.snapshots {
        if !instances.insert(record.instance_id.as_str()) {
            return Err(invalid_snapshot_store(
                "snapshot store retains more than the latest instance capture",
            ));
        }
        validate_user_config_snapshot_record(record)?;
    }
    Ok(())
}

fn validate_user_config_snapshot_record(record: &UserConfigSnapshotRecord) -> io::Result<()> {
    if !is_canonical_instance_id(&record.instance_id)
        || record.instance_created_at.len() > 64
        || chrono::DateTime::parse_from_rfc3339(&record.instance_created_at).is_err()
        || !valid_reconciliation_fingerprint(&record.incarnation_fingerprint)
        || !valid_reconciliation_fingerprint(&record.inventory_fingerprint)
        || record.r3_operation_id.is_empty()
        || record.r3_operation_id.len() > 128
        || record.r3_operation_id.chars().any(char::is_control)
        || chrono::DateTime::parse_from_rfc3339(&record.captured_at).is_err()
        || !is_lower_hex_digest(&record.snapshot_id)
        || record.entries.is_empty()
        || record.entries.len() > USER_CONFIG_SNAPSHOT_MAX_ENTRIES
        || record.entries.windows(2).any(|pair| {
            user_config_snapshot_entry_slot(&pair[0]) >= user_config_snapshot_entry_slot(&pair[1])
        })
    {
        return Err(invalid_snapshot_store("snapshot record is not canonical"));
    }
    let mut file_count = 0_usize;
    let mut raw_bytes = 0_u64;
    let mut options_count = 0_usize;
    for entry in &record.entries {
        let slot = user_config_snapshot_entry_slot(entry);
        if slot == "options.txt" {
            options_count += 1;
        } else if !is_canonical_config_slot(slot) {
            return Err(invalid_snapshot_store(
                "snapshot slot is outside the closed set",
            ));
        }
        match entry {
            UserConfigSnapshotEntry::Absent { .. } => {
                if slot != "options.txt" {
                    return Err(invalid_snapshot_store(
                        "only options may be explicitly absent",
                    ));
                }
            }
            UserConfigSnapshotEntry::File {
                size,
                sha256,
                bytes,
                ..
            } => {
                file_count += 1;
                if file_count > USER_CONFIG_SNAPSHOT_MAX_FILES
                    || *size > USER_CONFIG_SNAPSHOT_MAX_FILE_BYTES
                    || !is_lower_hex_digest(sha256)
                {
                    return Err(invalid_snapshot_store("snapshot file bound is invalid"));
                }
                let decoded = base64::engine::general_purpose::STANDARD
                    .decode(bytes)
                    .map_err(|_| invalid_snapshot_store("snapshot bytes are not base64"))?;
                if decoded.len() as u64 != *size
                    || std::str::from_utf8(&decoded).is_err()
                    || base64::engine::general_purpose::STANDARD.encode(&decoded) != *bytes
                    || hex_lower(&Sha256::digest(&decoded)) != *sha256
                {
                    return Err(invalid_snapshot_store(
                        "snapshot bytes do not match evidence",
                    ));
                }
                raw_bytes = raw_bytes
                    .checked_add(*size)
                    .filter(|total| *total <= USER_CONFIG_SNAPSHOT_MAX_RAW_BYTES)
                    .ok_or_else(|| invalid_snapshot_store("snapshot raw byte bound exceeded"))?;
            }
        }
    }
    if options_count != 1 || user_config_snapshot_id(record)? != record.snapshot_id {
        return Err(invalid_snapshot_store("snapshot identity is not canonical"));
    }
    Ok(())
}

fn user_config_snapshot_record(
    identity: &UserConfigSnapshotIdentity,
    capture: UserConfigCapture,
) -> io::Result<UserConfigSnapshotRecord> {
    let mut entries = Vec::new();
    for entry in capture.into_entries() {
        let (slot, content) = entry.into_parts();
        entries.push(match content {
            None => UserConfigSnapshotEntry::Absent { slot },
            Some((bytes, sha256)) => UserConfigSnapshotEntry::File {
                slot,
                size: bytes.len() as u64,
                sha256: hex_lower(&sha256),
                bytes: base64::engine::general_purpose::STANDARD.encode(bytes),
            },
        });
    }
    entries.sort_by(|left, right| {
        user_config_snapshot_entry_slot(left).cmp(user_config_snapshot_entry_slot(right))
    });
    let mut record = UserConfigSnapshotRecord {
        snapshot_id: "0".repeat(64),
        instance_id: identity.instance_id.clone(),
        instance_created_at: identity.instance_created_at.clone(),
        incarnation_fingerprint: identity.incarnation_fingerprint.as_str().to_string(),
        inventory_fingerprint: identity.inventory_fingerprint.as_str().to_string(),
        r3_operation_id: identity.r3_operation_id.as_str().to_string(),
        captured_at: identity
            .captured_at
            .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
        entries,
    };
    record.snapshot_id = user_config_snapshot_id(&record)?;
    validate_user_config_snapshot_record(&record)?;
    Ok(record)
}

fn user_config_snapshot_id(record: &UserConfigSnapshotRecord) -> io::Result<String> {
    let mut digest = Sha256::new();
    digest.update(USER_CONFIG_SNAPSHOT_ID_DOMAIN);
    update_snapshot_id_field(&mut digest, record.instance_id.as_bytes());
    update_snapshot_id_field(&mut digest, record.instance_created_at.as_bytes());
    update_snapshot_id_field(&mut digest, record.incarnation_fingerprint.as_bytes());
    update_snapshot_id_field(&mut digest, record.inventory_fingerprint.as_bytes());
    update_snapshot_id_field(&mut digest, record.r3_operation_id.as_bytes());
    update_snapshot_id_field(&mut digest, record.captured_at.as_bytes());
    for entry in &record.entries {
        update_snapshot_id_field(
            &mut digest,
            user_config_snapshot_entry_slot(entry).as_bytes(),
        );
        match entry {
            UserConfigSnapshotEntry::Absent { .. } => digest.update([0]),
            UserConfigSnapshotEntry::File {
                size,
                sha256,
                bytes,
                ..
            } => {
                digest.update([1]);
                digest.update(size.to_be_bytes());
                update_snapshot_id_field(&mut digest, sha256.as_bytes());
                let decoded = base64::engine::general_purpose::STANDARD
                    .decode(bytes)
                    .map_err(|_| invalid_snapshot_store("snapshot identity bytes are invalid"))?;
                update_snapshot_id_field(&mut digest, &decoded);
            }
        }
    }
    Ok(hex_lower(&digest.finalize()))
}

fn update_snapshot_id_field(digest: &mut Sha256, field: &[u8]) {
    digest.update((field.len() as u64).to_be_bytes());
    digest.update(field);
}

fn snapshot_record_order(
    left: &UserConfigSnapshotRecord,
    right: &UserConfigSnapshotRecord,
) -> std::cmp::Ordering {
    (&left.captured_at, &left.snapshot_id).cmp(&(&right.captured_at, &right.snapshot_id))
}

fn user_config_snapshot_entry_slot(entry: &UserConfigSnapshotEntry) -> &str {
    match entry {
        UserConfigSnapshotEntry::Absent { slot } | UserConfigSnapshotEntry::File { slot, .. } => {
            slot
        }
    }
}

fn is_canonical_config_slot(slot: &str) -> bool {
    let Some(name) = slot.strip_prefix("config/") else {
        return false;
    };
    classify_user_config_leaf(name).is_ok_and(|selected| selected)
}

fn is_lower_hex_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn hex_lower(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut encoded, "{byte:02x}").expect("writing to a String cannot fail");
    }
    encoded
}

fn invalid_snapshot_store(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn user_config_snapshot_target() -> TargetDescriptor {
    classify_current_artifact(
        CurrentArtifact::UserConfigSnapshot,
        "guardian_user_config_snapshots",
    )
    .target
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::file::{FileWriteRequest, write_file_atomically};
    use crate::execution::persistence::AtomicWriteBackend;
    use crate::execution::user_owned_state::capture_user_config;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
    use std::time::Duration;

    static NEXT_ROOT: AtomicU64 = AtomicU64::new(0);
    static_assertions::assert_not_impl_any!(
        UserConfigSnapshotReceipt: Clone, std::fmt::Debug, serde::Serialize, serde::de::DeserializeOwned
    );

    struct ControlledFileBackend {
        attempts: AtomicUsize,
        failures: AtomicUsize,
        permanent_failure: AtomicBool,
    }

    impl ControlledFileBackend {
        fn fail_once() -> Self {
            Self {
                attempts: AtomicUsize::new(0),
                failures: AtomicUsize::new(1),
                permanent_failure: AtomicBool::new(false),
            }
        }

        fn permanently_failing() -> Self {
            Self {
                attempts: AtomicUsize::new(0),
                failures: AtomicUsize::new(0),
                permanent_failure: AtomicBool::new(true),
            }
        }

        fn recover(&self) {
            self.permanent_failure.store(false, Ordering::SeqCst);
        }
    }

    impl AtomicWriteBackend for ControlledFileBackend {
        fn write(
            &self,
            target: &TargetDescriptor,
            destination: &Path,
            contents: &[u8],
        ) -> io::Result<()> {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            let injected = self.permanent_failure.load(Ordering::SeqCst)
                || self
                    .failures
                    .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                        (remaining > 0).then(|| remaining - 1)
                    })
                    .is_ok();
            if injected {
                return Err(io::Error::other(
                    "injected user config snapshot write failure",
                ));
            }
            write_file_atomically(FileWriteRequest::new(target.clone(), destination, contents))
                .map(drop)
                .map_err(io::Error::from)
        }
    }

    struct BlockingFirstFileBackend {
        attempts: AtomicUsize,
        entered: Mutex<Option<std::sync::mpsc::Sender<()>>>,
        release: Mutex<std::sync::mpsc::Receiver<()>>,
    }

    impl AtomicWriteBackend for BlockingFirstFileBackend {
        fn write(
            &self,
            target: &TargetDescriptor,
            destination: &Path,
            contents: &[u8],
        ) -> io::Result<()> {
            let attempt = self.attempts.fetch_add(1, Ordering::SeqCst);
            if attempt == 0 {
                if let Some(entered) = self.entered.lock().expect("entered lock").take() {
                    entered.send(()).expect("publish blocked write");
                }
                self.release
                    .lock()
                    .expect("release lock")
                    .recv()
                    .expect("release blocked write");
            }
            write_file_atomically(FileWriteRequest::new(target.clone(), destination, contents))
                .map(drop)
                .map_err(io::Error::from)
        }
    }

    struct TestPaths {
        root: PathBuf,
        paths: AppPaths,
    }

    impl TestPaths {
        fn new(label: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "axial-user-config-snapshot-{label}-{}-{}",
                std::process::id(),
                NEXT_ROOT.fetch_add(1, Ordering::Relaxed)
            ));
            let config_dir = root.join("config");
            fs::create_dir_all(&config_dir).expect("create config root");
            Self {
                paths: AppPaths {
                    config_file: config_dir.join("config.json"),
                    instances_file: config_dir.join("instances.json"),
                    instances_dir: root.join("instances"),
                    music_dir: root.join("music"),
                    library_dir: root.join("library"),
                    config_dir,
                },
                root,
            }
        }

        fn snapshot_path(&self) -> PathBuf {
            self.paths.config_dir.join(USER_CONFIG_SNAPSHOT_FILE)
        }

        fn game_dir(&self, label: &str) -> PathBuf {
            self.root.join("games").join(label)
        }
    }

    impl Drop for TestPaths {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn instance(id: &str) -> Instance {
        crate::state::new_instance(
            id.to_string(),
            "Snapshot".to_string(),
            "1.21.1".to_string(),
            String::new(),
            String::new(),
        )
    }

    fn identity(instance: &Instance, index: usize) -> UserConfigSnapshotIdentity {
        UserConfigSnapshotIdentity {
            instance_id: instance.id.clone(),
            instance_created_at: instance.created_at.clone(),
            incarnation_fingerprint: ReconciliationIncarnationFingerprint::from_digest(
                test_fingerprint(index),
            ),
            inventory_fingerprint: ReconciliationInventoryFingerprint::from_digest(
                test_fingerprint(index + 1024),
            ),
            r3_operation_id: OperationId::new(format!("snapshot-operation-{index}")),
            captured_at: chrono::DateTime::parse_from_rfc3339(&format!(
                "2026-01-01T00:00:{:02}Z",
                index % 60
            ))
            .expect("fixed capture time"),
        }
    }

    fn test_fingerprint(index: usize) -> String {
        let digest = format!("{index:064x}")
            .as_bytes()
            .chunks(8)
            .map(|chunk| std::str::from_utf8(chunk).expect("hex is ASCII"))
            .collect::<Vec<_>>()
            .join(".");
        format!("sha256.{digest}")
    }

    async fn capture(fixture: &TestPaths, label: &str, bytes: &[u8]) -> UserConfigCapture {
        let game_dir = fixture.game_dir(label);
        fs::create_dir_all(game_dir.join("config")).expect("create game config");
        fs::write(game_dir.join("options.txt"), bytes).expect("write exact options");
        fs::write(game_dir.join("config/client.toml"), bytes).expect("write exact config");
        capture_user_config(game_dir).await.expect("capture config")
    }

    #[tokio::test]
    async fn snapshot_round_trip_is_strict_scope_bound_and_byte_exact() {
        let fixture = TestPaths::new("roundtrip");
        let instance = instance("0000000000000001");
        let exact = b"guiScale:3\nresourcePacks:[\"vanilla\"]\n";
        let store =
            UserConfigSnapshotStore::claim(&fixture.paths, std::slice::from_ref(&instance), true)
                .expect("claim snapshot store");
        let receipt = store
            .persist_before_core(
                identity(&instance, 1),
                capture(&fixture, "roundtrip", exact).await,
            )
            .await
            .expect("persist before core")
            .confirm();
        store.close().await.expect("close snapshot store");
        drop(store);

        let loaded =
            load_user_config_snapshot_file(&fixture.snapshot_path()).expect("load strict snapshot");
        assert_eq!(loaded.snapshots.len(), 1);
        let record = &loaded.snapshots[0];
        assert!(receipt.matches(record));
        assert_eq!(record.incarnation_fingerprint, test_fingerprint(1));
        assert_eq!(record.inventory_fingerprint, test_fingerprint(1025));
        for entry in &record.entries {
            if let UserConfigSnapshotEntry::File { bytes, .. } = entry {
                assert_eq!(
                    base64::engine::general_purpose::STANDARD
                        .decode(bytes)
                        .expect("decode stored bytes"),
                    exact
                );
            }
        }
        let encoded = fs::read_to_string(fixture.snapshot_path()).expect("read snapshot text");
        assert!(!encoded.contains(fixture.root.to_string_lossy().as_ref()));
        assert!(!encoded.contains("servers.dat"));
        assert!(!encoded.contains("resourcepacks"));

        let canonical = serde_json::to_value(&loaded).expect("encode loaded snapshot");
        let mut wrong_identity = canonical.clone();
        wrong_identity["snapshots"][0]["inventory_fingerprint"] =
            serde_json::Value::String("0".repeat(64));
        let mut legacy_schema = canonical.clone();
        legacy_schema["schema_version"] = serde_json::Value::from(0);
        let mut unknown_field = canonical;
        unknown_field["legacy"] = serde_json::Value::Bool(true);
        for rejected in [wrong_identity, legacy_schema, unknown_field] {
            fs::write(
                fixture.snapshot_path(),
                serde_json::to_vec(&rejected).expect("encode rejected snapshot"),
            )
            .expect("write rejected snapshot");
            assert_eq!(
                load_user_config_snapshot_file(&fixture.snapshot_path())
                    .err()
                    .expect("reject non-current or tampered snapshot")
                    .kind(),
                io::ErrorKind::InvalidData
            );
        }
        fs::write(
            fixture.snapshot_path(),
            vec![b' '; USER_CONFIG_SNAPSHOT_MAX_ENCODED_BYTES + 1],
        )
        .expect("write oversized store");
        assert_eq!(
            load_user_config_snapshot_file(&fixture.snapshot_path())
                .err()
                .expect("reject oversized encoded store")
                .kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[tokio::test]
    async fn retention_keeps_latest_per_instance_and_global_oldest_eight() {
        let fixture = TestPaths::new("retention");
        let instances = (0..9)
            .map(|index| instance(&format!("{index:016x}")))
            .collect::<Vec<_>>();
        let store = UserConfigSnapshotStore::claim(&fixture.paths, &instances, true)
            .expect("claim snapshot store");
        for (index, instance) in instances.iter().enumerate() {
            let receipt = store
                .persist_before_core(
                    identity(instance, index + 1),
                    capture(&fixture, &format!("retention-{index}"), b"exact\n").await,
                )
                .await
                .expect("persist retained snapshot");
            drop(receipt.confirm());
        }
        let replacement = identity(&instances[8], 10);
        let replacement_receipt = store
            .persist_before_core(
                replacement,
                capture(&fixture, "retention-replacement", b"replacement\n").await,
            )
            .await
            .expect("replace latest instance snapshot");
        drop(replacement_receipt.confirm());
        {
            let state = store
                .state
                .lock()
                .expect(USER_CONFIG_SNAPSHOT_LOCK_INVARIANT);
            assert_eq!(
                state.visible.snapshots.len(),
                USER_CONFIG_SNAPSHOT_MAX_RECORDS
            );
            assert!(
                state
                    .visible
                    .snapshots
                    .iter()
                    .all(|record| record.instance_id != instances[0].id)
            );
            assert_eq!(
                state
                    .visible
                    .snapshots
                    .iter()
                    .filter(|record| record.instance_id == instances[8].id)
                    .count(),
                1
            );
        }
        store.close().await.expect("close retained store");
    }

    #[tokio::test]
    async fn failed_pre_core_write_returns_promptly_and_close_retries_then_discards() {
        let fixture = TestPaths::new("fail-once");
        let instance = instance("0000000000000010");
        let backend = Arc::new(ControlledFileBackend::fail_once());
        let coordinator =
            PersistenceCoordinator::for_test(backend.clone(), Duration::ZERO, Duration::ZERO);
        let store = UserConfigSnapshotStore::claim_with_coordinator(
            &fixture.paths,
            std::slice::from_ref(&instance),
            true,
            coordinator,
        )
        .expect("claim controlled store");

        assert!(
            tokio::time::timeout(
                Duration::from_secs(1),
                store.persist_before_core(
                    identity(&instance, 1),
                    capture(&fixture, "fail-once", b"exact\n").await,
                ),
            )
            .await
            .expect("failed producer must not hang")
            .is_err()
        );
        store
            .close()
            .await
            .expect("retry exact capture then discard");
        assert_eq!(backend.attempts.load(Ordering::SeqCst), 3);
        assert!(
            load_user_config_snapshot_file(&fixture.snapshot_path())
                .expect("load compensated snapshot store")
                .snapshots
                .is_empty()
        );
    }

    #[tokio::test]
    async fn permanent_failure_never_pins_producer_or_close() {
        let fixture = TestPaths::new("permanent-failure");
        let instance = instance("0000000000000011");
        let backend = Arc::new(ControlledFileBackend::permanently_failing());
        let coordinator =
            PersistenceCoordinator::for_test(backend.clone(), Duration::ZERO, Duration::ZERO);
        let store = UserConfigSnapshotStore::claim_with_coordinator(
            &fixture.paths,
            std::slice::from_ref(&instance),
            true,
            coordinator,
        )
        .expect("claim permanently failing store");

        assert!(
            tokio::time::timeout(
                Duration::from_secs(1),
                store.persist_before_core(
                    identity(&instance, 1),
                    capture(&fixture, "permanent", b"exact\n").await,
                ),
            )
            .await
            .expect("producer returned within bound")
            .is_err()
        );
        tokio::time::timeout(Duration::from_secs(1), store.close())
            .await
            .expect("close returned within bound")
            .expect_err("close reports the one failed retry");
        assert_eq!(backend.attempts.load(Ordering::SeqCst), 2);

        backend.recover();
        store
            .close()
            .await
            .expect("later close completes obligations");
        assert_eq!(backend.attempts.load(Ordering::SeqCst), 4);
    }

    #[tokio::test]
    async fn cancelled_pre_core_publish_retains_discard_intent() {
        let fixture = TestPaths::new("cancelled-publish");
        let instance = instance("0000000000000012");
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let backend = Arc::new(BlockingFirstFileBackend {
            attempts: AtomicUsize::new(0),
            entered: Mutex::new(Some(entered_tx)),
            release: Mutex::new(release_rx),
        });
        let coordinator =
            PersistenceCoordinator::for_test(backend.clone(), Duration::ZERO, Duration::ZERO);
        let store = Arc::new(
            UserConfigSnapshotStore::claim_with_coordinator(
                &fixture.paths,
                std::slice::from_ref(&instance),
                true,
                coordinator,
            )
            .expect("claim blocking store"),
        );
        let capture = capture(&fixture, "cancelled", b"exact\n").await;
        let publishing = {
            let store = store.clone();
            let instance = instance.clone();
            tokio::spawn(async move {
                store
                    .persist_before_core(identity(&instance, 1), capture)
                    .await
            })
        };
        tokio::task::spawn_blocking(move || entered_rx.recv().expect("observe blocked write"))
            .await
            .expect("join entered observer");
        publishing.abort();
        release_tx.send(()).expect("release accepted write");
        assert!(publishing.await.is_err_and(|error| error.is_cancelled()));

        store
            .close()
            .await
            .expect("close removes capture whose receipt was never delivered");
        assert_eq!(backend.attempts.load(Ordering::SeqCst), 2);
        assert!(
            load_user_config_snapshot_file(&fixture.snapshot_path())
                .expect("load cancellation-compensated store")
                .snapshots
                .is_empty()
        );
    }

    #[tokio::test]
    async fn cancelled_after_persist_before_settlement_discards_on_close() {
        let fixture = TestPaths::new("cancelled-after-persist");
        let instance = instance("0000000000000016");
        let store = Arc::new(
            UserConfigSnapshotStore::claim(&fixture.paths, std::slice::from_ref(&instance), true)
                .expect("claim cancellation store"),
        );
        let capture = capture(&fixture, "cancelled-after-persist", b"exact\n").await;
        let (persisted_tx, persisted_rx) = tokio::sync::oneshot::channel();
        let publishing = {
            let store = store.clone();
            let instance = instance.clone();
            tokio::spawn(async move {
                let receipt = store
                    .persist_before_core(identity(&instance, 1), capture)
                    .await
                    .expect("persist before simulated Core");
                let _ = persisted_tx.send(());
                std::future::pending::<()>().await;
                drop(receipt);
            })
        };
        persisted_rx.await.expect("receipt became in flight");
        {
            let state = store
                .state
                .lock()
                .expect(USER_CONFIG_SNAPSHOT_LOCK_INVARIANT);
            assert_eq!(state.in_flight.len(), 1);
            assert!(state.pending_discards.is_empty());
        }
        assert_eq!(
            store
                .close()
                .await
                .expect_err("close refuses a live receipt")
                .kind(),
            io::ErrorKind::WouldBlock
        );
        publishing.abort();
        assert!(publishing.await.is_err_and(|error| error.is_cancelled()));
        {
            let state = store
                .state
                .lock()
                .expect(USER_CONFIG_SNAPSHOT_LOCK_INVARIANT);
            assert!(state.in_flight.is_empty());
            assert_eq!(state.pending_discards.len(), 1);
        }

        store
            .close()
            .await
            .expect("close persists cancellation discard");
        assert!(
            load_user_config_snapshot_file(&fixture.snapshot_path())
                .expect("load cancellation-cleaned store")
                .snapshots
                .is_empty()
        );
    }

    #[tokio::test]
    async fn instance_retirement_removes_exact_capture() {
        let fixture = TestPaths::new("retirement");
        let instance = instance("0000000000000013");
        let store =
            UserConfigSnapshotStore::claim(&fixture.paths, std::slice::from_ref(&instance), true)
                .expect("claim snapshot store");
        let receipt = store
            .persist_before_core(
                identity(&instance, 1),
                capture(&fixture, "retirement", b"exact\n").await,
            )
            .await
            .expect("persist exact capture");
        drop(receipt.confirm());
        store
            .retire_instance(instance.id.clone())
            .await
            .expect("retire exact instance");
        assert!(
            store
                .state
                .lock()
                .expect(USER_CONFIG_SNAPSHOT_LOCK_INVARIANT)
                .visible
                .snapshots
                .is_empty()
        );
        store.close().await.expect("close retired store");
    }

    #[tokio::test]
    async fn authoritative_restart_prunes_absent_and_stale_incarnations() {
        let fixture = TestPaths::new("restart-pruning");
        let current = instance("0000000000000014");
        let stale = instance("0000000000000015");
        let store =
            UserConfigSnapshotStore::claim(&fixture.paths, &[current.clone(), stale.clone()], true)
                .expect("claim initial store");
        for (index, instance) in [&current, &stale].into_iter().enumerate() {
            let receipt = store
                .persist_before_core(
                    identity(instance, index + 1),
                    capture(&fixture, &format!("restart-{index}"), b"exact\n").await,
                )
                .await
                .expect("persist restart fixture");
            drop(receipt.confirm());
        }
        store.close().await.expect("close initial store");
        drop(store);

        let mut replacement = current.clone();
        replacement.created_at = "2026-02-01T00:00:00Z".to_string();
        let pruned = UserConfigSnapshotStore::claim(
            &fixture.paths,
            std::slice::from_ref(&replacement),
            true,
        )
        .expect("claim authoritative restart");
        assert!(
            pruned
                .state
                .lock()
                .expect(USER_CONFIG_SNAPSHOT_LOCK_INVARIANT)
                .visible
                .snapshots
                .is_empty()
        );
        pruned
            .retain_successful_incarnations(&BTreeSet::new())
            .await
            .expect("startup reconciliation persists authoritative pruning");
        assert!(
            load_user_config_snapshot_file(&fixture.snapshot_path())
                .expect("read immediately persisted pruning")
                .snapshots
                .is_empty()
        );
        pruned.close().await.expect("persist restart pruning");
        drop(pruned);

        assert!(
            load_user_config_snapshot_file(&fixture.snapshot_path())
                .expect("load pruned restart store")
                .snapshots
                .is_empty()
        );
    }

    #[tokio::test]
    async fn corrupt_registry_fallback_preserves_snapshot_file_byte_for_byte() {
        let fixture = TestPaths::new("registry-fallback");
        let instance = instance("0000000000000017");
        let store =
            UserConfigSnapshotStore::claim(&fixture.paths, std::slice::from_ref(&instance), true)
                .expect("claim seed snapshot store");
        let receipt = store
            .persist_before_core(
                identity(&instance, 1),
                capture(&fixture, "registry-fallback", b"exact\n").await,
            )
            .await
            .expect("persist seed snapshot")
            .confirm();
        assert!(!receipt.armed);
        store.close().await.expect("close seed snapshot store");
        drop((receipt, store));
        let exact_snapshot = fs::read(fixture.snapshot_path()).expect("read seed snapshot bytes");

        fs::write(&fixture.paths.instances_file, b"{not valid registry")
            .expect("write corrupt instance registry");
        let startup = axial_config::InstanceStore::load_for_startup(fixture.paths.clone());
        assert!(!startup.store.mutation_allowed());
        let config = Arc::new(
            axial_config::ConfigStore::load_from(fixture.paths.clone())
                .expect("load fallback config"),
        );
        let state = crate::state::AppState::new(crate::state::AppStateInit {
            app_name: "Axial".to_string(),
            version: "test".to_string(),
            config,
            instances: Arc::new(startup.store),
            installs: Arc::new(crate::state::InstallStore::new()),
            sessions: Arc::new(crate::state::SessionStore::new()),
            performance: Arc::new(
                axial_performance::PerformanceManager::load_for_startup(&fixture.paths.config_dir)
                    .expect("load fallback performance"),
            ),
            startup_warnings: startup.warnings,
            frontend_dir: fixture.root.join("frontend"),
        });
        assert!(!state.instances.is_authoritative());
        state
            .reconcile_user_config_snapshot_startup()
            .await
            .expect("non-authoritative startup skips pruning");
        assert_eq!(
            fs::read(fixture.snapshot_path()).expect("read preserved snapshot bytes"),
            exact_snapshot
        );
        state
            .close_user_config_snapshots()
            .await
            .expect("close preserved snapshot store");
    }
}
