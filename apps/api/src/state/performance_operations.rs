use crate::execution::anchored_record::{AnchoredRecordDirectory, AnchoredRecordObservation};
use crate::execution::file::{DeleteFileRequest, delete_launcher_managed_file, file_fact};
#[cfg(test)]
use crate::execution::file::{
    FileWriteRequest, PromoteTempFileRequest, promote_temp_file, write_file_atomically,
};
use crate::execution::persistence::{
    AcceptedWrite, AtomicSnapshotWriter, PersistenceCoordinator, PersistenceOwnerLease,
    WriteUrgency,
};
use crate::execution::{ExecutionFact, ExecutionFactKind};
use crate::logging::timestamp_utc;
use crate::observability::{RedactionAudience, sanitize_public_diagnostic_text};
use crate::state::contracts::RollbackState;
use crate::state::ownership::{CurrentArtifact, classify_current_artifact};
use crate::state::persisted_state_load::{
    MAX_REJECTED_RESTART_RECORDS_PER_STORE, MAX_RESTART_RECORD_BYTES,
    PersistedStateRecordRejection, PersistedStateRecordStore, PersistedStateRejectedRecord,
};
use axial_config::AppPaths;
use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
#[cfg(test)]
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as SyncMutex, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex as AsyncMutex;
use tracing::warn;

pub const PERFORMANCE_OPERATION_ID_PREFIX: &str = "performance-install-";
pub const PERFORMANCE_COMMITTING_COMPLETE_STATE: &str = "committing_complete";
pub const PERFORMANCE_COMMITTING_FAILED_STATE: &str = "committing_failed";
pub const PERFORMANCE_EFFECT_STARTED_STATE: &str = "effect_started";
pub const PERFORMANCE_RESUME_BLOCKED_STATE: &str = "resume_blocked";
const MAX_OPERATION_ERROR_CHARS: usize = 160;
const MAX_OPERATION_FILENAME_STEM: usize = 96;
const MAX_RESUMABLE_OPERATIONS: usize = 16;
const MAX_RETAINED_TERMINAL_OPERATIONS: usize = 32;
const PERFORMANCE_OPERATION_LOCK_INVARIANT: &str =
    "performance operation records lock poisoned; in-memory and persisted state may diverge";

