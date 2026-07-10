use crate::execution::file::{DeleteFileRequest, delete_launcher_managed_file, file_fact};
use crate::execution::persistence::{
    AcceptedWrite, AtomicSnapshotWriter, PersistenceCoordinator, PersistenceOwnerLease,
    WriteUrgency,
};
use crate::execution::{ExecutionFact, ExecutionFactKind};
use crate::logging::timestamp_utc;
use crate::observability::{
    RedactionAudience, sanitize_evidence_token, sanitize_public_diagnostic_text,
};
use crate::state::benchmark_suites::{
    BenchmarkSuiteRetentionClaims, BenchmarkSuiteRetentionHandle,
};
use crate::state::ownership::{CurrentArtifact, classify_current_artifact};
use axial_config::AppPaths;
use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as SyncMutex, RwLock};
use tokio::sync::{Mutex as AsyncMutex, watch};
use tracing::warn;

const MAX_DRIVER_ERROR_CHARS: usize = 160;
const DRIVER_ID_PREFIX: &str = "benchmark-suite-driver-";
const AUTOMATIC_RESUME_QUEUED_ERROR: &str = "driver automatic resume queued after restart";
const AUTOMATIC_RESUME_STARTED_ERROR: &str = "driver automatic resume started after restart";
const AUTOMATIC_RESUME_LIMIT_ERROR: &str = "driver ignored after restart resume limit";
const MAX_DRIVER_FILENAME_STEM: usize = 96;
const MAX_RESUMABLE_DRIVERS: usize = 8;
const MAX_RETAINED_TERMINAL_DRIVERS: usize = 32;
const MAX_DRIVER_RUNS: usize = 64;
const MIN_DRIVER_INTERVAL_MS: u64 = 5_000;
const MAX_DRIVER_INTERVAL_MS: u64 = 3_600_000;
const CRITICAL_RETRY_INITIAL_DELAY: std::time::Duration = std::time::Duration::from_millis(20);
const CRITICAL_RETRY_MAX_DELAY: std::time::Duration = std::time::Duration::from_secs(1);
const DRIVER_STORE_LOCK_INVARIANT: &str =
    "benchmark suite driver store lock poisoned; in-memory and persisted state may diverge";

#[derive(Debug, thiserror::Error)]
pub enum BenchmarkSuiteDriverStoreError {
    #[error("benchmark suite driver does not exist")]
    MissingDriver,
    #[error("benchmark suite driver is already terminal")]
    TerminalDriver,
    #[error("benchmark suite driver has a failed critical transition that must be retried")]
    RetryRequired,
    #[error("benchmark suite driver has no failed critical transition to retry")]
    RetryUnavailable,
    #[error("benchmark suite retention is changing")]
    RetentionConflict,
    #[error("benchmark suite driver persistence failed: {0}")]
    Persistence(#[source] io::Error),
}

impl BenchmarkSuiteDriverStoreError {
    pub const fn class(&self) -> &'static str {
        match self {
            Self::MissingDriver => "missing_driver",
            Self::TerminalDriver => "terminal_driver",
            Self::RetryRequired => "retry_required",
            Self::RetryUnavailable => "retry_unavailable",
            Self::RetentionConflict => "retention_conflict",
            Self::Persistence(_) => "persistence",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum BenchmarkSuiteDriverStartError {
    #[error("a benchmark suite driver is already active")]
    Conflict,
    #[error("benchmark suite driver {driver_id} could not start: {source}")]
    Store {
        driver_id: String,
        #[source]
        source: BenchmarkSuiteDriverStoreError,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BenchmarkSuiteDriverStatus {
    pub id: String,
    pub suite_id: String,
    pub mode: String,
    pub state: String,
    pub interval_ms: u64,
    pub run_count: usize,
    pub launched_run_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending_run_index: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_run_index: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BenchmarkSuiteDriverSuiteSummary {
    pub run_count: usize,
    pub launched_run_count: usize,
    pub pending_run_index: Option<usize>,
}

#[derive(Debug)]
pub struct BenchmarkSuiteDriverStart {
    pub status: BenchmarkSuiteDriverStatus,
    pub effect_owner: BenchmarkSuiteDriverEffectOwner,
}

#[derive(Debug)]
pub struct BenchmarkSuiteDriverEffectOwner {
    driver_id: String,
    suite_id: String,
    stop_rx: watch::Receiver<bool>,
    owners: Arc<SyncMutex<HashMap<String, String>>>,
}

impl BenchmarkSuiteDriverEffectOwner {
    pub fn stop_receiver(&self) -> watch::Receiver<bool> {
        self.stop_rx.clone()
    }
}

impl Drop for BenchmarkSuiteDriverEffectOwner {
    fn drop(&mut self) {
        let mut owners = self.owners.lock().expect(DRIVER_STORE_LOCK_INVARIANT);
        if owners
            .get(&self.suite_id)
            .is_some_and(|driver_id| driver_id == &self.driver_id)
        {
            owners.remove(&self.suite_id);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BenchmarkSuiteDriverLoadIssueKind {
    DirectoryUnreadable,
    DirectoryEntryUnreadable,
    StatusUnreadable,
    StatusInvalid,
    UnsafeDriverId,
    UnknownState,
    NonCanonicalFilename,
    DuplicateDriverId,
    ConflictingActiveSuite,
    TimestampInvalid,
    UnsafePublicField,
    IncoherentStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BenchmarkSuiteDriverLoadIssue {
    pub kind: BenchmarkSuiteDriverLoadIssueKind,
    pub count: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BenchmarkSuiteDriverRetentionIssueKind {
    WriterSettlement,
    Delete,
    BlockingTask,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BenchmarkSuiteDriverRetentionIssue {
    pub driver_id: String,
    pub kind: BenchmarkSuiteDriverRetentionIssueKind,
    pub facts: Vec<ExecutionFact>,
}

#[derive(Clone)]
struct BenchmarkSuiteDriverEntry {
    status: BenchmarkSuiteDriverStatus,
    stop_tx: watch::Sender<bool>,
}

#[derive(Default)]
struct BenchmarkSuiteDriverInner {
    next_id: u64,
    drivers: HashMap<String, BenchmarkSuiteDriverEntry>,
    active_by_suite: HashMap<String, String>,
    restart_candidates: Vec<BenchmarkSuiteDriverEntry>,
    ready_resume_ids: Vec<String>,
}

#[derive(Default)]
struct BenchmarkSuiteDriverLoadState {
    inner: BenchmarkSuiteDriverInner,
    issues: Vec<BenchmarkSuiteDriverLoadIssue>,
    retention_excluded_ids: HashSet<String>,
    suite_retention_claims: Vec<(String, String)>,
}

struct BenchmarkSuiteDriverPersistence {
    owner: PersistenceOwnerLease,
    storage_dir: PathBuf,
    writers: SyncMutex<HashMap<String, AtomicSnapshotWriter>>,
}

impl BenchmarkSuiteDriverPersistence {
    fn claim(
        storage_dir: &Path,
        coordinator: PersistenceCoordinator,
    ) -> Result<Self, BenchmarkSuiteDriverStoreError> {
        let owner = coordinator
            .claim_owner(storage_dir)
            .map_err(driver_persistence_error)?;
        Ok(Self {
            owner,
            storage_dir: storage_dir.to_path_buf(),
            writers: SyncMutex::new(HashMap::new()),
        })
    }

    fn writer(
        &self,
        driver_id: &str,
    ) -> Result<AtomicSnapshotWriter, BenchmarkSuiteDriverStoreError> {
        let mut writers = self.writers.lock().expect(DRIVER_STORE_LOCK_INVARIANT);
        if let Some(writer) = writers.get(driver_id) {
            return Ok(writer.clone());
        }
        let writer = self
            .owner
            .writer(
                driver_path(&self.storage_dir, driver_id),
                benchmark_suite_driver_target(driver_id),
            )
            .map_err(driver_persistence_error)?;
        writers.insert(driver_id.to_string(), writer.clone());
        Ok(writer)
    }

    async fn settle_writers(&self) -> Result<(), BenchmarkSuiteDriverStoreError> {
        let mut writers = self
            .writers
            .lock()
            .expect(DRIVER_STORE_LOCK_INVARIANT)
            .iter()
            .map(|(driver_id, writer)| (driver_id.clone(), writer.clone()))
            .collect::<Vec<_>>();
        writers.sort_by(|left, right| left.0.cmp(&right.0));
        for (_driver_id, writer) in writers {
            writer.settle().await.map_err(driver_persistence_error)?;
        }
        Ok(())
    }

    fn take_writer(
        &self,
        driver_id: &str,
    ) -> Result<AtomicSnapshotWriter, BenchmarkSuiteDriverStoreError> {
        if let Some(writer) = self
            .writers
            .lock()
            .expect(DRIVER_STORE_LOCK_INVARIANT)
            .remove(driver_id)
        {
            return Ok(writer);
        }
        self.owner
            .writer(
                driver_path(&self.storage_dir, driver_id),
                benchmark_suite_driver_target(driver_id),
            )
            .map_err(driver_persistence_error)
    }

    fn restore_writer(&self, driver_id: &str, writer: AtomicSnapshotWriter) {
        self.writers
            .lock()
            .expect(DRIVER_STORE_LOCK_INVARIANT)
            .insert(driver_id.to_string(), writer);
    }

    #[cfg(test)]
    fn writer_count(&self) -> usize {
        self.writers
            .lock()
            .expect(DRIVER_STORE_LOCK_INVARIANT)
            .len()
    }
}

pub struct BenchmarkSuiteDriverStore {
    inner: Arc<RwLock<BenchmarkSuiteDriverInner>>,
    mutation_gate: Arc<AsyncMutex<()>>,
    persistence: Option<Arc<BenchmarkSuiteDriverPersistence>>,
    retry_candidates: Arc<SyncMutex<HashMap<String, BenchmarkSuiteDriverEntry>>>,
    retention_issues: Arc<SyncMutex<HashMap<String, BenchmarkSuiteDriverRetentionIssue>>>,
    handoff_obligation_ids: Arc<SyncMutex<HashSet<String>>>,
    effect_owners: Arc<SyncMutex<HashMap<String, String>>>,
    retention_excluded_ids: Arc<HashSet<String>>,
    suite_retention_claims: BenchmarkSuiteRetentionClaims,
    suite_retention: BenchmarkSuiteRetentionHandle,
    load_issues: Vec<BenchmarkSuiteDriverLoadIssue>,
}

pub(crate) struct PreparedBenchmarkSuiteDriverStore {
    storage_dir: PathBuf,
    load_state: BenchmarkSuiteDriverLoadState,
    suite_retention_claims: BenchmarkSuiteRetentionClaims,
}

impl PreparedBenchmarkSuiteDriverStore {
    pub(crate) fn bind(
        self,
        suite_retention: BenchmarkSuiteRetentionHandle,
    ) -> BenchmarkSuiteDriverStore {
        BenchmarkSuiteDriverStore::finish_load(
            self,
            PersistenceCoordinator::global(),
            suite_retention,
        )
        .unwrap_or_else(|error| {
            panic!("failed to initialize benchmark suite driver persistence: {error}")
        })
    }
}

impl BenchmarkSuiteDriverStore {
    #[cfg(test)]
    pub fn new() -> Self {
        Self::new_with_retention_claims(BenchmarkSuiteRetentionClaims::default())
    }

    #[cfg(test)]
    fn new_with_retention_claims(suite_retention_claims: BenchmarkSuiteRetentionClaims) -> Self {
        let suite_retention =
            crate::state::benchmark_suites::BenchmarkSuiteStore::new_with_retention_claims(
                suite_retention_claims.clone(),
            )
            .retention_handle();
        Self::new_with_retention(suite_retention_claims, suite_retention)
    }

    #[cfg(test)]
    fn new_with_retention(
        suite_retention_claims: BenchmarkSuiteRetentionClaims,
        suite_retention: BenchmarkSuiteRetentionHandle,
    ) -> Self {
        Self {
            inner: Arc::new(RwLock::new(BenchmarkSuiteDriverInner::default())),
            mutation_gate: Arc::new(AsyncMutex::new(())),
            persistence: None,
            retry_candidates: Arc::new(SyncMutex::new(HashMap::new())),
            retention_issues: Arc::new(SyncMutex::new(HashMap::new())),
            handoff_obligation_ids: Arc::new(SyncMutex::new(HashSet::new())),
            effect_owners: Arc::new(SyncMutex::new(HashMap::new())),
            retention_excluded_ids: Arc::new(HashSet::new()),
            suite_retention_claims,
            suite_retention,
            load_issues: Vec::new(),
        }
    }

    #[cfg(test)]
    pub fn load_from_paths(paths: &AppPaths) -> Self {
        Self::load_from_paths_with_retention_claims(paths, BenchmarkSuiteRetentionClaims::default())
    }

    pub(crate) fn prepare_load_from_paths(
        paths: &AppPaths,
        suite_retention_claims: BenchmarkSuiteRetentionClaims,
    ) -> PreparedBenchmarkSuiteDriverStore {
        Self::prepare_load(paths, suite_retention_claims).unwrap_or_else(|error| {
            panic!("failed to prepare benchmark suite driver persistence: {error}")
        })
    }

    fn prepare_load(
        paths: &AppPaths,
        suite_retention_claims: BenchmarkSuiteRetentionClaims,
    ) -> Result<PreparedBenchmarkSuiteDriverStore, BenchmarkSuiteDriverStoreError> {
        let storage_dir = driver_dir(paths);
        let load_state = load_persisted_driver_inner(&storage_dir);
        for (driver_id, suite_id) in &load_state.suite_retention_claims {
            suite_retention_claims
                .claim(driver_id, suite_id)
                .map_err(|_| BenchmarkSuiteDriverStoreError::RetentionConflict)?;
        }
        Ok(PreparedBenchmarkSuiteDriverStore {
            storage_dir,
            load_state,
            suite_retention_claims,
        })
    }

    #[cfg(test)]
    pub(crate) fn load_from_paths_with_retention_claims(
        paths: &AppPaths,
        suite_retention_claims: BenchmarkSuiteRetentionClaims,
    ) -> Self {
        let prepared =
            Self::prepare_load(paths, suite_retention_claims.clone()).unwrap_or_else(|error| {
                panic!("failed to prepare benchmark suite driver persistence: {error}")
            });
        let suite_retention =
            crate::state::benchmark_suites::BenchmarkSuiteStore::new_with_retention_claims(
                suite_retention_claims,
            )
            .retention_handle();
        Self::finish_load(prepared, PersistenceCoordinator::global(), suite_retention)
            .unwrap_or_else(|error| {
                panic!("failed to initialize benchmark suite driver persistence: {error}")
            })
    }

    #[cfg(test)]
    fn try_load_from_paths_with_coordinator(
        paths: &AppPaths,
        coordinator: PersistenceCoordinator,
    ) -> Result<Self, BenchmarkSuiteDriverStoreError> {
        Self::try_load_from_paths_with_coordinator_and_retention_claims(
            paths,
            coordinator,
            BenchmarkSuiteRetentionClaims::default(),
        )
    }

    #[cfg(test)]
    fn try_load_from_paths_with_coordinator_and_retention_claims(
        paths: &AppPaths,
        coordinator: PersistenceCoordinator,
        suite_retention_claims: BenchmarkSuiteRetentionClaims,
    ) -> Result<Self, BenchmarkSuiteDriverStoreError> {
        let prepared = Self::prepare_load(paths, suite_retention_claims.clone())?;
        let suite_retention =
            crate::state::benchmark_suites::BenchmarkSuiteStore::new_with_retention_claims(
                suite_retention_claims,
            )
            .retention_handle();
        Self::finish_load(prepared, coordinator, suite_retention)
    }

    fn finish_load(
        prepared: PreparedBenchmarkSuiteDriverStore,
        coordinator: PersistenceCoordinator,
        suite_retention: BenchmarkSuiteRetentionHandle,
    ) -> Result<Self, BenchmarkSuiteDriverStoreError> {
        let PreparedBenchmarkSuiteDriverStore {
            storage_dir,
            load_state,
            suite_retention_claims,
        } = prepared;
        Ok(Self {
            inner: Arc::new(RwLock::new(load_state.inner)),
            mutation_gate: Arc::new(AsyncMutex::new(())),
            persistence: Some(Arc::new(BenchmarkSuiteDriverPersistence::claim(
                &storage_dir,
                coordinator,
            )?)),
            retry_candidates: Arc::new(SyncMutex::new(HashMap::new())),
            retention_issues: Arc::new(SyncMutex::new(HashMap::new())),
            handoff_obligation_ids: Arc::new(SyncMutex::new(HashSet::new())),
            effect_owners: Arc::new(SyncMutex::new(HashMap::new())),
            retention_excluded_ids: Arc::new(load_state.retention_excluded_ids),
            suite_retention_claims,
            suite_retention,
            load_issues: load_state.issues,
        })
    }

    pub async fn start(
        &self,
        suite_id: String,
        mode: String,
        interval_ms: u64,
        summary: BenchmarkSuiteDriverSuiteSummary,
    ) -> Result<BenchmarkSuiteDriverStart, BenchmarkSuiteDriverStartError> {
        let mutation = self.mutation_gate.clone().lock_owned().await;
        let (candidate, effect_owner) = {
            let mut inner = self.inner.write().expect(DRIVER_STORE_LOCK_INVARIANT);
            if let Some(existing_id) = inner.active_by_suite.get(&suite_id)
                && inner
                    .drivers
                    .get(existing_id)
                    .map(|entry| is_non_terminal(&entry.status.state))
                    .unwrap_or(false)
            {
                return Err(BenchmarkSuiteDriverStartError::Conflict);
            }
            if inner
                .restart_candidates
                .iter()
                .any(|candidate| candidate.status.suite_id == suite_id)
                || self
                    .retry_candidates
                    .lock()
                    .expect(DRIVER_STORE_LOCK_INVARIANT)
                    .values()
                    .any(|candidate| candidate.status.suite_id == suite_id)
                || self
                    .effect_owners
                    .lock()
                    .expect(DRIVER_STORE_LOCK_INVARIANT)
                    .contains_key(&suite_id)
            {
                return Err(BenchmarkSuiteDriverStartError::Conflict);
            }

            inner.next_id = inner.next_id.saturating_add(1);
            let id = format!("{DRIVER_ID_PREFIX}{:016x}", inner.next_id);
            let now = timestamp_utc();
            let (stop_tx, stop_rx) = watch::channel(false);
            let status = BenchmarkSuiteDriverStatus {
                id: id.clone(),
                suite_id: suite_id.clone(),
                mode,
                state: "scheduled".to_string(),
                interval_ms,
                run_count: summary.run_count,
                launched_run_count: summary.launched_run_count,
                pending_run_index: summary.pending_run_index,
                active_session_id: None,
                last_run_index: None,
                last_session_id: None,
                error: None,
                created_at: now.clone(),
                updated_at: now,
            };
            self.effect_owners
                .lock()
                .expect(DRIVER_STORE_LOCK_INVARIANT)
                .insert(suite_id.clone(), id.clone());
            let effect_owner = BenchmarkSuiteDriverEffectOwner {
                driver_id: id,
                suite_id,
                stop_rx,
                owners: self.effect_owners.clone(),
            };
            (BenchmarkSuiteDriverEntry { status, stop_tx }, effect_owner)
        };
        let driver_id = candidate.status.id.clone();
        let suite_id = candidate.status.suite_id.clone();
        let status = candidate.status.clone();
        if self
            .suite_retention_claims
            .claim(&driver_id, &suite_id)
            .is_err()
        {
            return Err(BenchmarkSuiteDriverStartError::Conflict);
        }
        if let Err(source) = self.commit_transition(candidate, mutation).await {
            if !self.has_retry_candidate(&driver_id)
                && !self.transition_matches(&status)
                && self.suite_retention_claims.release(&driver_id, &suite_id)
            {
                self.suite_retention.retry_detached().await;
            }
            return Err(BenchmarkSuiteDriverStartError::Store { driver_id, source });
        }
        Ok(BenchmarkSuiteDriverStart {
            status,
            effect_owner,
        })
    }

    pub async fn get(&self, id: &str) -> Option<BenchmarkSuiteDriverStatus> {
        self.inner
            .read()
            .expect(DRIVER_STORE_LOCK_INVARIANT)
            .drivers
            .get(id)
            .map(|entry| entry.status.clone())
    }

    pub fn load_issues(&self) -> Vec<BenchmarkSuiteDriverLoadIssue> {
        self.load_issues.clone()
    }

    pub fn load_issue_count(&self) -> usize {
        self.load_issues.iter().map(|issue| issue.count).sum()
    }

    pub async fn take_restart_interrupted_resumable_drivers(
        &self,
    ) -> Result<Vec<BenchmarkSuiteDriverStatus>, BenchmarkSuiteDriverStoreError> {
        loop {
            let mutation = self.mutation_gate.clone().lock_owned().await;
            let candidate = self
                .inner
                .read()
                .expect(DRIVER_STORE_LOCK_INVARIANT)
                .restart_candidates
                .first()
                .cloned();
            let Some(candidate) = candidate else {
                drop(mutation);
                break;
            };
            self.commit_transition(candidate, mutation).await?;
        }

        let _mutation = self.mutation_gate.lock().await;
        self.prune_terminal_drivers().await;
        let mut inner = self.inner.write().expect(DRIVER_STORE_LOCK_INVARIANT);
        let ids = std::mem::take(&mut inner.ready_resume_ids);
        Ok(ids
            .into_iter()
            .filter_map(|id| inner.drivers.get(&id).map(|entry| entry.status.clone()))
            .collect())
    }

    pub async fn record_restart_resume_started(
        &self,
        id: &str,
    ) -> Result<(), BenchmarkSuiteDriverStoreError> {
        self.update_restart_resume_consumed_error(id, AUTOMATIC_RESUME_STARTED_ERROR.to_string())
            .await
    }

    pub(crate) async fn consume_restart_handoff_started(
        &self,
        id: &str,
    ) -> Result<bool, BenchmarkSuiteDriverStoreError> {
        if !self
            .handoff_obligation_ids
            .lock()
            .expect(DRIVER_STORE_LOCK_INVARIANT)
            .contains(id)
        {
            return Ok(false);
        }
        self.record_restart_resume_started(id).await?;
        Ok(true)
    }

    pub async fn record_restart_resume_failed(
        &self,
        id: &str,
        error: &str,
    ) -> Result<(), BenchmarkSuiteDriverStoreError> {
        let error = sanitize_driver_error(error);
        self.update_restart_resume_consumed_error(
            id,
            format!("driver automatic resume failed: {error}"),
        )
        .await
    }

    pub async fn stop(
        &self,
        id: &str,
    ) -> Result<BenchmarkSuiteDriverStatus, BenchmarkSuiteDriverStoreError> {
        let mutation = self.mutation_gate.clone().lock_owned().await;
        let mut candidate = self.visible_entry_for_update(id)?;
        if !is_non_terminal(&candidate.status.state) {
            return Err(BenchmarkSuiteDriverStoreError::TerminalDriver);
        }
        let _ = candidate.stop_tx.send(true);
        candidate.status.state = "stopped".to_string();
        candidate.status.active_session_id = None;
        candidate.status.updated_at = timestamp_utc();
        let status = candidate.status.clone();
        self.commit_transition(candidate, mutation).await?;
        Ok(status)
    }

    pub async fn list_recent(&self, limit: usize) -> Vec<BenchmarkSuiteDriverStatus> {
        let mut drivers = self
            .inner
            .read()
            .expect(DRIVER_STORE_LOCK_INVARIANT)
            .drivers
            .values()
            .map(|entry| entry.status.clone())
            .collect::<Vec<_>>();
        drivers.sort_by(|left, right| {
            right
                .updated_at
                .cmp(&left.updated_at)
                .then_with(|| right.id.cmp(&left.id))
        });
        drivers.truncate(limit);
        drivers
    }

    pub async fn record_active(
        &self,
        id: &str,
        summary: BenchmarkSuiteDriverSuiteSummary,
        active_session_id: Option<String>,
    ) -> Result<(), BenchmarkSuiteDriverStoreError> {
        self.update_non_terminal(id, |status| {
            status.state = "active".to_string();
            apply_summary(status, summary);
            status.active_session_id = active_session_id;
            status.error = None;
        })
        .await
    }

    pub async fn record_launched(
        &self,
        id: &str,
        summary: BenchmarkSuiteDriverSuiteSummary,
        run_index: usize,
        session_id: Option<String>,
    ) -> Result<(), BenchmarkSuiteDriverStoreError> {
        self.update_non_terminal(id, |status| {
            status.state = "launched_next".to_string();
            apply_summary(status, summary);
            status.active_session_id = None;
            status.last_run_index = Some(run_index);
            status.last_session_id = session_id;
            status.error = None;
        })
        .await
    }

    pub async fn record_complete(
        &self,
        id: &str,
        summary: BenchmarkSuiteDriverSuiteSummary,
    ) -> Result<(), BenchmarkSuiteDriverStoreError> {
        self.update_terminal(id, "complete", None, Some(summary))
            .await
    }

    pub async fn record_failed(
        &self,
        id: &str,
        error: &str,
    ) -> Result<(), BenchmarkSuiteDriverStoreError> {
        self.update_terminal(id, "failed", Some(sanitize_driver_error(error)), None)
            .await
    }

    pub async fn record_stopped(&self, id: &str) -> Result<(), BenchmarkSuiteDriverStoreError> {
        self.update_terminal(id, "stopped", None, None).await
    }

    async fn update_non_terminal(
        &self,
        id: &str,
        update: impl FnOnce(&mut BenchmarkSuiteDriverStatus),
    ) -> Result<(), BenchmarkSuiteDriverStoreError> {
        let _mutation = self.mutation_gate.lock().await;
        let mut candidate = self.visible_entry_for_update(id)?;
        if !is_non_terminal(&candidate.status.state) {
            return Err(BenchmarkSuiteDriverStoreError::TerminalDriver);
        }
        update(&mut candidate.status);
        candidate.status.updated_at = timestamp_utc();
        self.accept_progress(candidate)
    }

    async fn update_terminal(
        &self,
        id: &str,
        state: &str,
        error: Option<String>,
        summary: Option<BenchmarkSuiteDriverSuiteSummary>,
    ) -> Result<(), BenchmarkSuiteDriverStoreError> {
        let mutation = self.mutation_gate.clone().lock_owned().await;
        let mut candidate = self.visible_entry_for_update(id)?;
        if !is_non_terminal(&candidate.status.state) {
            return Err(BenchmarkSuiteDriverStoreError::TerminalDriver);
        }
        if let Some(summary) = summary {
            apply_summary(&mut candidate.status, summary);
        }
        candidate.status.state = state.to_string();
        candidate.status.active_session_id = None;
        candidate.status.error = error;
        candidate.status.updated_at = timestamp_utc();
        self.commit_transition(candidate, mutation).await
    }

    async fn update_restart_resume_consumed_error(
        &self,
        id: &str,
        error: String,
    ) -> Result<(), BenchmarkSuiteDriverStoreError> {
        let mutation = self.mutation_gate.clone().lock_owned().await;
        let mut candidate = self.visible_entry_for_update(id)?;
        if candidate.status.state != "interrupted"
            || !matches!(
                candidate.status.error.as_deref(),
                Some(AUTOMATIC_RESUME_QUEUED_ERROR) | Some(AUTOMATIC_RESUME_STARTED_ERROR)
            )
        {
            return Err(BenchmarkSuiteDriverStoreError::TerminalDriver);
        }
        candidate.status.error = Some(sanitize_driver_error(&error));
        candidate.status.updated_at = timestamp_utc();
        self.commit_transition(candidate, mutation).await
    }

    fn visible_entry_for_update(
        &self,
        id: &str,
    ) -> Result<BenchmarkSuiteDriverEntry, BenchmarkSuiteDriverStoreError> {
        if self
            .retry_candidates
            .lock()
            .expect(DRIVER_STORE_LOCK_INVARIANT)
            .contains_key(id)
        {
            return Err(BenchmarkSuiteDriverStoreError::RetryRequired);
        }
        self.inner
            .read()
            .expect(DRIVER_STORE_LOCK_INVARIANT)
            .drivers
            .get(id)
            .cloned()
            .ok_or(BenchmarkSuiteDriverStoreError::MissingDriver)
    }

    fn accept_progress(
        &self,
        candidate: BenchmarkSuiteDriverEntry,
    ) -> Result<(), BenchmarkSuiteDriverStoreError> {
        if let Some(persistence) = &self.persistence {
            persistence
                .writer(&candidate.status.id)?
                .accept(
                    candidate.status.clone(),
                    WriteUrgency::Debounced,
                    encode_driver_status,
                )
                .map_err(driver_persistence_error)?;
        }
        let released_suite_claim = apply_driver_transition(
            &mut self.inner.write().expect(DRIVER_STORE_LOCK_INVARIANT),
            candidate,
            &self.handoff_obligation_ids,
            &self.suite_retention_claims,
        );
        debug_assert!(!released_suite_claim);
        Ok(())
    }

    async fn commit_transition(
        &self,
        candidate: BenchmarkSuiteDriverEntry,
        mutation: tokio::sync::OwnedMutexGuard<()>,
    ) -> Result<(), BenchmarkSuiteDriverStoreError> {
        let expected = candidate.status.clone();
        match self.commit_transition_once(candidate, mutation).await {
            Ok(()) => Ok(()),
            Err(error @ BenchmarkSuiteDriverStoreError::Persistence(_))
            | Err(error @ BenchmarkSuiteDriverStoreError::RetryRequired) => {
                self.reconcile_critical_transition(&expected, error).await
            }
            Err(error) => Err(error),
        }
    }

    async fn commit_transition_once(
        &self,
        candidate: BenchmarkSuiteDriverEntry,
        mutation: tokio::sync::OwnedMutexGuard<()>,
    ) -> Result<(), BenchmarkSuiteDriverStoreError> {
        let driver_id = candidate.status.id.clone();
        if self
            .retry_candidates
            .lock()
            .expect(DRIVER_STORE_LOCK_INVARIANT)
            .contains_key(&driver_id)
        {
            return Err(BenchmarkSuiteDriverStoreError::RetryRequired);
        }
        let Some(persistence) = &self.persistence else {
            let terminal = !is_non_terminal(&candidate.status.state);
            let released_suite_claim = apply_driver_transition(
                &mut self.inner.write().expect(DRIVER_STORE_LOCK_INVARIANT),
                candidate,
                &self.handoff_obligation_ids,
                &self.suite_retention_claims,
            );
            if released_suite_claim {
                self.suite_retention.retry_detached().await;
            }
            if terminal {
                self.prune_terminal_drivers().await;
            }
            drop(mutation);
            return Ok(());
        };
        let ticket = persistence
            .writer(&driver_id)?
            .accept(
                candidate.status.clone(),
                WriteUrgency::Immediate,
                encode_driver_status,
            )
            .map_err(driver_persistence_error)?;
        self.await_commit(candidate, ticket, mutation).await
    }

    async fn reconcile_critical_transition(
        &self,
        expected: &BenchmarkSuiteDriverStatus,
        mut error: BenchmarkSuiteDriverStoreError,
    ) -> Result<(), BenchmarkSuiteDriverStoreError> {
        let mut delay = CRITICAL_RETRY_INITIAL_DELAY;
        loop {
            if self.transition_matches(expected) {
                return Ok(());
            }
            if !self.has_retry_candidate(&expected.id) {
                return Err(error);
            }

            warn!(
                error_class = error.class(),
                "benchmark suite driver critical transition reconciliation failed; retrying"
            );
            tokio::time::sleep(delay).await;
            delay = delay.saturating_mul(2).min(CRITICAL_RETRY_MAX_DELAY);

            match self.retry_critical(&expected.id).await {
                Ok(()) | Err(BenchmarkSuiteDriverStoreError::RetryUnavailable) => {}
                Err(next_error @ BenchmarkSuiteDriverStoreError::Persistence(_))
                | Err(next_error @ BenchmarkSuiteDriverStoreError::RetryRequired) => {
                    error = next_error;
                }
                Err(next_error) => return Err(next_error),
            }
        }
    }

    fn transition_matches(&self, expected: &BenchmarkSuiteDriverStatus) -> bool {
        self.inner
            .read()
            .expect(DRIVER_STORE_LOCK_INVARIANT)
            .drivers
            .get(&expected.id)
            .is_some_and(|entry| entry.status == *expected)
    }

    fn has_retry_candidate(&self, id: &str) -> bool {
        self.retry_candidates
            .lock()
            .expect(DRIVER_STORE_LOCK_INVARIANT)
            .contains_key(id)
    }

    async fn await_commit(
        &self,
        candidate: BenchmarkSuiteDriverEntry,
        ticket: AcceptedWrite,
        mutation: tokio::sync::OwnedMutexGuard<()>,
    ) -> Result<(), BenchmarkSuiteDriverStoreError> {
        let mutation = self
            .await_commit_holding_gate(candidate, ticket, mutation)
            .await?;
        drop(mutation);
        Ok(())
    }

    async fn await_commit_holding_gate(
        &self,
        candidate: BenchmarkSuiteDriverEntry,
        ticket: AcceptedWrite,
        mutation: tokio::sync::OwnedMutexGuard<()>,
    ) -> Result<tokio::sync::OwnedMutexGuard<()>, BenchmarkSuiteDriverStoreError> {
        let inner = self.inner.clone();
        let retry_candidates = self.retry_candidates.clone();
        let persistence = self.persistence.clone();
        let retention_issues = self.retention_issues.clone();
        let handoff_obligation_ids = self.handoff_obligation_ids.clone();
        let suite_retention_claims = self.suite_retention_claims.clone();
        let suite_retention = self.suite_retention.clone();
        let retention_excluded_ids = self.retention_excluded_ids.clone();
        let driver_id = candidate.status.id.clone();
        let (completed_tx, completed_rx) = tokio::sync::oneshot::channel();
        ticket.observe_async(move |result| async move {
            let result = match result {
                Ok(_) => {
                    let terminal = !is_non_terminal(&candidate.status.state);
                    let released_suite_claim = apply_driver_transition(
                        &mut inner.write().expect(DRIVER_STORE_LOCK_INVARIANT),
                        candidate,
                        &handoff_obligation_ids,
                        &suite_retention_claims,
                    );
                    if released_suite_claim {
                        suite_retention.retry_detached().await;
                    }
                    retry_candidates
                        .lock()
                        .expect(DRIVER_STORE_LOCK_INVARIANT)
                        .remove(&driver_id);
                    if terminal {
                        prune_terminal_drivers(
                            inner,
                            persistence,
                            retry_candidates,
                            retention_issues,
                            handoff_obligation_ids,
                            retention_excluded_ids,
                        )
                        .await;
                    }
                    Ok(())
                }
                Err(error) => {
                    retry_candidates
                        .lock()
                        .expect(DRIVER_STORE_LOCK_INVARIANT)
                        .insert(driver_id, candidate);
                    Err(driver_persistence_error(error))
                }
            };
            let _ = completed_tx.send((result, mutation));
        });
        let (result, mutation) = completed_rx.await.map_err(|_| {
            BenchmarkSuiteDriverStoreError::Persistence(io::Error::other(
                "benchmark suite driver commit observer stopped",
            ))
        })?;
        result?;
        Ok(mutation)
    }

    pub async fn retry_critical(&self, id: &str) -> Result<(), BenchmarkSuiteDriverStoreError> {
        let mutation = self.mutation_gate.clone().lock_owned().await;
        let candidate = self
            .retry_candidates
            .lock()
            .expect(DRIVER_STORE_LOCK_INVARIANT)
            .get(id)
            .cloned()
            .ok_or(BenchmarkSuiteDriverStoreError::RetryUnavailable)?;
        let persistence = self
            .persistence
            .as_ref()
            .ok_or(BenchmarkSuiteDriverStoreError::RetryUnavailable)?;
        let ticket = persistence
            .writer(id)?
            .retry()
            .map_err(driver_persistence_error)?;
        self.await_commit(candidate, ticket, mutation).await
    }

    pub async fn flush(&self) -> Result<(), BenchmarkSuiteDriverStoreError> {
        let mutation = self.mutation_gate.clone().lock_owned().await;
        let _mutation = self.retry_retained_candidates_once(mutation).await?;
        self.prune_terminal_drivers().await;
        if let Some(persistence) = &self.persistence {
            persistence.settle_writers().await?;
            persistence
                .owner
                .flush()
                .await
                .map_err(driver_persistence_error)?;
        }
        if !self.retention_issues().is_empty() {
            return Err(BenchmarkSuiteDriverStoreError::Persistence(
                io::Error::other("benchmark suite driver terminal retention cleanup is pending"),
            ));
        }
        Ok(())
    }

    pub async fn close(&self) -> Result<(), BenchmarkSuiteDriverStoreError> {
        let mutation = self.mutation_gate.clone().lock_owned().await;
        let _mutation = self.retry_retained_candidates_once(mutation).await?;
        self.prune_terminal_drivers().await;
        if let Some(persistence) = &self.persistence {
            persistence.settle_writers().await?;
            persistence
                .owner
                .flush()
                .await
                .map_err(driver_persistence_error)?;
        }
        if !self.retention_issues().is_empty() {
            return Err(BenchmarkSuiteDriverStoreError::Persistence(
                io::Error::other("benchmark suite driver terminal retention cleanup is pending"),
            ));
        }
        if let Some(persistence) = &self.persistence {
            persistence
                .owner
                .close()
                .await
                .map_err(driver_persistence_error)?;
        }
        Ok(())
    }

    pub fn retention_issues(&self) -> Vec<BenchmarkSuiteDriverRetentionIssue> {
        let mut issues = self
            .retention_issues
            .lock()
            .expect(DRIVER_STORE_LOCK_INVARIANT)
            .values()
            .cloned()
            .collect::<Vec<_>>();
        issues.sort_by(|left, right| left.driver_id.cmp(&right.driver_id));
        issues
    }

    pub async fn retry_terminal_retention(&self) -> Vec<BenchmarkSuiteDriverRetentionIssue> {
        let _mutation = self.mutation_gate.lock().await;
        self.prune_terminal_drivers().await;
        self.retention_issues()
    }

    async fn prune_terminal_drivers(&self) {
        prune_terminal_drivers(
            self.inner.clone(),
            self.persistence.clone(),
            self.retry_candidates.clone(),
            self.retention_issues.clone(),
            self.handoff_obligation_ids.clone(),
            self.retention_excluded_ids.clone(),
        )
        .await;
    }

    async fn retry_retained_candidates_once(
        &self,
        mut mutation: tokio::sync::OwnedMutexGuard<()>,
    ) -> Result<tokio::sync::OwnedMutexGuard<()>, BenchmarkSuiteDriverStoreError> {
        let mut ids = self
            .retry_candidates
            .lock()
            .expect(DRIVER_STORE_LOCK_INVARIANT)
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        ids.sort();
        for id in ids {
            let candidate = self
                .retry_candidates
                .lock()
                .expect(DRIVER_STORE_LOCK_INVARIANT)
                .get(&id)
                .cloned();
            let Some(candidate) = candidate else {
                continue;
            };
            let persistence = self
                .persistence
                .as_ref()
                .ok_or(BenchmarkSuiteDriverStoreError::RetryUnavailable)?;
            let ticket = persistence
                .writer(&id)?
                .retry()
                .map_err(driver_persistence_error)?;
            mutation = self
                .await_commit_holding_gate(candidate, ticket, mutation)
                .await?;
        }
        Ok(mutation)
    }

    #[cfg(test)]
    fn retry_candidate_ids(&self) -> Vec<String> {
        self.retry_candidates
            .lock()
            .expect(DRIVER_STORE_LOCK_INVARIANT)
            .keys()
            .cloned()
            .collect()
    }
}

#[cfg(test)]
impl Default for BenchmarkSuiteDriverStore {
    fn default() -> Self {
        Self::new()
    }
}

fn apply_summary(
    status: &mut BenchmarkSuiteDriverStatus,
    summary: BenchmarkSuiteDriverSuiteSummary,
) {
    status.run_count = summary.run_count;
    status.launched_run_count = summary.launched_run_count;
    status.pending_run_index = summary.pending_run_index;
}

fn is_non_terminal(state: &str) -> bool {
    !matches!(state, "complete" | "failed" | "stopped" | "interrupted")
}

fn is_restart_queued_marker(status: &BenchmarkSuiteDriverStatus) -> bool {
    status.state == "interrupted" && status.error.as_deref() == Some(AUTOMATIC_RESUME_QUEUED_ERROR)
}

fn is_restart_recoverable_marker(status: &BenchmarkSuiteDriverStatus) -> bool {
    is_restart_queued_marker(status)
}

fn is_known_driver_state(state: &str) -> bool {
    matches!(
        state,
        "scheduled"
            | "active"
            | "launched_next"
            | "complete"
            | "failed"
            | "stopped"
            | "interrupted"
    )
}

fn normalize_and_validate_loaded_status(
    status: &mut BenchmarkSuiteDriverStatus,
) -> Result<(), BenchmarkSuiteDriverLoadIssueKind> {
    if !is_known_driver_state(&status.state) {
        return Err(BenchmarkSuiteDriverLoadIssueKind::UnknownState);
    }
    let created_at = normalize_driver_timestamp(&mut status.created_at)
        .ok_or(BenchmarkSuiteDriverLoadIssueKind::TimestampInvalid)?;
    let updated_at = normalize_driver_timestamp(&mut status.updated_at)
        .ok_or(BenchmarkSuiteDriverLoadIssueKind::TimestampInvalid)?;
    if !is_canonical_suite_id(&status.suite_id)
        || !matches!(
            status.mode.as_str(),
            "development" | "qualification" | "release_validation"
        )
        || status
            .active_session_id
            .as_deref()
            .is_some_and(|value| !is_safe_public_token(value))
        || status
            .last_session_id
            .as_deref()
            .is_some_and(|value| !is_safe_public_token(value))
    {
        return Err(BenchmarkSuiteDriverLoadIssueKind::UnsafePublicField);
    }
    let counts_coherent = status.run_count > 0
        && status.run_count <= MAX_DRIVER_RUNS
        && status.launched_run_count <= status.run_count
        && status
            .pending_run_index
            .is_none_or(|index| index < status.run_count)
        && status
            .last_run_index
            .is_none_or(|index| index < status.run_count)
        && (status.last_session_id.is_none() || status.last_run_index.is_some());
    let state_coherent = match status.state.as_str() {
        "scheduled" => status.active_session_id.is_none() && status.pending_run_index.is_some(),
        "active" => status.active_session_id.is_some(),
        "launched_next" => status.active_session_id.is_none() && status.last_run_index.is_some(),
        "complete" => status.active_session_id.is_none() && status.pending_run_index.is_none(),
        "failed" | "stopped" | "interrupted" => status.active_session_id.is_none(),
        _ => false,
    };
    if created_at > updated_at
        || !(MIN_DRIVER_INTERVAL_MS..=MAX_DRIVER_INTERVAL_MS).contains(&status.interval_ms)
        || !counts_coherent
        || !state_coherent
    {
        return Err(BenchmarkSuiteDriverLoadIssueKind::IncoherentStatus);
    }
    Ok(())
}

fn normalize_driver_timestamp(value: &mut String) -> Option<DateTime<Utc>> {
    let parsed = DateTime::parse_from_rfc3339(value.trim())
        .ok()?
        .with_timezone(&Utc);
    *value = parsed.to_rfc3339_opts(SecondsFormat::AutoSi, true);
    Some(parsed)
}

fn is_canonical_suite_id(value: &str) -> bool {
    crate::state::benchmark_suites::normalize_suite_id(value).as_deref() == Some(value)
}

fn is_safe_public_token(value: &str) -> bool {
    sanitize_evidence_token(value, RedactionAudience::UserVisible, 96).as_deref() == Some(value)
}

fn load_persisted_driver_inner(storage_dir: &Path) -> BenchmarkSuiteDriverLoadState {
    let mut load_state = BenchmarkSuiteDriverLoadState::default();
    let entries = match fs::read_dir(storage_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return load_state,
        Err(error) => {
            warn!(
                error_kind = ?error.kind(),
                "failed to read benchmark suite driver status directory"
            );
            record_load_issue(
                &mut load_state.issues,
                BenchmarkSuiteDriverLoadIssueKind::DirectoryUnreadable,
            );
            return load_state;
        }
    };

    let mut paths = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                warn!(
                    error_kind = ?error.kind(),
                    "failed to read benchmark suite driver status directory entry"
                );
                record_load_issue(
                    &mut load_state.issues,
                    BenchmarkSuiteDriverLoadIssueKind::DirectoryEntryUnreadable,
                );
                continue;
            }
        };
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        paths.push(path);
    }
    paths.sort();

    let mut candidates = BTreeMap::<String, Vec<(PathBuf, BenchmarkSuiteDriverStatus)>>::new();
    let mut max_seen_index = 0;
    for path in paths {
        let mut status = match load_status_file(&path) {
            Ok(status) => status,
            Err(error) => {
                warn!(
                    error_kind = ?error.kind(),
                    "failed to load benchmark suite driver status"
                );
                record_load_issue(
                    &mut load_state.issues,
                    driver_status_load_issue_kind(&error),
                );
                continue;
            }
        };
        if !is_safe_driver_id(&status.id) {
            warn!("skipping persisted benchmark suite driver with unsafe id");
            record_load_issue(
                &mut load_state.issues,
                BenchmarkSuiteDriverLoadIssueKind::UnsafeDriverId,
            );
            continue;
        }
        max_seen_index =
            max_seen_index.max(driver_id_index(&status.id).expect("safe driver id has an index"));
        if let Err(kind) = normalize_and_validate_loaded_status(&mut status) {
            warn!(issue_kind = ?kind, "skipping invalid benchmark suite driver status");
            record_load_issue(&mut load_state.issues, kind);
            continue;
        }
        candidates
            .entry(status.id.clone())
            .or_default()
            .push((path, status));
    }
    load_state.inner.next_id = max_seen_index;

    let mut accepted = Vec::new();
    for mut records in candidates.into_values() {
        records.sort_by(|left, right| left.0.cmp(&right.0));
        if records.len() > 1 {
            warn!("skipping duplicate persisted benchmark suite driver id");
            load_state
                .suite_retention_claims
                .extend(records.iter().enumerate().map(|(index, (_, status))| {
                    (
                        duplicate_retention_claim_owner(&status.id, index),
                        status.suite_id.clone(),
                    )
                }));
            for _ in 1..records.len() {
                record_load_issue(
                    &mut load_state.issues,
                    BenchmarkSuiteDriverLoadIssueKind::DuplicateDriverId,
                );
            }
            continue;
        }
        let (path, status) = records
            .pop()
            .expect("persisted driver candidate group is non-empty");
        if !is_canonical_driver_path(&path, &status.id) {
            warn!("skipping persisted benchmark suite driver with noncanonical filename");
            record_load_issue(
                &mut load_state.issues,
                BenchmarkSuiteDriverLoadIssueKind::NonCanonicalFilename,
            );
            continue;
        }
        accepted.push(status);
    }

    let mut suites = BTreeMap::<String, Vec<BenchmarkSuiteDriverStatus>>::new();
    for status in accepted {
        suites
            .entry(status.suite_id.clone())
            .or_default()
            .push(status);
    }
    for mut statuses in suites.into_values() {
        statuses.sort_by(|left, right| left.id.cmp(&right.id));
        admit_loaded_suite(&mut load_state, statuses);
    }

    load_state
}

fn admit_loaded_suite(
    load_state: &mut BenchmarkSuiteDriverLoadState,
    statuses: Vec<BenchmarkSuiteDriverStatus>,
) {
    let replayable = statuses
        .iter()
        .enumerate()
        .filter_map(|(index, status)| is_non_terminal(&status.state).then_some(index))
        .collect::<Vec<_>>();
    if replayable.len() > 1 {
        admit_conflicting_loaded_suite(load_state, statuses, &replayable, None);
        return;
    }
    let replay_index = if let Some(index) = replayable.first().copied() {
        if index + 1 != statuses.len() {
            let newest_index = statuses.len() - 1;
            let replay_newest =
                is_restart_recoverable_marker(&statuses[newest_index]).then_some(newest_index);
            admit_conflicting_loaded_suite(load_state, statuses, &[index], replay_newest);
            return;
        }
        Some(index)
    } else {
        statuses
            .iter()
            .enumerate()
            .rev()
            .find_map(|(index, status)| {
                (is_restart_recoverable_marker(status) && index + 1 == statuses.len())
                    .then_some(index)
            })
    };

    for (index, status) in statuses.into_iter().enumerate() {
        admit_loaded_driver(load_state, status, replay_index == Some(index));
    }
}

fn admit_conflicting_loaded_suite(
    load_state: &mut BenchmarkSuiteDriverLoadState,
    statuses: Vec<BenchmarkSuiteDriverStatus>,
    conflicting_indices: &[usize],
    replay_index: Option<usize>,
) {
    warn!("skipping conflicting persisted benchmark suite drivers for one suite");
    load_state
        .retention_excluded_ids
        .extend(statuses.iter().map(|status| status.id.clone()));
    load_state.suite_retention_claims.extend(
        statuses
            .iter()
            .map(|status| (status.id.clone(), status.suite_id.clone())),
    );
    for (index, status) in statuses.into_iter().enumerate() {
        if conflicting_indices.contains(&index) {
            record_load_issue(
                &mut load_state.issues,
                BenchmarkSuiteDriverLoadIssueKind::ConflictingActiveSuite,
            );
        } else {
            admit_loaded_driver(load_state, status, replay_index == Some(index));
        }
    }
}

fn admit_loaded_driver(
    load_state: &mut BenchmarkSuiteDriverLoadState,
    mut status: BenchmarkSuiteDriverStatus,
    replay: bool,
) {
    if let Some(error) = status.error.take() {
        status.error = Some(sanitize_driver_error(&error));
    }
    if replay {
        status.state = "interrupted".to_string();
        status.active_session_id = None;
        let resumable = load_state.inner.restart_candidates.len() < MAX_RESUMABLE_DRIVERS;
        load_state
            .suite_retention_claims
            .push((status.id.clone(), status.suite_id.clone()));
        status.error = Some(
            if resumable {
                AUTOMATIC_RESUME_QUEUED_ERROR
            } else {
                AUTOMATIC_RESUME_LIMIT_ERROR
            }
            .to_string(),
        );
        status.updated_at = timestamp_utc();
        let (stop_tx, _stop_rx) = watch::channel(true);
        load_state
            .inner
            .restart_candidates
            .push(BenchmarkSuiteDriverEntry { status, stop_tx });
        return;
    }
    let (stop_tx, _stop_rx) = watch::channel(!is_non_terminal(&status.state));
    load_state.inner.drivers.insert(
        status.id.clone(),
        BenchmarkSuiteDriverEntry { status, stop_tx },
    );
}

fn record_load_issue(
    issues: &mut Vec<BenchmarkSuiteDriverLoadIssue>,
    kind: BenchmarkSuiteDriverLoadIssueKind,
) {
    if let Some(issue) = issues.iter_mut().find(|issue| issue.kind == kind) {
        issue.count = issue.count.saturating_add(1);
    } else {
        issues.push(BenchmarkSuiteDriverLoadIssue { kind, count: 1 });
    }
}

fn duplicate_retention_claim_owner(driver_id: &str, index: usize) -> String {
    format!("{driver_id}:duplicate:{index}")
}

fn driver_status_load_issue_kind(error: &io::Error) -> BenchmarkSuiteDriverLoadIssueKind {
    if error.kind() == io::ErrorKind::InvalidData {
        BenchmarkSuiteDriverLoadIssueKind::StatusInvalid
    } else {
        BenchmarkSuiteDriverLoadIssueKind::StatusUnreadable
    }
}

fn is_canonical_driver_path(path: &Path, driver_id: &str) -> bool {
    let expected = safe_driver_filename(driver_id);
    path.file_name()
        .and_then(|value| value.to_str())
        .is_some_and(|filename| filename == expected.as_str())
}

fn load_status_file(path: &Path) -> io::Result<BenchmarkSuiteDriverStatus> {
    let data = fs::read_to_string(path)?;
    serde_json::from_str(&data).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn encode_driver_status(status: BenchmarkSuiteDriverStatus) -> io::Result<Vec<u8>> {
    serde_json::to_vec_pretty(&status)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn apply_driver_transition(
    inner: &mut BenchmarkSuiteDriverInner,
    candidate: BenchmarkSuiteDriverEntry,
    handoff_obligation_ids: &SyncMutex<HashSet<String>>,
    suite_retention_claims: &BenchmarkSuiteRetentionClaims,
) -> bool {
    let driver_id = candidate.status.id.clone();
    let suite_id = candidate.status.suite_id.clone();
    let restart_candidate = inner
        .restart_candidates
        .first()
        .is_some_and(|entry| entry.status.id == driver_id);
    if restart_candidate {
        inner.restart_candidates.remove(0);
        if candidate.status.error.as_deref() == Some(AUTOMATIC_RESUME_QUEUED_ERROR) {
            inner.ready_resume_ids.push(driver_id.clone());
            handoff_obligation_ids
                .lock()
                .expect(DRIVER_STORE_LOCK_INVARIANT)
                .insert(driver_id.clone());
        }
    }
    if !is_restart_queued_marker(&candidate.status) {
        handoff_obligation_ids
            .lock()
            .expect(DRIVER_STORE_LOCK_INVARIANT)
            .remove(&driver_id);
    }

    let retains_suite = is_non_terminal(&candidate.status.state)
        || handoff_obligation_ids
            .lock()
            .expect(DRIVER_STORE_LOCK_INVARIANT)
            .contains(&driver_id);
    let released_suite_claim = if retains_suite {
        if suite_retention_claims.claim(&driver_id, &suite_id).is_err() {
            panic!("benchmark suite retention claim disappeared during a driver transition");
        }
        false
    } else {
        suite_retention_claims.release(&driver_id, &suite_id)
    };

    if is_non_terminal(&candidate.status.state) {
        inner
            .active_by_suite
            .insert(suite_id.clone(), driver_id.clone());
    } else {
        if inner
            .active_by_suite
            .get(&suite_id)
            .is_some_and(|active_id| active_id == &driver_id)
        {
            inner.active_by_suite.remove(&suite_id);
        }
        let _ = candidate.stop_tx.send(true);
    }
    inner.drivers.insert(driver_id, candidate);
    released_suite_claim
}

async fn prune_terminal_drivers(
    inner: Arc<RwLock<BenchmarkSuiteDriverInner>>,
    persistence: Option<Arc<BenchmarkSuiteDriverPersistence>>,
    retry_candidates: Arc<SyncMutex<HashMap<String, BenchmarkSuiteDriverEntry>>>,
    retention_issues: Arc<SyncMutex<HashMap<String, BenchmarkSuiteDriverRetentionIssue>>>,
    handoff_obligation_ids: Arc<SyncMutex<HashSet<String>>>,
    retention_excluded_ids: Arc<HashSet<String>>,
) {
    let retry_entries = retry_candidates
        .lock()
        .expect(DRIVER_STORE_LOCK_INVARIANT)
        .clone();
    let handoff_obligation_ids = handoff_obligation_ids
        .lock()
        .expect(DRIVER_STORE_LOCK_INVARIANT)
        .clone();
    let retention_issue_ids = retention_issues
        .lock()
        .expect(DRIVER_STORE_LOCK_INVARIANT)
        .keys()
        .cloned()
        .collect::<HashSet<_>>();
    let candidates = {
        let inner = inner.read().expect(DRIVER_STORE_LOCK_INVARIANT);
        terminal_driver_prune_candidates(
            &inner,
            &retry_entries,
            &handoff_obligation_ids,
            &retention_excluded_ids,
            &retention_issue_ids,
        )
    };
    for status in candidates {
        prune_terminal_driver(
            &status,
            inner.clone(),
            persistence.clone(),
            retention_issues.clone(),
        )
        .await;
    }
}

fn terminal_driver_prune_candidates(
    inner: &BenchmarkSuiteDriverInner,
    retry_entries: &HashMap<String, BenchmarkSuiteDriverEntry>,
    handoff_obligation_ids: &HashSet<String>,
    retention_excluded_ids: &HashSet<String>,
    retention_issue_ids: &HashSet<String>,
) -> Vec<BenchmarkSuiteDriverStatus> {
    let retry_ids = retry_entries.keys().cloned().collect::<HashSet<_>>();
    let restart_ids = inner
        .restart_candidates
        .iter()
        .map(|entry| entry.status.id.as_str())
        .collect::<HashSet<_>>();
    let ready_ids = inner
        .ready_resume_ids
        .iter()
        .map(String::as_str)
        .collect::<HashSet<_>>();

    let mut terminals = inner
        .drivers
        .values()
        .map(|entry| &entry.status)
        .filter(|status| {
            if is_non_terminal(&status.state)
                || retry_ids.contains(&status.id)
                || restart_ids.contains(status.id.as_str())
                || ready_ids.contains(status.id.as_str())
                || handoff_obligation_ids.contains(&status.id)
                || retention_excluded_ids.contains(&status.id)
            {
                return false;
            }
            true
        })
        .collect::<Vec<_>>();
    terminals.sort_by(|left, right| compare_driver_recency(left, right));
    terminals.reverse();
    let retry_issue_candidates = terminals
        .iter()
        .filter(|status| retention_issue_ids.contains(&status.id))
        .map(|status| (*status).clone())
        .collect::<Vec<_>>();

    let mut retained_ids = HashSet::new();
    let mut retained_suites = HashSet::new();
    for status in &terminals {
        if retained_ids.len() >= MAX_RETAINED_TERMINAL_DRIVERS {
            break;
        }
        if retained_suites.insert(status.suite_id.clone()) {
            retained_ids.insert(status.id.clone());
        }
    }
    for status in &terminals {
        if retained_ids.len() >= MAX_RETAINED_TERMINAL_DRIVERS {
            break;
        }
        retained_ids.insert(status.id.clone());
    }

    let mut candidates = terminals
        .into_iter()
        .filter(|status| !retained_ids.contains(&status.id))
        .cloned()
        .collect::<Vec<_>>();
    let mut candidate_ids = candidates
        .iter()
        .map(|status| status.id.clone())
        .collect::<HashSet<_>>();
    candidates.extend(
        retry_issue_candidates
            .into_iter()
            .filter(|status| candidate_ids.insert(status.id.clone())),
    );
    candidates.sort_by(compare_driver_recency);
    candidates
}

fn compare_driver_recency(
    left: &BenchmarkSuiteDriverStatus,
    right: &BenchmarkSuiteDriverStatus,
) -> std::cmp::Ordering {
    parsed_driver_timestamp(&left.updated_at)
        .cmp(&parsed_driver_timestamp(&right.updated_at))
        .then_with(|| {
            parsed_driver_timestamp(&left.created_at)
                .cmp(&parsed_driver_timestamp(&right.created_at))
        })
        .then_with(|| driver_id_index(&left.id).cmp(&driver_id_index(&right.id)))
        .then_with(|| left.id.cmp(&right.id))
}

fn parsed_driver_timestamp(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|value| value.with_timezone(&Utc))
}

async fn prune_terminal_driver(
    status: &BenchmarkSuiteDriverStatus,
    inner: Arc<RwLock<BenchmarkSuiteDriverInner>>,
    persistence: Option<Arc<BenchmarkSuiteDriverPersistence>>,
    retention_issues: Arc<SyncMutex<HashMap<String, BenchmarkSuiteDriverRetentionIssue>>>,
) {
    let driver_id = status.id.clone();
    let target = benchmark_suite_driver_target(&driver_id);
    if let Some(persistence) = persistence {
        let writer = match persistence.take_writer(&driver_id) {
            Ok(writer) => writer,
            Err(_) => {
                record_driver_retention_issue(
                    &retention_issues,
                    &driver_id,
                    BenchmarkSuiteDriverRetentionIssueKind::WriterSettlement,
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
            persistence.restore_writer(&driver_id, writer);
            record_driver_retention_issue(
                &retention_issues,
                &driver_id,
                BenchmarkSuiteDriverRetentionIssueKind::WriterSettlement,
                vec![file_fact(
                    ExecutionFactKind::PrimitiveRefused,
                    None,
                    &target,
                )],
            );
            return;
        }

        let path = driver_path(&persistence.storage_dir, &driver_id);
        let delete_target = target.clone();
        let delete = tokio::task::spawn_blocking(move || {
            delete_launcher_managed_file(DeleteFileRequest::new(delete_target, &path))
        })
        .await;
        match delete {
            Ok(Ok(_)) => drop(writer),
            Ok(Err(error)) => {
                let facts = error.facts.clone();
                persistence.restore_writer(&driver_id, writer);
                record_driver_retention_issue(
                    &retention_issues,
                    &driver_id,
                    BenchmarkSuiteDriverRetentionIssueKind::Delete,
                    facts,
                );
                return;
            }
            Err(_) => {
                persistence.restore_writer(&driver_id, writer);
                record_driver_retention_issue(
                    &retention_issues,
                    &driver_id,
                    BenchmarkSuiteDriverRetentionIssueKind::BlockingTask,
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

    let mut inner = inner.write().expect(DRIVER_STORE_LOCK_INVARIANT);
    if inner
        .drivers
        .get(&driver_id)
        .is_some_and(|entry| entry.status == *status)
    {
        inner.drivers.remove(&driver_id);
        if inner
            .active_by_suite
            .get(&status.suite_id)
            .is_some_and(|active_id| active_id == &driver_id)
        {
            inner.active_by_suite.remove(&status.suite_id);
        }
        inner
            .restart_candidates
            .retain(|entry| entry.status.id != driver_id);
        inner.ready_resume_ids.retain(|id| id != &driver_id);
    }
    retention_issues
        .lock()
        .expect(DRIVER_STORE_LOCK_INVARIANT)
        .remove(&driver_id);
}

fn record_driver_retention_issue(
    retention_issues: &SyncMutex<HashMap<String, BenchmarkSuiteDriverRetentionIssue>>,
    driver_id: &str,
    kind: BenchmarkSuiteDriverRetentionIssueKind,
    facts: Vec<ExecutionFact>,
) {
    retention_issues
        .lock()
        .expect(DRIVER_STORE_LOCK_INVARIANT)
        .insert(
            driver_id.to_string(),
            BenchmarkSuiteDriverRetentionIssue {
                driver_id: driver_id.to_string(),
                kind,
                facts,
            },
        );
}

fn driver_persistence_error(
    error: crate::execution::persistence::PersistenceError,
) -> BenchmarkSuiteDriverStoreError {
    BenchmarkSuiteDriverStoreError::Persistence(error.into())
}

fn benchmark_suite_driver_target(driver_id: &str) -> crate::state::contracts::TargetDescriptor {
    classify_current_artifact(CurrentArtifact::BenchmarkSuiteDriverStatus, driver_id).target
}

fn driver_dir(paths: &AppPaths) -> PathBuf {
    paths.config_dir.join("benchmarks").join("suite-drivers")
}

fn driver_path(storage_dir: &Path, driver_id: &str) -> PathBuf {
    storage_dir.join(safe_driver_filename(driver_id))
}

fn safe_driver_filename(driver_id: &str) -> String {
    let mut stem = driver_id
        .chars()
        .map(|value| {
            if value.is_ascii_alphanumeric() || matches!(value, '-' | '_') {
                value
            } else {
                '_'
            }
        })
        .take(MAX_DRIVER_FILENAME_STEM)
        .collect::<String>();
    stem = stem.trim_matches('_').to_string();
    if stem.is_empty() {
        "driver.json".to_string()
    } else {
        format!("{stem}.json")
    }
}

fn is_safe_driver_id(driver_id: &str) -> bool {
    driver_id_index(driver_id).is_some_and(|index| {
        let canonical = format!("{DRIVER_ID_PREFIX}{index:016x}");
        driver_id == canonical.as_str()
    })
}

fn driver_id_index(driver_id: &str) -> Option<u64> {
    let suffix = driver_id.strip_prefix(DRIVER_ID_PREFIX)?;
    if suffix.len() != 16 || !suffix.chars().all(|value| value.is_ascii_hexdigit()) {
        return None;
    }
    u64::from_str_radix(suffix, 16).ok()
}

pub fn sanitize_driver_error(value: &str) -> String {
    sanitize_public_diagnostic_text(
        value,
        RedactionAudience::UserVisible,
        MAX_DRIVER_ERROR_CHARS,
        "driver error",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::persistence::{AtomicWriteBackend, PersistenceCoordinator};
    use crate::state::contracts::TargetDescriptor;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    #[derive(Default)]
    struct ControlledBackend {
        fail_writes: AtomicBool,
        fail_next_writes: AtomicUsize,
        fail_destination: SyncMutex<Option<PathBuf>>,
        gate_writes: AtomicBool,
        entered_write: AtomicBool,
        writes: AtomicUsize,
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

        fn fail_next(&self) {
            self.fail_next_writes.fetch_add(1, Ordering::SeqCst);
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

        fn write_count(&self) -> usize {
            self.writes.load(Ordering::SeqCst)
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
            let fail_next = self
                .fail_next_writes
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                    remaining.checked_sub(1)
                })
                .is_ok();
            let destination_failed = self
                .fail_destination
                .lock()
                .expect("controlled backend failure destination lock")
                .as_ref()
                .is_some_and(|failed| failed == destination);
            if self.fail_writes.load(Ordering::SeqCst) || fail_next || destination_failed {
                return Err(io::Error::other("injected suite driver status failure"));
            }
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(destination, contents)?;
            self.writes.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[tokio::test]
    async fn start_conflicts_for_non_terminal_suite_driver() {
        let store = BenchmarkSuiteDriverStore::new();
        let suite_id = test_suite_id("start-conflict", "development");
        let summary = BenchmarkSuiteDriverSuiteSummary {
            run_count: 2,
            launched_run_count: 0,
            pending_run_index: Some(0),
        };

        store
            .start(
                suite_id.clone(),
                "development".to_string(),
                30_000,
                summary.clone(),
            )
            .await
            .expect("first driver should start");
        let conflict = store
            .start(suite_id, "development".to_string(), 30_000, summary)
            .await;

        assert!(matches!(
            conflict,
            Err(BenchmarkSuiteDriverStartError::Conflict)
        ));
    }

    #[tokio::test]
    async fn start_claim_and_suite_prune_reservation_have_one_winner() {
        let retention_claims = BenchmarkSuiteRetentionClaims::default();
        let suite_id = test_suite_id("claim-prune-race", "development");
        let prune = retention_claims
            .try_begin_prune(&suite_id)
            .expect("unclaimed suite can reserve pruning");
        let store = BenchmarkSuiteDriverStore::new_with_retention_claims(retention_claims.clone());

        assert!(matches!(
            store
                .start(
                    suite_id.clone(),
                    "development".to_string(),
                    30_000,
                    test_summary(),
                )
                .await,
            Err(BenchmarkSuiteDriverStartError::Conflict)
        ));
        assert!(retention_claims.claimed_suite_ids().is_empty());

        drop(prune);
        let started = store
            .start(
                suite_id.clone(),
                "development".to_string(),
                30_000,
                test_summary(),
            )
            .await
            .expect("driver claims after pruning reservation ends");
        assert!(retention_claims.has_claim(&started.status.id, &suite_id));
        store
            .record_stopped(&started.status.id)
            .await
            .expect("terminal commit releases claim");
        assert!(!retention_claims.has_claim(&started.status.id, &suite_id));
        drop(started.effect_owner);
    }

    #[tokio::test]
    async fn preaccept_start_failure_releases_suite_claim() {
        let root = test_root("preaccept-claim-release");
        let paths = test_paths(&root);
        let retention_claims = BenchmarkSuiteRetentionClaims::default();
        let suite_id = test_suite_id("preaccept-claim-release", "development");
        let store = BenchmarkSuiteDriverStore::load_from_paths_with_retention_claims(
            &paths,
            retention_claims.clone(),
        );
        store.close().await.expect("close persistence owner");

        assert!(matches!(
            store
                .start(
                    suite_id.clone(),
                    "development".to_string(),
                    30_000,
                    test_summary(),
                )
                .await,
            Err(BenchmarkSuiteDriverStartError::Store {
                source: BenchmarkSuiteDriverStoreError::Persistence(_),
                ..
            })
        ));
        assert!(retention_claims.claimed_suite_ids().is_empty());
        cleanup(&root);
    }

    #[tokio::test]
    async fn terminal_driver_release_promptly_retries_suite_retention() {
        use crate::state::benchmark_suites::{BenchmarkSuiteRunInput, BenchmarkSuiteStore};

        let claims = BenchmarkSuiteRetentionClaims::default();
        let suites = BenchmarkSuiteStore::new_with_retention_claims(claims.clone());
        let drivers = BenchmarkSuiteDriverStore::new_with_retention(
            claims.clone(),
            suites.retention_handle(),
        );
        let mut suite_ids = Vec::new();
        for index in 0..33 {
            let suite_id = test_suite_id(&format!("release-retention-{index}"), "development");
            let session_id = format!("session-{index}");
            claims
                .claim(&format!("fixture-{index}"), &suite_id)
                .expect("protect fixture during setup");
            let selected = suites
                .select_reservation(
                    &suite_id,
                    &format!("instance-{index}"),
                    "development",
                    &[BenchmarkSuiteRunInput {
                        run_index: 0,
                        profile: "managed_default".to_string(),
                        run_type: "repeat".to_string(),
                        target_id: Some("target-current".to_string()),
                        benchmark_id: format!("benchmark-{index:016x}"),
                    }],
                    None,
                )
                .await
                .expect("select fixture");
            suites
                .reserve(selected, &session_id, "2026-01-01T00:00:00Z", false)
                .await
                .expect("reserve fixture");
            suites
                .update_run_state_for_session(&session_id, "completed")
                .await
                .expect("complete fixture");
            suite_ids.push(suite_id);
        }

        let protected_suite = suite_ids[0].clone();
        let started = drivers
            .start(
                protected_suite.clone(),
                "development".to_string(),
                30_000,
                test_summary(),
            )
            .await
            .expect("driver protects oldest suite");
        for (index, suite_id) in suite_ids.iter().enumerate() {
            assert!(claims.release(&format!("fixture-{index}"), suite_id));
        }
        assert!(suites.retry_terminal_retention().await.is_empty());
        assert!(
            suites
                .get(&protected_suite)
                .expect("read protected")
                .is_some()
        );
        assert_eq!(
            suite_ids
                .iter()
                .filter(|suite_id| suites.get(suite_id).expect("read suite").is_some())
                .count(),
            33
        );

        drivers
            .record_stopped(&started.status.id)
            .await
            .expect("terminal commit releases suite");

        assert!(!claims.has_claim(&started.status.id, &protected_suite));
        assert_eq!(
            suite_ids
                .iter()
                .filter(|suite_id| suites.get(suite_id).expect("read suite").is_some())
                .count(),
            32
        );
        drop(started.effect_owner);
    }

    #[tokio::test]
    async fn stopped_driver_blocks_successor_until_effect_owner_exits() {
        let store = BenchmarkSuiteDriverStore::new();
        let suite_id = test_suite_id("stopped-owner", "development");
        let summary = BenchmarkSuiteDriverSuiteSummary {
            run_count: 2,
            launched_run_count: 0,
            pending_run_index: Some(0),
        };
        let started = store
            .start(
                suite_id.clone(),
                "development".to_string(),
                30_000,
                summary.clone(),
            )
            .await
            .expect("driver should start");

        let stopped = store.stop(&started.status.id).await.expect("driver status");

        assert_eq!(stopped.state, "stopped");
        assert_eq!(
            store
                .get(&started.status.id)
                .await
                .expect("stored status")
                .state,
            "stopped"
        );
        assert!(matches!(
            store
                .start(
                    suite_id.clone(),
                    "development".to_string(),
                    30_000,
                    summary.clone(),
                )
                .await,
            Err(BenchmarkSuiteDriverStartError::Conflict)
        ));
        drop(started.effect_owner);
        store
            .start(suite_id, "development".to_string(), 30_000, summary)
            .await
            .expect("terminal driver should not conflict");
    }

    #[tokio::test]
    async fn stopping_terminal_driver_does_not_clear_new_active_driver() {
        let store = BenchmarkSuiteDriverStore::new();
        let suite_id = test_suite_id("terminal-successor", "development");
        let summary = BenchmarkSuiteDriverSuiteSummary {
            run_count: 2,
            launched_run_count: 0,
            pending_run_index: Some(0),
        };
        let first = store
            .start(
                suite_id.clone(),
                "development".to_string(),
                30_000,
                summary.clone(),
            )
            .await
            .expect("first driver should start");
        store
            .record_stopped(&first.status.id)
            .await
            .expect("first driver stops");
        drop(first.effect_owner);
        let _second = store
            .start(
                suite_id.clone(),
                "development".to_string(),
                30_000,
                summary.clone(),
            )
            .await
            .expect("second driver should start");

        let stopped_first = store.stop(&first.status.id).await;
        let conflict = store
            .start(suite_id, "development".to_string(), 30_000, summary)
            .await;

        assert!(matches!(
            stopped_first,
            Err(BenchmarkSuiteDriverStoreError::TerminalDriver)
        ));
        assert!(matches!(
            conflict,
            Err(BenchmarkSuiteDriverStartError::Conflict)
        ));
    }

    #[tokio::test]
    async fn unknown_driver_status_is_missing() {
        let store = BenchmarkSuiteDriverStore::new();

        assert!(store.get("missing").await.is_none());
        assert!(matches!(
            store.stop("missing").await,
            Err(BenchmarkSuiteDriverStoreError::MissingDriver)
        ));
    }

    #[tokio::test]
    async fn persistence_owner_rejects_duplicate_store_for_exact_directory() {
        let root = test_root("duplicate-owner");
        let paths = test_paths(&root);
        let backend = Arc::new(ControlledBackend::default());
        let coordinator = backend.coordinator();
        let first = BenchmarkSuiteDriverStore::try_load_from_paths_with_coordinator(
            &paths,
            coordinator.clone(),
        )
        .expect("first owner");

        let duplicate = BenchmarkSuiteDriverStore::try_load_from_paths_with_coordinator(
            &paths,
            coordinator.clone(),
        );
        assert!(matches!(
            duplicate,
            Err(BenchmarkSuiteDriverStoreError::Persistence(ref error))
                if error.kind() == io::ErrorKind::AlreadyExists
        ));

        first.close().await.expect("first owner closes");
        BenchmarkSuiteDriverStore::try_load_from_paths_with_coordinator(&paths, coordinator)
            .expect("closed owner releases exact directory");
        cleanup(&root);
    }

    #[tokio::test]
    async fn critical_start_stays_hidden_and_promotes_after_waiter_is_aborted() {
        let root = test_root("critical-abort-observer");
        let paths = test_paths(&root);
        let backend = Arc::new(ControlledBackend::default());
        let retention_claims = BenchmarkSuiteRetentionClaims::default();
        let suite_id = test_suite_id("suite-a", "development");
        backend.gate();
        let store = Arc::new(
            BenchmarkSuiteDriverStore::try_load_from_paths_with_coordinator_and_retention_claims(
                &paths,
                backend.coordinator(),
                retention_claims.clone(),
            )
            .expect("store"),
        );
        let task_store = store.clone();
        let task_suite_id = suite_id.clone();
        let task = tokio::spawn(async move {
            task_store
                .start(
                    task_suite_id,
                    "development".to_string(),
                    30_000,
                    test_summary(),
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
        let driver_id = "benchmark-suite-driver-0000000000000001";
        assert!(retention_claims.has_claim(driver_id, &suite_id));
        assert!(store.get(driver_id).await.is_none());
        assert!(store.list_recent(10).await.is_empty());

        task.abort();
        let _ = task.await;
        backend.release();
        tokio::time::timeout(Duration::from_secs(2), async {
            while store.get(driver_id).await.is_none() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("detached observer promotes committed start");
        assert_eq!(
            store.get(driver_id).await.expect("visible driver").state,
            "scheduled"
        );
        assert!(retention_claims.has_claim(driver_id, &suite_id));
        store
            .record_stopped(driver_id)
            .await
            .expect("observer releases the mutation gate");
        assert!(!retention_claims.has_claim(driver_id, &suite_id));
        store.close().await.expect("store closes");
        cleanup(&root);
    }

    #[tokio::test]
    async fn fail_once_start_retries_exact_candidate_before_returning_effect_handle() {
        let root = test_root("start-fail-once");
        let paths = test_paths(&root);
        let backend = Arc::new(ControlledBackend::default());
        backend.fail_next();
        let store = BenchmarkSuiteDriverStore::try_load_from_paths_with_coordinator(
            &paths,
            backend.coordinator(),
        )
        .expect("store");

        let started = store
            .start(
                test_suite_id("suite-a", "development"),
                "development".to_string(),
                30_000,
                test_summary(),
            )
            .await
            .expect("start retries before returning effect ownership");

        assert_eq!(started.status.id, "benchmark-suite-driver-0000000000000001");
        assert_eq!(
            store
                .get(&started.status.id)
                .await
                .expect("committed start")
                .state,
            "scheduled"
        );
        assert!(store.retry_candidate_ids().is_empty());
        store.close().await.expect("store closes");
        cleanup(&root);
    }

    #[tokio::test]
    async fn failed_critical_start_has_one_effect_owner_with_competing_retrier() {
        let root = test_root("critical-retry");
        let paths = test_paths(&root);
        let backend = Arc::new(ControlledBackend::default());
        backend.set_fail_writes(true);
        let store = Arc::new(
            BenchmarkSuiteDriverStore::try_load_from_paths_with_coordinator(
                &paths,
                backend.coordinator(),
            )
            .expect("store"),
        );
        let original_store = store.clone();
        let original = tokio::spawn(async move {
            original_store
                .start(
                    test_suite_id("suite-a", "development"),
                    "development".to_string(),
                    30_000,
                    test_summary(),
                )
                .await
        });
        let driver_id = "benchmark-suite-driver-0000000000000001";
        wait_for_retry_candidate(&store, driver_id).await;

        assert!(matches!(
            store
                .start(
                    test_suite_id("suite-a", "development"),
                    "development".to_string(),
                    30_000,
                    test_summary(),
                )
                .await,
            Err(BenchmarkSuiteDriverStartError::Conflict)
        ));

        backend.set_fail_writes(false);
        let competing_retry = store.retry_critical(driver_id).await;
        assert!(matches!(
            competing_retry,
            Ok(()) | Err(BenchmarkSuiteDriverStoreError::RetryUnavailable)
        ));
        let started = original
            .await
            .expect("original start task")
            .expect("original effect owner recovers");
        assert_eq!(started.status.id, driver_id);
        assert!(store.retry_candidate_ids().is_empty());
        assert_eq!(
            store.get(driver_id).await.expect("promoted driver").state,
            "scheduled"
        );
        assert!(matches!(
            store
                .start(
                    test_suite_id("suite-a", "development"),
                    "development".to_string(),
                    30_000,
                    test_summary(),
                )
                .await,
            Err(BenchmarkSuiteDriverStartError::Conflict)
        ));
        store.close().await.expect("store closes");
        cleanup(&root);
    }

    #[tokio::test]
    async fn abort_during_retry_backoff_retains_exact_start_and_blocks_duplicate() {
        let root = test_root("critical-backoff-abort");
        let paths = test_paths(&root);
        let backend = Arc::new(ControlledBackend::default());
        backend.set_fail_writes(true);
        let store = Arc::new(
            BenchmarkSuiteDriverStore::try_load_from_paths_with_coordinator(
                &paths,
                backend.coordinator(),
            )
            .expect("store"),
        );
        let task_store = store.clone();
        let task = tokio::spawn(async move {
            task_store
                .start(
                    test_suite_id("suite-a", "development"),
                    "development".to_string(),
                    30_000,
                    test_summary(),
                )
                .await
        });
        let driver_id = "benchmark-suite-driver-0000000000000001";
        wait_for_retry_candidate(&store, driver_id).await;
        task.abort();
        let _ = task.await;
        wait_for_retry_candidate(&store, driver_id).await;

        assert_eq!(store.retry_candidate_ids(), vec![driver_id.to_string()]);
        assert!(matches!(
            store
                .start(
                    test_suite_id("suite-a", "development"),
                    "development".to_string(),
                    30_000,
                    test_summary(),
                )
                .await,
            Err(BenchmarkSuiteDriverStartError::Conflict)
        ));

        backend.set_fail_writes(false);
        store
            .retry_critical(driver_id)
            .await
            .expect("retained exact start retries");
        assert_eq!(
            store.get(driver_id).await.expect("promoted driver").state,
            "scheduled"
        );
        store.close().await.expect("store closes");
        cleanup(&root);
    }

    #[tokio::test]
    async fn fail_once_terminal_transition_retries_exact_status() {
        let root = test_root("terminal-retry");
        let paths = test_paths(&root);
        let backend = Arc::new(ControlledBackend::default());
        let store = BenchmarkSuiteDriverStore::try_load_from_paths_with_coordinator(
            &paths,
            backend.coordinator(),
        )
        .expect("store");
        let started = store
            .start(
                test_suite_id("suite-a", "development"),
                "development".to_string(),
                30_000,
                test_summary(),
            )
            .await
            .expect("start persists");

        backend.fail_next();
        store
            .record_complete(
                &started.status.id,
                BenchmarkSuiteDriverSuiteSummary {
                    run_count: 2,
                    launched_run_count: 2,
                    pending_run_index: None,
                },
            )
            .await
            .expect("terminal transition retries");

        let status = store
            .get(&started.status.id)
            .await
            .expect("terminal status");
        assert_eq!(status.state, "complete");
        assert_eq!(
            load_status_file(&driver_path(&driver_dir(&paths), &started.status.id))
                .expect("terminal file")
                .state,
            "complete"
        );
        store.close().await.expect("store closes");
        cleanup(&root);
    }

    #[tokio::test]
    async fn admitted_stop_signals_loop_before_fail_once_terminal_commit() {
        let root = test_root("stop-signal-before-commit");
        let paths = test_paths(&root);
        let backend = Arc::new(ControlledBackend::default());
        let store = Arc::new(
            BenchmarkSuiteDriverStore::try_load_from_paths_with_coordinator(
                &paths,
                backend.coordinator(),
            )
            .expect("store"),
        );
        let started = store
            .start(
                test_suite_id("suite-a", "development"),
                "development".to_string(),
                30_000,
                test_summary(),
            )
            .await
            .expect("start persists");
        let stop_rx = started.effect_owner.stop_receiver();
        backend.fail_next();
        backend.gate();
        let driver_id = started.status.id.clone();
        let stop_store = store.clone();
        let stop_task = tokio::spawn(async move { stop_store.stop(&driver_id).await });
        tokio::time::timeout(Duration::from_secs(2), async {
            while !backend.entered_write.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("stop write entered");

        assert!(*stop_rx.borrow());
        assert_eq!(
            store
                .get(&started.status.id)
                .await
                .expect("public state remains committed")
                .state,
            "scheduled"
        );

        backend.release();
        let stopped = stop_task
            .await
            .expect("stop task")
            .expect("stop retries exact terminal bytes");
        assert_eq!(stopped.state, "stopped");
        assert_eq!(
            store
                .get(&started.status.id)
                .await
                .expect("committed stop")
                .state,
            "stopped"
        );
        store.close().await.expect("store closes");
        cleanup(&root);
    }

    #[tokio::test]
    async fn accepted_progress_is_visible_and_debounced_to_fewer_writes() {
        let root = test_root("debounced-progress");
        let paths = test_paths(&root);
        let backend = Arc::new(ControlledBackend::default());
        let store = BenchmarkSuiteDriverStore::try_load_from_paths_with_coordinator(
            &paths,
            backend.coordinator(),
        )
        .expect("store");
        let started = store
            .start(
                test_suite_id("suite-a", "development"),
                "development".to_string(),
                30_000,
                test_summary(),
            )
            .await
            .expect("start persists");

        for launched_run_count in 1..=20 {
            store
                .record_active(
                    &started.status.id,
                    BenchmarkSuiteDriverSuiteSummary {
                        run_count: 20,
                        launched_run_count,
                        pending_run_index: Some(launched_run_count),
                    },
                    Some(format!("session-{launched_run_count}")),
                )
                .await
                .expect("progress accepted");
        }
        assert_eq!(
            store
                .get(&started.status.id)
                .await
                .expect("accepted progress is visible")
                .launched_run_count,
            20
        );

        store.flush().await.expect("progress flushes");
        assert!(
            backend.write_count() < 21,
            "expected coalescing, observed {} physical writes",
            backend.write_count()
        );
        let persisted = load_status_file(&driver_path(&driver_dir(&paths), &started.status.id))
            .expect("latest progress persisted");
        assert_eq!(persisted.launched_run_count, 20);
        assert_eq!(persisted.active_session_id.as_deref(), Some("session-20"));
        store.close().await.expect("store closes");
        cleanup(&root);
    }

    #[tokio::test]
    async fn flush_retries_latest_failed_debounced_progress() {
        let root = test_root("progress-flush-retry");
        let paths = test_paths(&root);
        let backend = Arc::new(ControlledBackend::default());
        let store = BenchmarkSuiteDriverStore::try_load_from_paths_with_coordinator(
            &paths,
            backend.coordinator(),
        )
        .expect("store");
        let started = store
            .start(
                test_suite_id("suite-a", "development"),
                "development".to_string(),
                30_000,
                test_summary(),
            )
            .await
            .expect("start persists");
        backend.fail_next();
        store
            .record_active(
                &started.status.id,
                BenchmarkSuiteDriverSuiteSummary {
                    run_count: 2,
                    launched_run_count: 1,
                    pending_run_index: Some(1),
                },
                Some("session-1".to_string()),
            )
            .await
            .expect("progress is accepted");

        store.flush().await.expect("flush retries latest progress");

        let persisted = load_status_file(&driver_path(&driver_dir(&paths), &started.status.id))
            .expect("progress file");
        assert_eq!(persisted.state, "active");
        assert_eq!(persisted.launched_run_count, 1);
        assert_eq!(persisted.active_session_id.as_deref(), Some("session-1"));
        store.close().await.expect("store closes");
        cleanup(&root);
    }

    #[tokio::test]
    async fn poisoned_store_lock_panics_instead_of_returning_stale_state() {
        let store = BenchmarkSuiteDriverStore::new();
        let inner = store.inner.clone();
        let poison = std::thread::spawn(move || {
            let _guard = inner.write().expect("lock starts healthy");
            panic!("poison benchmark suite driver lock");
        });
        assert!(poison.join().is_err());

        let read = tokio::spawn(async move { store.get("missing").await }).await;
        assert!(read.expect_err("poisoned lock must panic").is_panic());
    }

    #[tokio::test]
    async fn persisted_driver_status_survives_restart_and_interrupts_active_driver() {
        let root = test_root("restart-interrupt");
        let paths = test_paths(&root);
        let summary = BenchmarkSuiteDriverSuiteSummary {
            run_count: 2,
            launched_run_count: 0,
            pending_run_index: Some(0),
        };
        let store = BenchmarkSuiteDriverStore::load_from_paths(&paths);
        let started = store
            .start(
                test_suite_id("suite-dev", "development"),
                "development".to_string(),
                30_000,
                summary.clone(),
            )
            .await
            .expect("driver starts");
        store
            .record_active(
                &started.status.id,
                summary.clone(),
                Some("session-1".to_string()),
            )
            .await
            .expect("active progress accepted");

        store.flush().await.expect("active progress persisted");

        let path = driver_path(&driver_dir(&paths), &started.status.id);
        assert!(path.is_file());
        let persisted = load_status_file(&path).expect("persisted status should load");
        assert_eq!(persisted.state, "active");

        store.close().await.expect("first store closes");
        let reloaded = BenchmarkSuiteDriverStore::load_from_paths(&paths);
        assert!(reloaded.get(&started.status.id).await.is_none());
        let unchanged = load_status_file(&path).expect("load does not rewrite status");
        assert_eq!(unchanged.state, "active");

        let pending = reloaded
            .take_restart_interrupted_resumable_drivers()
            .await
            .expect("restart checkpoint persists");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, started.status.id);
        let interrupted = reloaded
            .get(&started.status.id)
            .await
            .expect("committed interrupted driver");
        assert_eq!(interrupted.state, "interrupted");
        assert_eq!(
            interrupted.error.as_deref(),
            Some(AUTOMATIC_RESUME_QUEUED_ERROR)
        );
        assert_eq!(interrupted.active_session_id, None);
        let rewritten = load_status_file(&path).expect("checkpoint persisted");
        assert_eq!(rewritten.state, "interrupted");
        assert_eq!(
            reloaded
                .take_restart_interrupted_resumable_drivers()
                .await
                .expect("second take succeeds")
                .len(),
            0
        );

        let next = reloaded
            .start(
                test_suite_id("suite-dev", "development"),
                "development".to_string(),
                30_000,
                summary,
            )
            .await
            .expect("interrupted driver should not conflict");
        assert_eq!(next.status.id, "benchmark-suite-driver-0000000000000002");

        cleanup(&root);
    }

    #[tokio::test]
    async fn fail_once_restart_checkpoint_retries_before_exposing_resume() {
        let root = test_root("restart-checkpoint-retry");
        let paths = test_paths(&root);
        let dir = driver_dir(&paths);
        fs::create_dir_all(&dir).expect("create driver dir");
        let status = status_fixture(1, "active", None);
        fs::write(
            driver_path(&dir, &status.id),
            serde_json::to_vec_pretty(&status).expect("serialize driver"),
        )
        .expect("write active driver");
        let backend = Arc::new(ControlledBackend::default());
        backend.fail_next();
        let store = BenchmarkSuiteDriverStore::try_load_from_paths_with_coordinator(
            &paths,
            backend.coordinator(),
        )
        .expect("store");

        let pending = store
            .take_restart_interrupted_resumable_drivers()
            .await
            .expect("restart checkpoint retries");

        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, status.id);
        assert_eq!(
            pending[0].error.as_deref(),
            Some(AUTOMATIC_RESUME_QUEUED_ERROR)
        );
        assert_eq!(
            load_status_file(&driver_path(&dir, &status.id))
                .expect("checkpoint file")
                .error
                .as_deref(),
            Some(AUTOMATIC_RESUME_QUEUED_ERROR)
        );
        store.close().await.expect("store closes");
        cleanup(&root);
    }

    #[test]
    fn queued_restart_handoff_alone_remains_recoverable() {
        let root = test_root("queued-handoff-alone");
        let paths = test_paths(&root);
        let dir = driver_dir(&paths);
        fs::create_dir_all(&dir).expect("create driver dir");
        let queued = status_fixture(1, "interrupted", Some(AUTOMATIC_RESUME_QUEUED_ERROR));
        write_status_fixture(&dir, &queued);

        let load_state = load_persisted_driver_inner(&dir);

        assert_eq!(load_state.inner.restart_candidates.len(), 1);
        assert_eq!(load_state.inner.restart_candidates[0].status.id, queued.id);
        assert!(load_state.inner.drivers.is_empty());
        assert!(load_state.issues.is_empty());
        cleanup(&root);
    }

    #[test]
    fn started_restart_handoff_alone_is_consumed() {
        let root = test_root("started-handoff-alone");
        let paths = test_paths(&root);
        let dir = driver_dir(&paths);
        fs::create_dir_all(&dir).expect("create driver dir");
        let started = status_fixture(1, "interrupted", Some(AUTOMATIC_RESUME_STARTED_ERROR));
        write_status_fixture(&dir, &started);

        let load_state = load_persisted_driver_inner(&dir);

        assert!(load_state.inner.restart_candidates.is_empty());
        assert_eq!(load_state.inner.drivers.len(), 1);
        assert!(load_state.issues.is_empty());
        cleanup(&root);
    }

    #[test]
    fn queued_or_started_handoff_with_one_successor_replays_only_successor() {
        for (name, marker) in [
            ("queued", AUTOMATIC_RESUME_QUEUED_ERROR),
            ("started", AUTOMATIC_RESUME_STARTED_ERROR),
        ] {
            let root = test_root(&format!("handoff-successor-{name}"));
            let paths = test_paths(&root);
            let dir = driver_dir(&paths);
            fs::create_dir_all(&dir).expect("create driver dir");
            let mut previous = status_fixture(1, "interrupted", Some(marker));
            previous.suite_id = test_suite_id("same-suite", "development");
            let mut successor = status_fixture(2, "scheduled", None);
            successor.suite_id = test_suite_id("same-suite", "development");
            write_status_fixture(&dir, &previous);
            write_status_fixture(&dir, &successor);

            let load_state = load_persisted_driver_inner(&dir);

            assert_eq!(load_state.inner.restart_candidates.len(), 1);
            assert_eq!(
                load_state.inner.restart_candidates[0].status.id,
                successor.id
            );
            assert_eq!(
                load_state
                    .inner
                    .drivers
                    .get(&previous.id)
                    .expect("consumed marker remains visible")
                    .status
                    .error
                    .as_deref(),
                Some(marker)
            );
            assert!(load_state.issues.is_empty());
            cleanup(&root);
        }
    }

    #[test]
    fn newer_terminal_driver_consumes_queued_handoff_without_replay() {
        let root = test_root("handoff-terminal-successor");
        let paths = test_paths(&root);
        let dir = driver_dir(&paths);
        fs::create_dir_all(&dir).expect("create driver dir");
        let mut previous = status_fixture(1, "interrupted", Some(AUTOMATIC_RESUME_QUEUED_ERROR));
        previous.suite_id = test_suite_id("same-suite", "development");
        let mut terminal = status_fixture(2, "stopped", None);
        terminal.suite_id = test_suite_id("same-suite", "development");
        write_status_fixture(&dir, &previous);
        write_status_fixture(&dir, &terminal);

        let load_state = load_persisted_driver_inner(&dir);

        assert!(load_state.inner.restart_candidates.is_empty());
        assert_eq!(load_state.inner.drivers.len(), 2);
        assert!(load_state.issues.is_empty());
        cleanup(&root);
    }

    #[test]
    fn stale_started_history_does_not_block_one_newer_nonterminal_successor() {
        let root = test_root("stale-handoff-history");
        let paths = test_paths(&root);
        let dir = driver_dir(&paths);
        fs::create_dir_all(&dir).expect("create driver dir");
        for (index, state, error) in [
            (1, "interrupted", Some(AUTOMATIC_RESUME_STARTED_ERROR)),
            (2, "stopped", None),
            (3, "interrupted", Some(AUTOMATIC_RESUME_STARTED_ERROR)),
            (4, "failed", Some("bounded failure")),
            (5, "scheduled", None),
        ] {
            let mut status = status_fixture(index, state, error);
            status.suite_id = test_suite_id("same-suite", "development");
            write_status_fixture(&dir, &status);
        }

        let load_state = load_persisted_driver_inner(&dir);

        assert_eq!(load_state.inner.restart_candidates.len(), 1);
        assert_eq!(
            load_state.inner.restart_candidates[0].status.id,
            "benchmark-suite-driver-0000000000000005"
        );
        assert_eq!(load_state.inner.drivers.len(), 4);
        assert!(load_state.issues.is_empty());
        cleanup(&root);
    }

    #[test]
    fn handoff_with_multiple_nonterminal_successors_replays_none() {
        let root = test_root("ambiguous-handoff-successors");
        let paths = test_paths(&root);
        let dir = driver_dir(&paths);
        fs::create_dir_all(&dir).expect("create driver dir");
        for (index, state, error) in [
            (1, "interrupted", Some(AUTOMATIC_RESUME_QUEUED_ERROR)),
            (2, "scheduled", None),
            (3, "active", None),
        ] {
            let mut status = status_fixture(index, state, error);
            status.suite_id = test_suite_id("same-suite", "development");
            write_status_fixture(&dir, &status);
        }

        let load_state = load_persisted_driver_inner(&dir);

        assert!(load_state.inner.restart_candidates.is_empty());
        assert_eq!(load_state.inner.drivers.len(), 1);
        assert_eq!(
            load_state.issues,
            vec![BenchmarkSuiteDriverLoadIssue {
                kind: BenchmarkSuiteDriverLoadIssueKind::ConflictingActiveSuite,
                count: 2,
            }]
        );
        cleanup(&root);
    }

    #[test]
    fn newer_recoverable_marker_supersedes_one_stale_nonterminal() {
        for (name, marker) in [
            ("queued", AUTOMATIC_RESUME_QUEUED_ERROR),
            ("started", AUTOMATIC_RESUME_STARTED_ERROR),
        ] {
            let root = test_root(&format!("inverse-handoff-{name}"));
            let paths = test_paths(&root);
            let dir = driver_dir(&paths);
            fs::create_dir_all(&dir).expect("create driver dir");
            let mut nonterminal = status_fixture(1, "scheduled", None);
            nonterminal.suite_id = test_suite_id("same-suite", "development");
            let mut newer_marker = status_fixture(2, "interrupted", Some(marker));
            newer_marker.suite_id = test_suite_id("same-suite", "development");
            write_status_fixture(&dir, &nonterminal);
            write_status_fixture(&dir, &newer_marker);

            let load_state = load_persisted_driver_inner(&dir);

            if marker == AUTOMATIC_RESUME_STARTED_ERROR {
                assert!(load_state.inner.restart_candidates.is_empty());
                assert_eq!(load_state.inner.drivers.len(), 1);
                assert!(load_state.inner.drivers.contains_key(&newer_marker.id));
            } else {
                assert_eq!(load_state.inner.restart_candidates.len(), 1);
                assert_eq!(
                    load_state.inner.restart_candidates[0].status.id,
                    newer_marker.id
                );
                assert!(load_state.inner.drivers.is_empty());
            }
            assert_eq!(
                load_state.issues,
                vec![BenchmarkSuiteDriverLoadIssue {
                    kind: BenchmarkSuiteDriverLoadIssueKind::ConflictingActiveSuite,
                    count: 1,
                }]
            );
            cleanup(&root);
        }
    }

    #[test]
    fn newer_terminal_history_consumes_stale_nonterminal_record() {
        for (name, terminal_state) in [
            ("stopped", "stopped"),
            ("failed", "failed"),
            ("complete", "complete"),
        ] {
            let root = test_root(&format!("stale-nonterminal-{name}"));
            let paths = test_paths(&root);
            let dir = driver_dir(&paths);
            fs::create_dir_all(&dir).expect("create driver dir");
            let mut stale = status_fixture(1, "scheduled", None);
            stale.suite_id = test_suite_id("same-suite", "development");
            let mut terminal = status_fixture(2, terminal_state, None);
            terminal.suite_id = test_suite_id("same-suite", "development");
            if terminal_state == "complete" {
                terminal.launched_run_count = terminal.run_count;
            }
            write_status_fixture(&dir, &stale);
            write_status_fixture(&dir, &terminal);

            let load_state = load_persisted_driver_inner(&dir);

            assert!(load_state.inner.restart_candidates.is_empty());
            assert_eq!(load_state.inner.drivers.len(), 1);
            assert!(load_state.inner.drivers.contains_key(&terminal.id));
            assert_eq!(
                load_state.issues,
                vec![BenchmarkSuiteDriverLoadIssue {
                    kind: BenchmarkSuiteDriverLoadIssueKind::ConflictingActiveSuite,
                    count: 1,
                }]
            );
            cleanup(&root);
        }
    }

    #[tokio::test]
    async fn flush_and_close_retry_canceled_candidates_and_release_owner() {
        let root = test_root("lifecycle-retry");
        let paths = test_paths(&root);
        let backend = Arc::new(ControlledBackend::default());
        let coordinator = backend.coordinator();
        let retention_claims = BenchmarkSuiteRetentionClaims::default();
        let suite_id = test_suite_id("suite-a", "development");
        backend.set_fail_writes(true);
        let store = Arc::new(
            BenchmarkSuiteDriverStore::try_load_from_paths_with_coordinator_and_retention_claims(
                &paths,
                coordinator.clone(),
                retention_claims.clone(),
            )
            .expect("store"),
        );
        let task_store = store.clone();
        let task_suite_id = suite_id.clone();
        let start_task = tokio::spawn(async move {
            task_store
                .start(
                    task_suite_id,
                    "development".to_string(),
                    30_000,
                    test_summary(),
                )
                .await
        });
        let driver_id = "benchmark-suite-driver-0000000000000001";
        wait_for_retry_candidate(&store, driver_id).await;
        start_task.abort();
        let _ = start_task.await;
        wait_for_retry_candidate(&store, driver_id).await;
        assert!(retention_claims.has_claim(driver_id, &suite_id));

        assert!(matches!(
            store.flush().await,
            Err(BenchmarkSuiteDriverStoreError::Persistence(_))
        ));
        assert!(store.has_retry_candidate(driver_id));
        backend.set_fail_writes(false);
        backend.gate();
        let flush_store = store.clone();
        let flush_task = tokio::spawn(async move { flush_store.flush().await });
        tokio::time::timeout(Duration::from_secs(2), async {
            while !backend.entered_write.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("lifecycle retry entered writer");
        let competing_store = store.clone();
        let competing_suite_id = suite_id.clone();
        let competing_start = tokio::spawn(async move {
            competing_store
                .start(
                    competing_suite_id,
                    "development".to_string(),
                    30_000,
                    test_summary(),
                )
                .await
        });
        tokio::task::yield_now().await;
        assert!(!competing_start.is_finished());
        backend.release();
        flush_task
            .await
            .expect("flush task")
            .expect("flush retries retained start");
        assert!(matches!(
            competing_start.await.expect("competing start task"),
            Err(BenchmarkSuiteDriverStartError::Conflict)
        ));
        assert!(store.retry_candidate_ids().is_empty());
        assert_eq!(
            store.get(driver_id).await.expect("flushed start").state,
            "scheduled"
        );

        backend.set_fail_writes(true);
        let task_store = store.clone();
        let terminal_task = tokio::spawn(async move { task_store.record_stopped(driver_id).await });
        wait_for_retry_candidate(&store, driver_id).await;
        terminal_task.abort();
        let _ = terminal_task.await;
        wait_for_retry_candidate(&store, driver_id).await;
        assert!(matches!(
            store.close().await,
            Err(BenchmarkSuiteDriverStoreError::Persistence(_))
        ));
        assert!(store.has_retry_candidate(driver_id));
        assert!(retention_claims.has_claim(driver_id, &suite_id));
        assert!(matches!(
            BenchmarkSuiteDriverStore::try_load_from_paths_with_coordinator(
                &paths,
                coordinator.clone(),
            ),
            Err(BenchmarkSuiteDriverStoreError::Persistence(ref error))
                if error.kind() == io::ErrorKind::AlreadyExists
        ));
        backend.set_fail_writes(false);
        store
            .close()
            .await
            .expect("close retries terminal state and releases owner");
        assert!(store.retry_candidate_ids().is_empty());
        assert!(!retention_claims.has_claim(driver_id, &suite_id));

        let reloaded =
            BenchmarkSuiteDriverStore::try_load_from_paths_with_coordinator(&paths, coordinator)
                .expect("closed owner can be reclaimed");
        assert_eq!(
            reloaded
                .get(driver_id)
                .await
                .expect("reloaded terminal")
                .state,
            "stopped"
        );
        reloaded.close().await.expect("reloaded store closes");
        cleanup(&root);
    }

    #[tokio::test]
    async fn restart_resume_queue_skips_terminal_and_manual_interrupted_drivers() {
        let root = test_root("resume-skip-terminal");
        let paths = test_paths(&root);
        let dir = driver_dir(&paths);
        fs::create_dir_all(&dir).expect("create driver dir");
        for (index, state, error) in [
            (1, "stopped", None),
            (2, "failed", Some("manual failure")),
            (3, "complete", None),
            (4, "interrupted", Some("driver stopped by user")),
        ] {
            let status = status_fixture(index, state, error);
            fs::write(
                driver_path(&dir, &status.id),
                serde_json::to_string_pretty(&status).expect("serialize driver"),
            )
            .expect("write driver");
        }

        let store = BenchmarkSuiteDriverStore::load_from_paths(&paths);

        assert!(
            store
                .take_restart_interrupted_resumable_drivers()
                .await
                .expect("terminal drivers need no reconciliation")
                .is_empty()
        );
        cleanup(&root);
    }

    #[tokio::test]
    async fn restart_resume_queue_is_capped() {
        let root = test_root("resume-cap");
        let paths = test_paths(&root);
        let dir = driver_dir(&paths);
        fs::create_dir_all(&dir).expect("create driver dir");
        let total = MAX_RESUMABLE_DRIVERS + 3;
        for index in 1..=total {
            let status = status_fixture(index as u64, "active", None);
            fs::write(
                driver_path(&dir, &status.id),
                serde_json::to_string_pretty(&status).expect("serialize driver"),
            )
            .expect("write driver");
        }

        let retention_claims = BenchmarkSuiteRetentionClaims::default();
        let store = BenchmarkSuiteDriverStore::load_from_paths_with_retention_claims(
            &paths,
            retention_claims.clone(),
        );
        for index in 1..=total {
            let status = status_fixture(index as u64, "active", None);
            assert!(retention_claims.has_claim(&status.id, &status.suite_id));
        }
        let pending = store
            .take_restart_interrupted_resumable_drivers()
            .await
            .expect("restart candidates persist");
        let limited = store
            .list_recent(total)
            .await
            .into_iter()
            .filter(|status| status.error.as_deref() == Some(AUTOMATIC_RESUME_LIMIT_ERROR))
            .count();

        assert_eq!(pending.len(), MAX_RESUMABLE_DRIVERS);
        assert_eq!(limited, total - MAX_RESUMABLE_DRIVERS);
        let pending_ids = pending
            .iter()
            .map(|status| status.id.as_str())
            .collect::<HashSet<_>>();
        for index in 1..=total {
            let status = status_fixture(index as u64, "active", None);
            assert_eq!(
                retention_claims.has_claim(&status.id, &status.suite_id),
                pending_ids.contains(status.id.as_str())
            );
        }
        cleanup(&root);
    }

    #[tokio::test]
    async fn persisted_terminal_driver_status_remains_visible_after_restart() {
        let root = test_root("terminal-visible");
        let paths = test_paths(&root);
        let summary = BenchmarkSuiteDriverSuiteSummary {
            run_count: 2,
            launched_run_count: 1,
            pending_run_index: Some(1),
        };
        let store = BenchmarkSuiteDriverStore::load_from_paths(&paths);
        let started = store
            .start(
                test_suite_id("suite-dev", "development"),
                "development".to_string(),
                30_000,
                summary.clone(),
            )
            .await
            .expect("driver starts");
        store
            .record_complete(
                &started.status.id,
                BenchmarkSuiteDriverSuiteSummary {
                    run_count: 2,
                    launched_run_count: 2,
                    pending_run_index: None,
                },
            )
            .await
            .expect("terminal state persists");
        store.close().await.expect("first store closes");

        let reloaded = BenchmarkSuiteDriverStore::load_from_paths(&paths);
        let status = reloaded
            .get(&started.status.id)
            .await
            .expect("loaded complete driver");

        assert_eq!(status.state, "complete");
        assert_eq!(status.error, None);

        cleanup(&root);
    }

    #[tokio::test]
    async fn terminal_retention_is_absolute_and_prefers_newest_per_suite() {
        let root = test_root("terminal-retention-bound");
        let paths = test_paths(&root);
        let backend = Arc::new(ControlledBackend::default());
        let coordinator = backend.coordinator();
        let store = BenchmarkSuiteDriverStore::try_load_from_paths_with_coordinator(
            &paths,
            coordinator.clone(),
        )
        .expect("store");
        let mut ids = Vec::new();
        for index in 0..(MAX_RETAINED_TERMINAL_DRIVERS + 8) {
            let suite_id = if index >= MAX_RETAINED_TERMINAL_DRIVERS {
                test_suite_id("suite-repeat", "development")
            } else {
                test_suite_id(&format!("suite-{index:02}"), "development")
            };
            ids.push(persist_complete_driver(&store, suite_id).await);
        }

        let statuses = store.list_recent(100).await;
        assert_eq!(statuses.len(), MAX_RETAINED_TERMINAL_DRIVERS);
        assert!(
            statuses
                .iter()
                .all(|status| !is_non_terminal(&status.state))
        );
        for pruned in std::iter::once(&ids[0]).chain(ids[MAX_RETAINED_TERMINAL_DRIVERS..39].iter())
        {
            assert!(store.get(pruned).await.is_none());
            assert!(!driver_path(&driver_dir(&paths), pruned).exists());
        }
        for retained in ids[1..MAX_RETAINED_TERMINAL_DRIVERS]
            .iter()
            .chain(std::iter::once(&ids[39]))
        {
            assert!(store.get(retained).await.is_some());
            assert!(driver_path(&driver_dir(&paths), retained).is_file());
        }
        assert_eq!(
            store
                .persistence
                .as_ref()
                .expect("persistence")
                .writer_count(),
            MAX_RETAINED_TERMINAL_DRIVERS
        );

        let reclaimed_path = driver_path(&driver_dir(&paths), &ids[0]);
        let reclaimed = coordinator
            .claim_owner(&reclaimed_path)
            .expect("pruned exact path owner is released");
        reclaimed
            .writer(&reclaimed_path, benchmark_suite_driver_target(&ids[0]))
            .expect("pruned exact writer is released");
        reclaimed.close().await.expect("reclaimed owner closes");
        store.close().await.expect("store closes");
        cleanup(&root);
    }

    #[test]
    fn terminal_retention_protects_exact_obligations_not_historical_marker_shapes() {
        let mut inner = BenchmarkSuiteDriverInner::default();
        let historical_marker =
            status_fixture(1, "interrupted", Some(AUTOMATIC_RESUME_QUEUED_ERROR));
        insert_test_driver(&mut inner, historical_marker.clone());
        for index in 2..=(MAX_RETAINED_TERMINAL_DRIVERS as u64 + 5) {
            insert_test_driver(&mut inner, status_fixture(index, "complete", None));
        }

        let retry_status = status_fixture(100, "complete", None);
        insert_test_driver(&mut inner, retry_status.clone());
        let retry_entries =
            HashMap::from([(retry_status.id.clone(), test_entry(retry_status.clone()))]);
        let active = status_fixture(101, "active", None);
        insert_test_driver(&mut inner, active.clone());
        let handoff = status_fixture(102, "interrupted", Some(AUTOMATIC_RESUME_QUEUED_ERROR));
        insert_test_driver(&mut inner, handoff.clone());
        let excluded = status_fixture(103, "complete", None);
        insert_test_driver(&mut inner, excluded.clone());

        let candidates = terminal_driver_prune_candidates(
            &inner,
            &retry_entries,
            &HashSet::from([handoff.id.clone()]),
            &HashSet::from([excluded.id.clone()]),
            &HashSet::new(),
        );

        assert!(
            candidates
                .iter()
                .any(|status| status.id == historical_marker.id)
        );
        assert!(!candidates.iter().any(|status| status.id == retry_status.id));
        assert!(!candidates.iter().any(|status| status.id == active.id));
        assert!(!candidates.iter().any(|status| status.id == handoff.id));
        assert!(!candidates.iter().any(|status| status.id == excluded.id));

        let retained_issue_id = format!(
            "{DRIVER_ID_PREFIX}{:016x}",
            MAX_RETAINED_TERMINAL_DRIVERS as u64 + 5
        );
        let retry_candidates = terminal_driver_prune_candidates(
            &inner,
            &retry_entries,
            &HashSet::from([handoff.id]),
            &HashSet::from([excluded.id]),
            &HashSet::from([retained_issue_id.clone()]),
        );
        assert!(
            retry_candidates
                .iter()
                .any(|status| status.id == retained_issue_id),
            "an exact failed cleanup remains retryable inside the current horizon"
        );
    }

    #[test]
    fn terminal_retention_with_more_than_32_suites_keeps_newest_tied_ids() {
        let mut inner = BenchmarkSuiteDriverInner::default();
        let total = MAX_RETAINED_TERMINAL_DRIVERS + 3;
        for index in 1..=total {
            insert_test_driver(&mut inner, status_fixture(index as u64, "complete", None));
        }

        let candidates = terminal_driver_prune_candidates(
            &inner,
            &HashMap::new(),
            &HashSet::new(),
            &HashSet::new(),
            &HashSet::new(),
        );

        assert_eq!(
            candidates
                .iter()
                .map(|status| status.id.clone())
                .collect::<Vec<_>>(),
            (1..=3)
                .map(|index| format!("{DRIVER_ID_PREFIX}{index:016x}"))
                .collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn aged_terminal_prunes_while_its_effect_lease_blocks_successor() {
        let root = test_root("terminal-retention-live-effect");
        let paths = test_paths(&root);
        let store = BenchmarkSuiteDriverStore::load_from_paths(&paths);
        let held_suite_id = test_suite_id("suite-held", "development");
        let held = store
            .start(
                held_suite_id.clone(),
                "development".to_string(),
                30_000,
                test_summary(),
            )
            .await
            .expect("held driver starts");
        let held_id = held.status.id.clone();
        let held_owner = held.effect_owner;
        store
            .record_complete(
                &held_id,
                BenchmarkSuiteDriverSuiteSummary {
                    run_count: 2,
                    launched_run_count: 2,
                    pending_run_index: None,
                },
            )
            .await
            .expect("held driver completes");
        for index in 0..MAX_RETAINED_TERMINAL_DRIVERS {
            persist_complete_driver(
                &store,
                test_suite_id(&format!("suite-new-{index}"), "development"),
            )
            .await;
        }

        assert!(store.get(&held_id).await.is_none());
        assert!(!driver_path(&driver_dir(&paths), &held_id).exists());
        assert!(matches!(
            store
                .start(
                    held_suite_id.clone(),
                    "development".to_string(),
                    30_000,
                    test_summary(),
                )
                .await,
            Err(BenchmarkSuiteDriverStartError::Conflict)
        ));

        drop(held_owner);
        let replacement = store
            .start(
                held_suite_id,
                "development".to_string(),
                30_000,
                test_summary(),
            )
            .await
            .expect("released effect lease allows successor");
        store
            .record_stopped(&replacement.status.id)
            .await
            .expect("replacement stops");
        drop(replacement.effect_owner);
        store.close().await.expect("store closes");
        cleanup(&root);
    }

    #[test]
    fn queued_handoff_obligation_survives_ready_drain_until_exact_consumption() {
        let queued = status_fixture(1, "interrupted", Some(AUTOMATIC_RESUME_QUEUED_ERROR));
        let mut inner = BenchmarkSuiteDriverInner::default();
        inner.restart_candidates.push(test_entry(queued.clone()));
        let handoff_obligation_ids = SyncMutex::new(HashSet::new());
        let suite_retention_claims = BenchmarkSuiteRetentionClaims::default();

        apply_driver_transition(
            &mut inner,
            test_entry(queued.clone()),
            &handoff_obligation_ids,
            &suite_retention_claims,
        );
        assert_eq!(inner.ready_resume_ids, vec![queued.id.clone()]);
        assert!(
            handoff_obligation_ids
                .lock()
                .expect("handoff lock")
                .contains(&queued.id)
        );
        assert!(suite_retention_claims.has_claim(&queued.id, &queued.suite_id));

        inner.ready_resume_ids.clear();
        let mut successor = status_fixture(2, "scheduled", None);
        successor.suite_id = queued.suite_id.clone();
        let successor_id = successor.id.clone();
        apply_driver_transition(
            &mut inner,
            test_entry(successor.clone()),
            &handoff_obligation_ids,
            &suite_retention_claims,
        );
        assert!(
            handoff_obligation_ids
                .lock()
                .expect("handoff lock")
                .contains(&queued.id)
        );
        assert!(suite_retention_claims.has_claim(&queued.id, &queued.suite_id));
        assert!(suite_retention_claims.has_claim(&successor_id, &queued.suite_id));

        let mut consumed = queued.clone();
        consumed.error = Some(AUTOMATIC_RESUME_STARTED_ERROR.to_string());
        apply_driver_transition(
            &mut inner,
            test_entry(consumed),
            &handoff_obligation_ids,
            &suite_retention_claims,
        );
        assert!(
            handoff_obligation_ids
                .lock()
                .expect("handoff lock")
                .is_empty()
        );
        assert!(!suite_retention_claims.has_claim(&queued.id, &queued.suite_id));
        assert!(suite_retention_claims.has_claim(&successor_id, &queued.suite_id));

        successor.state = "complete".to_string();
        apply_driver_transition(
            &mut inner,
            test_entry(successor),
            &handoff_obligation_ids,
            &suite_retention_claims,
        );
        assert!(!suite_retention_claims.has_claim(&successor_id, &queued.suite_id));
    }

    #[tokio::test]
    async fn failed_terminal_delete_blocks_lifecycle_until_exact_retry() {
        let root = test_root("terminal-retention-delete-retry");
        let paths = test_paths(&root);
        let backend = Arc::new(ControlledBackend::default());
        let coordinator = backend.coordinator();
        let store = BenchmarkSuiteDriverStore::try_load_from_paths_with_coordinator(
            &paths,
            coordinator.clone(),
        )
        .expect("store");
        let mut ids = Vec::new();
        for index in 0..MAX_RETAINED_TERMINAL_DRIVERS {
            ids.push(
                persist_complete_driver(
                    &store,
                    test_suite_id(&format!("suite-{index}"), "development"),
                )
                .await,
            );
        }
        let oldest_path = driver_path(&driver_dir(&paths), &ids[0]);
        fs::remove_file(&oldest_path).expect("remove oldest status");
        fs::create_dir(&oldest_path).expect("block oldest status deletion");

        persist_complete_driver(&store, test_suite_id("suite-new", "development")).await;

        assert_eq!(
            store.list_recent(100).await.len(),
            MAX_RETAINED_TERMINAL_DRIVERS + 1
        );
        let issues = store.retention_issues();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].driver_id, ids[0]);
        assert_eq!(
            issues[0].kind,
            BenchmarkSuiteDriverRetentionIssueKind::Delete
        );
        assert!(
            issues[0]
                .facts
                .iter()
                .any(|fact| fact.kind == ExecutionFactKind::PrimitiveRefused)
        );
        assert!(matches!(
            store.flush().await,
            Err(BenchmarkSuiteDriverStoreError::Persistence(ref error))
                if error.to_string()
                    == "benchmark suite driver terminal retention cleanup is pending"
        ));
        assert!(matches!(
            store.close().await,
            Err(BenchmarkSuiteDriverStoreError::Persistence(ref error))
                if error.to_string()
                    == "benchmark suite driver terminal retention cleanup is pending"
        ));
        assert!(matches!(
            BenchmarkSuiteDriverStore::try_load_from_paths_with_coordinator(
                &paths,
                coordinator.clone(),
            ),
            Err(BenchmarkSuiteDriverStoreError::Persistence(ref error))
                if error.kind() == io::ErrorKind::AlreadyExists
        ));

        fs::remove_dir(&oldest_path).expect("unblock oldest status deletion");
        assert!(store.retry_terminal_retention().await.is_empty());
        assert!(store.get(&ids[0]).await.is_none());
        assert_eq!(
            store.list_recent(100).await.len(),
            MAX_RETAINED_TERMINAL_DRIVERS
        );
        store.close().await.expect("cleanup retry allows close");

        let reclaimed = coordinator
            .claim_owner(driver_dir(&paths))
            .expect("closed store owner is released");
        reclaimed
            .writer(&oldest_path, benchmark_suite_driver_target(&ids[0]))
            .expect("pruned exact writer is released");
        reclaimed.close().await.expect("reclaimed owner closes");
        cleanup(&root);
    }

    #[tokio::test]
    async fn failed_terminal_writer_settlement_retries_exact_status() {
        let root = test_root("terminal-retention-settle-retry");
        let paths = test_paths(&root);
        let backend = Arc::new(ControlledBackend::default());
        let store = BenchmarkSuiteDriverStore::try_load_from_paths_with_coordinator(
            &paths,
            backend.coordinator(),
        )
        .expect("store");
        let mut ids = Vec::new();
        for index in 0..MAX_RETAINED_TERMINAL_DRIVERS {
            ids.push(
                persist_complete_driver(
                    &store,
                    test_suite_id(&format!("suite-{index}"), "development"),
                )
                .await,
            );
        }
        let oldest_id = &ids[0];
        let oldest_path = driver_path(&driver_dir(&paths), oldest_id);
        let oldest_status = store.get(oldest_id).await.expect("oldest terminal status");
        backend.set_fail_destination(Some(oldest_path.clone()));
        store
            .persistence
            .as_ref()
            .expect("persistence")
            .writer(oldest_id)
            .expect("oldest writer")
            .accept(oldest_status, WriteUrgency::Debounced, encode_driver_status)
            .expect("pending exact status accepted");

        persist_complete_driver(&store, test_suite_id("suite-new", "development")).await;

        let issues = store.retention_issues();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].driver_id, *oldest_id);
        assert_eq!(
            issues[0].kind,
            BenchmarkSuiteDriverRetentionIssueKind::WriterSettlement
        );
        assert!(store.get(oldest_id).await.is_some());
        assert!(oldest_path.is_file());

        backend.set_fail_destination(None);
        assert!(store.retry_terminal_retention().await.is_empty());
        assert!(store.get(oldest_id).await.is_none());
        assert!(!oldest_path.exists());
        assert_eq!(
            store.list_recent(100).await.len(),
            MAX_RETAINED_TERMINAL_DRIVERS
        );
        store.close().await.expect("store closes after retry");
        cleanup(&root);
    }

    #[tokio::test]
    async fn startup_retention_prunes_only_unambiguous_canonical_terminals() {
        let root = test_root("startup-terminal-retention");
        let paths = test_paths(&root);
        let dir = driver_dir(&paths);
        fs::create_dir_all(&dir).expect("create driver dir");
        let total = MAX_RETAINED_TERMINAL_DRIVERS + 3;
        let mut ids = Vec::new();
        for index in 1..=total {
            let mut status = status_fixture(index as u64, "complete", None);
            status.created_at = format!("2026-07-10T00:{index:02}:00Z");
            status.updated_at = status.created_at.clone();
            write_status_fixture(&dir, &status);
            ids.push(status.id);
        }
        let malformed_path = dir.join("malformed.json");
        fs::write(&malformed_path, b"{not-json").expect("write malformed status");
        let noncanonical = status_fixture(100, "complete", None);
        let noncanonical_path = dir.join("copied-terminal.json");
        fs::write(
            &noncanonical_path,
            serde_json::to_vec_pretty(&noncanonical).expect("encode noncanonical status"),
        )
        .expect("write noncanonical status");
        let unsafe_path = dir.join("unknown-owned.json");
        let mut unsafe_status = status_fixture(101, "complete", None);
        unsafe_status.id = "../../unknown-owned".to_string();
        fs::write(
            &unsafe_path,
            serde_json::to_vec_pretty(&unsafe_status).expect("encode unsafe status"),
        )
        .expect("write unsafe status");

        let mut ambiguous_terminal = status_fixture(200, "complete", None);
        ambiguous_terminal.suite_id = test_suite_id("suite-ambiguous", "development");
        write_status_fixture(&dir, &ambiguous_terminal);
        for index in [201, 202] {
            let mut active = status_fixture(index, "active", None);
            active.suite_id = ambiguous_terminal.suite_id.clone();
            write_status_fixture(&dir, &active);
        }

        let retention_claims = BenchmarkSuiteRetentionClaims::default();
        let store = BenchmarkSuiteDriverStore::load_from_paths_with_retention_claims(
            &paths,
            retention_claims.clone(),
        );
        let pending = store
            .take_restart_interrupted_resumable_drivers()
            .await
            .expect("startup retention settles");

        assert!(pending.is_empty());
        for id in &ids[..3] {
            assert!(store.get(id).await.is_none());
            assert!(!driver_path(&dir, id).exists());
        }
        for id in &ids[3..] {
            assert!(store.get(id).await.is_some());
            assert!(driver_path(&dir, id).is_file());
        }
        assert!(store.get(&ambiguous_terminal.id).await.is_some());
        assert!(driver_path(&dir, &ambiguous_terminal.id).is_file());
        assert!(retention_claims.has_claim(&ambiguous_terminal.id, &ambiguous_terminal.suite_id));
        assert!(malformed_path.is_file());
        assert!(noncanonical_path.is_file());
        assert!(unsafe_path.is_file());
        for index in [201, 202] {
            let status = status_fixture(index, "active", None);
            assert!(driver_path(&dir, &status.id).is_file());
            assert!(retention_claims.has_claim(&status.id, &ambiguous_terminal.suite_id));
        }
        assert_eq!(
            store.list_recent(100).await.len(),
            MAX_RETAINED_TERMINAL_DRIVERS + 1
        );
        store.close().await.expect("store closes");
        assert!(retention_claims.has_claim(&ambiguous_terminal.id, &ambiguous_terminal.suite_id));

        let reloaded = BenchmarkSuiteDriverStore::load_from_paths_with_retention_claims(
            &paths,
            retention_claims,
        );
        assert!(
            reloaded
                .take_restart_interrupted_resumable_drivers()
                .await
                .expect("startup retention is idempotent")
                .is_empty()
        );
        let reloaded_statuses = reloaded.list_recent(100).await;
        assert_eq!(
            reloaded_statuses
                .iter()
                .filter(|status| status.id != ambiguous_terminal.id)
                .count(),
            MAX_RETAINED_TERMINAL_DRIVERS
        );
        assert!(
            reloaded_statuses
                .iter()
                .any(|status| status.id == ambiguous_terminal.id)
        );
        assert!(driver_path(&dir, &ambiguous_terminal.id).is_file());
        assert!(malformed_path.is_file());
        assert!(noncanonical_path.is_file());
        assert!(unsafe_path.is_file());
        for index in [201, 202] {
            assert!(driver_path(&dir, &status_fixture(index, "active", None).id).is_file());
        }
        reloaded.close().await.expect("reloaded store closes");
        cleanup(&root);
    }

    #[tokio::test]
    async fn terminal_retention_finishes_after_waiting_caller_is_aborted() {
        let root = test_root("terminal-retention-abort");
        let paths = test_paths(&root);
        let backend = Arc::new(ControlledBackend::default());
        let store = Arc::new(
            BenchmarkSuiteDriverStore::try_load_from_paths_with_coordinator(
                &paths,
                backend.coordinator(),
            )
            .expect("store"),
        );
        let mut ids = Vec::new();
        for index in 0..MAX_RETAINED_TERMINAL_DRIVERS {
            ids.push(
                persist_complete_driver(
                    &store,
                    test_suite_id(&format!("suite-{index}"), "development"),
                )
                .await,
            );
        }
        let started = store
            .start(
                test_suite_id("suite-new", "development"),
                "development".to_string(),
                30_000,
                test_summary(),
            )
            .await
            .expect("new driver starts");
        let terminal_id = started.status.id.clone();
        drop(started.effect_owner);
        backend.gate();
        let task_store = store.clone();
        let task_id = terminal_id.clone();
        let task = tokio::spawn(async move {
            task_store
                .record_complete(
                    &task_id,
                    BenchmarkSuiteDriverSuiteSummary {
                        run_count: 2,
                        launched_run_count: 2,
                        pending_run_index: None,
                    },
                )
                .await
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            while !backend.entered_write.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("terminal writer entered");
        task.abort();
        let _ = task.await;
        backend.release();

        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if store.get(&ids[0]).await.is_none()
                    && store
                        .get(&terminal_id)
                        .await
                        .is_some_and(|status| status.state == "complete")
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("detached commit observer completes retention");
        assert_eq!(
            store.list_recent(100).await.len(),
            MAX_RETAINED_TERMINAL_DRIVERS
        );
        assert!(!driver_path(&driver_dir(&paths), &ids[0]).exists());
        store.close().await.expect("store closes");
        cleanup(&root);
    }

    #[test]
    fn persisted_driver_with_unknown_fields_is_not_loaded_and_records_safe_issue() {
        let root = test_root("unknown-field");
        let paths = test_paths(&root);
        let dir = driver_dir(&paths);
        fs::create_dir_all(&dir).expect("create driver dir");
        let path = driver_path(&dir, "benchmark-suite-driver-0000000000000001");
        let suite_id = test_suite_id("unknown-field", "development");
        fs::write(
            path,
            serde_json::to_string_pretty(&serde_json::json!({
                "id": "benchmark-suite-driver-0000000000000001",
                "suite_id": suite_id,
                "mode": "development",
                "state": "complete",
                "interval_ms": 30000,
                "run_count": 1,
                "launched_run_count": 1,
                "unexpected_state": true,
                "created_at": "2026-01-01T00:00:00.000Z",
                "updated_at": "2026-01-01T00:01:00.000Z"
            }))
            .expect("serialize driver"),
        )
        .expect("write driver");

        let load_state = load_persisted_driver_inner(&dir);

        assert!(load_state.inner.drivers.is_empty());
        assert!(load_state.inner.restart_candidates.is_empty());
        assert_eq!(
            load_state.issues,
            vec![BenchmarkSuiteDriverLoadIssue {
                kind: BenchmarkSuiteDriverLoadIssueKind::StatusInvalid,
                count: 1,
            }]
        );
        let encoded = format!("{:?}", load_state.issues);
        assert!(!encoded.contains(root.to_string_lossy().as_ref()));
        assert!(!encoded.contains("unexpected_state"));
        cleanup(&root);
    }

    #[test]
    fn persisted_driver_with_noncanonical_filename_is_not_loaded() {
        let root = test_root("noncanonical-filename");
        let paths = test_paths(&root);
        let dir = driver_dir(&paths);
        fs::create_dir_all(&dir).expect("create driver dir");
        let status = status_fixture(1, "active", None);
        fs::write(
            dir.join("copied-driver.json"),
            serde_json::to_vec_pretty(&status).expect("serialize driver"),
        )
        .expect("write noncanonical driver");

        let load_state = load_persisted_driver_inner(&dir);

        assert!(load_state.inner.drivers.is_empty());
        assert!(load_state.inner.restart_candidates.is_empty());
        assert_eq!(
            load_state.issues,
            vec![BenchmarkSuiteDriverLoadIssue {
                kind: BenchmarkSuiteDriverLoadIssueKind::NonCanonicalFilename,
                count: 1,
            }]
        );
        cleanup(&root);
    }

    #[tokio::test]
    async fn duplicate_driver_id_protects_every_valid_suite_without_resume() {
        let root = test_root("duplicate-id");
        let paths = test_paths(&root);
        let dir = driver_dir(&paths);
        fs::create_dir_all(&dir).expect("create driver dir");
        let status = status_fixture(1, "active", None);
        let mut duplicate = status.clone();
        duplicate.suite_id = test_suite_id("duplicate-other-suite", "development");
        let encoded = serde_json::to_vec_pretty(&status).expect("serialize driver");
        fs::write(driver_path(&dir, &status.id), &encoded).expect("write canonical driver");
        fs::write(
            dir.join("aaa-duplicate.json"),
            serde_json::to_vec_pretty(&duplicate).expect("serialize duplicate driver"),
        )
        .expect("write duplicate driver");

        let retention_claims = BenchmarkSuiteRetentionClaims::default();
        let store = BenchmarkSuiteDriverStore::load_from_paths_with_retention_claims(
            &paths,
            retention_claims.clone(),
        );

        assert!(
            store
                .inner
                .read()
                .expect(DRIVER_STORE_LOCK_INVARIANT)
                .drivers
                .is_empty()
        );
        assert_eq!(
            store
                .inner
                .read()
                .expect(DRIVER_STORE_LOCK_INVARIANT)
                .next_id,
            1
        );
        assert_eq!(
            store.load_issues(),
            vec![BenchmarkSuiteDriverLoadIssue {
                kind: BenchmarkSuiteDriverLoadIssueKind::DuplicateDriverId,
                count: 1,
            }]
        );
        assert!(retention_claims.has_claim(
            &duplicate_retention_claim_owner(&status.id, 0),
            &duplicate.suite_id
        ));
        assert!(retention_claims.has_claim(
            &duplicate_retention_claim_owner(&status.id, 1),
            &status.suite_id
        ));
        store.close().await.expect("store closes");
        assert_eq!(retention_claims.claimed_suite_ids().len(), 2);
        cleanup(&root);
    }

    #[test]
    fn persisted_driver_with_unknown_state_is_not_loaded_or_resumed() {
        let root = test_root("unknown-state");
        let paths = test_paths(&root);
        let dir = driver_dir(&paths);
        fs::create_dir_all(&dir).expect("create driver dir");
        let status = status_fixture(1, "future_running_state", None);
        fs::write(
            driver_path(&dir, &status.id),
            serde_json::to_vec_pretty(&status).expect("serialize driver"),
        )
        .expect("write unknown state");

        let load_state = load_persisted_driver_inner(&dir);

        assert!(load_state.inner.drivers.is_empty());
        assert!(load_state.inner.restart_candidates.is_empty());
        assert_eq!(load_state.inner.next_id, 1);
        assert_eq!(
            load_state.issues,
            vec![BenchmarkSuiteDriverLoadIssue {
                kind: BenchmarkSuiteDriverLoadIssueKind::UnknownState,
                count: 1,
            }]
        );
        cleanup(&root);
    }

    #[test]
    fn every_produced_driver_state_round_trips_through_strict_loader() {
        let root = test_root("produced-state-roundtrip");
        let paths = test_paths(&root);
        let dir = driver_dir(&paths);
        fs::create_dir_all(&dir).expect("create driver dir");
        let shapes = [
            (1, "scheduled", None),
            (2, "active", None),
            (3, "launched_next", None),
            (4, "complete", None),
            (5, "failed", Some("bounded failure")),
            (6, "stopped", None),
            (7, "interrupted", Some(AUTOMATIC_RESUME_QUEUED_ERROR)),
            (8, "interrupted", Some(AUTOMATIC_RESUME_STARTED_ERROR)),
        ];
        for (index, state, error) in shapes {
            let mut status = status_fixture(index, state, error);
            let mode = match index % 3 {
                0 => "development",
                1 => "qualification",
                _ => "release_validation",
            };
            status.mode = mode.to_string();
            status.suite_id = test_suite_id(&format!("fixture-{index}"), mode);
            if state == "launched_next" {
                status.last_run_index = Some(0);
                status.last_session_id = Some(format!("session-{index}"));
            }
            if state == "complete" {
                status.launched_run_count = status.run_count;
            }
            write_status_fixture(&dir, &status);
        }

        let load_state = load_persisted_driver_inner(&dir);

        assert!(load_state.issues.is_empty());
        assert_eq!(load_state.inner.restart_candidates.len(), 4);
        assert_eq!(load_state.inner.drivers.len(), 4);
        cleanup(&root);
    }

    #[test]
    fn invalid_driver_timestamps_are_rejected_with_bounded_issue() {
        let root = test_root("invalid-timestamp");
        let paths = test_paths(&root);
        let dir = driver_dir(&paths);
        fs::create_dir_all(&dir).expect("create driver dir");
        let mut status = status_fixture(1, "stopped", None);
        status.updated_at = "not-a-timestamp /home/secret".to_string();
        write_status_fixture(&dir, &status);

        let load_state = load_persisted_driver_inner(&dir);

        assert!(load_state.inner.drivers.is_empty());
        assert!(load_state.inner.restart_candidates.is_empty());
        assert_eq!(
            load_state.issues,
            vec![BenchmarkSuiteDriverLoadIssue {
                kind: BenchmarkSuiteDriverLoadIssueKind::TimestampInvalid,
                count: 1,
            }]
        );
        cleanup(&root);
    }

    #[test]
    fn unsafe_suite_mode_and_session_fields_are_rejected_without_echo() {
        let root = test_root("unsafe-public-fields");
        let paths = test_paths(&root);
        let dir = driver_dir(&paths);
        fs::create_dir_all(&dir).expect("create driver dir");
        let mut unsafe_suite = status_fixture(1, "stopped", None);
        unsafe_suite.suite_id = "/home/secret/suite".to_string();
        let mut unsafe_mode = status_fixture(2, "stopped", None);
        unsafe_mode.mode = "../../secret-mode".to_string();
        let mut unsafe_session = status_fixture(3, "active", None);
        unsafe_session.active_session_id = Some("C:\\Users\\Secret\\session".to_string());
        for status in [&unsafe_suite, &unsafe_mode, &unsafe_session] {
            write_status_fixture(&dir, status);
        }

        let load_state = load_persisted_driver_inner(&dir);

        assert!(load_state.inner.drivers.is_empty());
        assert!(load_state.inner.restart_candidates.is_empty());
        assert_eq!(
            load_state.issues,
            vec![BenchmarkSuiteDriverLoadIssue {
                kind: BenchmarkSuiteDriverLoadIssueKind::UnsafePublicField,
                count: 3,
            }]
        );
        let encoded = format!("{:?}", load_state.issues);
        assert!(!encoded.contains("Secret"));
        assert!(!encoded.contains(root.to_string_lossy().as_ref()));
        cleanup(&root);
    }

    #[test]
    fn incoherent_driver_state_is_rejected_without_public_admission() {
        let root = test_root("incoherent-state");
        let paths = test_paths(&root);
        let dir = driver_dir(&paths);
        fs::create_dir_all(&dir).expect("create driver dir");
        let mut status = status_fixture(1, "scheduled", None);
        status.active_session_id = Some("session-should-not-be-active".to_string());
        status.launched_run_count = status.run_count + 1;
        write_status_fixture(&dir, &status);

        let load_state = load_persisted_driver_inner(&dir);

        assert!(load_state.inner.drivers.is_empty());
        assert!(load_state.inner.restart_candidates.is_empty());
        assert_eq!(
            load_state.issues,
            vec![BenchmarkSuiteDriverLoadIssue {
                kind: BenchmarkSuiteDriverLoadIssueKind::IncoherentStatus,
                count: 1,
            }]
        );
        cleanup(&root);
    }

    #[test]
    fn conflicting_active_drivers_for_one_suite_are_not_resumed() {
        let root = test_root("conflicting-suite");
        let paths = test_paths(&root);
        let dir = driver_dir(&paths);
        fs::create_dir_all(&dir).expect("create driver dir");
        for index in [1, 2] {
            let mut status = status_fixture(index, "active", None);
            status.suite_id = test_suite_id("same-suite", "development");
            fs::write(
                driver_path(&dir, &status.id),
                serde_json::to_vec_pretty(&status).expect("serialize driver"),
            )
            .expect("write active driver");
        }

        let load_state = load_persisted_driver_inner(&dir);

        assert!(load_state.inner.drivers.is_empty());
        assert!(load_state.inner.restart_candidates.is_empty());
        assert_eq!(
            load_state.issues,
            vec![BenchmarkSuiteDriverLoadIssue {
                kind: BenchmarkSuiteDriverLoadIssueKind::ConflictingActiveSuite,
                count: 2,
            }]
        );
        cleanup(&root);
    }

    #[test]
    fn driver_status_path_uses_sanitized_local_filename() {
        let root = test_root("safe-filename");
        let paths = test_paths(&root);
        let dir = driver_dir(&paths);
        let path = driver_path(&dir, "../../secret\\driver;id");
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

    #[test]
    fn driver_error_sanitizer_bounds_and_removes_sensitive_shapes() {
        let error = sanitize_driver_error(
            "failed command java_path /home/secret/.minecraft --jvm-args username Secret",
        );
        let lower = error.to_ascii_lowercase();

        assert_eq!(error, "driver error");
        assert!(error.len() <= MAX_DRIVER_ERROR_CHARS);
        assert!(!error.contains('/'));
        assert!(!error.contains('\\'));
        assert!(!lower.contains("command"));
        assert!(!lower.contains("java_path"));
        assert!(!lower.contains("jvm"));
        assert!(!lower.contains("username"));
        assert!(!lower.contains("args"));

        let long = "x".repeat(MAX_DRIVER_ERROR_CHARS + 32);
        assert_eq!(sanitize_driver_error(&long).len(), MAX_DRIVER_ERROR_CHARS);
    }

    async fn wait_for_retry_candidate(store: &BenchmarkSuiteDriverStore, driver_id: &str) {
        tokio::time::timeout(Duration::from_secs(2), async {
            while !store.has_retry_candidate(driver_id) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("retry candidate retained");
    }

    fn test_root(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "axial-suite-driver-{name}-{}-{nanos}",
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

    fn test_summary() -> BenchmarkSuiteDriverSuiteSummary {
        BenchmarkSuiteDriverSuiteSummary {
            run_count: 2,
            launched_run_count: 0,
            pending_run_index: Some(0),
        }
    }

    fn test_suite_id(label: &str, mode: &str) -> String {
        crate::state::benchmark_suites::derive_suite_id(label, mode)
    }

    async fn persist_complete_driver(
        store: &BenchmarkSuiteDriverStore,
        suite_id: String,
    ) -> String {
        let started = store
            .start(suite_id, "development".to_string(), 30_000, test_summary())
            .await
            .expect("driver starts");
        let driver_id = started.status.id.clone();
        store
            .record_complete(
                &driver_id,
                BenchmarkSuiteDriverSuiteSummary {
                    run_count: 2,
                    launched_run_count: 2,
                    pending_run_index: None,
                },
            )
            .await
            .expect("driver completes");
        drop(started.effect_owner);
        driver_id
    }

    fn test_entry(status: BenchmarkSuiteDriverStatus) -> BenchmarkSuiteDriverEntry {
        let (stop_tx, _stop_rx) = watch::channel(!is_non_terminal(&status.state));
        BenchmarkSuiteDriverEntry { status, stop_tx }
    }

    fn insert_test_driver(
        inner: &mut BenchmarkSuiteDriverInner,
        status: BenchmarkSuiteDriverStatus,
    ) {
        inner.drivers.insert(status.id.clone(), test_entry(status));
    }

    fn status_fixture(index: u64, state: &str, error: Option<&str>) -> BenchmarkSuiteDriverStatus {
        BenchmarkSuiteDriverStatus {
            id: format!("benchmark-suite-driver-{index:016x}"),
            suite_id: test_suite_id(&format!("fixture-{index}"), "development"),
            mode: "development".to_string(),
            state: state.to_string(),
            interval_ms: 30_000,
            run_count: 2,
            launched_run_count: 0,
            pending_run_index: (state != "complete").then_some(0),
            active_session_id: (state == "active").then(|| format!("session-{index}")),
            last_run_index: None,
            last_session_id: None,
            error: error.map(str::to_string),
            created_at: "2026-01-01T00:00:00.000Z".to_string(),
            updated_at: "2026-01-01T00:01:00.000Z".to_string(),
        }
    }

    fn write_status_fixture(dir: &Path, status: &BenchmarkSuiteDriverStatus) {
        fs::write(
            driver_path(dir, &status.id),
            serde_json::to_vec_pretty(status).expect("serialize driver status"),
        )
        .expect("write driver status");
    }

    fn cleanup(root: &Path) {
        let _ = fs::remove_dir_all(root);
    }
}