#[derive(Debug, thiserror::Error)]
pub enum PerformanceOperationStoreError {
    #[error("performance operation persistence failed: {0}")]
    Persistence(#[source] io::Error),
    #[error("performance operation has no failed critical transition to retry")]
    RetryUnavailable,
    #[error("performance operation has a failed critical transition that must be retried")]
    RetryRequired,
    #[error("performance operation does not exist")]
    MissingOperation,
    #[error("performance operation is already terminal with a different state")]
    TerminalMismatch,
    #[error("performance operation journal identity is invalid")]
    InvalidIdentity,
}

impl PerformanceOperationStoreError {
    pub const fn class(&self) -> &'static str {
        match self {
            Self::Persistence(_) => "persistence",
            Self::RetryUnavailable => "retry_unavailable",
            Self::RetryRequired => "retry_required",
            Self::MissingOperation => "missing_operation",
            Self::TerminalMismatch => "terminal_mismatch",
            Self::InvalidIdentity => "invalid_identity",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PerformanceOperationStartError {
    #[error("a performance operation is already queued for this instance")]
    Conflict,
    #[error("performance operation {operation_id} could not be started: {source}")]
    Store {
        operation_id: String,
        #[source]
        source: PerformanceOperationStoreError,
    },
}

impl PerformanceOperationStartError {
    pub fn operation_id(&self) -> Option<&str> {
        match self {
            Self::Conflict => None,
            Self::Store { operation_id, .. } => Some(operation_id),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PerformanceOperationPayload {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub game_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub loader: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rollback_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PerformanceOperationStatus {
    pub id: String,
    pub instance_id: String,
    pub action: String,
    pub payload: PerformanceOperationPayload,
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    #[serde(skip)]
    pub(crate) journal_identity: Option<PerformanceOperationJournalIdentity>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct PerformanceOperationJournalIdentity {
    pub action: String,
    pub target_id: String,
    pub rollback: RollbackState,
}

impl PerformanceOperationJournalIdentity {
    pub(crate) fn new(
        action: impl Into<String>,
        target_id: impl Into<String>,
        rollback: RollbackState,
    ) -> Self {
        Self {
            action: action.into(),
            target_id: target_id.into(),
            rollback,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedPerformanceOperationStatus {
    id: String,
    instance_id: String,
    action: String,
    payload: PerformanceOperationPayload,
    state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    created_at: String,
    updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    journal_identity: Option<PerformanceOperationJournalIdentity>,
}

impl From<PerformanceOperationStatus> for PersistedPerformanceOperationStatus {
    fn from(status: PerformanceOperationStatus) -> Self {
        Self {
            id: status.id,
            instance_id: status.instance_id,
            action: status.action,
            payload: status.payload,
            state: status.state,
            error: status.error,
            created_at: status.created_at,
            updated_at: status.updated_at,
            journal_identity: status.journal_identity,
        }
    }
}

impl From<PersistedPerformanceOperationStatus> for PerformanceOperationStatus {
    fn from(status: PersistedPerformanceOperationStatus) -> Self {
        Self {
            id: status.id,
            instance_id: status.instance_id,
            action: status.action,
            payload: status.payload,
            state: status.state,
            error: status.error,
            created_at: status.created_at,
            updated_at: status.updated_at,
            journal_identity: status.journal_identity,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PerformanceOperationConflict;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PerformanceOperationLoadIssueKind {
    DirectoryUnreadable,
    StatusUnreadable,
    StatusInvalid,
    UnsafeOperationId,
    MalformedOperationStatus,
    NonCanonicalFilename,
    DuplicateOperationId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PerformanceOperationLoadIssue {
    kind: PerformanceOperationLoadIssueKind,
    count: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PerformanceOperationRetentionIssueKind {
    WriterSettlement,
    Delete,
    BlockingTask,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PerformanceOperationRetentionIssue {
    pub operation_id: String,
    pub kind: PerformanceOperationRetentionIssueKind,
    pub facts: Vec<ExecutionFact>,
}

#[derive(Default)]
struct PerformanceOperationInner {
    operations: HashMap<String, PerformanceOperationStatus>,
    active_by_instance: HashMap<String, String>,
    starting_by_instance: HashMap<String, String>,
    reserved_operation_ids: HashSet<String>,
    pending_resume_ids: Vec<String>,
}

#[must_use]
pub(crate) struct PerformanceOperationIdReservation {
    operation_id: String,
    inner: Arc<RwLock<PerformanceOperationInner>>,
}

impl PerformanceOperationIdReservation {
    pub(crate) fn operation_id(&self) -> &str {
        &self.operation_id
    }
}

impl Drop for PerformanceOperationIdReservation {
    fn drop(&mut self) {
        self.inner
            .write()
            .expect(PERFORMANCE_OPERATION_LOCK_INVARIANT)
            .reserved_operation_ids
            .remove(&self.operation_id);
    }
}

#[derive(Default)]
struct PerformanceOperationLoadState {
    inner: PerformanceOperationInner,
    issues: Vec<PerformanceOperationLoadIssue>,
    rejected_records: Vec<PersistedStateRejectedRecord>,
}

struct PerformanceOperationPersistence {
    owner: PersistenceOwnerLease,
    storage_dir: PathBuf,
    writers: SyncMutex<HashMap<String, AtomicSnapshotWriter>>,
}

impl PerformanceOperationPersistence {
    fn claim(
        storage_dir: &Path,
        coordinator: PersistenceCoordinator,
    ) -> Result<Self, PerformanceOperationStoreError> {
        let owner = coordinator
            .claim_owner(storage_dir)
            .map_err(performance_operation_persistence_error)?;
        Ok(Self {
            owner,
            storage_dir: storage_dir.to_path_buf(),
            writers: SyncMutex::new(HashMap::new()),
        })
    }

    fn writer(
        &self,
        operation_id: &str,
    ) -> Result<AtomicSnapshotWriter, PerformanceOperationStoreError> {
        let mut writers = self
            .writers
            .lock()
            .expect(PERFORMANCE_OPERATION_LOCK_INVARIANT);
        if let Some(writer) = writers.get(operation_id) {
            return Ok(writer.clone());
        }
        let path = operation_path(&self.storage_dir, operation_id);
        let writer = self
            .owner
            .writer(&path, performance_operation_status_target(operation_id))
            .map_err(performance_operation_persistence_error)?;
        writers.insert(operation_id.to_string(), writer.clone());
        Ok(writer)
    }

    fn take_writer(
        &self,
        operation_id: &str,
    ) -> Result<AtomicSnapshotWriter, PerformanceOperationStoreError> {
        if let Some(writer) = self
            .writers
            .lock()
            .expect(PERFORMANCE_OPERATION_LOCK_INVARIANT)
            .remove(operation_id)
        {
            return Ok(writer);
        }
        self.owner
            .writer(
                operation_path(&self.storage_dir, operation_id),
                performance_operation_status_target(operation_id),
            )
            .map_err(performance_operation_persistence_error)
    }

    fn restore_writer(&self, operation_id: &str, writer: AtomicSnapshotWriter) {
        self.writers
            .lock()
            .expect(PERFORMANCE_OPERATION_LOCK_INVARIANT)
            .insert(operation_id.to_string(), writer);
    }

    async fn settle_writers(
        &self,
        excluded_ids: &HashSet<String>,
    ) -> Result<(), PerformanceOperationStoreError> {
        let mut writers = self
            .writers
            .lock()
            .expect(PERFORMANCE_OPERATION_LOCK_INVARIANT)
            .iter()
            .filter(|(operation_id, _)| !excluded_ids.contains(*operation_id))
            .map(|(operation_id, writer)| (operation_id.clone(), writer.clone()))
            .collect::<Vec<_>>();
        writers.sort_by(|left, right| left.0.cmp(&right.0));

        let mut first_error = None;
        for (_, writer) in writers {
            if let Err(error) = writer.settle().await
                && first_error.is_none()
            {
                first_error = Some(performance_operation_persistence_error(error));
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    #[cfg(test)]
    fn writer_count(&self) -> usize {
        self.writers
            .lock()
            .expect(PERFORMANCE_OPERATION_LOCK_INVARIANT)
            .len()
    }
}

pub struct PerformanceOperationStore {
    inner: Arc<RwLock<PerformanceOperationInner>>,
    mutation_gate: Arc<AsyncMutex<()>>,
    persistence: Option<Arc<PerformanceOperationPersistence>>,
    retry_candidates: Arc<SyncMutex<HashMap<String, PerformanceOperationStatus>>>,
    retention_issues: Arc<SyncMutex<HashMap<String, PerformanceOperationRetentionIssue>>>,
    load_issues: Vec<PerformanceOperationLoadIssue>,
}

pub(super) struct LoadedPerformanceOperationStore {
    store: PerformanceOperationStore,
    rejected_records: Vec<PersistedStateRejectedRecord>,
}

impl LoadedPerformanceOperationStore {
    pub(super) fn into_parts(
        self,
    ) -> (PerformanceOperationStore, Vec<PersistedStateRejectedRecord>) {
        (self.store, self.rejected_records)
    }

    #[cfg(test)]
    fn into_store(self) -> PerformanceOperationStore {
        self.store
    }
}

impl PerformanceOperationStore {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(PerformanceOperationInner::default())),
            mutation_gate: Arc::new(AsyncMutex::new(())),
            persistence: None,
            retry_candidates: Arc::new(SyncMutex::new(HashMap::new())),
            retention_issues: Arc::new(SyncMutex::new(HashMap::new())),
            load_issues: Vec::new(),
        }
    }

    #[cfg(test)]
    pub fn load_from_paths(paths: &AppPaths) -> Self {
        Self::try_load_from_paths(paths).unwrap_or_else(|error| {
            panic!("failed to initialize performance operation persistence: {error}")
        })
    }

    #[cfg(test)]
    pub fn try_load_from_paths(paths: &AppPaths) -> Result<Self, PerformanceOperationStoreError> {
        Self::try_load_from_paths_with_coordinator(paths, PersistenceCoordinator::global())
    }

    pub(super) fn load_from_paths_for_startup(paths: &AppPaths) -> LoadedPerformanceOperationStore {
        Self::try_load_from_paths_with_coordinator_for_startup(
            paths,
            PersistenceCoordinator::global(),
        )
        .unwrap_or_else(|error| {
            panic!("failed to initialize performance operation persistence: {error}")
        })
    }

    #[cfg(test)]
    pub(crate) fn try_load_from_paths_with_coordinator(
        paths: &AppPaths,
        coordinator: PersistenceCoordinator,
    ) -> Result<Self, PerformanceOperationStoreError> {
        Self::try_load_from_paths_with_coordinator_for_startup(paths, coordinator)
            .map(LoadedPerformanceOperationStore::into_store)
    }

    fn try_load_from_paths_with_coordinator_for_startup(
        paths: &AppPaths,
        coordinator: PersistenceCoordinator,
    ) -> Result<LoadedPerformanceOperationStore, PerformanceOperationStoreError> {
        let storage_dir = operation_dir(paths);
        let load_state = load_persisted_operation_inner(&storage_dir);
        let persistence = Arc::new(PerformanceOperationPersistence::claim(
            &storage_dir,
            coordinator,
        )?);
        let PerformanceOperationLoadState {
            inner,
            issues,
            rejected_records,
        } = load_state;
        let store = Self {
            inner: Arc::new(RwLock::new(inner)),
            mutation_gate: Arc::new(AsyncMutex::new(())),
            persistence: Some(persistence),
            retry_candidates: Arc::new(SyncMutex::new(HashMap::new())),
            retention_issues: Arc::new(SyncMutex::new(HashMap::new())),
            load_issues: issues,
        };
        Ok(LoadedPerformanceOperationStore {
            store,
            rejected_records,
        })
    }

    pub(crate) fn reserve_operation_id(&self) -> PerformanceOperationIdReservation {
        let operation_id = loop {
            let candidate = generate_performance_operation_id();
            let mut inner = self
                .inner
                .write()
                .expect(PERFORMANCE_OPERATION_LOCK_INVARIANT);
            if inner.operations.contains_key(&candidate)
                || inner
                    .starting_by_instance
                    .values()
                    .any(|operation_id| operation_id == &candidate)
                || !inner.reserved_operation_ids.insert(candidate.clone())
            {
                continue;
            }
            break candidate;
        };
        PerformanceOperationIdReservation {
            operation_id,
            inner: self.inner.clone(),
        }
    }

    #[cfg(test)]
    pub async fn start(
        &self,
        instance_id: String,
        action: String,
        payload: PerformanceOperationPayload,
    ) -> Result<PerformanceOperationStatus, PerformanceOperationStartError> {
        self.start_internal(
            self.reserve_operation_id(),
            instance_id,
            action,
            payload,
            None,
        )
        .await
    }

    #[cfg(test)]
    pub(crate) async fn start_with_identity(
        &self,
        instance_id: String,
        action: String,
        payload: PerformanceOperationPayload,
        journal_identity: PerformanceOperationJournalIdentity,
    ) -> Result<PerformanceOperationStatus, PerformanceOperationStartError> {
        self.start_reserved_with_identity(
            self.reserve_operation_id(),
            instance_id,
            action,
            payload,
            journal_identity,
        )
        .await
    }

    pub(crate) async fn start_reserved_with_identity(
        &self,
        reservation: PerformanceOperationIdReservation,
        instance_id: String,
        action: String,
        payload: PerformanceOperationPayload,
        journal_identity: PerformanceOperationJournalIdentity,
    ) -> Result<PerformanceOperationStatus, PerformanceOperationStartError> {
        self.start_internal(
            reservation,
            instance_id,
            action,
            payload,
            Some(journal_identity),
        )
        .await
    }

    async fn start_internal(
        &self,
        reservation: PerformanceOperationIdReservation,
        instance_id: String,
        action: String,
        payload: PerformanceOperationPayload,
        journal_identity: Option<PerformanceOperationJournalIdentity>,
    ) -> Result<PerformanceOperationStatus, PerformanceOperationStartError> {
        let mutation = self.mutation_gate.clone().lock_owned().await;
        let operation_id = reservation.operation_id().to_string();
        if journal_identity
            .as_ref()
            .is_some_and(|identity| !valid_journal_identity(identity, &action))
        {
            return Err(PerformanceOperationStartError::Store {
                operation_id,
                source: PerformanceOperationStoreError::InvalidIdentity,
            });
        }
        let status = {
            let inner = self
                .inner
                .read()
                .expect(PERFORMANCE_OPERATION_LOCK_INVARIANT);
            if !inner.reserved_operation_ids.contains(&operation_id)
                || inner.operations.contains_key(&operation_id)
            {
                return Err(PerformanceOperationStartError::Store {
                    operation_id,
                    source: PerformanceOperationStoreError::InvalidIdentity,
                });
            }
            if let Some(existing_id) = inner.active_by_instance.get(&instance_id)
                && inner
                    .operations
                    .get(existing_id)
                    .map(|status| is_non_terminal(&status.state))
                    .unwrap_or(false)
            {
                return Err(PerformanceOperationStartError::Conflict);
            }
            if inner.starting_by_instance.contains_key(&instance_id) {
                return Err(PerformanceOperationStartError::Conflict);
            }
            let now = timestamp_utc();
            PerformanceOperationStatus {
                id: operation_id,
                instance_id: instance_id.clone(),
                action,
                payload,
                state: "queued".to_string(),
                error: None,
                created_at: now.clone(),
                updated_at: now,
                journal_identity,
            }
        };
        self.inner
            .write()
            .expect(PERFORMANCE_OPERATION_LOCK_INVARIANT)
            .starting_by_instance
            .insert(instance_id, status.id.clone());
        if let Err(source) = self.commit_transition(status.clone(), mutation).await {
            if !self.has_retry_candidate(&status.id) {
                let mut inner = self
                    .inner
                    .write()
                    .expect(PERFORMANCE_OPERATION_LOCK_INVARIANT);
                if inner
                    .starting_by_instance
                    .get(&status.instance_id)
                    .is_some_and(|id| id == &status.id)
                {
                    inner.starting_by_instance.remove(&status.instance_id);
                }
            }
            return Err(PerformanceOperationStartError::Store {
                operation_id: status.id,
                source,
            });
        }
        Ok(status)
    }

    pub async fn take_pending_resumable_operations(&self) -> Vec<PerformanceOperationStatus> {
        let _mutation = self.mutation_gate.lock().await;
        prune_terminal_operations(
            self.inner.clone(),
            self.persistence.clone(),
            self.retry_candidates.clone(),
            self.retention_issues.clone(),
        )
        .await;
        let mut inner = self
            .inner
            .write()
            .expect(PERFORMANCE_OPERATION_LOCK_INVARIANT);
        let take_count = inner.pending_resume_ids.len().min(MAX_RESUMABLE_OPERATIONS);
        let ids = inner
            .pending_resume_ids
            .drain(..take_count)
            .collect::<Vec<_>>();
        ids.into_iter()
            .filter_map(|id| inner.operations.get(&id).cloned())
            .filter(|status| is_non_terminal(&status.state))
            .collect()
    }

    pub(crate) fn has_pending_resumable_operations(&self) -> bool {
        !self
            .inner
            .read()
            .expect(PERFORMANCE_OPERATION_LOCK_INVARIANT)
            .pending_resume_ids
            .is_empty()
    }

    pub(crate) fn has_reconciliation_obligation(&self, operation_id: &str) -> bool {
        if !is_safe_operation_id(operation_id) {
            return false;
        }
        let in_memory = {
            let inner = self
                .inner
                .read()
                .expect(PERFORMANCE_OPERATION_LOCK_INVARIANT);
            inner.operations.contains_key(operation_id)
                || inner
                    .starting_by_instance
                    .values()
                    .any(|candidate| candidate == operation_id)
                || inner.reserved_operation_ids.contains(operation_id)
        };
        in_memory
            || self
                .retry_candidates
                .lock()
                .expect(PERFORMANCE_OPERATION_LOCK_INVARIANT)
                .contains_key(operation_id)
    }

    pub async fn get(&self, id: &str) -> Option<PerformanceOperationStatus> {
        if !is_safe_operation_id(id) {
            return None;
        }
        self.inner
            .read()
            .expect(PERFORMANCE_OPERATION_LOCK_INVARIANT)
            .operations
            .get(id)
            .cloned()
    }

    pub(crate) fn list(&self) -> Vec<PerformanceOperationStatus> {
        self.inner
            .read()
            .expect(PERFORMANCE_OPERATION_LOCK_INVARIANT)
            .operations
            .values()
            .cloned()
            .collect()
    }

    pub(crate) fn load_issue_count(&self) -> usize {
        self.load_issues
            .iter()
            .map(|issue| issue.count)
            .fold(0usize, usize::saturating_add)
    }

    pub async fn current_or_latest_for_instance(
        &self,
        instance_id: &str,
    ) -> Option<PerformanceOperationStatus> {
        let instance_id = instance_id.trim();
        if instance_id.is_empty() {
            return None;
        }

        let inner = self
            .inner
            .read()
            .expect(PERFORMANCE_OPERATION_LOCK_INVARIANT);
        if let Some(active_id) = inner.active_by_instance.get(instance_id)
            && let Some(status) = inner.operations.get(active_id)
            && is_non_terminal(&status.state)
        {
            return Some(status.clone());
        }

        inner
            .operations
            .values()
            .filter(|status| status.instance_id == instance_id)
            .max_by(compare_operation_recency)
            .cloned()
    }

    pub async fn record_progress(
        &self,
        id: &str,
        state: &str,
    ) -> Result<(), PerformanceOperationStoreError> {
        let _mutation = self.mutation_gate.lock().await;
        let status = {
            let inner = self
                .inner
                .read()
                .expect(PERFORMANCE_OPERATION_LOCK_INVARIANT);
            let Some(status) = inner.operations.get(id) else {
                return Err(PerformanceOperationStoreError::MissingOperation);
            };
            if !is_non_terminal(&status.state) {
                return Err(PerformanceOperationStoreError::TerminalMismatch);
            }
            let mut status = status.clone();
            status.state = state.to_string();
            status.error = None;
            status.updated_at = timestamp_utc();
            status
        };
        self.accept_progress(status)?;
        Ok(())
    }

    pub async fn record_effect_started(
        &self,
        id: &str,
    ) -> Result<(), PerformanceOperationStoreError> {
        self.record_critical_state(id, PERFORMANCE_EFFECT_STARTED_STATE, None)
            .await
    }

    pub async fn record_committing_complete(
        &self,
        id: &str,
    ) -> Result<(), PerformanceOperationStoreError> {
        self.record_critical_state(id, PERFORMANCE_COMMITTING_COMPLETE_STATE, None)
            .await
    }

    pub async fn record_committing_failed(
        &self,
        id: &str,
        error: &str,
    ) -> Result<(), PerformanceOperationStoreError> {
        self.record_critical_state(
            id,
            PERFORMANCE_COMMITTING_FAILED_STATE,
            Some(sanitize_operation_error(error)),
        )
        .await
    }

    pub async fn record_complete(&self, id: &str) -> Result<(), PerformanceOperationStoreError> {
        self.record_critical_state(id, "complete", None).await
    }

    pub async fn record_failed(
        &self,
        id: &str,
        error: &str,
    ) -> Result<(), PerformanceOperationStoreError> {
        self.record_critical_state(id, "failed", Some(sanitize_operation_error(error)))
            .await
    }

    pub(crate) async fn record_reconciliation_failed(
        &self,
        id: &str,
        error: &str,
        action: &str,
    ) -> Result<(), PerformanceOperationStoreError> {
        if !matches!(action, "install" | "remove" | "rollback") {
            return Err(PerformanceOperationStoreError::InvalidIdentity);
        }
        let mutation = self.mutation_gate.clone().lock_owned().await;
        let message = sanitize_operation_error(error);
        let status = {
            let inner = self
                .inner
                .read()
                .expect(PERFORMANCE_OPERATION_LOCK_INVARIANT);
            let Some(status) = inner.operations.get(id) else {
                return Err(PerformanceOperationStoreError::MissingOperation);
            };
            if status.state == "failed"
                && status.error.as_deref() == Some(message.as_str())
                && status.action == action
                && status.journal_identity.as_ref().is_some_and(|identity| {
                    identity.action == action
                        && identity.target_id == "performance_reconciliation"
                        && identity.rollback == RollbackState::Unavailable
                })
            {
                return Ok(());
            }
            let mut status = status.clone();
            status.action = action.to_string();
            status.state = "failed".to_string();
            status.error = Some(message);
            status.updated_at = timestamp_utc();
            status.journal_identity = Some(PerformanceOperationJournalIdentity::new(
                action,
                "performance_reconciliation",
                RollbackState::Unavailable,
            ));
            status
        };
        self.commit_transition(status, mutation).await
    }

    async fn record_critical_state(
        &self,
        id: &str,
        state: &str,
        error: Option<String>,
    ) -> Result<(), PerformanceOperationStoreError> {
        let mutation = self.mutation_gate.clone().lock_owned().await;
        let status = {
            let inner = self
                .inner
                .read()
                .expect(PERFORMANCE_OPERATION_LOCK_INVARIANT);
            let Some(status) = inner.operations.get(id) else {
                return Err(PerformanceOperationStoreError::MissingOperation);
            };
            if !is_non_terminal(&status.state) {
                let requested_error = error.as_deref().map(sanitize_operation_error);
                if status.state == state && status.error == requested_error {
                    return Ok(());
                }
                return Err(PerformanceOperationStoreError::TerminalMismatch);
            }
            let mut status = status.clone();
            status.state = state.to_string();
            status.error = error;
            status.updated_at = timestamp_utc();
            status
        };
        self.commit_transition(status, mutation).await
    }

    pub async fn retry_critical(&self, id: &str) -> Result<(), PerformanceOperationStoreError> {
        let mutation = self.mutation_gate.clone().lock_owned().await;
        let (result, mutation) = self.retry_critical_holding_gate(id, mutation).await;
        drop(mutation);
        result
    }

    async fn retry_critical_holding_gate(
        &self,
        id: &str,
        mutation: tokio::sync::OwnedMutexGuard<()>,
    ) -> (
        Result<(), PerformanceOperationStoreError>,
        Option<tokio::sync::OwnedMutexGuard<()>>,
    ) {
        let candidate = match self
            .retry_candidates
            .lock()
            .expect(PERFORMANCE_OPERATION_LOCK_INVARIANT)
            .get(id)
            .cloned()
        {
            Some(candidate) => candidate,
            None => {
                return (
                    Err(PerformanceOperationStoreError::RetryUnavailable),
                    Some(mutation),
                );
            }
        };
        let Some(persistence) = &self.persistence else {
            apply_status_transition(
                &mut self
                    .inner
                    .write()
                    .expect(PERFORMANCE_OPERATION_LOCK_INVARIANT),
                candidate,
            );
            self.retry_candidates
                .lock()
                .expect(PERFORMANCE_OPERATION_LOCK_INVARIANT)
                .remove(id);
            return (Ok(()), Some(mutation));
        };
        let writer = match persistence.writer(id) {
            Ok(writer) => writer,
            Err(error) => return (Err(error), Some(mutation)),
        };
        let ticket = match writer
            .retry()
            .map_err(performance_operation_persistence_error)
        {
            Ok(ticket) => ticket,
            Err(error) => return (Err(error), Some(mutation)),
        };
        self.await_commit_holding_gate(candidate, ticket, mutation)
            .await
    }

    pub(crate) fn has_retry_candidate(&self, id: &str) -> bool {
        self.retry_candidates
            .lock()
            .expect(PERFORMANCE_OPERATION_LOCK_INVARIANT)
            .contains_key(id)
    }

    pub fn retention_issues(&self) -> Vec<PerformanceOperationRetentionIssue> {
        let mut issues = self
            .retention_issues
            .lock()
            .expect(PERFORMANCE_OPERATION_LOCK_INVARIANT)
            .values()
            .cloned()
            .collect::<Vec<_>>();
        issues.sort_by(|left, right| left.operation_id.cmp(&right.operation_id));
        issues
    }

    pub async fn retry_terminal_retention(&self) -> Vec<PerformanceOperationRetentionIssue> {
        let _mutation = self.mutation_gate.lock().await;
        prune_terminal_operations(
            self.inner.clone(),
            self.persistence.clone(),
            self.retry_candidates.clone(),
            self.retention_issues.clone(),
        )
        .await;
        self.retention_issues()
    }

    fn retry_candidate_ids(&self) -> Vec<String> {
        self.retry_candidates
            .lock()
            .expect(PERFORMANCE_OPERATION_LOCK_INVARIANT)
            .keys()
            .cloned()
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn retry_candidate_ids_for_test(&self) -> Vec<String> {
        self.retry_candidate_ids()
    }

    pub async fn flush(&self) -> Result<(), PerformanceOperationStoreError> {
        let _mutation = self.mutation_gate.lock().await;
        prune_terminal_operations(
            self.inner.clone(),
            self.persistence.clone(),
            self.retry_candidates.clone(),
            self.retention_issues.clone(),
        )
        .await;
        if let Some(persistence) = &self.persistence {
            persistence
                .owner
                .flush()
                .await
                .map_err(performance_operation_persistence_error)?;
        }
        if !self.retention_issues().is_empty() {
            return Err(PerformanceOperationStoreError::Persistence(
                io::Error::other("performance operation terminal retention cleanup is pending"),
            ));
        }
        Ok(())
    }

    pub async fn close(&self) -> Result<(), PerformanceOperationStoreError> {
        let mut mutation = self.mutation_gate.clone().lock_owned().await;
        let mut retry_ids = self.retry_candidate_ids();
        retry_ids.sort();
        let mut first_retry_error = None;
        for id in retry_ids {
            let (result, next_mutation) = self.retry_critical_holding_gate(&id, mutation).await;
            let Some(next_mutation) = next_mutation else {
                return result;
            };
            mutation = next_mutation;
            if let Err(error) = result
                && first_retry_error.is_none()
            {
                first_retry_error = Some(error);
            }
        }
        prune_terminal_operations(
            self.inner.clone(),
            self.persistence.clone(),
            self.retry_candidates.clone(),
            self.retention_issues.clone(),
        )
        .await;
        let retry_ids = self
            .retry_candidates
            .lock()
            .expect(PERFORMANCE_OPERATION_LOCK_INVARIANT)
            .keys()
            .cloned()
            .collect::<HashSet<_>>();
        if let Some(persistence) = &self.persistence
            && let Err(error) = persistence.settle_writers(&retry_ids).await
            && first_retry_error.is_none()
        {
            first_retry_error = Some(error);
        }
        if !self.retention_issues().is_empty() {
            first_retry_error.get_or_insert_with(|| {
                PerformanceOperationStoreError::Persistence(io::Error::other(
                    "performance operation terminal retention cleanup is pending",
                ))
            });
        }
        if let Some(error) = first_retry_error {
            return Err(error);
        }
        if let Some(persistence) = &self.persistence {
            persistence
                .owner
                .close()
                .await
                .map_err(performance_operation_persistence_error)?;
        }
        drop(mutation);
        Ok(())
    }

    fn accept_progress(
        &self,
        status: PerformanceOperationStatus,
    ) -> Result<(), PerformanceOperationStoreError> {
        if self
            .retry_candidates
            .lock()
            .expect(PERFORMANCE_OPERATION_LOCK_INVARIANT)
            .contains_key(&status.id)
        {
            return Err(PerformanceOperationStoreError::RetryRequired);
        }
        if let Some(persistence) = &self.persistence {
            persistence
                .writer(&status.id)?
                .accept(status.clone(), WriteUrgency::Debounced, encode_status)
                .map_err(performance_operation_persistence_error)?;
        }
        apply_status_transition(
            &mut self
                .inner
                .write()
                .expect(PERFORMANCE_OPERATION_LOCK_INVARIANT),
            status,
        );
        Ok(())
    }

    async fn commit_transition(
        &self,
        status: PerformanceOperationStatus,
        mutation: tokio::sync::OwnedMutexGuard<()>,
    ) -> Result<(), PerformanceOperationStoreError> {
        if self
            .retry_candidates
            .lock()
            .expect(PERFORMANCE_OPERATION_LOCK_INVARIANT)
            .contains_key(&status.id)
        {
            return Err(PerformanceOperationStoreError::RetryRequired);
        }
        let Some(persistence) = &self.persistence else {
            apply_status_transition(
                &mut self
                    .inner
                    .write()
                    .expect(PERFORMANCE_OPERATION_LOCK_INVARIANT),
                status,
            );
            prune_terminal_operations(
                self.inner.clone(),
                None,
                self.retry_candidates.clone(),
                self.retention_issues.clone(),
            )
            .await;
            drop(mutation);
            return Ok(());
        };
        let ticket = persistence
            .writer(&status.id)?
            .accept(status.clone(), WriteUrgency::Immediate, encode_status)
            .map_err(performance_operation_persistence_error)?;
        self.await_commit(status, ticket, mutation).await
    }

    async fn await_commit(
        &self,
        status: PerformanceOperationStatus,
        ticket: AcceptedWrite,
        mutation: tokio::sync::OwnedMutexGuard<()>,
    ) -> Result<(), PerformanceOperationStoreError> {
        let (result, mutation) = self
            .await_commit_holding_gate(status, ticket, mutation)
            .await;
        drop(mutation);
        result
    }

    async fn await_commit_holding_gate(
        &self,
        status: PerformanceOperationStatus,
        ticket: AcceptedWrite,
        mutation: tokio::sync::OwnedMutexGuard<()>,
    ) -> (
        Result<(), PerformanceOperationStoreError>,
        Option<tokio::sync::OwnedMutexGuard<()>>,
    ) {
        let inner = self.inner.clone();
        let retry_candidates = self.retry_candidates.clone();
        let persistence = self.persistence.clone();
        let retention_issues = self.retention_issues.clone();
        let operation_id = status.id.clone();
        let (completed_tx, completed_rx) = tokio::sync::oneshot::channel();
        ticket.observe_async(move |result| async move {
            let result = match result {
                Ok(_) => {
                    let is_terminal = !is_non_terminal(&status.state);
                    apply_status_transition(
                        &mut inner.write().expect(PERFORMANCE_OPERATION_LOCK_INVARIANT),
                        status,
                    );
                    retry_candidates
                        .lock()
                        .expect(PERFORMANCE_OPERATION_LOCK_INVARIANT)
                        .remove(&operation_id);
                    if is_terminal {
                        prune_terminal_operations(
                            inner,
                            persistence,
                            retry_candidates,
                            retention_issues,
                        )
                        .await;
                    }
                    Ok(())
                }
                Err(error) => {
                    retry_candidates
                        .lock()
                        .expect(PERFORMANCE_OPERATION_LOCK_INVARIANT)
                        .insert(operation_id, status);
                    Err(performance_operation_persistence_error(error))
                }
            };
            let _ = completed_tx.send((result, mutation));
        });
        match completed_rx.await {
            Ok((result, mutation)) => (result, Some(mutation)),
            Err(_) => (
                Err(PerformanceOperationStoreError::Persistence(
                    io::Error::other("performance operation commit observer stopped"),
                )),
                None,
            ),
        }
    }
}

impl Default for PerformanceOperationStore {
    fn default() -> Self {
        Self::new()
    }
}

fn apply_status_transition(
    inner: &mut PerformanceOperationInner,
    status: PerformanceOperationStatus,
) {
    let operation_id = status.id.clone();
    let instance_id = status.instance_id.clone();
    if inner
        .starting_by_instance
        .get(&instance_id)
        .is_some_and(|starting_id| starting_id == &operation_id)
    {
        inner.starting_by_instance.remove(&instance_id);
    }
    if is_non_terminal(&status.state) && !instance_id.trim().is_empty() {
        match inner.active_by_instance.get(&instance_id) {
            None => {
                inner
                    .active_by_instance
                    .insert(instance_id.clone(), operation_id.clone());
            }
            Some(active_id) if active_id == &operation_id => {}
            Some(_) => {}
        }
    } else if inner
        .active_by_instance
        .get(&instance_id)
        .is_some_and(|active_id| active_id == &operation_id)
    {
        inner.active_by_instance.remove(&instance_id);
    }
    inner.operations.insert(operation_id, status);
}

async fn prune_terminal_operations(
    inner: Arc<RwLock<PerformanceOperationInner>>,
    persistence: Option<Arc<PerformanceOperationPersistence>>,
    retry_candidates: Arc<SyncMutex<HashMap<String, PerformanceOperationStatus>>>,
    retention_issues: Arc<SyncMutex<HashMap<String, PerformanceOperationRetentionIssue>>>,
) {
    let retry_ids = retry_candidates
        .lock()
        .expect(PERFORMANCE_OPERATION_LOCK_INVARIANT)
        .keys()
        .cloned()
        .collect::<HashSet<_>>();
    let candidates = {
        let inner = inner.read().expect(PERFORMANCE_OPERATION_LOCK_INVARIANT);
        terminal_prune_candidates(&inner, &retry_ids)
    };
    for status in candidates {
        prune_terminal_operation(
            &status,
            inner.clone(),
            persistence.clone(),
            retention_issues.clone(),
        )
        .await;
    }
}

fn terminal_prune_candidates(
    inner: &PerformanceOperationInner,
    retry_ids: &HashSet<String>,
) -> Vec<PerformanceOperationStatus> {
    let mut terminals = inner
        .operations
        .values()
        .filter(|status| !is_non_terminal(&status.state) && !retry_ids.contains(&status.id))
        .collect::<Vec<_>>();
    terminals.sort_by(compare_operation_recency);
    terminals.reverse();

    let mut retained_ids = HashSet::new();
    let mut retained_instances = HashSet::new();
    for status in &terminals {
        if retained_ids.len() >= MAX_RETAINED_TERMINAL_OPERATIONS {
            break;
        }
        if retained_instances.insert(status.instance_id.clone()) {
            retained_ids.insert(status.id.clone());
        }
    }
    for status in &terminals {
        if retained_ids.len() >= MAX_RETAINED_TERMINAL_OPERATIONS {
            break;
        }
        retained_ids.insert(status.id.clone());
    }

    let mut candidates = terminals
        .into_iter()
        .filter(|status| !retained_ids.contains(&status.id))
        .cloned()
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| compare_operation_recency(&left, &right));
    candidates
}

async fn prune_terminal_operation(
    status: &PerformanceOperationStatus,
    inner: Arc<RwLock<PerformanceOperationInner>>,
    persistence: Option<Arc<PerformanceOperationPersistence>>,
    retention_issues: Arc<SyncMutex<HashMap<String, PerformanceOperationRetentionIssue>>>,
) {
    let operation_id = status.id.clone();
    let target = performance_operation_status_target(&operation_id);
    if let Some(persistence) = persistence {
        let writer = match persistence.take_writer(&operation_id) {
            Ok(writer) => writer,
            Err(_) => {
                record_retention_issue(
                    &retention_issues,
                    &operation_id,
                    PerformanceOperationRetentionIssueKind::WriterSettlement,
                    vec![file_fact(
                        ExecutionFactKind::PrimitiveRefused,
                        None,
                        &target,
                    )],
                );
                return;
            }
        };
        if writer.settle().await.is_err() {
            persistence.restore_writer(&operation_id, writer);
            record_retention_issue(
                &retention_issues,
                &operation_id,
                PerformanceOperationRetentionIssueKind::WriterSettlement,
                vec![file_fact(
                    ExecutionFactKind::PrimitiveRefused,
                    None,
                    &target,
                )],
            );
            return;
        }

        let path = operation_path(&persistence.storage_dir, &operation_id);
        let delete_target = target.clone();
        let delete = tokio::task::spawn_blocking(move || {
            delete_launcher_managed_file(DeleteFileRequest::new(delete_target, &path))
        })
        .await;
        match delete {
            Ok(Ok(_)) => drop(writer),
            Ok(Err(error)) => {
                let facts = error.facts.clone();
                persistence.restore_writer(&operation_id, writer);
                record_retention_issue(
                    &retention_issues,
                    &operation_id,
                    PerformanceOperationRetentionIssueKind::Delete,
                    facts,
                );
                return;
            }
            Err(_) => {
                persistence.restore_writer(&operation_id, writer);
                record_retention_issue(
                    &retention_issues,
                    &operation_id,
                    PerformanceOperationRetentionIssueKind::BlockingTask,
                    vec![file_fact(
                        ExecutionFactKind::PrimitiveRefused,
                        None,
                        &target,
                    )],
                );
                return;
            }
        }
    }

    let mut inner = inner.write().expect(PERFORMANCE_OPERATION_LOCK_INVARIANT);
    if inner.operations.get(&operation_id) == Some(status) {
        inner.operations.remove(&operation_id);
        inner.pending_resume_ids.retain(|id| id != &operation_id);
    }
    retention_issues
        .lock()
        .expect(PERFORMANCE_OPERATION_LOCK_INVARIANT)
        .remove(&operation_id);
}

fn record_retention_issue(
    retention_issues: &SyncMutex<HashMap<String, PerformanceOperationRetentionIssue>>,
    operation_id: &str,
    kind: PerformanceOperationRetentionIssueKind,
    facts: Vec<ExecutionFact>,
) {
    retention_issues
        .lock()
        .expect(PERFORMANCE_OPERATION_LOCK_INVARIANT)
        .insert(
            operation_id.to_string(),
            PerformanceOperationRetentionIssue {
                operation_id: operation_id.to_string(),
                kind,
                facts,
            },
        );
}

fn encode_status(status: PerformanceOperationStatus) -> io::Result<Vec<u8>> {
    serde_json::to_vec_pretty(&PersistedPerformanceOperationStatus::from(status))
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn performance_operation_persistence_error(
    error: crate::execution::persistence::PersistenceError,
) -> PerformanceOperationStoreError {
    PerformanceOperationStoreError::Persistence(error.into())
}

fn load_persisted_operation_inner(storage_dir: &Path) -> PerformanceOperationLoadState {
    let mut load_state = PerformanceOperationLoadState::default();
    let mut candidates = HashMap::<String, LoadedPerformanceOperationRecord>::new();
    let mut conflicting_ids = HashSet::<String>::new();
    let mut logical_occurrences = HashMap::<String, usize>::new();
    let mut deferred_identity_rejections =
        BTreeMap::<String, (String, PersistedStateRecordRejection)>::new();
    let mut rejected_records = BTreeMap::<String, PersistedStateRecordRejection>::new();
    let directory = match AnchoredRecordDirectory::open(storage_dir) {
        Ok(directory) => directory,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return load_state,
        Err(error) => {
            warn!(
                error_kind = ?error.kind(),
                "failed to read performance operation status directory"
            );
            record_load_issue(
                &mut load_state.issues,
                PerformanceOperationLoadIssueKind::DirectoryUnreadable,
            );
            return load_state;
        }
    };

    let mut names = match directory.names() {
        Ok(names) => names,
        Err(error) => {
            warn!(
                error_kind = ?error.kind(),
                "failed to enumerate anchored performance operation status directory"
            );
            record_load_issue(
                &mut load_state.issues,
                PerformanceOperationLoadIssueKind::DirectoryUnreadable,
            );
            return load_state;
        }
    };
    names.retain(|name| {
        Path::new(name).extension().and_then(|value| value.to_str()) == Some("json")
    });
    names.sort();

    for name in names {
        let physical_id = canonical_operation_id_from_name(&name);
        let observation = match directory.read(&name, MAX_RESTART_RECORD_BYTES) {
            Ok(observation) => observation,
            Err(error) => {
                warn!(
                    error_kind = ?error.kind(),
                    "failed to open or read anchored performance operation status"
                );
                record_load_issue(
                    &mut load_state.issues,
                    PerformanceOperationLoadIssueKind::StatusUnreadable,
                );
                continue;
            }
        };
        if observation.is_oversized() {
            record_load_issue(
                &mut load_state.issues,
                PerformanceOperationLoadIssueKind::StatusInvalid,
            );
            if let Some(physical_id) = physical_id {
                rejected_records.insert(
                    safe_operation_filename(&physical_id),
                    PersistedStateRecordRejection::Oversized,
                );
            }
            continue;
        }
        let mut status = match serde_json::from_slice::<PersistedPerformanceOperationStatus>(
            observation
                .bytes()
                .expect("non-oversized anchored observation has bytes"),
        ) {
            Ok(status) => PerformanceOperationStatus::from(status),
            Err(error) => {
                warn!(error = %error, "failed to decode performance operation status");
                record_load_issue(
                    &mut load_state.issues,
                    PerformanceOperationLoadIssueKind::StatusInvalid,
                );
                if let Some(physical_id) = physical_id {
                    rejected_records.insert(
                        safe_operation_filename(&physical_id),
                        PersistedStateRecordRejection::InvalidSchema,
                    );
                }
                continue;
            }
        };
        if !is_safe_operation_id(&status.id) {
            warn!("skipping persisted performance operation with unsafe id");
            record_load_issue(
                &mut load_state.issues,
                PerformanceOperationLoadIssueKind::UnsafeOperationId,
            );
            if let Some(physical_id) = physical_id {
                rejected_records.insert(
                    safe_operation_filename(&physical_id),
                    PersistedStateRecordRejection::InvalidIdentity,
                );
            }
            continue;
        }
        let occurrence_count = logical_occurrences.entry(status.id.clone()).or_default();
        *occurrence_count = occurrence_count.saturating_add(1);
        if physical_id.as_deref() != Some(status.id.as_str()) {
            record_load_issue(
                &mut load_state.issues,
                PerformanceOperationLoadIssueKind::NonCanonicalFilename,
            );
            conflicting_ids.insert(status.id.clone());
            if let Some(physical_id) = physical_id {
                deferred_identity_rejections.insert(
                    safe_operation_filename(&physical_id),
                    (status.id, PersistedStateRecordRejection::InvalidIdentity),
                );
            }
            continue;
        }
        if let Some(error) = status.error.take() {
            status.error = Some(sanitize_operation_error(&error));
        }
        let created_at_valid = normalize_operation_timestamp(&mut status.created_at);
        let updated_at_valid = normalize_operation_timestamp(&mut status.updated_at);
        let operation_id = status.id.clone();
        let locally_invalid = !created_at_valid || !updated_at_valid;
        if candidates
            .insert(
                operation_id.clone(),
                LoadedPerformanceOperationRecord {
                    status,
                    locally_invalid,
                },
            )
            .is_some()
        {
            record_load_issue(
                &mut load_state.issues,
                PerformanceOperationLoadIssueKind::DuplicateOperationId,
            );
            conflicting_ids.insert(operation_id.clone());
        }
        if !created_at_valid || !updated_at_valid {
            record_load_issue(
                &mut load_state.issues,
                PerformanceOperationLoadIssueKind::MalformedOperationStatus,
            );
            conflicting_ids.insert(operation_id);
        }
    }

    for (physical_name, (logical_id, rejection)) in deferred_identity_rejections {
        if logical_occurrences.get(&logical_id).copied() == Some(1) {
            rejected_records.insert(physical_name, rejection);
        }
    }

    let mut candidates = candidates.into_values().collect::<Vec<_>>();
    candidates.sort_by(|left, right| left.status.id.cmp(&right.status.id));
    for candidate in candidates {
        let LoadedPerformanceOperationRecord {
            mut status,
            mut locally_invalid,
        } = candidate;
        locally_invalid |= !is_valid_loaded_status(&status);
        if locally_invalid {
            warn!(
                operation_id = %status.id,
                "rejected locally invalid persisted performance operation"
            );
            record_load_issue(
                &mut load_state.issues,
                PerformanceOperationLoadIssueKind::MalformedOperationStatus,
            );
            if logical_occurrences.get(&status.id).copied() == Some(1) {
                rejected_records.insert(
                    safe_operation_filename(&status.id),
                    PersistedStateRecordRejection::InvalidSemantics,
                );
            }
            continue;
        }
        if conflicting_ids.contains(&status.id) {
            warn!(
                operation_id = %status.id,
                "skipping ambiguous persisted performance operation"
            );
            continue;
        }
        if is_non_terminal(&status.state) {
            let duplicate_instance = !status.instance_id.trim().is_empty()
                && load_state
                    .inner
                    .active_by_instance
                    .contains_key(&status.instance_id);
            let beyond_batch =
                load_state.inner.pending_resume_ids.len() >= MAX_RESUMABLE_OPERATIONS;
            if duplicate_instance || beyond_batch {
                status.state = PERFORMANCE_RESUME_BLOCKED_STATE.to_string();
            } else if !status.instance_id.trim().is_empty() {
                load_state
                    .inner
                    .active_by_instance
                    .insert(status.instance_id.clone(), status.id.clone());
            }
            load_state.inner.pending_resume_ids.push(status.id.clone());
        }
        load_state
            .inner
            .operations
            .insert(status.id.clone(), status);
    }

    load_state.rejected_records =
        retain_performance_rejected_records(&directory, rejected_records, &mut load_state.issues);

    load_state
}

struct LoadedPerformanceOperationRecord {
    status: PerformanceOperationStatus,
    locally_invalid: bool,
}

fn retain_performance_rejected_records(
    directory: &AnchoredRecordDirectory,
    rejected: BTreeMap<String, PersistedStateRecordRejection>,
    issues: &mut Vec<PerformanceOperationLoadIssue>,
) -> Vec<PersistedStateRejectedRecord> {
    let mut retained = Vec::new();
    for (physical_name, rejection) in rejected {
        if retained.len() == MAX_REJECTED_RESTART_RECORDS_PER_STORE {
            break;
        }
        let Some(physical_id) = physical_name.strip_suffix(".json") else {
            continue;
        };
        let observation = match directory.read_for_mutation(
            std::ffi::OsStr::new(&physical_name),
            MAX_RESTART_RECORD_BYTES,
        ) {
            Ok(observation) => observation,
            Err(error) => {
                warn!(
                    error_kind = ?error.kind(),
                    "failed to reacquire rejected performance operation status"
                );
                record_load_issue(issues, PerformanceOperationLoadIssueKind::StatusUnreadable);
                continue;
            }
        };
        if !performance_rejection_still_holds(&observation, physical_id, rejection) {
            warn!("performance operation rejection changed during startup load");
            record_load_issue(issues, PerformanceOperationLoadIssueKind::StatusUnreadable);
            continue;
        }
        retained.push(PersistedStateRejectedRecord::new(
            PersistedStateRecordStore::PerformanceOperation,
            rejection,
            rejected_performance_operation_target(physical_id),
            observation.into_identity(),
        ));
    }
    retained
}

fn rejected_performance_operation_target(
    physical_id: &str,
) -> crate::state::contracts::TargetDescriptor {
    debug_assert!(is_safe_operation_id(physical_id));
    let mut target = performance_operation_status_target(physical_id);
    target.id = physical_id.to_string();
    target
}

fn performance_rejection_still_holds(
    observation: &AnchoredRecordObservation,
    physical_id: &str,
    rejection: PersistedStateRecordRejection,
) -> bool {
    if rejection == PersistedStateRecordRejection::Oversized {
        return observation.is_oversized();
    }
    let Some(bytes) = observation.bytes() else {
        return false;
    };
    let decoded = serde_json::from_slice::<PersistedPerformanceOperationStatus>(bytes)
        .map(PerformanceOperationStatus::from);
    match rejection {
        PersistedStateRecordRejection::Oversized => false,
        PersistedStateRecordRejection::InvalidSchema => decoded.is_err(),
        PersistedStateRecordRejection::InvalidIdentity => decoded
            .is_ok_and(|status| !is_safe_operation_id(&status.id) || status.id != physical_id),
        PersistedStateRecordRejection::InvalidSemantics => decoded.is_ok_and(|mut status| {
            if !is_safe_operation_id(&status.id) || status.id != physical_id {
                return false;
            }
            let timestamps_valid = normalize_operation_timestamp(&mut status.created_at)
                && normalize_operation_timestamp(&mut status.updated_at);
            !timestamps_valid || !is_valid_loaded_status(&status)
        }),
    }
}

fn record_load_issue(
    issues: &mut Vec<PerformanceOperationLoadIssue>,
    kind: PerformanceOperationLoadIssueKind,
) {
    if let Some(issue) = issues.iter_mut().find(|issue| issue.kind == kind) {
        issue.count = issue.count.saturating_add(1);
    } else {
        issues.push(PerformanceOperationLoadIssue { kind, count: 1 });
    }
}

fn is_non_terminal(state: &str) -> bool {
    !matches!(state, "complete" | "failed" | "interrupted")
}

fn is_valid_loaded_status(status: &PerformanceOperationStatus) -> bool {
    matches!(
        status.state.as_str(),
        "queued"
            | "planning"
            | "applying"
            | "removing"
            | "rolling_back"
            | PERFORMANCE_EFFECT_STARTED_STATE
            | PERFORMANCE_COMMITTING_COMPLETE_STATE
            | PERFORMANCE_COMMITTING_FAILED_STATE
            | "complete"
            | "failed"
            | "interrupted"
    ) && matches!(status.action.as_str(), "install" | "remove" | "rollback")
        && !status.instance_id.trim().is_empty()
        && status
            .journal_identity
            .as_ref()
            .is_some_and(|identity| valid_journal_identity(identity, &status.action))
}

fn valid_journal_identity(
    identity: &PerformanceOperationJournalIdentity,
    expected_action: &str,
) -> bool {
    matches!(
        (expected_action, identity.action.as_str()),
        ("install", "install" | "remove") | ("remove", "remove") | ("rollback", "rollback")
    ) && !identity.target_id.trim().is_empty()
        && identity.target_id.len() <= MAX_OPERATION_FILENAME_STEM
        && identity
            .target_id
            .chars()
            .all(|value| value.is_ascii_alphanumeric() || matches!(value, '-' | '_' | '.'))
}

fn normalize_operation_timestamp(value: &mut String) -> bool {
    let Some(normalized) = normalized_operation_timestamp(value) else {
        *value = "unknown".to_string();
        return false;
    };
    *value = normalized;
    true
}

pub(crate) fn normalized_operation_timestamp(value: &str) -> Option<String> {
    DateTime::parse_from_rfc3339(value.trim())
        .ok()
        .map(|value| {
            value
                .with_timezone(&Utc)
                .to_rfc3339_opts(SecondsFormat::AutoSi, true)
        })
}

#[cfg(test)]
fn decode_persisted_status_fixture(path: &Path) -> io::Result<PerformanceOperationStatus> {
    let data = fs::read(path)?;
    serde_json::from_slice::<PersistedPerformanceOperationStatus>(&data)
        .map(Into::into)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

#[cfg(test)]
fn persist_status_to_dir(
    storage_dir: &Path,
    status: &PerformanceOperationStatus,
) -> io::Result<()> {
    fs::create_dir_all(storage_dir)?;
    let path = operation_path(storage_dir, &status.id);
    let data = encode_status(status.clone())?;
    write_file_atomically(FileWriteRequest::new(
        performance_operation_status_target(&status.id),
        &path,
        &data,
    ))
    .map(|_| ())
    .map_err(io::Error::from)
}

#[cfg(test)]
fn replace_file(source: &Path, destination: &Path) -> io::Result<()> {
    promote_temp_file(PromoteTempFileRequest::new(
        performance_operation_status_target("performance_operation_status"),
        source,
        destination,
    ))
    .map(|_| ())
    .map_err(io::Error::from)
}

fn performance_operation_status_target(
    operation_id: &str,
) -> crate::state::contracts::TargetDescriptor {
    classify_current_artifact(CurrentArtifact::PerformanceOperationStatus, operation_id).target
}

pub fn operation_dir(paths: &AppPaths) -> PathBuf {
    paths.config_dir.join("performance").join("operations")
}

pub fn operation_path(storage_dir: &Path, operation_id: &str) -> PathBuf {
    storage_dir.join(safe_operation_filename(operation_id))
}

fn safe_operation_filename(operation_id: &str) -> String {
    let mut stem = operation_id
        .chars()
        .map(|value| {
            if value.is_ascii_alphanumeric() || matches!(value, '-' | '_') {
                value
            } else {
                '_'
            }
        })
        .take(MAX_OPERATION_FILENAME_STEM)
        .collect::<String>();
    stem = stem.trim_matches('_').to_string();
    if stem.is_empty() {
        "operation.json".to_string()
    } else {
        format!("{stem}.json")
    }
}

fn is_safe_operation_id(operation_id: &str) -> bool {
    operation_id_index(operation_id).is_some()
}

fn canonical_operation_id_from_name(name: &std::ffi::OsStr) -> Option<String> {
    let filename = name.to_str()?;
    let operation_id = filename.strip_suffix(".json")?;
    is_safe_operation_id(operation_id).then(|| operation_id.to_string())
}

fn operation_id_index(operation_id: &str) -> Option<u128> {
    let suffix = operation_id.strip_prefix(PERFORMANCE_OPERATION_ID_PREFIX)?;
    if suffix.len() != 32
        || !suffix
            .bytes()
            .all(|value| value.is_ascii_digit() || matches!(value, b'a'..=b'f'))
    {
        return None;
    }
    u128::from_str_radix(suffix, 16).ok()
}

fn compare_operation_recency(
    left: &&PerformanceOperationStatus,
    right: &&PerformanceOperationStatus,
) -> std::cmp::Ordering {
    parsed_operation_timestamp(&left.updated_at)
        .cmp(&parsed_operation_timestamp(&right.updated_at))
        .then_with(|| {
            parsed_operation_timestamp(&left.created_at)
                .cmp(&parsed_operation_timestamp(&right.created_at))
        })
        .then_with(|| operation_id_index(&left.id).cmp(&operation_id_index(&right.id)))
        .then_with(|| left.id.cmp(&right.id))
}

fn parsed_operation_timestamp(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value.trim())
        .ok()
        .map(|value| value.with_timezone(&Utc))
}

pub fn generate_performance_operation_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_nanos())
        .unwrap_or_default();
    format!("{PERFORMANCE_OPERATION_ID_PREFIX}{nanos:032x}")
}

pub fn sanitize_operation_error(value: &str) -> String {
    sanitize_public_diagnostic_text(
        value,
        RedactionAudience::UserVisible,
        MAX_OPERATION_ERROR_CHARS,
        "performance operation failed",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::persistence::{AtomicWriteBackend, PersistenceCoordinator};
    use crate::state::contracts::TargetDescriptor;
    use static_assertions::assert_not_impl_any;
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    assert_not_impl_any!(LoadedPerformanceOperationStore: Clone);

    #[derive(Default)]
    struct ControlledBackend {
        fail_writes: AtomicBool,
        fail_destination: SyncMutex<Option<PathBuf>>,
        gate_writes: AtomicBool,
        entered_write: AtomicBool,
        writes: SyncMutex<Vec<(PathBuf, Vec<u8>)>>,
    }

    impl ControlledBackend {
        fn coordinator(self: &Arc<Self>) -> PersistenceCoordinator {
            PersistenceCoordinator::for_test(
                self.clone(),
                Duration::from_millis(25),
                Duration::from_millis(100),
            )
        }

        fn set_fail_writes(&self, fail: bool) {
            self.fail_writes.store(fail, Ordering::SeqCst);
        }

        fn set_fail_destination(&self, destination: Option<PathBuf>) {
            *self
                .fail_destination
                .lock()
                .expect("controlled backend failure destination lock") = destination;
        }

        fn gate(&self) {
            self.entered_write.store(false, Ordering::SeqCst);
            self.gate_writes.store(true, Ordering::SeqCst);
        }

        fn release(&self) {
            self.gate_writes.store(false, Ordering::SeqCst);
        }

        fn write_count_for(&self, destination: &Path) -> usize {
            self.writes
                .lock()
                .expect("controlled backend writes lock")
                .iter()
                .filter(|(path, _)| path == destination)
                .count()
        }
    }

    impl AtomicWriteBackend for ControlledBackend {
        fn write(
            &self,
            _target: &TargetDescriptor,
            destination: &Path,
            contents: &[u8],
        ) -> io::Result<()> {
            self.entered_write.store(true, Ordering::SeqCst);
            while self.gate_writes.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(1));
            }
            let destination_failed = self
                .fail_destination
                .lock()
                .expect("controlled backend failure destination lock")
                .as_ref()
                .is_some_and(|failed| failed == destination);
            if self.fail_writes.load(Ordering::SeqCst) || destination_failed {
                return Err(io::Error::other("injected performance status failure"));
            }
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(destination, contents)?;
            self.writes
                .lock()
                .expect("controlled backend writes lock")
                .push((destination.to_path_buf(), contents.to_vec()));
            Ok(())
        }
    }

    #[tokio::test]
    async fn persisted_operation_status_survives_restart_as_pending_resume() {
        let root = test_root("restart-resume");
        let paths = test_paths(&root);
        let store = PerformanceOperationStore::load_from_paths(&paths);
        let started = store
            .start_with_identity(
                "instance-a".to_string(),
                "install".to_string(),
                test_payload(),
                PerformanceOperationJournalIdentity::new(
                    "install",
                    "restart_composition",
                    RollbackState::Unavailable,
                ),
            )
            .await
            .expect("operation starts");
        store
            .record_progress(&started.id, "applying")
            .await
            .expect("progress accepted");
        store.flush().await.expect("progress persisted");

        let path = operation_path(&operation_dir(&paths), &started.id);
        assert!(path.is_file());
        let persisted =
            decode_persisted_status_fixture(&path).expect("persisted status should load");
        assert_eq!(persisted.state, "applying");

        store.close().await.expect("store closes before restart");
        let reloaded = PerformanceOperationStore::load_from_paths(&paths);
        let resumable = reloaded
            .get(&started.id)
            .await
            .expect("loaded resumable operation");
        assert_eq!(resumable.state, "applying");
        assert_eq!(resumable.error, None);
        let by_instance = reloaded
            .current_or_latest_for_instance("instance-a")
            .await
            .expect("loaded instance operation");
        assert_eq!(by_instance.id, started.id);
        assert_eq!(by_instance.state, "applying");
        let pending = reloaded.take_pending_resumable_operations().await;
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, started.id);
        assert!(
            reloaded
                .take_pending_resumable_operations()
                .await
                .is_empty()
        );

        let conflict = reloaded
            .start(
                "instance-a".to_string(),
                "remove".to_string(),
                test_payload(),
            )
            .await;
        assert!(matches!(
            conflict,
            Err(PerformanceOperationStartError::Conflict)
        ));
        reloaded
            .record_complete(&started.id)
            .await
            .expect("operation completes");
        reloaded
            .start(
                "instance-a".to_string(),
                "remove".to_string(),
                test_payload(),
            )
            .await
            .expect("completed resumed operation should not conflict");

        cleanup(&root);
    }

    #[tokio::test]
    async fn terminal_operation_status_remains_visible_after_restart() {
        let root = test_root("terminal-visible");
        let paths = test_paths(&root);
        let store = PerformanceOperationStore::load_from_paths(&paths);
        let started = store
            .start_with_identity(
                "instance-a".to_string(),
                "remove".to_string(),
                test_payload(),
                PerformanceOperationJournalIdentity::new(
                    "remove",
                    "terminal_visible",
                    RollbackState::Unavailable,
                ),
            )
            .await
            .expect("operation starts");
        store
            .record_complete(&started.id)
            .await
            .expect("operation completes");
        store.close().await.expect("store closes before restart");

        let reloaded = PerformanceOperationStore::load_from_paths(&paths);
        let status = reloaded
            .get(&started.id)
            .await
            .expect("loaded complete operation");

        assert_eq!(status.state, "complete");
        assert_eq!(status.error, None);
        let by_instance = reloaded
            .current_or_latest_for_instance("instance-a")
            .await
            .expect("loaded terminal instance operation");
        assert_eq!(by_instance.id, started.id);
        assert_eq!(by_instance.state, "complete");

        cleanup(&root);
    }

    #[tokio::test]
    async fn current_or_latest_for_instance_prefers_active_over_newer_terminal() {
        let store = PerformanceOperationStore::new();
        let failed = store
            .start(
                "instance-a".to_string(),
                "install".to_string(),
                test_payload(),
            )
            .await
            .expect("operation starts");
        store
            .record_failed(&failed.id, "failed")
            .await
            .expect("operation fails");
        let active = store
            .start(
                "instance-a".to_string(),
                "remove".to_string(),
                test_payload(),
            )
            .await
            .expect("second operation starts");

        let by_instance = store
            .current_or_latest_for_instance("instance-a")
            .await
            .expect("instance operation");

        assert_eq!(by_instance.id, active.id);
        assert_eq!(by_instance.state, "queued");
    }

    #[tokio::test]
    async fn progress_rejects_missing_and_terminal_operations() {
        let store = PerformanceOperationStore::new();
        assert!(matches!(
            store.record_progress("missing-operation", "applying").await,
            Err(PerformanceOperationStoreError::MissingOperation)
        ));

        let started = store
            .start(
                "instance-a".to_string(),
                "install".to_string(),
                test_payload(),
            )
            .await
            .expect("operation starts");
        store
            .record_complete(&started.id)
            .await
            .expect("operation completes");

        assert!(matches!(
            store.record_progress(&started.id, "applying").await,
            Err(PerformanceOperationStoreError::TerminalMismatch)
        ));
        assert_eq!(
            store
                .get(&started.id)
                .await
                .expect("terminal status retained")
                .state,
            "complete"
        );
    }

    #[tokio::test]
    async fn non_terminal_same_instance_operation_conflicts_during_runtime() {
        let store = PerformanceOperationStore::new();
        store
            .start(
                "instance-a".to_string(),
                "install".to_string(),
                test_payload(),
            )
            .await
            .expect("operation starts");

        let conflict = store
            .start(
                "instance-a".to_string(),
                "remove".to_string(),
                test_payload(),
            )
            .await;

        assert!(matches!(
            conflict,
            Err(PerformanceOperationStartError::Conflict)
        ));
    }

    #[tokio::test]
    async fn terminal_same_instance_operation_allows_new_work() {
        let store = PerformanceOperationStore::new();
        let started = store
            .start(
                "instance-a".to_string(),
                "install".to_string(),
                test_payload(),
            )
            .await
            .expect("operation starts");
        store
            .record_failed(&started.id, "failed")
            .await
            .expect("operation fails");

        store
            .start(
                "instance-a".to_string(),
                "remove".to_string(),
                test_payload(),
            )
            .await
            .expect("terminal operation should not conflict");
    }

    #[tokio::test]
    async fn persistence_owner_rejects_duplicate_store_for_exact_directory() {
        let root = test_root("duplicate-owner");
        let paths = test_paths(&root);
        let backend = Arc::new(ControlledBackend::default());
        let coordinator = backend.coordinator();
        let first = PerformanceOperationStore::try_load_from_paths_with_coordinator(
            &paths,
            coordinator.clone(),
        )
        .expect("first owner");

        let duplicate = PerformanceOperationStore::try_load_from_paths_with_coordinator(
            &paths,
            coordinator.clone(),
        );
        assert!(matches!(
            duplicate,
            Err(PerformanceOperationStoreError::Persistence(ref error))
                if error.kind() == io::ErrorKind::AlreadyExists
        ));

        first.close().await.expect("first owner closes");
        PerformanceOperationStore::try_load_from_paths_with_coordinator(&paths, coordinator)
            .expect("closed owner releases exact directory");
        cleanup(&root);
    }

    #[tokio::test]
    async fn failed_start_reserves_instance_until_exact_candidate_is_reconciled() {
        let root = test_root("failed-start-reservation");
        let paths = test_paths(&root);
        let backend = Arc::new(ControlledBackend::default());
        backend.set_fail_writes(true);
        let store = PerformanceOperationStore::try_load_from_paths_with_coordinator(
            &paths,
            backend.coordinator(),
        )
        .expect("store");

        let failed = store
            .start(
                "instance-a".to_string(),
                "install".to_string(),
                test_payload(),
            )
            .await
            .expect_err("physical start write fails");
        let failed_id = failed.operation_id().expect("failed start id").to_string();
        assert!(store.has_retry_candidate(&failed_id));
        assert!(
            store
                .current_or_latest_for_instance("instance-a")
                .await
                .is_none()
        );
        assert!(matches!(
            store
                .start(
                    "instance-a".to_string(),
                    "remove".to_string(),
                    test_payload(),
                )
                .await,
            Err(PerformanceOperationStartError::Conflict)
        ));

        backend.set_fail_writes(false);
        store
            .retry_critical(&failed_id)
            .await
            .expect("exact queued candidate retries");
        store
            .record_failed(&failed_id, "start persistence failed")
            .await
            .expect("reconciled start terminalizes");
        store
            .start(
                "instance-a".to_string(),
                "remove".to_string(),
                test_payload(),
            )
            .await
            .expect("terminalized reservation releases instance");
        cleanup(&root);
    }

    #[tokio::test]
    async fn close_retries_failed_status_and_releases_owner() {
        let root = test_root("close-retries-failed-status");
        let paths = test_paths(&root);
        let backend = Arc::new(ControlledBackend::default());
        let coordinator = backend.coordinator();
        backend.set_fail_writes(true);
        let store = PerformanceOperationStore::try_load_from_paths_with_coordinator(
            &paths,
            coordinator.clone(),
        )
        .expect("store");

        let failed = store
            .start_with_identity(
                "instance-a".to_string(),
                "install".to_string(),
                test_payload(),
                PerformanceOperationJournalIdentity::new(
                    "install",
                    "close_retry_failed_status",
                    RollbackState::Unavailable,
                ),
            )
            .await
            .expect_err("physical start write fails");
        let failed_id = failed.operation_id().expect("failed start id").to_string();
        assert!(store.has_retry_candidate(&failed_id));

        backend.set_fail_writes(false);
        store
            .close()
            .await
            .expect("close retries exact failed status");
        store.close().await.expect("close is idempotent");

        let reloaded =
            PerformanceOperationStore::try_load_from_paths_with_coordinator(&paths, coordinator)
                .expect("closed owner is released");
        let status = reloaded
            .get(&failed_id)
            .await
            .expect("retried status reloads");
        assert_eq!(status.state, "queued");
        assert_eq!(status.instance_id, "instance-a");
        reloaded.close().await.expect("close reloaded store");
        cleanup(&root);
    }

    #[tokio::test]
    async fn close_attempts_every_failed_status_before_returning_first_error() {
        let root = test_root("close-attempts-every-failed-status");
        let paths = test_paths(&root);
        let backend = Arc::new(ControlledBackend::default());
        backend.set_fail_writes(true);
        let store = PerformanceOperationStore::try_load_from_paths_with_coordinator(
            &paths,
            backend.coordinator(),
        )
        .expect("store");

        let mut failed_ids = Vec::new();
        for instance_id in ["instance-a", "instance-b"] {
            let failed = store
                .start(
                    instance_id.to_string(),
                    "install".to_string(),
                    test_payload(),
                )
                .await
                .expect_err("physical start write fails");
            failed_ids.push(failed.operation_id().expect("failed start id").to_string());
        }
        failed_ids.sort();
        let first_id = failed_ids[0].clone();
        let later_id = failed_ids[1].clone();
        backend.set_fail_writes(false);
        backend.set_fail_destination(Some(operation_path(&operation_dir(&paths), &first_id)));

        assert!(matches!(
            store.close().await,
            Err(PerformanceOperationStoreError::Persistence(_))
        ));
        assert!(store.has_retry_candidate(&first_id));
        assert!(!store.has_retry_candidate(&later_id));
        assert_eq!(
            store
                .get(&later_id)
                .await
                .expect("later retry commits")
                .state,
            "queued"
        );

        backend.set_fail_destination(None);
        store.close().await.expect("remaining retry closes store");
        cleanup(&root);
    }

    #[tokio::test]
    async fn close_settles_every_failed_debounced_writer_before_returning_first_error() {
        let root = test_root("close-settles-debounced-writers");
        let paths = test_paths(&root);
        let backend = Arc::new(ControlledBackend::default());
        let coordinator = backend.coordinator();
        let store = PerformanceOperationStore::try_load_from_paths_with_coordinator(
            &paths,
            coordinator.clone(),
        )
        .expect("store");
        let mut ids = Vec::new();
        for (index, instance_id) in ["instance-a", "instance-b"].into_iter().enumerate() {
            ids.push(
                store
                    .start_with_identity(
                        instance_id.to_string(),
                        "install".to_string(),
                        test_payload(),
                        PerformanceOperationJournalIdentity::new(
                            "install",
                            format!("close_debounced_{index}"),
                            RollbackState::Unavailable,
                        ),
                    )
                    .await
                    .expect("operation starts")
                    .id,
            );
        }

        backend.set_fail_writes(true);
        for id in &ids {
            store
                .record_progress(id, "planning")
                .await
                .expect("accept first progress");
            store
                .record_progress(id, "applying")
                .await
                .expect("accept latest progress");
            assert!(
                store
                    .persistence
                    .as_ref()
                    .expect("persistence")
                    .writer(id)
                    .expect("writer")
                    .flush()
                    .await
                    .is_err()
            );
            assert!(!store.has_retry_candidate(id));
        }

        ids.sort();
        let first_id = ids[0].clone();
        let later_id = ids[1].clone();
        backend.set_fail_writes(false);
        backend.set_fail_destination(Some(operation_path(&operation_dir(&paths), &first_id)));
        assert!(matches!(
            store.close().await,
            Err(PerformanceOperationStoreError::Persistence(_))
        ));

        let later =
            decode_persisted_status_fixture(&operation_path(&operation_dir(&paths), &later_id))
                .expect("later debounced writer settles before close returns");
        assert_eq!(later.state, "applying");

        backend.set_fail_destination(None);
        store.close().await.expect("remaining writer retry closes");
        let reloaded =
            PerformanceOperationStore::try_load_from_paths_with_coordinator(&paths, coordinator)
                .expect("closed owner is released");
        for id in ids {
            assert_eq!(
                reloaded
                    .get(&id)
                    .await
                    .expect("latest progress reloads")
                    .state,
                "applying"
            );
        }
        reloaded.close().await.expect("close reloaded store");
        cleanup(&root);
    }

    #[tokio::test]
    async fn close_holds_mutation_gate_across_candidate_retries_and_owner_close() {
        let root = test_root("close-holds-gate-across-retries");
        let paths = test_paths(&root);
        let backend = Arc::new(ControlledBackend::default());
        backend.set_fail_writes(true);
        let store = Arc::new(
            PerformanceOperationStore::try_load_from_paths_with_coordinator(
                &paths,
                backend.coordinator(),
            )
            .expect("store"),
        );

        for instance_id in ["instance-a", "instance-b"] {
            store
                .start(
                    instance_id.to_string(),
                    "install".to_string(),
                    test_payload(),
                )
                .await
                .expect_err("physical start write fails");
        }
        backend.set_fail_writes(false);
        backend.gate();

        let close_store = store.clone();
        let close = tokio::spawn(async move { close_store.close().await });
        tokio::time::timeout(Duration::from_secs(2), async {
            while !backend.entered_write.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("first retry enters backend");

        let competing_store = store.clone();
        let competing = tokio::spawn(async move {
            competing_store
                .start(
                    "instance-c".to_string(),
                    "install".to_string(),
                    test_payload(),
                )
                .await
        });
        tokio::task::yield_now().await;
        assert!(!competing.is_finished());

        backend.release();
        close
            .await
            .expect("close task")
            .expect("all retries and owner close succeed");
        assert!(matches!(
            competing.await.expect("competing mutation task"),
            Err(PerformanceOperationStartError::Store {
                source: PerformanceOperationStoreError::Persistence(_),
                ..
            })
        ));
        cleanup(&root);
    }

    #[tokio::test]
    async fn critical_commit_promotes_after_awaiting_caller_is_aborted() {
        let root = test_root("critical-abort-observer");
        let paths = test_paths(&root);
        let backend = Arc::new(ControlledBackend::default());
        backend.gate();
        let store = Arc::new(
            PerformanceOperationStore::try_load_from_paths_with_coordinator(
                &paths,
                backend.coordinator(),
            )
            .expect("store"),
        );
        let task_store = store.clone();
        let task = tokio::spawn(async move {
            task_store
                .start(
                    "instance-a".to_string(),
                    "install".to_string(),
                    test_payload(),
                )
                .await
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            while !backend.entered_write.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("writer entered");
        task.abort();
        backend.release();

        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if store
                    .current_or_latest_for_instance("instance-a")
                    .await
                    .is_some_and(|status| status.state == "queued")
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("detached observer promotes queued status");
        assert!(matches!(
            store
                .start(
                    "instance-a".to_string(),
                    "remove".to_string(),
                    test_payload(),
                )
                .await,
            Err(PerformanceOperationStartError::Conflict)
        ));
        cleanup(&root);
    }

    #[tokio::test]
    async fn critical_failure_retries_exact_bytes_and_competing_retrier_exits() {
        let root = test_root("critical-retry-race");
        let paths = test_paths(&root);
        let backend = Arc::new(ControlledBackend::default());
        let store = Arc::new(
            PerformanceOperationStore::try_load_from_paths_with_coordinator(
                &paths,
                backend.coordinator(),
            )
            .expect("store"),
        );
        let started = store
            .start(
                "instance-a".to_string(),
                "install".to_string(),
                test_payload(),
            )
            .await
            .expect("start");
        backend.set_fail_writes(true);
        assert!(matches!(
            store.record_effect_started(&started.id).await,
            Err(PerformanceOperationStoreError::Persistence(_))
        ));
        assert_eq!(
            store.get(&started.id).await.expect("visible status").state,
            "queued"
        );
        assert!(matches!(
            store.record_committing_complete(&started.id).await,
            Err(PerformanceOperationStoreError::RetryRequired)
        ));

        backend.set_fail_writes(false);
        let first_store = store.clone();
        let first_id = started.id.clone();
        let first = tokio::spawn(async move { first_store.retry_critical(&first_id).await });
        let second_store = store.clone();
        let second_id = started.id.clone();
        let second = tokio::spawn(async move { second_store.retry_critical(&second_id).await });
        let first = first.await.expect("first retrier task");
        let second = second.await.expect("second retrier task");
        assert!(first.is_ok() ^ second.is_ok());
        assert!(matches!(
            first.as_ref().err().or(second.as_ref().err()),
            Some(PerformanceOperationStoreError::RetryUnavailable)
        ));
        assert!(!store.has_retry_candidate(&started.id));
        assert_eq!(
            store.get(&started.id).await.expect("promoted status").state,
            PERFORMANCE_EFFECT_STARTED_STATE
        );
        let persisted =
            decode_persisted_status_fixture(&operation_path(&operation_dir(&paths), &started.id))
                .expect("retried exact status bytes");
        assert_eq!(persisted.state, PERFORMANCE_EFFECT_STARTED_STATE);
        cleanup(&root);
    }

    #[tokio::test]
    async fn critical_missing_and_terminal_mismatch_are_typed() {
        let store = PerformanceOperationStore::new();
        assert!(matches!(
            store
                .record_failed(
                    "performance-install-00000000000000000000000000000000",
                    "missing",
                )
                .await,
            Err(PerformanceOperationStoreError::MissingOperation)
        ));
        let started = store
            .start(
                "instance-a".to_string(),
                "install".to_string(),
                test_payload(),
            )
            .await
            .expect("start");
        store
            .record_failed(&started.id, "same failure")
            .await
            .expect("terminal failure");
        store
            .record_failed(&started.id, "same failure")
            .await
            .expect("same terminal transition is idempotent");
        assert!(matches!(
            store.record_complete(&started.id).await,
            Err(PerformanceOperationStoreError::TerminalMismatch)
        ));
        assert!(matches!(
            store.record_failed(&started.id, "different").await,
            Err(PerformanceOperationStoreError::TerminalMismatch)
        ));
    }

    #[tokio::test]
    async fn progress_bursts_coalesce_independently_per_operation_id() {
        let root = test_root("per-id-coalescing");
        let paths = test_paths(&root);
        let backend = Arc::new(ControlledBackend::default());
        let store = PerformanceOperationStore::try_load_from_paths_with_coordinator(
            &paths,
            backend.coordinator(),
        )
        .expect("store");
        let first = store
            .start(
                "instance-a".to_string(),
                "install".to_string(),
                test_payload(),
            )
            .await
            .expect("first start");
        let second = store
            .start(
                "instance-b".to_string(),
                "remove".to_string(),
                test_payload(),
            )
            .await
            .expect("second start");
        for index in 0..40 {
            store
                .record_progress(&first.id, &format!("first_{index}"))
                .await
                .expect("first progress");
            store
                .record_progress(&second.id, &format!("second_{index}"))
                .await
                .expect("second progress");
        }
        store.flush().await.expect("bursts flush");
        let dir = operation_dir(&paths);
        let first_path = operation_path(&dir, &first.id);
        let second_path = operation_path(&dir, &second.id);
        assert!(backend.write_count_for(&first_path) <= 2);
        assert!(backend.write_count_for(&second_path) <= 2);
        assert_eq!(
            decode_persisted_status_fixture(&first_path)
                .expect("first latest")
                .state,
            "first_39"
        );
        assert_eq!(
            decode_persisted_status_fixture(&second_path)
                .expect("second latest")
                .state,
            "second_39"
        );
        cleanup(&root);
    }

    #[tokio::test]
    async fn terminal_retention_is_absolute_and_preserves_recent_instance_lookup() {
        let root = test_root("terminal-retention-bound");
        let paths = test_paths(&root);
        let backend = Arc::new(ControlledBackend::default());
        let coordinator = backend.coordinator();
        let store = PerformanceOperationStore::try_load_from_paths_with_coordinator(
            &paths,
            coordinator.clone(),
        )
        .expect("store");
        let mut ids = Vec::new();
        for index in 0..(MAX_RETAINED_TERMINAL_OPERATIONS + 8) {
            let instance_id = if index >= MAX_RETAINED_TERMINAL_OPERATIONS {
                "repeat-instance".to_string()
            } else {
                format!("instance-{index:02}")
            };
            let started = store
                .start_with_identity(
                    instance_id,
                    "install".to_string(),
                    test_payload(),
                    PerformanceOperationJournalIdentity::new(
                        "install",
                        format!("composition-{index:02}"),
                        RollbackState::Unavailable,
                    ),
                )
                .await
                .expect("operation starts");
            if index % 2 == 0 {
                store
                    .record_reconciliation_failed(&started.id, "reconciled", "install")
                    .await
                    .expect("reconciliation terminalizes");
            } else {
                store
                    .record_complete(&started.id)
                    .await
                    .expect("operation completes");
            }
            ids.push(started.id);
        }

        let statuses = store.list();
        assert_eq!(statuses.len(), MAX_RETAINED_TERMINAL_OPERATIONS);
        assert!(
            statuses
                .iter()
                .all(|status| !is_non_terminal(&status.state))
        );
        let pruned_ids = std::iter::once(&ids[0])
            .chain(ids[MAX_RETAINED_TERMINAL_OPERATIONS..39].iter())
            .collect::<Vec<_>>();
        for pruned in pruned_ids {
            assert!(store.get(pruned).await.is_none());
            assert!(!operation_path(&operation_dir(&paths), pruned).exists());
        }
        for retained in ids[1..MAX_RETAINED_TERMINAL_OPERATIONS]
            .iter()
            .chain(std::iter::once(&ids[39]))
        {
            assert!(store.get(retained).await.is_some());
            assert!(operation_path(&operation_dir(&paths), retained).is_file());
        }
        assert!(
            store
                .current_or_latest_for_instance("instance-00")
                .await
                .is_none()
        );
        assert_eq!(
            store
                .current_or_latest_for_instance("repeat-instance")
                .await
                .expect("repeated instance latest status")
                .id,
            ids[39]
        );
        assert_eq!(
            store
                .persistence
                .as_ref()
                .expect("persistence")
                .writer_count(),
            MAX_RETAINED_TERMINAL_OPERATIONS
        );

        let reclaimed_path = operation_path(&operation_dir(&paths), &ids[0]);
        let reclaimed = coordinator
            .claim_owner(&reclaimed_path)
            .expect("pruned exact path owner is released");
        reclaimed
            .writer(
                &reclaimed_path,
                performance_operation_status_target(&ids[0]),
            )
            .expect("pruned exact path writer is released");
        reclaimed.close().await.expect("reclaimed owner closes");
        cleanup(&root);
    }

    #[test]
    fn terminal_retention_keeps_nonterminal_and_critical_retry_records() {
        let mut inner = PerformanceOperationInner::default();
        let retry_id = "performance-install-00000000000000000000000000000001";
        let active_id = "performance-install-00000000000000000000000000000002";
        inner.operations.insert(
            retry_id.to_string(),
            test_status(
                retry_id,
                "retry-instance",
                "install",
                "failed",
                test_payload(),
            ),
        );
        inner.operations.insert(
            active_id.to_string(),
            test_status(
                active_id,
                "active-instance",
                "install",
                "applying",
                test_payload(),
            ),
        );
        for index in 3..=(MAX_RETAINED_TERMINAL_OPERATIONS + 4) {
            let id = format!("performance-install-{index:032x}");
            inner.operations.insert(
                id.clone(),
                test_status(
                    &id,
                    &format!("instance-{index}"),
                    "install",
                    "complete",
                    test_payload(),
                ),
            );
        }
        let retry_ids = HashSet::from([retry_id.to_string()]);

        let candidates = terminal_prune_candidates(&inner, &retry_ids);

        assert!(!candidates.iter().any(|status| status.id == retry_id));
        assert!(!candidates.iter().any(|status| status.id == active_id));
        assert_eq!(candidates.len(), 2);
    }

    #[tokio::test]
    async fn failed_terminal_delete_blocks_shutdown_until_retry_and_releases_owner() {
        let root = test_root("terminal-retention-delete-retry");
        let paths = test_paths(&root);
        let backend = Arc::new(ControlledBackend::default());
        let coordinator = backend.coordinator();
        let store = PerformanceOperationStore::try_load_from_paths_with_coordinator(
            &paths,
            coordinator.clone(),
        )
        .expect("store");
        let mut ids = Vec::new();
        for index in 0..MAX_RETAINED_TERMINAL_OPERATIONS {
            let started = store
                .start(
                    format!("instance-{index}"),
                    "install".to_string(),
                    test_payload(),
                )
                .await
                .expect("operation starts");
            store
                .record_complete(&started.id)
                .await
                .expect("operation completes");
            ids.push(started.id);
        }
        let oldest_path = operation_path(&operation_dir(&paths), &ids[0]);
        fs::remove_file(&oldest_path).expect("remove oldest status");
        fs::create_dir(&oldest_path).expect("block oldest status deletion");

        let newest = store
            .start(
                "instance-new".to_string(),
                "install".to_string(),
                test_payload(),
            )
            .await
            .expect("new operation starts");
        store
            .record_complete(&newest.id)
            .await
            .expect("terminal commit remains authoritative");

        assert_eq!(store.list().len(), MAX_RETAINED_TERMINAL_OPERATIONS + 1);
        assert!(store.get(&ids[0]).await.is_some());
        let issues = store.retention_issues();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].operation_id, ids[0]);
        assert_eq!(
            issues[0].kind,
            PerformanceOperationRetentionIssueKind::Delete
        );
        assert!(
            issues[0]
                .facts
                .iter()
                .any(|fact| fact.kind == ExecutionFactKind::PrimitiveRefused)
        );
        assert_eq!(
            store
                .persistence
                .as_ref()
                .expect("persistence")
                .writer_count(),
            MAX_RETAINED_TERMINAL_OPERATIONS + 1
        );

        assert!(matches!(
            store.flush().await,
            Err(PerformanceOperationStoreError::Persistence(ref error))
                if error.to_string()
                    == "performance operation terminal retention cleanup is pending"
        ));
        assert!(matches!(
            store.close().await,
            Err(PerformanceOperationStoreError::Persistence(ref error))
                if error.to_string()
                    == "performance operation terminal retention cleanup is pending"
        ));
        let open_after_failed_close = store
            .start(
                "instance-open-after-failed-close".to_string(),
                "install".to_string(),
                test_payload(),
            )
            .await
            .expect("failed close leaves owner open and retryable");
        assert!(store.get(&open_after_failed_close.id).await.is_some());

        fs::remove_dir(&oldest_path).expect("unblock oldest status deletion");
        assert!(store.retry_terminal_retention().await.is_empty());
        assert!(store.get(&ids[0]).await.is_none());
        assert_eq!(
            store
                .list()
                .into_iter()
                .filter(|status| !is_non_terminal(&status.state))
                .count(),
            MAX_RETAINED_TERMINAL_OPERATIONS
        );
        assert_eq!(store.list().len(), MAX_RETAINED_TERMINAL_OPERATIONS + 1);
        assert!(
            store
                .get(&open_after_failed_close.id)
                .await
                .is_some_and(|status| is_non_terminal(&status.state))
        );
        assert_eq!(
            store
                .persistence
                .as_ref()
                .expect("persistence")
                .writer_count(),
            MAX_RETAINED_TERMINAL_OPERATIONS + 1
        );
        store
            .flush()
            .await
            .expect("cleanup retry makes flush truthful");
        store.close().await.expect("cleanup retry allows close");

        let reclaimed = coordinator
            .claim_owner(operation_dir(&paths))
            .expect("closed status owner is released");
        reclaimed
            .writer(&oldest_path, performance_operation_status_target(&ids[0]))
            .expect("pruned status path is released");
        reclaimed.close().await.expect("reclaimed owner closes");
        cleanup(&root);
    }

    #[tokio::test]
    async fn failed_terminal_writer_settlement_retains_status_until_exact_retry() {
        let root = test_root("terminal-retention-settle-retry");
        let paths = test_paths(&root);
        let backend = Arc::new(ControlledBackend::default());
        let store = PerformanceOperationStore::try_load_from_paths_with_coordinator(
            &paths,
            backend.coordinator(),
        )
        .expect("store");
        let mut ids = Vec::new();
        for index in 0..MAX_RETAINED_TERMINAL_OPERATIONS {
            let started = store
                .start(
                    format!("instance-{index}"),
                    "install".to_string(),
                    test_payload(),
                )
                .await
                .expect("operation starts");
            store
                .record_complete(&started.id)
                .await
                .expect("operation completes");
            ids.push(started.id);
        }

        let oldest_id = &ids[0];
        let oldest_path = operation_path(&operation_dir(&paths), oldest_id);
        let oldest_status = store.get(oldest_id).await.expect("oldest terminal status");
        backend.set_fail_destination(Some(oldest_path.clone()));
        store
            .persistence
            .as_ref()
            .expect("persistence")
            .writer(oldest_id)
            .expect("oldest writer")
            .accept(oldest_status, WriteUrgency::Debounced, encode_status)
            .expect("pending exact status accepted");

        let newest = store
            .start(
                "instance-new".to_string(),
                "install".to_string(),
                test_payload(),
            )
            .await
            .expect("new operation starts");
        store
            .record_complete(&newest.id)
            .await
            .expect("new terminal commit remains authoritative");

        let issues = store.retention_issues();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].operation_id, *oldest_id);
        assert_eq!(
            issues[0].kind,
            PerformanceOperationRetentionIssueKind::WriterSettlement
        );
        assert!(store.get(oldest_id).await.is_some());
        assert!(oldest_path.is_file());
        assert_eq!(
            store
                .persistence
                .as_ref()
                .expect("persistence")
                .writer_count(),
            MAX_RETAINED_TERMINAL_OPERATIONS + 1
        );

        backend.set_fail_destination(None);
        assert!(store.retry_terminal_retention().await.is_empty());
        assert!(store.get(oldest_id).await.is_none());
        assert!(!oldest_path.exists());
        assert_eq!(store.list().len(), MAX_RETAINED_TERMINAL_OPERATIONS);
        store.close().await.expect("store closes after exact retry");
        cleanup(&root);
    }

    #[tokio::test]
    async fn startup_retention_prunes_only_valid_canonical_terminal_records() {
        let root = test_root("startup-terminal-retention");
        let paths = test_paths(&root);
        let dir = operation_dir(&paths);
        fs::create_dir_all(&dir).expect("create operation dir");
        let total = MAX_RETAINED_TERMINAL_OPERATIONS + 3;
        let mut ids = Vec::new();
        for index in 1..=total {
            let id = format!("performance-install-{index:032x}");
            let mut status = test_status(
                &id,
                &format!("instance-{index:02}"),
                "install",
                "complete",
                test_payload(),
            );
            status.created_at = format!("2026-07-10T00:{index:02}:00Z");
            status.updated_at = status.created_at.clone();
            persist_status_to_dir(&dir, &status).expect("persist terminal status");
            ids.push(id);
        }
        let malformed_id = "performance-install-00000000000000000000000000000100";
        let malformed_path = operation_path(&dir, malformed_id);
        fs::write(&malformed_path, b"{not-json").expect("write malformed status");
        let noncanonical_id = "performance-install-00000000000000000000000000000101";
        let noncanonical_path = dir.join("copied-terminal.json");
        fs::write(
            &noncanonical_path,
            encode_status(test_status(
                noncanonical_id,
                "noncanonical-instance",
                "install",
                "complete",
                test_payload(),
            ))
            .expect("encode noncanonical status"),
        )
        .expect("write noncanonical status");
        let unsafe_path = dir.join("unknown-owned.json");
        let mut unsafe_status = test_status(
            "../../unknown-owned",
            "unsafe-instance",
            "install",
            "complete",
            test_payload(),
        );
        unsafe_status.id = "../../unknown-owned".to_string();
        fs::write(
            &unsafe_path,
            encode_status(unsafe_status).expect("encode unsafe status"),
        )
        .expect("write unsafe status");

        let store = PerformanceOperationStore::load_from_paths(&paths);
        let pending = store.take_pending_resumable_operations().await;

        assert_eq!(store.list().len(), MAX_RETAINED_TERMINAL_OPERATIONS);
        assert!(pending.is_empty());
        for id in &ids[..3] {
            assert!(store.get(id).await.is_none());
            assert!(!operation_path(&dir, id).exists());
        }
        for id in &ids[3..] {
            assert!(store.get(id).await.is_some());
            assert!(operation_path(&dir, id).is_file());
        }
        assert!(malformed_path.is_file());
        assert!(noncanonical_path.is_file());
        assert!(unsafe_path.is_file());
        store.close().await.expect("store closes");

        let reloaded = PerformanceOperationStore::load_from_paths(&paths);
        reloaded.take_pending_resumable_operations().await;
        assert_eq!(
            reloaded
                .list()
                .into_iter()
                .filter(|status| !is_non_terminal(&status.state))
                .count(),
            MAX_RETAINED_TERMINAL_OPERATIONS
        );
        assert!(malformed_path.is_file());
        assert!(noncanonical_path.is_file());
        assert!(unsafe_path.is_file());
        cleanup(&root);
    }

    #[tokio::test]
    async fn terminal_retention_completes_after_awaiting_caller_is_aborted() {
        let root = test_root("terminal-retention-abort");
        let paths = test_paths(&root);
        let backend = Arc::new(ControlledBackend::default());
        let store = Arc::new(
            PerformanceOperationStore::try_load_from_paths_with_coordinator(
                &paths,
                backend.coordinator(),
            )
            .expect("store"),
        );
        let mut ids = Vec::new();
        for index in 0..MAX_RETAINED_TERMINAL_OPERATIONS {
            let started = store
                .start(
                    format!("instance-{index}"),
                    "install".to_string(),
                    test_payload(),
                )
                .await
                .expect("operation starts");
            store
                .record_complete(&started.id)
                .await
                .expect("operation completes");
            ids.push(started.id);
        }
        let newest = store
            .start(
                "instance-new".to_string(),
                "install".to_string(),
                test_payload(),
            )
            .await
            .expect("new operation starts");
        backend.gate();
        let task_store = store.clone();
        let terminal_id = newest.id.clone();
        let task = tokio::spawn(async move { task_store.record_complete(&terminal_id).await });
        tokio::time::timeout(Duration::from_secs(2), async {
            while !backend.entered_write.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("terminal writer entered");
        task.abort();
        backend.release();

        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if store.get(&ids[0]).await.is_none()
                    && store
                        .get(&newest.id)
                        .await
                        .is_some_and(|status| status.state == "complete")
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("detached terminal observer prunes old status");
        assert_eq!(store.list().len(), MAX_RETAINED_TERMINAL_OPERATIONS);
        assert!(!operation_path(&operation_dir(&paths), &ids[0]).exists());
        assert!(store.retention_issues().is_empty());
        cleanup(&root);
    }

    #[tokio::test]
    async fn poisoned_operation_lock_panics_instead_of_returning_partial_state() {
        let store = Arc::new(PerformanceOperationStore::new());
        let inner = store.inner.clone();
        let _ = std::panic::catch_unwind(move || {
            let _guard = inner.write().expect("lock initially available");
            panic!("inject poison");
        });
        let task_store = store.clone();
        let failure = tokio::spawn(async move {
            task_store
                .get("performance-install-00000000000000000000000000000000")
                .await
        })
        .await
        .expect_err("poisoned invariant must panic");
        assert!(failure.is_panic());
    }

    #[test]
    fn operation_status_path_uses_sanitized_local_filename() {
        let root = test_root("safe-filename");
        let paths = test_paths(&root);
        let dir = operation_dir(&paths);
        let path = operation_path(&dir, "../../secret\\operation;id");
        let filename = path
            .file_name()
            .and_then(|value| value.to_str())
            .expect("filename");

        assert!(path.starts_with(&dir));
        assert_eq!(path.parent(), Some(dir.as_path()));
        assert!(!filename.contains('/'));
        assert!(!filename.contains('\\'));
        assert!(!filename.contains(';'));
        assert!(filename.ends_with(".json"));

        cleanup(&root);
    }

    #[tokio::test]
    async fn persisted_identity_is_internal_and_survives_reload() {
        let root = test_root("internal-journal-identity");
        let paths = test_paths(&root);
        let store = PerformanceOperationStore::load_from_paths(&paths);
        let started = store
            .start_with_identity(
                "instance-a".to_string(),
                "install".to_string(),
                test_payload(),
                PerformanceOperationJournalIdentity::new(
                    "install",
                    "stable_composition",
                    RollbackState::Available,
                ),
            )
            .await
            .expect("operation starts with identity");
        store.close().await.expect("status store closes");

        let public = serde_json::to_value(&started).expect("public status serializes");
        assert!(public.get("journal_identity").is_none());
        let path = operation_path(&operation_dir(&paths), &started.id);
        let persisted: serde_json::Value =
            serde_json::from_slice(&fs::read(path).expect("persisted status bytes"))
                .expect("persisted status json");
        assert_eq!(persisted["journal_identity"]["action"], "install");
        assert_eq!(
            persisted["journal_identity"]["target_id"],
            "stable_composition"
        );

        let reloaded = PerformanceOperationStore::load_from_paths(&paths);
        let status = reloaded.get(&started.id).await.expect("status reloads");
        assert_eq!(status.journal_identity, started.journal_identity);
        cleanup(&root);
    }

    #[test]
    fn noncanonical_duplicate_id_is_excluded_from_resume() {
        let root = test_root("noncanonical-duplicate-id");
        let paths = test_paths(&root);
        let dir = operation_dir(&paths);
        fs::create_dir_all(&dir).expect("create operation dir");
        let id = "performance-install-00000000000000000000000000000001";
        let status = test_status(id, "instance-a", "install", "applying", test_payload());
        persist_status_to_dir(&dir, &status).expect("persist canonical status");
        fs::copy(operation_path(&dir, id), dir.join("copied-status.json"))
            .expect("copy status under noncanonical filename");

        let load_state = load_persisted_operation_inner(&dir);

        assert!(load_state.inner.operations.is_empty());
        assert!(load_state.inner.pending_resume_ids.is_empty());
        assert!(load_state.rejected_records.is_empty());
        assert!(load_state.issues.iter().any(|issue| {
            issue.kind == PerformanceOperationLoadIssueKind::NonCanonicalFilename
                && issue.count == 1
        }));
        cleanup(&root);
    }

    #[test]
    fn invalid_terminal_records_are_rejected_without_resume_admission() {
        let root = test_root("invalid-terminal-records");
        let paths = test_paths(&root);
        let dir = operation_dir(&paths);
        fs::create_dir_all(&dir).expect("create operation dir");
        let missing_identity_id = "performance-install-00000000000000000000000000000001";
        let malformed_action_id = "performance-install-00000000000000000000000000000002";
        let empty_instance_id = "performance-install-00000000000000000000000000000003";
        let mut missing_identity = test_status(
            missing_identity_id,
            "instance-a",
            "install",
            "complete",
            test_payload(),
        );
        missing_identity.journal_identity = None;
        let malformed_action = test_status(
            malformed_action_id,
            "instance-b",
            "unexpected",
            "failed",
            test_payload(),
        );
        let empty_instance =
            test_status(empty_instance_id, "", "remove", "complete", test_payload());
        for status in [&missing_identity, &malformed_action, &empty_instance] {
            persist_status_to_dir(&dir, status).expect("persist invalid terminal status");
        }

        let load_state = load_persisted_operation_inner(&dir);

        assert!(load_state.inner.operations.is_empty());
        assert!(load_state.inner.pending_resume_ids.is_empty());
        assert_eq!(load_state.rejected_records.len(), 3);
        cleanup(&root);
    }

    #[test]
    fn noncanonical_terminal_duplicate_is_excluded_from_loaded_state() {
        let root = test_root("noncanonical-terminal-duplicate");
        let paths = test_paths(&root);
        let dir = operation_dir(&paths);
        fs::create_dir_all(&dir).expect("create operation dir");
        let id = "performance-install-00000000000000000000000000000001";
        let status = test_status(id, "instance-a", "install", "complete", test_payload());
        persist_status_to_dir(&dir, &status).expect("persist canonical terminal status");
        fs::copy(operation_path(&dir, id), dir.join("copied-terminal.json"))
            .expect("copy terminal status under noncanonical filename");

        let load_state = load_persisted_operation_inner(&dir);

        assert!(load_state.inner.operations.is_empty());
        assert!(load_state.inner.pending_resume_ids.is_empty());
        assert!(load_state.rejected_records.is_empty());
        cleanup(&root);
    }

    #[test]
    fn invalid_persisted_timestamps_are_rejected_without_resume_admission() {
        let root = test_root("invalid-timestamps");
        let paths = test_paths(&root);
        let dir = operation_dir(&paths);
        fs::create_dir_all(&dir).expect("create operation dir");
        let id = "performance-install-00000000000000000000000000000001";
        let mut status = test_status(id, "instance-a", "remove", "removing", test_payload());
        status.created_at = "/Users/alice/private/token=secret".to_string();
        status.updated_at = "not-a-timestamp".to_string();
        persist_status_to_dir(&dir, &status).expect("persist invalid timestamps");

        let load_state = load_persisted_operation_inner(&dir);
        assert!(load_state.inner.operations.is_empty());
        assert!(load_state.inner.pending_resume_ids.is_empty());
        assert_eq!(load_state.rejected_records.len(), 1);
        assert_eq!(
            load_state.rejected_records[0].evidence().rejection(),
            PersistedStateRecordRejection::InvalidSemantics
        );
        assert!(load_state.issues.iter().any(|issue| {
            issue.kind == PerformanceOperationLoadIssueKind::MalformedOperationStatus
        }));
        cleanup(&root);
    }

    #[tokio::test]
    async fn latest_terminal_operation_compares_timestamp_instants_across_offsets() {
        let root = test_root("timestamp-offset-order");
        let paths = test_paths(&root);
        let dir = operation_dir(&paths);
        fs::create_dir_all(&dir).expect("create operation dir");
        let earlier_id = "performance-install-00000000000000000000000000000001";
        let later_id = "performance-install-00000000000000000000000000000002";
        let mut earlier = test_status(
            earlier_id,
            "instance-a",
            "remove",
            "complete",
            test_payload(),
        );
        earlier.created_at = "2026-07-10T01:30:00+02:00".to_string();
        earlier.updated_at = earlier.created_at.clone();
        let mut later = test_status(later_id, "instance-a", "remove", "complete", test_payload());
        later.created_at = "2026-07-10T00:00:00Z".to_string();
        later.updated_at = later.created_at.clone();
        persist_status_to_dir(&dir, &earlier).expect("persist offset status");
        persist_status_to_dir(&dir, &later).expect("persist UTC status");

        let store = PerformanceOperationStore::load_from_paths(&paths);
        let latest = store
            .current_or_latest_for_instance("instance-a")
            .await
            .expect("latest terminal operation");

        assert_eq!(latest.id, later_id);
        assert_eq!(
            store
                .get(earlier_id)
                .await
                .expect("offset status loaded")
                .updated_at,
            "2026-07-09T23:30:00Z"
        );
        cleanup(&root);
    }

    #[test]
    fn unsafe_operation_ids_are_not_loaded_or_returned() {
        let root = test_root("unsafe-id");
        let paths = test_paths(&root);
        let dir = operation_dir(&paths);
        fs::create_dir_all(&dir).expect("create operation dir");
        let status = PerformanceOperationStatus {
            id: "../../secret".to_string(),
            instance_id: "instance-a".to_string(),
            action: "install".to_string(),
            payload: test_payload(),
            state: "complete".to_string(),
            error: None,
            created_at: timestamp_utc(),
            updated_at: timestamp_utc(),
            journal_identity: None,
        };
        persist_status_to_dir(&dir, &status).expect("persist unsafe status");

        let load_state = load_persisted_operation_inner(&dir);

        assert!(load_state.inner.operations.is_empty());
        assert_eq!(
            load_state.issues,
            vec![PerformanceOperationLoadIssue {
                kind: PerformanceOperationLoadIssueKind::UnsafeOperationId,
                count: 1,
            }]
        );

        cleanup(&root);
    }

    #[test]
    fn duplicate_pending_operations_for_instance_retain_extra_records_for_reconciliation() {
        let root = test_root("duplicate-pending");
        let paths = test_paths(&root);
        let dir = operation_dir(&paths);
        fs::create_dir_all(&dir).expect("create operation dir");
        let first = test_status(
            "performance-install-00000000000000000000000000000001",
            "instance-a",
            "install",
            "applying",
            test_payload(),
        );
        let second = test_status(
            "performance-install-00000000000000000000000000000002",
            "instance-a",
            "remove",
            "removing",
            test_payload(),
        );
        persist_status_to_dir(&dir, &first).expect("persist first status");
        persist_status_to_dir(&dir, &second).expect("persist second status");

        let load_state = load_persisted_operation_inner(&dir);

        assert_eq!(load_state.inner.pending_resume_ids.len(), 2);
        let blocked = load_state
            .inner
            .operations
            .values()
            .filter(|status| status.state == PERFORMANCE_RESUME_BLOCKED_STATE)
            .count();
        assert_eq!(blocked, 1);
        assert_eq!(
            load_state.inner.active_by_instance.get("instance-a"),
            load_state.inner.pending_resume_ids.first()
        );
        assert!(load_state.issues.is_empty());

        cleanup(&root);
    }

    #[test]
    fn malformed_current_schema_pending_operation_is_rejected() {
        let root = test_root("malformed-pending");
        let paths = test_paths(&root);
        let dir = operation_dir(&paths);
        fs::create_dir_all(&dir).expect("create operation dir");
        let status = test_status(
            "performance-install-00000000000000000000000000000001",
            "",
            "install",
            "applying",
            test_payload(),
        );
        persist_status_to_dir(&dir, &status).expect("persist malformed status");

        let load_state = load_persisted_operation_inner(&dir);

        assert!(load_state.inner.operations.is_empty());
        assert!(load_state.inner.active_by_instance.is_empty());
        assert!(load_state.inner.pending_resume_ids.is_empty());
        assert_eq!(load_state.rejected_records.len(), 1);
        assert_eq!(
            load_state.issues,
            vec![PerformanceOperationLoadIssue {
                kind: PerformanceOperationLoadIssueKind::MalformedOperationStatus,
                count: 1,
            }]
        );

        cleanup(&root);
    }

    #[test]
    fn persisted_operation_with_unknown_fields_is_not_loaded_and_records_safe_issue() {
        let root = test_root("unknown-field-pending");
        let paths = test_paths(&root);
        let dir = operation_dir(&paths);
        fs::create_dir_all(&dir).expect("create operation dir");
        let path = operation_path(&dir, "performance-install-00000000000000000000000000000001");
        let persisted_bytes = serde_json::to_vec(&serde_json::json!({
            "id": "performance-install-00000000000000000000000000000001",
            "instance_id": "instance-a",
            "action": "install",
            "payload": {
                "unexpected_mode": true
            },
            "state": "applying",
            "error": null,
            "created_at": timestamp_utc(),
            "updated_at": timestamp_utc()
        }))
        .expect("serialize status");
        fs::write(&path, &persisted_bytes).expect("write status");

        let load_state = load_persisted_operation_inner(&dir);

        assert!(load_state.inner.operations.is_empty());
        assert_eq!(
            load_state.issues,
            vec![PerformanceOperationLoadIssue {
                kind: PerformanceOperationLoadIssueKind::StatusInvalid,
                count: 1,
            }]
        );
        let encoded = format!("{:?}", load_state.issues);
        assert!(!encoded.contains(root.to_string_lossy().as_ref()));
        assert!(!encoded.contains("unexpected_mode"));
        assert_eq!(load_state.rejected_records.len(), 1);
        assert_eq!(
            load_state.rejected_records[0].evidence().rejection(),
            PersistedStateRecordRejection::InvalidSchema
        );
        assert_eq!(
            fs::read(&path).expect("rejected record remains"),
            persisted_bytes
        );

        cleanup(&root);
    }

    #[test]
    fn rejected_record_selection_is_creation_order_independent_and_bounded() {
        let mut selections = Vec::new();
        for descending in [false, true] {
            let root = test_root(if descending {
                "rejected-order-descending"
            } else {
                "rejected-order-ascending"
            });
            let dir = operation_dir(&test_paths(&root));
            fs::create_dir_all(&dir).expect("create operation dir");
            let mut indexes = (1_u128..=12).collect::<Vec<_>>();
            if descending {
                indexes.reverse();
            }
            for index in indexes {
                let id = format!("{PERFORMANCE_OPERATION_ID_PREFIX}{index:032x}");
                fs::write(operation_path(&dir, &id), b"{").expect("write invalid record");
            }

            let load_state = load_persisted_operation_inner(&dir);
            let selected = load_state
                .rejected_records
                .iter()
                .map(|record| record.evidence().target().id.clone())
                .collect::<Vec<_>>();

            assert_eq!(load_state.rejected_records.len(), 8);
            assert_eq!(
                load_state
                    .issues
                    .iter()
                    .find(|issue| issue.kind == PerformanceOperationLoadIssueKind::StatusInvalid)
                    .map(|issue| issue.count),
                Some(12)
            );
            selections.push(selected);
            drop(load_state);
            cleanup(&root);
        }

        assert_eq!(selections[0], selections[1]);
        assert_eq!(
            selections[0],
            (1_u128..=8)
                .map(|index| format!("{PERFORMANCE_OPERATION_ID_PREFIX}{index:032x}"))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn oversized_canonical_record_retains_exact_bounded_evidence() {
        let root = test_root("oversized-rejected-record");
        let dir = operation_dir(&test_paths(&root));
        fs::create_dir_all(&dir).expect("create operation dir");
        let id = format!("{PERFORMANCE_OPERATION_ID_PREFIX}{:032x}", 1);
        fs::write(
            operation_path(&dir, &id),
            vec![b'x'; MAX_RESTART_RECORD_BYTES as usize + 1],
        )
        .expect("write oversized record");

        let load_state = load_persisted_operation_inner(&dir);

        assert!(load_state.inner.operations.is_empty());
        assert_eq!(load_state.rejected_records.len(), 1);
        let evidence = load_state.rejected_records[0].evidence();
        assert_eq!(
            evidence.rejection(),
            PersistedStateRecordRejection::Oversized
        );
        assert_eq!(evidence.target().id, id);
        drop(load_state);
        cleanup(&root);
    }

    #[test]
    fn embedded_operation_id_mismatch_targets_canonical_physical_id() {
        let root = test_root("physical-id-mismatch");
        let dir = operation_dir(&test_paths(&root));
        fs::create_dir_all(&dir).expect("create operation dir");
        let embedded_id = format!("{PERFORMANCE_OPERATION_ID_PREFIX}{:032x}", 1);
        let physical_id = format!("{PERFORMANCE_OPERATION_ID_PREFIX}{:032x}", 2);
        let status = test_status(
            &embedded_id,
            "instance-a",
            "install",
            "applying",
            test_payload(),
        );
        fs::write(
            operation_path(&dir, &physical_id),
            serde_json::to_vec_pretty(&PersistedPerformanceOperationStatus::from(status))
                .expect("serialize mismatched operation"),
        )
        .expect("write mismatched operation");

        let load_state = load_persisted_operation_inner(&dir);

        assert!(load_state.inner.operations.is_empty());
        assert_eq!(load_state.rejected_records.len(), 1);
        let evidence = load_state.rejected_records[0].evidence();
        assert_eq!(
            evidence.rejection(),
            PersistedStateRecordRejection::InvalidIdentity
        );
        assert_eq!(evidence.target().id, physical_id);
        drop(load_state);
        cleanup(&root);
    }

    #[test]
    fn replacement_before_reacquire_does_not_mint_stale_rejection_identity() {
        let root = test_root("rejected-replacement");
        let dir = operation_dir(&test_paths(&root));
        fs::create_dir_all(&dir).expect("create operation dir");
        let id = format!("{PERFORMANCE_OPERATION_ID_PREFIX}{:032x}", 1);
        let path = operation_path(&dir, &id);
        fs::write(&path, b"{").expect("write rejected record");
        let directory = AnchoredRecordDirectory::open(&dir).expect("hold operation directory");
        let mut rejected = BTreeMap::new();
        rejected.insert(
            safe_operation_filename(&id),
            PersistedStateRecordRejection::InvalidSchema,
        );
        fs::rename(&path, dir.join("old-record")).expect("move rejected record");
        let valid = test_status(&id, "instance-a", "install", "complete", test_payload());
        fs::write(
            &path,
            serde_json::to_vec_pretty(&PersistedPerformanceOperationStatus::from(valid))
                .expect("serialize replacement"),
        )
        .expect("write replacement");
        let mut issues = Vec::new();

        let retained = retain_performance_rejected_records(&directory, rejected, &mut issues);

        assert!(retained.is_empty());
        assert_eq!(
            issues,
            vec![PerformanceOperationLoadIssue {
                kind: PerformanceOperationLoadIssueKind::StatusUnreadable,
                count: 1,
            }]
        );
        cleanup(&root);
    }

    #[test]
    fn load_issue_count_saturates() {
        let mut store = PerformanceOperationStore::new();
        store.load_issues = vec![
            PerformanceOperationLoadIssue {
                kind: PerformanceOperationLoadIssueKind::StatusInvalid,
                count: usize::MAX,
            },
            PerformanceOperationLoadIssue {
                kind: PerformanceOperationLoadIssueKind::StatusUnreadable,
                count: 1,
            },
        ];

        assert_eq!(store.load_issue_count(), usize::MAX);
    }

    #[test]
    fn uppercase_operation_id_is_neither_loaded_nor_retained() {
        let root = test_root("uppercase-id");
        let dir = operation_dir(&test_paths(&root));
        fs::create_dir_all(&dir).expect("create operation dir");
        let id = format!("{PERFORMANCE_OPERATION_ID_PREFIX}0000000000000000000000000000000A");
        let status = test_status(&id, "instance-a", "install", "applying", test_payload());
        fs::write(
            dir.join(format!("{id}.json")),
            serde_json::to_vec_pretty(&PersistedPerformanceOperationStatus::from(status))
                .expect("serialize uppercase record"),
        )
        .expect("write uppercase record");

        let load_state = load_persisted_operation_inner(&dir);

        assert!(load_state.inner.operations.is_empty());
        assert!(load_state.rejected_records.is_empty());
        assert!(
            load_state.issues.iter().any(|issue| {
                issue.kind == PerformanceOperationLoadIssueKind::UnsafeOperationId
            })
        );
        cleanup(&root);
    }

    #[cfg(unix)]
    #[test]
    fn links_for_canonical_names_are_warning_only() {
        let root = test_root("hostile-links");
        let dir = operation_dir(&test_paths(&root));
        fs::create_dir_all(&dir).expect("create operation dir");
        let outside = root.join("outside.json");
        fs::write(&outside, b"{").expect("write outside record");
        let symlink_id = format!("{PERFORMANCE_OPERATION_ID_PREFIX}{:032x}", 1);
        std::os::unix::fs::symlink(&outside, operation_path(&dir, &symlink_id))
            .expect("create symlink");
        let hardlink_source = root.join("hardlink-source");
        fs::write(&hardlink_source, b"{").expect("write hardlink source");
        let hardlink_id = format!("{PERFORMANCE_OPERATION_ID_PREFIX}{:032x}", 2);
        fs::hard_link(&hardlink_source, operation_path(&dir, &hardlink_id))
            .expect("create hard link");

        let load_state = load_persisted_operation_inner(&dir);

        assert!(load_state.inner.operations.is_empty());
        assert!(load_state.rejected_records.is_empty());
        assert_eq!(
            load_state
                .issues
                .iter()
                .find(|issue| issue.kind == PerformanceOperationLoadIssueKind::StatusUnreadable)
                .map(|issue| issue.count),
            Some(2)
        );
        cleanup(&root);
    }

    #[test]
    fn operation_error_sanitizer_bounds_error() {
        let long = "x".repeat(MAX_OPERATION_ERROR_CHARS + 32);
        let error = sanitize_operation_error(&format!("failed; {long}"));

        assert!(error.len() <= MAX_OPERATION_ERROR_CHARS);
        assert!(!error.contains(';'));
        assert_eq!(sanitize_operation_error(""), "performance operation failed");
    }

    #[test]
    fn operation_error_sanitizer_rejects_sensitive_fragments() {
        let cases = [
            "provider returned token=secret-token",
            "refresh_token=secret-refresh-token",
            "Authorization: Bearer raw-provider-token",
            "java_path=C:\\Users\\Alice\\secret\\java.exe",
            "command failed with -Dauth_token=secret",
            "username=SecretPlayer",
        ];

        for case in cases {
            assert_eq!(
                sanitize_operation_error(case),
                "performance operation failed"
            );
        }
    }

    #[test]
    fn replace_file_preserves_existing_destination_when_source_is_missing() {
        let root = test_root("missing-source");
        fs::create_dir_all(&root).expect("create test root");
        let source = root.join("operation.json.tmp");
        let destination = root.join("operation.json");
        fs::write(&destination, b"{\"state\":\"existing\"}").expect("write destination");

        let error = replace_file(&source, &destination).expect_err("replace should fail");

        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert_eq!(
            fs::read(&destination).expect("destination should remain readable"),
            b"{\"state\":\"existing\"}"
        );
        assert!(!source.exists());

        cleanup(&root);
    }

    #[test]
    fn replace_file_preserves_directory_destination_on_failed_promotion() {
        let root = test_root("directory-destination");
        fs::create_dir_all(&root).expect("create test root");
        let source = root.join("operation.json.tmp");
        let destination = root.join("operation.json");
        fs::write(&source, b"{\"state\":\"replacement\"}").expect("write source");
        fs::create_dir(&destination).expect("create destination directory");

        replace_file(&source, &destination).expect_err("replace should fail");

        assert!(destination.is_dir());
        assert!(source.exists());

        cleanup(&root);
    }

    fn test_payload() -> PerformanceOperationPayload {
        PerformanceOperationPayload {
            game_version: None,
            loader: None,
            mode: None,
            rollback_id: None,
        }
    }

    fn test_status(
        id: &str,
        instance_id: &str,
        action: &str,
        state: &str,
        payload: PerformanceOperationPayload,
    ) -> PerformanceOperationStatus {
        PerformanceOperationStatus {
            id: id.to_string(),
            instance_id: instance_id.to_string(),
            action: action.to_string(),
            payload,
            state: state.to_string(),
            error: None,
            created_at: timestamp_utc(),
            updated_at: timestamp_utc(),
            journal_identity: Some(PerformanceOperationJournalIdentity::new(
                action,
                "test_performance_target",
                RollbackState::Unavailable,
            )),
        }
    }

    fn test_root(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "axial-performance-operation-{name}-{}-{nanos}",
            std::process::id()
        ))
    }

    fn test_paths(root: &Path) -> AppPaths {
        let config_dir = root.join("config");
        AppPaths {
            config_file: config_dir.join("config.json"),
            instances_file: config_dir.join("instances.json"),
            instances_dir: config_dir.join("instances"),
            music_dir: config_dir.join("music"),
            library_dir: config_dir.join("library"),
            config_dir,
        }
    }

    fn cleanup(root: &Path) {
        let _ = fs::remove_dir_all(root);
    }
}
