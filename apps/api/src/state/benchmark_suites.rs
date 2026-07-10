use crate::execution::persistence::{
    AcceptedWrite, AtomicSnapshotWriter, PersistenceCoordinator, PersistenceOwnerLease,
    WriteUrgency,
};
use crate::logging::timestamp_utc;
use crate::observability::{RedactionAudience, sanitize_evidence_token};
use crate::state::ownership::{CurrentArtifact, classify_current_artifact};
use axial_config::AppPaths;
use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File};
use std::io;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as SyncMutex, RwLock};
use std::time::Duration;
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};
use tracing::warn;

const BENCHMARK_SUITE_SCHEMA: &str = "axial.launch.benchmark.suite";
const BENCHMARK_SUITE_SCHEMA_VERSION: u32 = 2;
const OPAQUE_ID_HEX_CHARS: usize = 16;
const BENCHMARK_ID_PREFIX: &str = "benchmark-";
const SUITE_ID_PREFIX: &str = "suite-";
const MAX_MANIFEST_FIELD_CHARS: usize = 96;
const MAX_MANIFEST_RUNS: usize = 64;
const MAX_MANIFEST_BYTES: u64 = 256 * 1024;
const RETRY_INITIAL_DELAY: Duration = Duration::from_millis(20);
const RETRY_MAX_DELAY: Duration = Duration::from_secs(1);
const SUITE_STORE_LOCK_INVARIANT: &str =
    "benchmark suite store lock poisoned; committed and persisted state may diverge";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BenchmarkSuiteManifest {
    pub schema: String,
    pub schema_version: u32,
    pub suite_id: String,
    pub instance_id: String,
    pub mode: String,
    pub created_at: String,
    pub updated_at: String,
    pub runs: Vec<BenchmarkSuiteManifestRun>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BenchmarkSuiteManifestRun {
    pub run_index: usize,
    pub profile: String,
    pub run_type: String,
    pub target_id: String,
    pub benchmark_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub launched_at: Option<String>,
    pub state: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BenchmarkSuiteRunInput {
    pub run_index: usize,
    pub profile: String,
    pub run_type: String,
    pub target_id: Option<String>,
    pub benchmark_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BenchmarkSuiteReservationPolicy {
    Auto,
    ExplicitRerun,
}

#[derive(Debug, Clone)]
pub struct BenchmarkSuiteSelection {
    suite_id: String,
    instance_id: String,
    mode: String,
    plan: Vec<BenchmarkSuiteRunInput>,
    generation: u64,
    run_index: usize,
    policy: BenchmarkSuiteReservationPolicy,
    previous_mapping: Option<SuiteSessionMapping>,
    prior_manifest: Option<BenchmarkSuiteManifest>,
}

impl BenchmarkSuiteSelection {
    pub const fn run_index(&self) -> usize {
        self.run_index
    }

    pub fn displaced_session_id(&self) -> Option<&str> {
        self.previous_mapping
            .as_ref()
            .map(|mapping| mapping.session_id.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BenchmarkSuiteReservation {
    pub manifest: BenchmarkSuiteManifest,
    pub run_index: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BenchmarkSuiteCompensationHandle {
    suite_id: String,
    obligation_id: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum BenchmarkSuiteStoreError {
    #[error("benchmark suite id is invalid")]
    InvalidSuiteId,
    #[error("benchmark suite identity does not match the existing manifest")]
    SuiteIdentityMismatch,
    #[error("benchmark suite auto reservation is stale or already active")]
    AutoConflict,
    #[error("benchmark suite explicit reservation changed before commit")]
    StaleSelection,
    #[error("benchmark suite explicit rerun targets an active session")]
    ExplicitActiveConflict,
    #[error("benchmark suite is complete")]
    Complete,
    #[error("benchmark suite run index is out of range")]
    InvalidRunIndex,
    #[error("benchmark suite manifest was rejected during startup")]
    RejectedManifest,
    #[error("benchmark suite mutations are disabled after an incomplete startup scan")]
    MutationLatched,
    #[error("benchmark suite session id is already assigned")]
    SessionConflict,
    #[error("benchmark suite has an exact failed write that must be reconciled")]
    RetryRequired,
    #[error("benchmark suite store is closed")]
    Closed,
    #[error("benchmark suite generation counter overflowed")]
    GenerationOverflow,
    #[error("benchmark suite obligation counter overflowed")]
    ObligationOverflow,
    #[error("benchmark suite persistence failed: {0}")]
    Persistence(#[source] io::Error),
}

impl BenchmarkSuiteStoreError {
    pub const fn class(&self) -> &'static str {
        match self {
            Self::InvalidSuiteId => "invalid_suite_id",
            Self::SuiteIdentityMismatch => "suite_identity_mismatch",
            Self::AutoConflict => "auto_conflict",
            Self::StaleSelection => "stale_selection",
            Self::ExplicitActiveConflict => "explicit_active_conflict",
            Self::Complete => "complete",
            Self::InvalidRunIndex => "invalid_run_index",
            Self::RejectedManifest => "rejected_manifest",
            Self::MutationLatched => "mutation_latched",
            Self::SessionConflict => "session_conflict",
            Self::RetryRequired => "retry_required",
            Self::Closed => "closed",
            Self::GenerationOverflow => "generation_overflow",
            Self::ObligationOverflow => "obligation_overflow",
            Self::Persistence(_) => "persistence",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum BenchmarkSuiteReserveError {
    #[error(transparent)]
    PreAccept(#[from] BenchmarkSuiteStoreError),
    #[error("benchmark suite reservation write failed after acceptance")]
    AcceptedWriteFailed {
        handle: BenchmarkSuiteCompensationHandle,
        #[source]
        source: io::Error,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BenchmarkSuiteLoadIssueKind {
    DirectoryUnreadable,
    DirectoryEntryUnreadable,
    NonRegularFile,
    ManifestOversized,
    ManifestUnreadable,
    ManifestInvalid,
    UnsupportedSchema,
    UnsafeSuiteId,
    NonCanonicalFilename,
    DuplicateSuiteId,
    TimestampInvalid,
    UnsafePublicField,
    IncoherentManifest,
    AmbiguousSessionId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BenchmarkSuiteLoadIssue {
    pub kind: BenchmarkSuiteLoadIssueKind,
    pub count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SuiteSessionMapping {
    suite_id: String,
    run_index: usize,
    session_id: String,
}

#[derive(Debug, Clone)]
struct VersionedManifest {
    generation: u64,
    manifest: BenchmarkSuiteManifest,
}

#[derive(Default)]
struct BenchmarkSuiteInner {
    suites: HashMap<String, VersionedManifest>,
    session_index: HashMap<String, SuiteSessionMapping>,
    live_reservations: HashSet<String>,
    rejected_ids: HashSet<String>,
}

#[derive(Default)]
struct BenchmarkSuiteLoadState {
    inner: BenchmarkSuiteInner,
    issues: Vec<BenchmarkSuiteLoadIssue>,
    mutation_latched: bool,
}

#[derive(Debug, Clone)]
enum ObligationState {
    Unarmed,
    Armed { revision: u64 },
}

#[derive(Debug, Clone)]
struct SuiteWriteObligation {
    obligation_id: u64,
    generation: u64,
    candidate: BenchmarkSuiteManifest,
    state: ObligationState,
    live_session_update: Option<(String, bool)>,
}

struct PendingReservationCommit {
    candidate: BenchmarkSuiteManifest,
    generation: u64,
    run_index: usize,
    reservation_session_id: String,
    previous_mapping: Option<SuiteSessionMapping>,
    compensation: BenchmarkSuiteManifest,
    compensation_generation: u64,
}

struct BenchmarkSuitePersistence {
    owner: PersistenceOwnerLease,
    storage_dir: PathBuf,
    writers: SyncMutex<HashMap<String, AtomicSnapshotWriter>>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum BenchmarkSuiteStoreLifecycle {
    #[default]
    Open,
    Closed,
}

impl BenchmarkSuitePersistence {
    fn claim(
        storage_dir: &Path,
        coordinator: PersistenceCoordinator,
    ) -> Result<Self, BenchmarkSuiteStoreError> {
        let owner = coordinator
            .claim_owner(storage_dir)
            .map_err(suite_persistence_error)?;
        Ok(Self {
            owner,
            storage_dir: storage_dir.to_path_buf(),
            writers: SyncMutex::new(HashMap::new()),
        })
    }

    fn writer(&self, suite_id: &str) -> Result<AtomicSnapshotWriter, BenchmarkSuiteStoreError> {
        let mut writers = self.writers.lock().expect(SUITE_STORE_LOCK_INVARIANT);
        if let Some(writer) = writers.get(suite_id) {
            return Ok(writer.clone());
        }
        let writer = self
            .owner
            .writer(
                suite_path_in_dir(&self.storage_dir, suite_id),
                benchmark_suite_target(suite_id),
            )
            .map_err(suite_persistence_error)?;
        writers.insert(suite_id.to_string(), writer.clone());
        Ok(writer)
    }

    async fn settle_writers(&self) -> Result<(), BenchmarkSuiteStoreError> {
        let mut writers = self
            .writers
            .lock()
            .expect(SUITE_STORE_LOCK_INVARIANT)
            .iter()
            .map(|(suite_id, writer)| (suite_id.clone(), writer.clone()))
            .collect::<Vec<_>>();
        writers.sort_by(|left, right| left.0.cmp(&right.0));
        for (_, writer) in writers {
            writer.settle().await.map_err(suite_persistence_error)?;
        }
        Ok(())
    }

    #[cfg(test)]
    fn writer_count(&self) -> usize {
        self.writers.lock().expect(SUITE_STORE_LOCK_INVARIANT).len()
    }
}

pub struct BenchmarkSuiteStore {
    inner: Arc<RwLock<BenchmarkSuiteInner>>,
    mutation_gate: Arc<AsyncMutex<()>>,
    persistence: Option<Arc<BenchmarkSuitePersistence>>,
    obligations: Arc<SyncMutex<HashMap<String, SuiteWriteObligation>>>,
    next_obligation_id: SyncMutex<u64>,
    load_issues: Vec<BenchmarkSuiteLoadIssue>,
    mutation_latched: bool,
    lifecycle: Arc<SyncMutex<BenchmarkSuiteStoreLifecycle>>,
}

impl BenchmarkSuiteStore {
    #[cfg(test)]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(BenchmarkSuiteInner::default())),
            mutation_gate: Arc::new(AsyncMutex::new(())),
            persistence: None,
            obligations: Arc::new(SyncMutex::new(HashMap::new())),
            next_obligation_id: SyncMutex::new(0),
            load_issues: Vec::new(),
            mutation_latched: false,
            lifecycle: Arc::new(SyncMutex::new(BenchmarkSuiteStoreLifecycle::Open)),
        }
    }

    pub fn load_from_paths(paths: &AppPaths) -> Self {
        Self::try_load_from_paths_with_coordinator(paths, PersistenceCoordinator::global())
            .unwrap_or_else(|error| {
                panic!("failed to initialize benchmark suite persistence: {error}")
            })
    }

    pub(crate) fn try_load_from_paths_with_coordinator(
        paths: &AppPaths,
        coordinator: PersistenceCoordinator,
    ) -> Result<Self, BenchmarkSuiteStoreError> {
        let storage_dir = suite_dir(paths);
        let load_state = load_persisted_suites(&storage_dir);
        Ok(Self {
            inner: Arc::new(RwLock::new(load_state.inner)),
            mutation_gate: Arc::new(AsyncMutex::new(())),
            persistence: Some(Arc::new(BenchmarkSuitePersistence::claim(
                &storage_dir,
                coordinator,
            )?)),
            obligations: Arc::new(SyncMutex::new(HashMap::new())),
            next_obligation_id: SyncMutex::new(0),
            load_issues: load_state.issues,
            mutation_latched: load_state.mutation_latched,
            lifecycle: Arc::new(SyncMutex::new(BenchmarkSuiteStoreLifecycle::Open)),
        })
    }

    pub fn get(
        &self,
        suite_id: &str,
    ) -> Result<Option<BenchmarkSuiteManifest>, BenchmarkSuiteStoreError> {
        let suite_id = require_canonical_suite_id(suite_id)?;
        let inner = self.inner.read().expect(SUITE_STORE_LOCK_INVARIANT);
        if inner.rejected_ids.contains(&suite_id) {
            return Err(BenchmarkSuiteStoreError::RejectedManifest);
        }
        Ok(inner
            .suites
            .get(&suite_id)
            .map(|entry| entry.manifest.clone()))
    }

    #[cfg(test)]
    pub fn load_issues(&self) -> Vec<BenchmarkSuiteLoadIssue> {
        self.load_issues.clone()
    }

    pub fn load_issue_count(&self) -> usize {
        self.load_issues.iter().map(|issue| issue.count).sum()
    }

    pub async fn select_reservation(
        &self,
        suite_id: &str,
        instance_id: &str,
        mode: &str,
        plan: &[BenchmarkSuiteRunInput],
        requested_run_index: Option<usize>,
    ) -> Result<BenchmarkSuiteSelection, BenchmarkSuiteStoreError> {
        self.ensure_open()?;
        let suite_id = require_canonical_suite_id(suite_id)?;
        let instance_id = require_safe_manifest_field(instance_id)?;
        let mode = require_suite_mode(mode)?;
        let plan = normalize_plan(plan)?;
        let mutation = self.mutation_gate.clone().lock_owned().await;
        self.ensure_open()?;
        let mutation = self.reconcile_suite_once(&suite_id, mutation).await?;
        self.ensure_mutation_allowed(&suite_id)?;

        let inner = self.inner.read().expect(SUITE_STORE_LOCK_INVARIANT);
        let current = inner.suites.get(&suite_id);
        if let Some(current) = current
            && (current.manifest.instance_id != instance_id || current.manifest.mode != mode)
        {
            return Err(BenchmarkSuiteStoreError::SuiteIdentityMismatch);
        }
        let policy = if requested_run_index.is_some() {
            BenchmarkSuiteReservationPolicy::ExplicitRerun
        } else {
            BenchmarkSuiteReservationPolicy::Auto
        };
        if policy == BenchmarkSuiteReservationPolicy::Auto
            && current.is_some_and(|entry| {
                entry
                    .manifest
                    .runs
                    .iter()
                    .filter_map(|run| run.session_id.as_ref())
                    .any(|session_id| inner.live_reservations.contains(session_id))
            })
        {
            return Err(BenchmarkSuiteStoreError::AutoConflict);
        }
        let run_index = match requested_run_index {
            Some(index) if index < plan.len() => index,
            Some(_) => return Err(BenchmarkSuiteStoreError::InvalidRunIndex),
            None => next_pending_run_index(current.map(|entry| &entry.manifest), plan.len())
                .ok_or(BenchmarkSuiteStoreError::Complete)?,
        };
        let previous_mapping = current
            .and_then(|entry| {
                entry
                    .manifest
                    .runs
                    .iter()
                    .find(|run| run.run_index == run_index)
            })
            .and_then(run_mapping)
            .map(|mapping| SuiteSessionMapping {
                suite_id: suite_id.clone(),
                ..mapping
            });
        let selection = BenchmarkSuiteSelection {
            suite_id,
            instance_id,
            mode,
            plan,
            generation: current.map(|entry| entry.generation).unwrap_or(0),
            run_index,
            policy,
            previous_mapping,
            prior_manifest: current.map(|entry| entry.manifest.clone()),
        };
        drop(inner);
        drop(mutation);
        Ok(selection)
    }

    pub async fn reserve(
        &self,
        selection: BenchmarkSuiteSelection,
        session_id: &str,
        launched_at: &str,
        displaced_session_active: bool,
    ) -> Result<BenchmarkSuiteReservation, BenchmarkSuiteReserveError> {
        self.ensure_open()?;
        let session_id = require_safe_manifest_field(session_id)?;
        let launched_at = normalize_timestamp_value(launched_at)
            .ok_or(BenchmarkSuiteStoreError::InvalidSuiteId)?;
        let mutation = self.mutation_gate.clone().lock_owned().await;
        self.ensure_open()?;
        let mutation = self
            .reconcile_suite_once(&selection.suite_id, mutation)
            .await?;
        self.ensure_mutation_allowed(&selection.suite_id)?;

        let (
            generation,
            current_manifest,
            current_mapping,
            session_conflict,
            live_conflict,
            selected_mapping_live,
        ) = {
            let inner = self.inner.read().expect(SUITE_STORE_LOCK_INVARIANT);
            let current = inner.suites.get(&selection.suite_id);
            (
                current.map(|entry| entry.generation).unwrap_or(0),
                current.map(|entry| entry.manifest.clone()),
                current
                    .and_then(|entry| {
                        entry
                            .manifest
                            .runs
                            .iter()
                            .find(|run| run.run_index == selection.run_index)
                    })
                    .and_then(run_mapping)
                    .map(|mapping| SuiteSessionMapping {
                        suite_id: selection.suite_id.clone(),
                        ..mapping
                    }),
                inner.session_index.get(&session_id).cloned(),
                current.is_some_and(|entry| {
                    entry
                        .manifest
                        .runs
                        .iter()
                        .filter_map(|run| run.session_id.as_ref())
                        .any(|session_id| inner.live_reservations.contains(session_id))
                }),
                selection
                    .previous_mapping
                    .as_ref()
                    .is_some_and(|mapping| inner.live_reservations.contains(&mapping.session_id)),
            )
        };
        if generation != selection.generation {
            return Err(match selection.policy {
                BenchmarkSuiteReservationPolicy::Auto => BenchmarkSuiteStoreError::AutoConflict,
                BenchmarkSuiteReservationPolicy::ExplicitRerun => {
                    BenchmarkSuiteStoreError::StaleSelection
                }
            }
            .into());
        }
        if current_mapping != selection.previous_mapping {
            return Err(match selection.policy {
                BenchmarkSuiteReservationPolicy::Auto => BenchmarkSuiteStoreError::AutoConflict,
                BenchmarkSuiteReservationPolicy::ExplicitRerun => {
                    BenchmarkSuiteStoreError::StaleSelection
                }
            }
            .into());
        }
        if selection.policy == BenchmarkSuiteReservationPolicy::Auto && live_conflict {
            return Err(BenchmarkSuiteStoreError::AutoConflict.into());
        }
        if displaced_session_active {
            return Err(match selection.policy {
                BenchmarkSuiteReservationPolicy::Auto => BenchmarkSuiteStoreError::AutoConflict,
                BenchmarkSuiteReservationPolicy::ExplicitRerun => {
                    BenchmarkSuiteStoreError::ExplicitActiveConflict
                }
            }
            .into());
        }
        if selection.policy == BenchmarkSuiteReservationPolicy::ExplicitRerun
            && selected_mapping_live
        {
            return Err(BenchmarkSuiteStoreError::ExplicitActiveConflict.into());
        }
        if session_conflict.is_some() {
            return Err(BenchmarkSuiteStoreError::SessionConflict.into());
        }
        if let Some(manifest) = &current_manifest
            && (manifest.instance_id != selection.instance_id || manifest.mode != selection.mode)
        {
            return Err(BenchmarkSuiteStoreError::SuiteIdentityMismatch.into());
        }

        let now = timestamp_utc();
        let mut base = current_manifest.unwrap_or_else(|| new_manifest(&selection, &now));
        for run in &selection.plan {
            upsert_plan_run(&mut base.runs, run);
        }
        let pending_compensation = selection
            .prior_manifest
            .clone()
            .unwrap_or_else(|| pending_manifest(&selection, &base.created_at));
        let compensation_generation = if selection.prior_manifest.is_some() {
            selection.generation
        } else {
            1
        };
        let reservation_session_id = session_id.clone();
        upsert_launched_run(
            &mut base.runs,
            &selection.plan[selection.run_index],
            Some(session_id),
            Some(launched_at.clone()),
        );
        base.updated_at = latest_timestamp_value(&[&base.updated_at, &now, &launched_at]);
        base.runs.sort_by_key(|run| run.run_index);
        base.runs.truncate(MAX_MANIFEST_RUNS);
        let candidate_generation = next_generation(selection.generation)?;
        self.commit_reservation(
            PendingReservationCommit {
                candidate: base,
                generation: candidate_generation,
                run_index: selection.run_index,
                reservation_session_id,
                previous_mapping: selection.previous_mapping,
                compensation: pending_compensation,
                compensation_generation,
            },
            mutation,
        )
        .await
    }

    pub async fn settle_compensation(
        &self,
        handle: &BenchmarkSuiteCompensationHandle,
    ) -> Result<(), BenchmarkSuiteStoreError> {
        let mut delay = RETRY_INITIAL_DELAY;
        loop {
            let mutation = self.mutation_gate.clone().lock_owned().await;
            let obligation_matches = self
                .obligations
                .lock()
                .expect(SUITE_STORE_LOCK_INVARIANT)
                .get(&handle.suite_id)
                .is_some_and(|obligation| obligation.obligation_id == handle.obligation_id);
            if !obligation_matches {
                drop(mutation);
                return Ok(());
            }
            match self.reconcile_suite_once(&handle.suite_id, mutation).await {
                Ok(mutation) => {
                    drop(mutation);
                    return Ok(());
                }
                Err(error) => {
                    warn!(
                        error_class = error.class(),
                        "benchmark suite compensation retry failed"
                    );
                    tokio::time::sleep(delay).await;
                    delay = delay.saturating_mul(2).min(RETRY_MAX_DELAY);
                }
            }
        }
    }

    pub async fn update_run_state_for_session(
        &self,
        launch_session_id: &str,
        outcome: &str,
    ) -> Result<bool, BenchmarkSuiteStoreError> {
        self.ensure_open()?;
        let Some(session_id) = safe_manifest_field(launch_session_id) else {
            return Ok(false);
        };
        let mapping = self
            .inner
            .read()
            .expect(SUITE_STORE_LOCK_INVARIANT)
            .session_index
            .get(&session_id)
            .cloned();
        let Some(mapping) = mapping else {
            return Ok(false);
        };
        let mutation = self.mutation_gate.clone().lock_owned().await;
        self.ensure_open()?;
        let mutation = self
            .reconcile_suite_once(&mapping.suite_id, mutation)
            .await?;
        self.ensure_mutation_allowed(&mapping.suite_id)?;
        let (generation, mut candidate) = {
            let inner = self.inner.read().expect(SUITE_STORE_LOCK_INVARIANT);
            let Some(entry) = inner.suites.get(&mapping.suite_id) else {
                return Ok(false);
            };
            let current_mapping = entry
                .manifest
                .runs
                .iter()
                .find(|run| run.run_index == mapping.run_index)
                .and_then(run_mapping);
            if current_mapping
                .as_ref()
                .map(|current| current.session_id.as_str())
                != Some(session_id.as_str())
            {
                return Ok(false);
            }
            (entry.generation, entry.manifest.clone())
        };
        let state = normalize_outcome_state(outcome);
        let Some(run) = candidate
            .runs
            .iter_mut()
            .find(|run| run.run_index == mapping.run_index)
        else {
            return Ok(false);
        };
        if run.state == state {
            drop(mutation);
            return Ok(true);
        }
        run.state = state.clone();
        let now = timestamp_utc();
        candidate.updated_at = latest_timestamp_value(&[&candidate.updated_at, &now]);
        self.commit_manifest(
            candidate,
            next_generation(generation)?,
            Some((session_id, state == "running")),
            mutation,
        )
        .await?;
        Ok(true)
    }

    pub async fn flush(&self) -> Result<(), BenchmarkSuiteStoreError> {
        let mut mutation = self.mutation_gate.clone().lock_owned().await;
        if self.is_closed() {
            return Ok(());
        }
        let mut suite_ids = self
            .obligations
            .lock()
            .expect(SUITE_STORE_LOCK_INVARIANT)
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        suite_ids.sort();
        for suite_id in suite_ids {
            mutation = self.reconcile_suite_once(&suite_id, mutation).await?;
        }
        if let Some(persistence) = &self.persistence {
            persistence.settle_writers().await?;
            persistence
                .owner
                .flush()
                .await
                .map_err(suite_persistence_error)?;
        }
        drop(mutation);
        Ok(())
    }

    pub async fn close(&self) -> Result<(), BenchmarkSuiteStoreError> {
        let mut mutation = self.mutation_gate.clone().lock_owned().await;
        if self.is_closed() {
            return Ok(());
        }
        let mut suite_ids = self
            .obligations
            .lock()
            .expect(SUITE_STORE_LOCK_INVARIANT)
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        suite_ids.sort();
        for suite_id in suite_ids {
            mutation = self.reconcile_suite_once(&suite_id, mutation).await?;
        }
        if let Some(persistence) = &self.persistence {
            persistence.settle_writers().await?;
            persistence
                .owner
                .close()
                .await
                .map_err(suite_persistence_error)?;
        }
        *self.lifecycle.lock().expect(SUITE_STORE_LOCK_INVARIANT) =
            BenchmarkSuiteStoreLifecycle::Closed;
        drop(mutation);
        Ok(())
    }

    fn ensure_open(&self) -> Result<(), BenchmarkSuiteStoreError> {
        if self.is_closed() {
            Err(BenchmarkSuiteStoreError::Closed)
        } else {
            Ok(())
        }
    }

    fn is_closed(&self) -> bool {
        *self.lifecycle.lock().expect(SUITE_STORE_LOCK_INVARIANT)
            == BenchmarkSuiteStoreLifecycle::Closed
    }

    fn allocate_obligation_id(&self) -> Result<u64, BenchmarkSuiteStoreError> {
        let mut next_id = self
            .next_obligation_id
            .lock()
            .expect(SUITE_STORE_LOCK_INVARIANT);
        let obligation_id = next_id
            .checked_add(1)
            .ok_or(BenchmarkSuiteStoreError::ObligationOverflow)?;
        *next_id = obligation_id;
        Ok(obligation_id)
    }

    fn ensure_mutation_allowed(&self, suite_id: &str) -> Result<(), BenchmarkSuiteStoreError> {
        if self.mutation_latched {
            return Err(BenchmarkSuiteStoreError::MutationLatched);
        }
        if self
            .inner
            .read()
            .expect(SUITE_STORE_LOCK_INVARIANT)
            .rejected_ids
            .contains(suite_id)
        {
            return Err(BenchmarkSuiteStoreError::RejectedManifest);
        }
        if self
            .obligations
            .lock()
            .expect(SUITE_STORE_LOCK_INVARIANT)
            .contains_key(suite_id)
        {
            return Err(BenchmarkSuiteStoreError::RetryRequired);
        }
        Ok(())
    }

    async fn commit_reservation(
        &self,
        commit: PendingReservationCommit,
        mutation: OwnedMutexGuard<()>,
    ) -> Result<BenchmarkSuiteReservation, BenchmarkSuiteReserveError> {
        let PendingReservationCommit {
            candidate,
            generation,
            run_index,
            reservation_session_id,
            previous_mapping,
            compensation,
            compensation_generation,
        } = commit;
        let suite_id = candidate.suite_id.clone();
        let Some(persistence) = &self.persistence else {
            publish_manifest(
                &mut self.inner.write().expect(SUITE_STORE_LOCK_INVARIANT),
                candidate.clone(),
                generation,
            );
            let mut inner = self.inner.write().expect(SUITE_STORE_LOCK_INVARIANT);
            if let Some(previous) = &previous_mapping {
                inner.live_reservations.remove(&previous.session_id);
            }
            inner
                .live_reservations
                .insert(reservation_session_id.clone());
            drop(mutation);
            return Ok(BenchmarkSuiteReservation {
                manifest: candidate,
                run_index,
            });
        };
        let writer = persistence.writer(&suite_id)?;
        let obligation_id = self.allocate_obligation_id()?;
        let ticket = writer
            .accept(candidate.clone(), WriteUrgency::Immediate, encode_manifest)
            .map_err(suite_persistence_error)?;
        let revision = ticket.revision().get();
        let inner = self.inner.clone();
        let obligations = self.obligations.clone();
        let previous_obligation = obligations
            .lock()
            .expect(SUITE_STORE_LOCK_INVARIANT)
            .insert(
                suite_id.clone(),
                SuiteWriteObligation {
                    obligation_id,
                    generation,
                    candidate: candidate.clone(),
                    state: ObligationState::Armed { revision },
                    live_session_update: Some((reservation_session_id.clone(), true)),
                },
            );
        debug_assert!(previous_obligation.is_none());
        let (completed_tx, completed_rx) = tokio::sync::oneshot::channel();
        ticket.observe_async(move |result| async move {
            let result = match result {
                Ok(_) => {
                    let mut retained = obligations.lock().expect(SUITE_STORE_LOCK_INVARIANT);
                    let owns_obligation = retained
                        .get(&suite_id)
                        .is_some_and(|obligation| obligation.obligation_id == obligation_id);
                    if owns_obligation {
                        let mut inner = inner.write().expect(SUITE_STORE_LOCK_INVARIANT);
                        publish_manifest(&mut inner, candidate.clone(), generation);
                        if let Some(previous) = &previous_mapping {
                            inner.live_reservations.remove(&previous.session_id);
                        }
                        inner
                            .live_reservations
                            .insert(reservation_session_id.clone());
                        drop(inner);
                        retained.remove(&suite_id);
                        Ok(BenchmarkSuiteReservation {
                            manifest: candidate,
                            run_index,
                        })
                    } else {
                        Err(BenchmarkSuiteReserveError::AcceptedWriteFailed {
                            handle: BenchmarkSuiteCompensationHandle {
                                suite_id,
                                obligation_id,
                            },
                            source: io::Error::other(
                                "benchmark suite reservation obligation changed before observation",
                            ),
                        })
                    }
                }
                Err(error) => {
                    let owns_obligation = {
                        let mut retained = obligations.lock().expect(SUITE_STORE_LOCK_INVARIANT);
                        retained
                            .get_mut(&suite_id)
                            .filter(|obligation| obligation.obligation_id == obligation_id)
                            .is_some_and(|obligation| {
                                obligation.generation = compensation_generation;
                                obligation.candidate = compensation.clone();
                                obligation.state = ObligationState::Unarmed;
                                obligation.live_session_update = None;
                                true
                            })
                    };
                    if owns_obligation
                        && let Ok(compensation_ticket) = writer.accept(
                            compensation.clone(),
                            WriteUrgency::Immediate,
                            encode_manifest,
                        )
                    {
                        let compensation_revision = compensation_ticket.revision().get();
                        let mut retained = obligations.lock().expect(SUITE_STORE_LOCK_INVARIANT);
                        if let Some(obligation) = retained
                            .get_mut(&suite_id)
                            .filter(|obligation| obligation.obligation_id == obligation_id)
                        {
                            obligation.state = ObligationState::Armed {
                                revision: compensation_revision,
                            };
                        }
                        drop(retained);
                        let compensation_inner = inner.clone();
                        let compensation_obligations = obligations.clone();
                        let compensation_suite_id = suite_id.clone();
                        compensation_ticket.observe(move |result| {
                            if result.is_ok() {
                                let mut retained = compensation_obligations
                                    .lock()
                                    .expect(SUITE_STORE_LOCK_INVARIANT);
                                if retained
                                    .get(&compensation_suite_id)
                                    .is_some_and(|obligation| {
                                        obligation.obligation_id == obligation_id
                                    })
                                {
                                    publish_manifest(
                                        &mut compensation_inner
                                            .write()
                                            .expect(SUITE_STORE_LOCK_INVARIANT),
                                        compensation,
                                        compensation_generation,
                                    );
                                    retained.remove(&compensation_suite_id);
                                }
                            }
                        });
                    }
                    Err(BenchmarkSuiteReserveError::AcceptedWriteFailed {
                        handle: BenchmarkSuiteCompensationHandle {
                            suite_id,
                            obligation_id,
                        },
                        source: io::Error::from(error),
                    })
                }
            };
            let _ = completed_tx.send((result, mutation));
        });
        let (result, mutation) = completed_rx.await.map_err(|_| {
            BenchmarkSuiteReserveError::PreAccept(BenchmarkSuiteStoreError::Persistence(
                io::Error::other("benchmark suite reservation observer stopped"),
            ))
        })?;
        drop(mutation);
        result
    }

    async fn commit_manifest(
        &self,
        candidate: BenchmarkSuiteManifest,
        generation: u64,
        live_session_update: Option<(String, bool)>,
        mutation: OwnedMutexGuard<()>,
    ) -> Result<(), BenchmarkSuiteStoreError> {
        let Some(persistence) = &self.persistence else {
            publish_manifest(
                &mut self.inner.write().expect(SUITE_STORE_LOCK_INVARIANT),
                candidate,
                generation,
            );
            if let Some((session_id, live)) = live_session_update {
                let mut inner = self.inner.write().expect(SUITE_STORE_LOCK_INVARIANT);
                if live {
                    inner.live_reservations.insert(session_id);
                } else {
                    inner.live_reservations.remove(&session_id);
                }
            }
            drop(mutation);
            return Ok(());
        };
        let suite_id = candidate.suite_id.clone();
        let obligation_id = self.allocate_obligation_id()?;
        let ticket = persistence
            .writer(&suite_id)?
            .accept(candidate.clone(), WriteUrgency::Immediate, encode_manifest)
            .map_err(suite_persistence_error)?;
        let revision = ticket.revision().get();
        let previous_obligation = self
            .obligations
            .lock()
            .expect(SUITE_STORE_LOCK_INVARIANT)
            .insert(
                suite_id,
                SuiteWriteObligation {
                    obligation_id,
                    generation,
                    candidate: candidate.clone(),
                    state: ObligationState::Armed { revision },
                    live_session_update: live_session_update.clone(),
                },
            );
        debug_assert!(previous_obligation.is_none());
        self.await_manifest_commit(
            ticket,
            obligation_id,
            candidate,
            generation,
            live_session_update,
            mutation,
        )
        .await
    }

    async fn await_manifest_commit(
        &self,
        ticket: AcceptedWrite,
        obligation_id: u64,
        candidate: BenchmarkSuiteManifest,
        generation: u64,
        live_session_update: Option<(String, bool)>,
        mutation: OwnedMutexGuard<()>,
    ) -> Result<(), BenchmarkSuiteStoreError> {
        let inner = self.inner.clone();
        let obligations = self.obligations.clone();
        let suite_id = candidate.suite_id.clone();
        let (completed_tx, completed_rx) = tokio::sync::oneshot::channel();
        ticket.observe(move |result| {
            let result = match result {
                Ok(_) => {
                    let mut retained = obligations.lock().expect(SUITE_STORE_LOCK_INVARIANT);
                    if retained
                        .get(&suite_id)
                        .is_some_and(|obligation| obligation.obligation_id == obligation_id)
                    {
                        let mut inner = inner.write().expect(SUITE_STORE_LOCK_INVARIANT);
                        publish_manifest(&mut inner, candidate, generation);
                        if let Some((session_id, live)) = live_session_update {
                            if live {
                                inner.live_reservations.insert(session_id);
                            } else {
                                inner.live_reservations.remove(&session_id);
                            }
                        }
                        drop(inner);
                        retained.remove(&suite_id);
                    }
                    Ok(())
                }
                Err(error) => Err(suite_persistence_error(error)),
            };
            let _ = completed_tx.send((result, mutation));
        });
        let (result, mutation) = completed_rx.await.map_err(|_| {
            BenchmarkSuiteStoreError::Persistence(io::Error::other(
                "benchmark suite commit observer stopped",
            ))
        })?;
        drop(mutation);
        result
    }

    async fn reconcile_suite_once(
        &self,
        suite_id: &str,
        mutation: OwnedMutexGuard<()>,
    ) -> Result<OwnedMutexGuard<()>, BenchmarkSuiteStoreError> {
        let obligation = self
            .obligations
            .lock()
            .expect(SUITE_STORE_LOCK_INVARIANT)
            .get(suite_id)
            .cloned();
        let Some(obligation) = obligation else {
            return Ok(mutation);
        };
        let obligation_id = obligation.obligation_id;
        let Some(persistence) = &self.persistence else {
            let mut retained = self.obligations.lock().expect(SUITE_STORE_LOCK_INVARIANT);
            if retained
                .get(suite_id)
                .is_some_and(|current| current.obligation_id == obligation_id)
            {
                let mut inner = self.inner.write().expect(SUITE_STORE_LOCK_INVARIANT);
                publish_manifest(&mut inner, obligation.candidate, obligation.generation);
                if let Some((session_id, live)) = obligation.live_session_update {
                    if live {
                        inner.live_reservations.insert(session_id);
                    } else {
                        inner.live_reservations.remove(&session_id);
                    }
                }
                drop(inner);
                retained.remove(suite_id);
            }
            return Ok(mutation);
        };
        let writer = persistence.writer(suite_id)?;
        let ticket_result = match obligation.state {
            ObligationState::Unarmed => writer.accept(
                obligation.candidate.clone(),
                WriteUrgency::Immediate,
                encode_manifest,
            ),
            ObligationState::Armed { revision } => writer.retry().inspect(|ticket| {
                assert_eq!(
                    ticket.revision().get(),
                    revision,
                    "benchmark suite retry revision diverged from its exact obligation"
                );
            }),
        };
        let ticket = match ticket_result {
            Ok(ticket) => ticket,
            Err(error)
                if self
                    .obligations
                    .lock()
                    .expect(SUITE_STORE_LOCK_INVARIANT)
                    .get(suite_id)
                    .is_none_or(|current| current.obligation_id != obligation_id) =>
            {
                return Ok(mutation);
            }
            Err(error) => return Err(suite_persistence_error(error)),
        };
        let revision = ticket.revision().get();
        if let Some(retained) = self
            .obligations
            .lock()
            .expect(SUITE_STORE_LOCK_INVARIANT)
            .get_mut(suite_id)
            .filter(|current| current.obligation_id == obligation_id)
        {
            retained.state = ObligationState::Armed { revision };
        }
        let inner = self.inner.clone();
        let obligations = self.obligations.clone();
        let suite_id = suite_id.to_string();
        let (completed_tx, completed_rx) = tokio::sync::oneshot::channel();
        ticket.observe(move |result| {
            let result = match result {
                Ok(_) => {
                    let mut retained = obligations.lock().expect(SUITE_STORE_LOCK_INVARIANT);
                    if retained
                        .get(&suite_id)
                        .is_some_and(|current| current.obligation_id == obligation_id)
                    {
                        let mut inner = inner.write().expect(SUITE_STORE_LOCK_INVARIANT);
                        publish_manifest(&mut inner, obligation.candidate, obligation.generation);
                        if let Some((session_id, live)) = obligation.live_session_update {
                            if live {
                                inner.live_reservations.insert(session_id);
                            } else {
                                inner.live_reservations.remove(&session_id);
                            }
                        }
                        drop(inner);
                        retained.remove(&suite_id);
                    }
                    Ok(())
                }
                Err(error) => Err(suite_persistence_error(error)),
            };
            let _ = completed_tx.send((result, mutation));
        });
        let (result, mutation) = completed_rx.await.map_err(|_| {
            BenchmarkSuiteStoreError::Persistence(io::Error::other(
                "benchmark suite retry observer stopped",
            ))
        })?;
        result?;
        Ok(mutation)
    }
}

#[cfg(test)]
impl Default for BenchmarkSuiteStore {
    fn default() -> Self {
        Self::new()
    }
}

pub fn derive_suite_id(instance_id: &str, mode: &str) -> String {
    let mode_token = match mode.trim() {
        "development" => "dev",
        "qualification" => "qual",
        "release_validation" => "release",
        _ => "custom",
    };
    format!(
        "suite-{mode_token}-{:016x}",
        stable_hash(&[instance_id.trim(), mode.trim()])
    )
}

pub fn normalize_suite_id(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed == value && is_canonical_suite_id(trimmed) {
        return Some(trimmed.to_string());
    }
    Some(format!("suite-custom-{:016x}", stable_hash(&[trimmed])))
}

#[cfg(test)]
pub fn suite_path(paths: &AppPaths, suite_id: &str) -> PathBuf {
    suite_path_in_dir(&suite_dir(paths), suite_id)
}

pub fn next_pending_run_index(
    manifest: Option<&BenchmarkSuiteManifest>,
    planned_run_count: usize,
) -> Option<usize> {
    let Some(manifest) = manifest else {
        return (planned_run_count > 0).then_some(0);
    };
    (0..planned_run_count).find(|run_index| {
        manifest
            .runs
            .iter()
            .find(|run| run.run_index == *run_index)
            .and_then(|run| run.session_id.as_ref())
            .is_none()
    })
}

fn load_persisted_suites(storage_dir: &Path) -> BenchmarkSuiteLoadState {
    let mut load_state = BenchmarkSuiteLoadState::default();
    match fs::symlink_metadata(storage_dir) {
        Ok(metadata) if metadata.file_type().is_dir() => {}
        Ok(_) => {
            record_load_issue(
                &mut load_state.issues,
                BenchmarkSuiteLoadIssueKind::DirectoryUnreadable,
            );
            load_state.mutation_latched = true;
            return load_state;
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => return load_state,
        Err(error) => {
            warn!(error_kind = ?error.kind(), "failed to inspect benchmark suite directory");
            record_load_issue(
                &mut load_state.issues,
                BenchmarkSuiteLoadIssueKind::DirectoryUnreadable,
            );
            load_state.mutation_latched = true;
            return load_state;
        }
    }
    let entries = match fs::read_dir(storage_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return load_state,
        Err(error) => {
            warn!(error_kind = ?error.kind(), "failed to scan benchmark suite directory");
            record_load_issue(
                &mut load_state.issues,
                BenchmarkSuiteLoadIssueKind::DirectoryUnreadable,
            );
            load_state.mutation_latched = true;
            return load_state;
        }
    };
    let mut paths = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                warn!(error_kind = ?error.kind(), "failed to inspect benchmark suite entry");
                record_load_issue(
                    &mut load_state.issues,
                    BenchmarkSuiteLoadIssueKind::DirectoryEntryUnreadable,
                );
                load_state.mutation_latched = true;
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

    let mut candidates =
        BTreeMap::<String, Vec<(PathBuf, Option<String>, BenchmarkSuiteManifest)>>::new();
    for path in paths {
        let identifiable_id = suite_id_from_filename(&path);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) => {
                warn!(error_kind = ?error.kind(), "failed to inspect benchmark suite file");
                reject_identifiable(
                    &mut load_state,
                    identifiable_id,
                    BenchmarkSuiteLoadIssueKind::ManifestUnreadable,
                );
                continue;
            }
        };
        if !metadata.file_type().is_file() {
            reject_identifiable(
                &mut load_state,
                identifiable_id,
                BenchmarkSuiteLoadIssueKind::NonRegularFile,
            );
            continue;
        }
        if metadata.len() > MAX_MANIFEST_BYTES {
            reject_identifiable(
                &mut load_state,
                identifiable_id,
                BenchmarkSuiteLoadIssueKind::ManifestOversized,
            );
            continue;
        }
        let data = match read_bounded_manifest(&path) {
            Ok(data) => data,
            Err(error) if error.kind() == io::ErrorKind::InvalidData => {
                reject_identifiable(
                    &mut load_state,
                    identifiable_id,
                    BenchmarkSuiteLoadIssueKind::ManifestOversized,
                );
                continue;
            }
            Err(error) => {
                warn!(error_kind = ?error.kind(), "failed to read benchmark suite file");
                reject_identifiable(
                    &mut load_state,
                    identifiable_id,
                    BenchmarkSuiteLoadIssueKind::ManifestUnreadable,
                );
                continue;
            }
        };
        let mut manifest: BenchmarkSuiteManifest = match serde_json::from_slice(&data) {
            Ok(manifest) => manifest,
            Err(_) => {
                reject_identifiable(
                    &mut load_state,
                    identifiable_id,
                    BenchmarkSuiteLoadIssueKind::ManifestInvalid,
                );
                continue;
            }
        };
        let manifest_id = manifest.suite_id.clone();
        if let Err(kind) = normalize_and_validate_loaded_manifest(&mut manifest) {
            reject_parsed_manifest(
                &mut load_state,
                identifiable_id.as_deref(),
                require_canonical_suite_id(&manifest_id).ok().as_deref(),
                kind,
            );
            continue;
        }
        if path.file_name().and_then(|value| value.to_str())
            != Some(format!("{}.json", manifest.suite_id).as_str())
        {
            record_load_issue(
                &mut load_state.issues,
                BenchmarkSuiteLoadIssueKind::NonCanonicalFilename,
            );
        }
        candidates
            .entry(manifest.suite_id.clone())
            .or_default()
            .push((path, identifiable_id, manifest));
    }

    let mut accepted = Vec::new();
    for (suite_id, mut records) in candidates {
        records.sort_by(|left, right| left.0.cmp(&right.0));
        if records.len() != 1 {
            reserve_rejected_id(&mut load_state, Some(&suite_id));
            for (_, filename_id, _) in &records {
                reserve_rejected_id(&mut load_state, filename_id.as_deref());
            }
            for _ in 1..records.len() {
                record_load_issue(
                    &mut load_state.issues,
                    BenchmarkSuiteLoadIssueKind::DuplicateSuiteId,
                );
            }
            continue;
        }
        let (path, filename_id, manifest) = records.pop().expect("suite candidate exists");
        if path.file_name().and_then(|value| value.to_str())
            != Some(format!("{}.json", manifest.suite_id).as_str())
        {
            reserve_rejected_id(&mut load_state, Some(&manifest.suite_id));
            reserve_rejected_id(&mut load_state, filename_id.as_deref());
            continue;
        }
        accepted.push(manifest);
    }

    let mut session_suites = HashMap::<String, HashSet<String>>::new();
    for manifest in &accepted {
        for session_id in manifest
            .runs
            .iter()
            .filter_map(|run| run.session_id.as_ref())
        {
            session_suites
                .entry(session_id.clone())
                .or_default()
                .insert(manifest.suite_id.clone());
        }
    }
    let ambiguous_suites = session_suites
        .values()
        .filter(|suite_ids| suite_ids.len() > 1)
        .flat_map(|suite_ids| suite_ids.iter().cloned())
        .collect::<HashSet<_>>();
    for manifest in accepted {
        if ambiguous_suites.contains(&manifest.suite_id) {
            load_state
                .inner
                .rejected_ids
                .insert(manifest.suite_id.clone());
            record_load_issue(
                &mut load_state.issues,
                BenchmarkSuiteLoadIssueKind::AmbiguousSessionId,
            );
            continue;
        }
        let generation = 1;
        publish_manifest(&mut load_state.inner, manifest, generation);
    }
    load_state.inner.live_reservations.clear();
    load_state
}

fn normalize_and_validate_loaded_manifest(
    manifest: &mut BenchmarkSuiteManifest,
) -> Result<(), BenchmarkSuiteLoadIssueKind> {
    if manifest.schema != BENCHMARK_SUITE_SCHEMA
        || manifest.schema_version != BENCHMARK_SUITE_SCHEMA_VERSION
    {
        return Err(BenchmarkSuiteLoadIssueKind::UnsupportedSchema);
    }
    if require_canonical_suite_id(&manifest.suite_id).is_err() {
        return Err(BenchmarkSuiteLoadIssueKind::UnsafeSuiteId);
    }
    if !is_safe_public_manifest_field(&manifest.instance_id)
        || require_suite_mode(&manifest.mode).is_err()
    {
        return Err(BenchmarkSuiteLoadIssueKind::UnsafePublicField);
    }
    let created_at = normalize_timestamp_value(&manifest.created_at)
        .ok_or(BenchmarkSuiteLoadIssueKind::TimestampInvalid)?;
    let updated_at = normalize_timestamp_value(&manifest.updated_at)
        .ok_or(BenchmarkSuiteLoadIssueKind::TimestampInvalid)?;
    let created_at_value =
        parsed_timestamp(&created_at).ok_or(BenchmarkSuiteLoadIssueKind::TimestampInvalid)?;
    let updated_at_value =
        parsed_timestamp(&updated_at).ok_or(BenchmarkSuiteLoadIssueKind::TimestampInvalid)?;
    if created_at_value > updated_at_value {
        return Err(BenchmarkSuiteLoadIssueKind::IncoherentManifest);
    }
    manifest.created_at = created_at;
    manifest.updated_at = updated_at;
    if manifest.runs.is_empty() || manifest.runs.len() > MAX_MANIFEST_RUNS {
        return Err(BenchmarkSuiteLoadIssueKind::IncoherentManifest);
    }
    let mut run_indices = HashSet::new();
    let mut session_ids = HashSet::new();
    for run in &mut manifest.runs {
        if run.run_index >= MAX_MANIFEST_RUNS || !run_indices.insert(run.run_index) {
            return Err(BenchmarkSuiteLoadIssueKind::IncoherentManifest);
        }
        if !is_safe_public_manifest_field(&run.profile)
            || !is_safe_public_manifest_field(&run.run_type)
            || (!run.target_id.is_empty() && !is_safe_public_manifest_field(&run.target_id))
            || !is_canonical_benchmark_id(&run.benchmark_id)
        {
            return Err(BenchmarkSuiteLoadIssueKind::UnsafePublicField);
        }
        if !is_known_run_state(&run.state) {
            return Err(BenchmarkSuiteLoadIssueKind::IncoherentManifest);
        }
        let normalized_launched_at = match (
            run.session_id.as_deref(),
            run.launched_at.as_deref(),
            run.state.as_str(),
        ) {
            (None, None, "pending") => None,
            (Some(session_id), Some(launched_at), state) if state != "pending" => {
                if !is_safe_public_manifest_field(session_id)
                    || !session_ids.insert(session_id.to_string())
                {
                    return Err(BenchmarkSuiteLoadIssueKind::AmbiguousSessionId);
                }
                let launched_at = normalize_timestamp_value(launched_at)
                    .ok_or(BenchmarkSuiteLoadIssueKind::TimestampInvalid)?;
                let launched_at_value = parsed_timestamp(&launched_at)
                    .ok_or(BenchmarkSuiteLoadIssueKind::TimestampInvalid)?;
                if launched_at_value > updated_at_value {
                    return Err(BenchmarkSuiteLoadIssueKind::IncoherentManifest);
                }
                Some(launched_at)
            }
            _ => return Err(BenchmarkSuiteLoadIssueKind::IncoherentManifest),
        };
        if let Some(launched_at) = normalized_launched_at {
            run.launched_at = Some(launched_at);
        }
    }
    manifest.runs.sort_by_key(|run| run.run_index);
    Ok(())
}

fn publish_manifest(
    inner: &mut BenchmarkSuiteInner,
    manifest: BenchmarkSuiteManifest,
    generation: u64,
) {
    let suite_id = manifest.suite_id.clone();
    if let Some(previous) = inner.suites.get(&suite_id) {
        for session_id in previous
            .manifest
            .runs
            .iter()
            .filter_map(|run| run.session_id.as_ref())
        {
            inner.session_index.remove(session_id);
        }
    }
    for run in &manifest.runs {
        let Some(mapping) = run_mapping(run) else {
            continue;
        };
        inner.session_index.insert(
            mapping.session_id.clone(),
            SuiteSessionMapping {
                suite_id: suite_id.clone(),
                ..mapping
            },
        );
    }
    inner.suites.insert(
        suite_id,
        VersionedManifest {
            generation,
            manifest,
        },
    );
}

fn run_mapping(run: &BenchmarkSuiteManifestRun) -> Option<SuiteSessionMapping> {
    Some(SuiteSessionMapping {
        suite_id: String::new(),
        run_index: run.run_index,
        session_id: run.session_id.clone()?,
    })
}

fn new_manifest(selection: &BenchmarkSuiteSelection, now: &str) -> BenchmarkSuiteManifest {
    BenchmarkSuiteManifest {
        schema: BENCHMARK_SUITE_SCHEMA.to_string(),
        schema_version: BENCHMARK_SUITE_SCHEMA_VERSION,
        suite_id: selection.suite_id.clone(),
        instance_id: selection.instance_id.clone(),
        mode: selection.mode.clone(),
        created_at: now.to_string(),
        updated_at: now.to_string(),
        runs: Vec::new(),
    }
}

fn pending_manifest(
    selection: &BenchmarkSuiteSelection,
    created_at: &str,
) -> BenchmarkSuiteManifest {
    let mut manifest = new_manifest(selection, created_at);
    for run in &selection.plan {
        upsert_plan_run(&mut manifest.runs, run);
    }
    manifest.runs.sort_by_key(|run| run.run_index);
    manifest
}

fn normalize_plan(
    plan: &[BenchmarkSuiteRunInput],
) -> Result<Vec<BenchmarkSuiteRunInput>, BenchmarkSuiteStoreError> {
    if plan.is_empty() || plan.len() > MAX_MANIFEST_RUNS {
        return Err(BenchmarkSuiteStoreError::InvalidSuiteId);
    }
    let mut indices = HashSet::new();
    let mut normalized = Vec::with_capacity(plan.len());
    for run in plan {
        if run.run_index >= MAX_MANIFEST_RUNS || !indices.insert(run.run_index) {
            return Err(BenchmarkSuiteStoreError::InvalidSuiteId);
        }
        normalized.push(BenchmarkSuiteRunInput {
            run_index: run.run_index,
            profile: require_safe_manifest_field(&run.profile)?,
            run_type: require_safe_manifest_field(&run.run_type)?,
            target_id: run
                .target_id
                .as_deref()
                .map(require_safe_manifest_field)
                .transpose()?,
            benchmark_id: require_canonical_benchmark_id(&run.benchmark_id)?,
        });
    }
    normalized.sort_by_key(|run| run.run_index);
    if normalized
        .iter()
        .enumerate()
        .any(|(expected, run)| expected != run.run_index)
    {
        return Err(BenchmarkSuiteStoreError::InvalidSuiteId);
    }
    Ok(normalized)
}

fn upsert_plan_run(runs: &mut Vec<BenchmarkSuiteManifestRun>, run: &BenchmarkSuiteRunInput) {
    let target_id = run.target_id.clone().unwrap_or_default();
    if let Some(existing) = runs
        .iter_mut()
        .find(|existing| existing.run_index == run.run_index)
    {
        existing.profile = run.profile.clone();
        existing.run_type = run.run_type.clone();
        existing.target_id = target_id;
        existing.benchmark_id = run.benchmark_id.clone();
        if existing.state.trim().is_empty() {
            existing.state = "pending".to_string();
        }
        return;
    }
    runs.push(BenchmarkSuiteManifestRun {
        run_index: run.run_index,
        profile: run.profile.clone(),
        run_type: run.run_type.clone(),
        target_id,
        benchmark_id: run.benchmark_id.clone(),
        session_id: None,
        launched_at: None,
        state: "pending".to_string(),
    });
}

fn upsert_launched_run(
    runs: &mut Vec<BenchmarkSuiteManifestRun>,
    run: &BenchmarkSuiteRunInput,
    session_id: Option<String>,
    launched_at: Option<String>,
) {
    upsert_plan_run(runs, run);
    if let Some(existing) = runs
        .iter_mut()
        .find(|existing| existing.run_index == run.run_index)
    {
        existing.session_id = session_id;
        existing.launched_at = launched_at;
        existing.state = "launching".to_string();
    }
}

fn is_known_run_state(value: &str) -> bool {
    matches!(
        value,
        "pending" | "launching" | "running" | "failed" | "stopped" | "exited" | "completed"
    )
}

fn normalize_outcome_state(value: &str) -> String {
    match value.trim() {
        "running" => "running",
        "failed" => "failed",
        "stopped" => "stopped",
        "exited" => "exited",
        "completed" => "completed",
        _ => "failed",
    }
    .to_string()
}

fn require_canonical_suite_id(value: &str) -> Result<String, BenchmarkSuiteStoreError> {
    is_canonical_suite_id(value)
        .then(|| value.to_string())
        .ok_or(BenchmarkSuiteStoreError::InvalidSuiteId)
}

fn require_canonical_benchmark_id(value: &str) -> Result<String, BenchmarkSuiteStoreError> {
    is_canonical_benchmark_id(value)
        .then(|| value.to_string())
        .ok_or(BenchmarkSuiteStoreError::InvalidSuiteId)
}

fn is_canonical_suite_id(value: &str) -> bool {
    let Some(identity) = value.strip_prefix(SUITE_ID_PREFIX) else {
        return false;
    };
    let Some((mode, digest)) = identity.rsplit_once('-') else {
        return false;
    };
    matches!(mode, "dev" | "qual" | "release" | "custom") && is_lower_hex_digest(digest)
}

fn is_canonical_benchmark_id(value: &str) -> bool {
    value
        .strip_prefix(BENCHMARK_ID_PREFIX)
        .is_some_and(is_lower_hex_digest)
}

fn is_lower_hex_digest(value: &str) -> bool {
    value.len() == OPAQUE_ID_HEX_CHARS
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn require_safe_manifest_field(value: &str) -> Result<String, BenchmarkSuiteStoreError> {
    is_safe_public_manifest_field(value)
        .then(|| value.to_string())
        .ok_or(BenchmarkSuiteStoreError::InvalidSuiteId)
}

fn require_suite_mode(value: &str) -> Result<String, BenchmarkSuiteStoreError> {
    matches!(
        value,
        "development" | "qualification" | "release_validation"
    )
    .then(|| value.to_string())
    .ok_or(BenchmarkSuiteStoreError::InvalidSuiteId)
}

fn safe_manifest_field(value: &str) -> Option<String> {
    let value = value
        .trim()
        .chars()
        .filter(|value| {
            !value.is_control() && *value != '/' && *value != '\\' && *value != ':' && *value != ';'
        })
        .take(MAX_MANIFEST_FIELD_CHARS)
        .collect::<String>();
    (!value.is_empty()).then_some(value)
}

fn is_safe_public_manifest_field(value: &str) -> bool {
    safe_manifest_field(value).as_deref() == Some(value)
        && sanitize_evidence_token(
            value,
            RedactionAudience::UserVisible,
            MAX_MANIFEST_FIELD_CHARS,
        )
        .as_deref()
            == Some(value)
}

fn read_bounded_manifest(path: &Path) -> io::Result<Vec<u8>> {
    let file = File::open(path)?;
    let mut data = Vec::new();
    file.take(MAX_MANIFEST_BYTES + 1).read_to_end(&mut data)?;
    if data.len() as u64 > MAX_MANIFEST_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "benchmark suite manifest exceeds the size limit",
        ));
    }
    Ok(data)
}

fn next_generation(current: u64) -> Result<u64, BenchmarkSuiteStoreError> {
    current
        .checked_add(1)
        .ok_or(BenchmarkSuiteStoreError::GenerationOverflow)
}

fn normalize_timestamp_value(value: &str) -> Option<String> {
    let parsed = DateTime::parse_from_rfc3339(value.trim())
        .ok()?
        .with_timezone(&Utc);
    Some(parsed.to_rfc3339_opts(SecondsFormat::AutoSi, true))
}

fn parsed_timestamp(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|value| value.with_timezone(&Utc))
}

fn latest_timestamp_value(values: &[&str]) -> String {
    values
        .iter()
        .filter_map(|value| parsed_timestamp(value))
        .max()
        .expect("runtime benchmark suite timestamps are normalized")
        .to_rfc3339_opts(SecondsFormat::AutoSi, true)
}

fn suite_id_from_filename(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    is_canonical_suite_id(stem).then(|| stem.to_string())
}

fn reject_identifiable(
    load_state: &mut BenchmarkSuiteLoadState,
    suite_id: Option<String>,
    kind: BenchmarkSuiteLoadIssueKind,
) {
    reserve_rejected_id(load_state, suite_id.as_deref());
    record_load_issue(&mut load_state.issues, kind);
}

fn reject_parsed_manifest(
    load_state: &mut BenchmarkSuiteLoadState,
    filename_id: Option<&str>,
    manifest_id: Option<&str>,
    kind: BenchmarkSuiteLoadIssueKind,
) {
    reserve_rejected_id(load_state, filename_id);
    reserve_rejected_id(load_state, manifest_id);
    record_load_issue(&mut load_state.issues, kind);
}

fn reserve_rejected_id(load_state: &mut BenchmarkSuiteLoadState, suite_id: Option<&str>) {
    if let Some(suite_id) = suite_id.filter(|value| is_canonical_suite_id(value)) {
        load_state.inner.rejected_ids.insert(suite_id.to_string());
    }
}

fn record_load_issue(issues: &mut Vec<BenchmarkSuiteLoadIssue>, kind: BenchmarkSuiteLoadIssueKind) {
    if let Some(issue) = issues.iter_mut().find(|issue| issue.kind == kind) {
        issue.count = issue.count.saturating_add(1);
    } else {
        issues.push(BenchmarkSuiteLoadIssue { kind, count: 1 });
    }
}

fn suite_dir(paths: &AppPaths) -> PathBuf {
    paths.config_dir.join("benchmarks").join("suites")
}

fn suite_path_in_dir(storage_dir: &Path, suite_id: &str) -> PathBuf {
    debug_assert!(is_canonical_suite_id(suite_id));
    storage_dir.join(format!("{suite_id}.json"))
}

fn encode_manifest(manifest: BenchmarkSuiteManifest) -> io::Result<Vec<u8>> {
    serde_json::to_vec_pretty(&manifest)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn benchmark_suite_target(suite_id: &str) -> crate::state::contracts::TargetDescriptor {
    classify_current_artifact(CurrentArtifact::BenchmarkSuiteManifest, suite_id).target
}

fn suite_persistence_error(
    error: crate::execution::persistence::PersistenceError,
) -> BenchmarkSuiteStoreError {
    BenchmarkSuiteStoreError::Persistence(error.into())
}

fn stable_hash(parts: &[&str]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for part in parts {
        for byte in part.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        hash ^= 0xff;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::file::{FileWriteRequest, write_file_atomically};
    use crate::execution::persistence::AtomicWriteBackend;
    use crate::state::contracts::TargetDescriptor;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Condvar, Mutex};
    use std::time::{SystemTime, UNIX_EPOCH};
    use tokio::sync::Notify;

    struct RecordingFileBackend {
        attempts: AtomicUsize,
        failures: AtomicUsize,
        committed: Mutex<Vec<Vec<u8>>>,
        started: Notify,
        gate: Mutex<Option<Arc<WriteGate>>>,
    }

    struct WriteGate {
        released: Mutex<bool>,
        changed: Condvar,
    }

    struct WriteGateHandle(Arc<WriteGate>);

    impl RecordingFileBackend {
        fn new() -> Self {
            Self {
                attempts: AtomicUsize::new(0),
                failures: AtomicUsize::new(0),
                committed: Mutex::new(Vec::new()),
                started: Notify::new(),
                gate: Mutex::new(None),
            }
        }

        fn fail_next(&self, count: usize) {
            self.failures.fetch_add(count, Ordering::SeqCst);
        }

        fn gate_next(&self) -> WriteGateHandle {
            let gate = Arc::new(WriteGate {
                released: Mutex::new(false),
                changed: Condvar::new(),
            });
            *self.gate.lock().expect("backend gate lock") = Some(gate.clone());
            WriteGateHandle(gate)
        }

        async fn wait_for_attempt(&self, expected: usize) {
            loop {
                let started = self.started.notified();
                if self.attempts.load(Ordering::SeqCst) >= expected {
                    return;
                }
                started.await;
            }
        }

        fn committed_manifests(&self) -> Vec<BenchmarkSuiteManifest> {
            self.committed
                .lock()
                .expect("committed snapshot lock")
                .iter()
                .map(|bytes| serde_json::from_slice(bytes).expect("decode manifest"))
                .collect()
        }
    }

    impl AtomicWriteBackend for RecordingFileBackend {
        fn write(
            &self,
            target: &TargetDescriptor,
            destination: &Path,
            contents: &[u8],
        ) -> io::Result<()> {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            self.started.notify_one();
            if let Some(gate) = self.gate.lock().expect("backend gate lock").take() {
                gate.wait();
            }
            if self
                .failures
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                    remaining.checked_sub(1)
                })
                .is_ok()
            {
                return Err(io::Error::other("injected benchmark suite write failure"));
            }
            write_file_atomically(FileWriteRequest::new(target.clone(), destination, contents))
                .map_err(io::Error::from)?;
            self.committed
                .lock()
                .expect("committed snapshot lock")
                .push(contents.to_vec());
            Ok(())
        }
    }

    impl WriteGate {
        fn wait(&self) {
            let mut released = self.released.lock().expect("write gate lock");
            while !*released {
                released = self.changed.wait(released).expect("wait on write gate");
            }
        }

        fn release(&self) {
            *self.released.lock().expect("write gate lock") = true;
            self.changed.notify_all();
        }
    }

    impl WriteGateHandle {
        fn release(&self) {
            self.0.release();
        }
    }

    impl Drop for WriteGateHandle {
        fn drop(&mut self) {
            self.0.release();
        }
    }

    fn persistence_fixture(
        name: &str,
    ) -> (
        PathBuf,
        AppPaths,
        Arc<RecordingFileBackend>,
        PersistenceCoordinator,
        BenchmarkSuiteStore,
    ) {
        let root = test_root(name);
        let paths = test_paths(&root);
        let backend = Arc::new(RecordingFileBackend::new());
        let coordinator = PersistenceCoordinator::for_test(
            backend.clone(),
            Duration::from_millis(20),
            Duration::from_millis(100),
        );
        let store =
            BenchmarkSuiteStore::try_load_from_paths_with_coordinator(&paths, coordinator.clone())
                .expect("claim suite persistence");
        (root, paths, backend, coordinator, store)
    }

    #[test]
    fn derived_suite_ids_are_private_bounded_deterministic_and_canonical() {
        let instance_id = "SecretPlayer-account-token-instance";
        for mode in ["development", "qualification", "release_validation"] {
            let first = derive_suite_id(instance_id, mode);
            let second = derive_suite_id(instance_id, mode);

            assert_eq!(first, second);
            assert!(is_canonical_suite_id(&first));
            assert!(!first.contains("SecretPlayer"));
            assert!(!first.contains("account"));
            assert!(!first.contains("token"));
            assert_eq!(normalize_suite_id(&first).as_deref(), Some(first.as_str()));
            assert_eq!(
                require_canonical_suite_id(&first).expect("canonical id"),
                first
            );
        }

        assert_ne!(
            derive_suite_id(instance_id, "development"),
            derive_suite_id(instance_id, "qualification")
        );
        assert_ne!(
            derive_suite_id(instance_id, "development"),
            derive_suite_id("another-instance", "development")
        );
    }

    #[test]
    fn normalized_suite_ids_hash_every_noncanonical_nonempty_input() {
        let long_id = format!("private-instance-{}", "a1".repeat(48));
        let raw_ids = [
            "suite-safe",
            "suite-release-validation-family-c",
            "suite-dev-0123456789ABCDEf",
            "account-token-secret",
            r"C:\Users\Alice\private-suite.json",
            " suite-safe ",
            long_id.as_str(),
        ];
        for raw in raw_ids {
            let normalized = normalize_suite_id(raw).expect("normalized suite id");

            assert!(normalized.starts_with("suite-custom-"));
            assert!(is_canonical_suite_id(&normalized));
            assert!(!normalized.contains("Alice"));
            assert!(!normalized.contains("account"));
            assert!(!normalized.contains("token"));
            assert!(!normalized.contains("private-instance"));
            assert_eq!(
                normalize_suite_id(&normalized).as_deref(),
                Some(normalized.as_str())
            );
            assert_eq!(
                require_canonical_suite_id(&normalized).expect("canonical normalized id"),
                normalized
            );
            assert_eq!(
                normalize_suite_id(raw).as_deref(),
                Some(normalized.as_str())
            );
        }
        assert_eq!(normalize_suite_id("   "), None);
    }

    #[test]
    fn canonical_suite_ids_are_preserved_exactly() {
        for suite_id in [
            derive_suite_id("instance", "development"),
            derive_suite_id("instance", "qualification"),
            derive_suite_id("instance", "release_validation"),
            test_suite_id("caller-selected"),
        ] {
            assert!(is_canonical_suite_id(&suite_id));
            assert_eq!(
                normalize_suite_id(&suite_id).as_deref(),
                Some(suite_id.as_str())
            );
            assert_eq!(
                require_canonical_suite_id(&suite_id).expect("canonical suite id"),
                suite_id
            );
        }
    }

    #[test]
    fn opaque_id_grammars_require_exact_prefix_mode_and_lowercase_digest() {
        for suite_id in [
            "suite-dev-0123456789abcdef",
            "suite-qual-0123456789abcdef",
            "suite-release-0123456789abcdef",
            "suite-custom-0123456789abcdef",
        ] {
            assert!(is_canonical_suite_id(suite_id));
        }
        for suite_id in [
            "suite-0123456789abcdef",
            "suite-release-validation-0123456789abcdef",
            "suite-dev-0123456789abcde",
            "suite-dev-0123456789abcdef0",
            "suite-dev-0123456789abcdeF",
        ] {
            assert!(!is_canonical_suite_id(suite_id));
        }

        assert!(is_canonical_benchmark_id("benchmark-0123456789abcdef"));
        for benchmark_id in [
            "benchmark-release-validation",
            "benchmark-0123456789abcde",
            "benchmark-0123456789abcdef0",
            "benchmark-0123456789abcdeF",
        ] {
            assert!(!is_canonical_benchmark_id(benchmark_id));
        }
    }

    #[tokio::test]
    async fn generated_suite_modes_and_benchmark_ids_pass_runtime_admission() {
        let store = BenchmarkSuiteStore::new();
        for mode in ["development", "qualification", "release_validation"] {
            let suite_id = derive_suite_id("private-instance", mode);
            let plan = crate::application::performance::benchmark_suite_plan(mode)
                .expect("generated benchmark suite plan");
            let runs =
                crate::application::performance::benchmark_suite_manifest_run_inputs(mode, &plan);

            assert!(is_canonical_suite_id(&suite_id));
            assert!(
                runs.iter()
                    .all(|run| is_canonical_benchmark_id(&run.benchmark_id))
            );
            store
                .select_reservation(&suite_id, "instance", mode, &runs, Some(0))
                .await
                .expect("generated ids pass strict runtime admission");
        }
    }

    #[tokio::test]
    async fn runtime_admission_rejects_semantic_suite_and_benchmark_ids() {
        let store = BenchmarkSuiteStore::new();
        assert!(matches!(
            store
                .select_reservation(
                    "suite-release-validation-family-c",
                    "instance",
                    "development",
                    &test_plan(),
                    None,
                )
                .await,
            Err(BenchmarkSuiteStoreError::InvalidSuiteId)
        ));

        let mut semantic_plan = test_plan();
        semantic_plan[0].benchmark_id = "release-validation-family-c-baseline".to_string();
        assert!(matches!(
            store
                .select_reservation(
                    &test_suite_id("semantic-benchmark-plan"),
                    "instance",
                    "development",
                    &semantic_plan,
                    None,
                )
                .await,
            Err(BenchmarkSuiteStoreError::InvalidSuiteId)
        ));
    }

    #[tokio::test]
    async fn auto_reservation_generation_has_one_winner() {
        let store = BenchmarkSuiteStore::new();
        let first = selection(&store, "suite-auto", None).await;
        let second = selection(&store, "suite-auto", None).await;

        let reserved = store
            .reserve(first, "session-1", test_timestamp(), false)
            .await
            .expect("first auto reservation wins");
        let conflict = store
            .reserve(second, "session-2", test_timestamp(), false)
            .await;

        assert_eq!(reserved.run_index, 0);
        assert!(matches!(
            conflict,
            Err(BenchmarkSuiteReserveError::PreAccept(
                BenchmarkSuiteStoreError::AutoConflict
            ))
        ));
        assert!(
            store
                .get(&test_suite_id("suite-auto"))
                .expect("manifest")
                .is_some()
        );
    }

    #[tokio::test]
    async fn reservation_updated_at_includes_future_launch_on_reload() {
        let (root, paths, _, coordinator, store) = persistence_fixture("future-launch-time");
        let launched_at = "2099-01-02T03:04:05.000Z";
        let selected = selection(&store, "suite-future-launch", None).await;

        let reservation = store
            .reserve(selected, "session-future", launched_at, false)
            .await
            .expect("reserve future launch");

        assert_eq!(
            reservation.manifest.updated_at,
            normalize_timestamp_value(launched_at).expect("future timestamp")
        );
        assert!(
            parsed_timestamp(&reservation.manifest.created_at).expect("created timestamp")
                <= parsed_timestamp(&reservation.manifest.updated_at).expect("updated timestamp")
        );
        store.close().await.expect("close store");
        drop(store);

        let reloaded =
            BenchmarkSuiteStore::try_load_from_paths_with_coordinator(&paths, coordinator)
                .expect("reload future launch");
        let manifest = reloaded
            .get(&test_suite_id("suite-future-launch"))
            .expect("read future launch")
            .expect("future launch manifest");
        assert_eq!(manifest.updated_at, reservation.manifest.updated_at);
        assert_eq!(
            manifest.runs[0].launched_at,
            normalize_timestamp_value(launched_at)
        );
        reloaded.close().await.expect("close reloaded store");
        cleanup(&root);
    }

    #[tokio::test]
    async fn reservation_updated_at_survives_runtime_clock_rollback() {
        let root = test_root("reservation-clock-rollback");
        let paths = test_paths(&root);
        let dir = suite_dir(&paths);
        fs::create_dir_all(&dir).expect("create suite dir");
        let previous_updated_at = "2099-02-03T04:05:06.000Z";
        let mut persisted = valid_manifest("suite-clock-rollback", "session-old");
        persisted.updated_at = previous_updated_at.to_string();
        persisted.runs[0].state = "failed".to_string();
        fs::write(
            suite_path(&paths, &persisted.suite_id),
            serde_json::to_vec_pretty(&persisted).expect("encode existing manifest"),
        )
        .expect("write existing manifest");
        let backend = Arc::new(RecordingFileBackend::new());
        let coordinator = PersistenceCoordinator::for_test(
            backend,
            Duration::from_millis(20),
            Duration::from_millis(100),
        );
        let store =
            BenchmarkSuiteStore::try_load_from_paths_with_coordinator(&paths, coordinator.clone())
                .expect("load future-dated manifest");
        let selected = selection(&store, "suite-clock-rollback", None).await;

        let reservation = store
            .reserve(selected, "session-new", test_timestamp(), false)
            .await
            .expect("reserve during clock rollback");

        assert_eq!(
            reservation.manifest.updated_at,
            normalize_timestamp_value(previous_updated_at).expect("previous timestamp")
        );
        assert!(
            parsed_timestamp(&reservation.manifest.created_at).expect("created timestamp")
                <= parsed_timestamp(&reservation.manifest.updated_at).expect("updated timestamp")
        );
        store.close().await.expect("close store");
        drop(store);
        let reloaded =
            BenchmarkSuiteStore::try_load_from_paths_with_coordinator(&paths, coordinator)
                .expect("reload clock-rollback reservation");
        assert!(
            reloaded
                .get(&test_suite_id("suite-clock-rollback"))
                .expect("read clock-rollback suite")
                .is_some()
        );
        reloaded.close().await.expect("close reloaded store");
        cleanup(&root);
    }

    #[tokio::test]
    async fn outcome_updated_at_is_monotonic_and_reloadable() {
        let (root, paths, _, coordinator, store) = persistence_fixture("outcome-monotonic-time");
        let launched_at = "2099-03-04T05:06:07.000Z";
        let selected = selection(&store, "suite-outcome-time", None).await;
        store
            .reserve(selected, "session-future", launched_at, false)
            .await
            .expect("reserve future launch");

        store
            .update_run_state_for_session("session-future", "completed")
            .await
            .expect("persist outcome during clock rollback");

        let manifest = store
            .get(&test_suite_id("suite-outcome-time"))
            .expect("read outcome")
            .expect("outcome manifest");
        assert_eq!(
            manifest.updated_at,
            normalize_timestamp_value(launched_at).expect("future timestamp")
        );
        assert_eq!(manifest.runs[0].state, "completed");
        store.close().await.expect("close store");
        drop(store);
        let reloaded =
            BenchmarkSuiteStore::try_load_from_paths_with_coordinator(&paths, coordinator)
                .expect("reload monotonic outcome");
        assert_eq!(
            reloaded
                .get(&test_suite_id("suite-outcome-time"))
                .expect("read reloaded outcome")
                .expect("reloaded outcome manifest"),
            manifest
        );
        reloaded.close().await.expect("close reloaded store");
        cleanup(&root);
    }

    #[tokio::test]
    async fn explicit_rerun_replaces_terminal_mapping_and_late_outcome_is_ignored() {
        let store = BenchmarkSuiteStore::new();
        let first = selection(&store, "suite-explicit", None).await;
        store
            .reserve(first, "session-old", test_timestamp(), false)
            .await
            .expect("initial reservation");
        assert!(
            store
                .update_run_state_for_session("session-old", "failed")
                .await
                .expect("terminal outcome")
        );

        let replacement = selection(&store, "suite-explicit", Some(0)).await;
        store
            .reserve(replacement, "session-replacement", test_timestamp(), false)
            .await
            .expect("explicit replacement");

        assert!(
            !store
                .update_run_state_for_session("session-old", "stopped")
                .await
                .expect("late outcome is harmless")
        );
        let manifest = store
            .get(&test_suite_id("suite-explicit"))
            .expect("read manifest")
            .expect("manifest exists");
        assert_eq!(
            manifest.runs[0].session_id.as_deref(),
            Some("session-replacement")
        );
        assert_eq!(manifest.runs[0].state, "launching");
    }

    #[tokio::test]
    async fn explicit_rerun_rejects_active_displaced_session() {
        let store = BenchmarkSuiteStore::new();
        let first = selection(&store, "suite-active", None).await;
        store
            .reserve(first, "session-old", test_timestamp(), false)
            .await
            .expect("initial reservation");
        let replacement = selection(&store, "suite-active", Some(0)).await;

        assert!(matches!(
            store
                .reserve(replacement, "session-new", test_timestamp(), true,)
                .await,
            Err(BenchmarkSuiteReserveError::PreAccept(
                BenchmarkSuiteStoreError::ExplicitActiveConflict
            ))
        ));
    }

    #[tokio::test]
    async fn explicit_rerun_rejects_store_known_live_mapping_without_caller_hint() {
        let store = BenchmarkSuiteStore::new();
        let first = selection(&store, "suite-store-live", None).await;
        store
            .reserve(first, "session-old", test_timestamp(), false)
            .await
            .expect("initial reservation");
        let replacement = selection(&store, "suite-store-live", Some(0)).await;

        assert!(matches!(
            store
                .reserve(replacement, "session-new", test_timestamp(), false)
                .await,
            Err(BenchmarkSuiteReserveError::PreAccept(
                BenchmarkSuiteStoreError::ExplicitActiveConflict
            ))
        ));
    }

    #[tokio::test]
    async fn existing_suite_identity_mismatch_does_not_mutate() {
        let store = BenchmarkSuiteStore::new();
        let first = selection(&store, "suite-identity", None).await;
        store
            .reserve(first, "session-1", test_timestamp(), false)
            .await
            .expect("initial reservation");

        let mismatch = store
            .select_reservation(
                &test_suite_id("suite-identity"),
                "different-instance",
                "development",
                &test_plan(),
                Some(0),
            )
            .await;

        assert!(matches!(
            mismatch,
            Err(BenchmarkSuiteStoreError::SuiteIdentityMismatch)
        ));
        assert_eq!(
            store
                .get(&test_suite_id("suite-identity"))
                .expect("manifest")
                .expect("stored")
                .instance_id,
            "instance"
        );
    }

    #[tokio::test]
    async fn session_id_cannot_be_assigned_to_another_suite() {
        let store = BenchmarkSuiteStore::new();
        let first = selection(&store, "suite-session-first", None).await;
        store
            .reserve(first, "shared-session", test_timestamp(), false)
            .await
            .expect("first reservation");
        let second = selection(&store, "suite-session-second", None).await;

        assert!(matches!(
            store
                .reserve(second, "shared-session", test_timestamp(), false)
                .await,
            Err(BenchmarkSuiteReserveError::PreAccept(
                BenchmarkSuiteStoreError::SessionConflict
            ))
        ));
    }

    #[tokio::test]
    async fn session_id_cannot_be_reused_for_its_existing_suite_mapping() {
        let store = BenchmarkSuiteStore::new();
        let first = selection(&store, "suite-session-same", None).await;
        store
            .reserve(first, "same-session", test_timestamp(), false)
            .await
            .expect("first reservation");
        store
            .update_run_state_for_session("same-session", "failed")
            .await
            .expect("terminal outcome");
        let before = store
            .get(&test_suite_id("suite-session-same"))
            .expect("manifest read")
            .expect("stored manifest");
        let replacement = selection(&store, "suite-session-same", Some(0)).await;

        assert!(matches!(
            store
                .reserve(replacement, "same-session", test_timestamp(), false)
                .await,
            Err(BenchmarkSuiteReserveError::PreAccept(
                BenchmarkSuiteStoreError::SessionConflict
            ))
        ));
        assert_eq!(
            store
                .get(&test_suite_id("suite-session-same"))
                .expect("manifest read")
                .expect("stored manifest"),
            before
        );
    }

    #[tokio::test]
    async fn successful_close_is_idempotent_and_rejects_later_mutations() {
        let store = BenchmarkSuiteStore::new();
        let before_close = selection(&store, "suite-before-close", None).await;

        store.close().await.expect("first close");
        store.close().await.expect("idempotent close");

        assert!(matches!(
            store
                .select_reservation(
                    &test_suite_id("suite-after-close"),
                    "instance",
                    "development",
                    &test_plan(),
                    None,
                )
                .await,
            Err(BenchmarkSuiteStoreError::Closed)
        ));
        assert!(matches!(
            store
                .reserve(before_close, "session-closed", test_timestamp(), false)
                .await,
            Err(BenchmarkSuiteReserveError::PreAccept(
                BenchmarkSuiteStoreError::Closed
            ))
        ));
        assert!(matches!(
            store
                .update_run_state_for_session("missing-session", "failed")
                .await,
            Err(BenchmarkSuiteStoreError::Closed)
        ));
    }

    #[tokio::test]
    async fn reservation_is_hidden_until_physical_commit_and_caller_cancel_is_safe() {
        let (root, _, backend, _, store) = persistence_fixture("hidden-cancel");
        let store = Arc::new(store);
        let selection = selection(&store, "suite-hidden", None).await;
        let gate = backend.gate_next();
        let task_store = store.clone();
        let task = tokio::spawn(async move {
            task_store
                .reserve(selection, "session-1", test_timestamp(), false)
                .await
        });

        backend.wait_for_attempt(1).await;
        assert!(
            store
                .get(&test_suite_id("suite-hidden"))
                .expect("read")
                .is_none()
        );
        task.abort();
        assert!(task.await.expect_err("caller cancelled").is_cancelled());
        gate.release();
        store.flush().await.expect("observer finishes commit");

        assert_eq!(
            store
                .get(&test_suite_id("suite-hidden"))
                .expect("read")
                .expect("committed")
                .runs[0]
                .state,
            "launching"
        );
        store.close().await.expect("close store");
        cleanup(&root);
    }

    #[tokio::test]
    async fn failed_accepted_reservation_commits_pending_compensation_not_launching() {
        let (root, _, backend, _, store) = persistence_fixture("compensation-success");
        backend.fail_next(1);
        let selected = selection(&store, "suite-compensated", None).await;

        let error = store
            .reserve(selected, "session-1", test_timestamp(), false)
            .await
            .expect_err("accepted reservation write fails");
        let handle = accepted_write_handle(error);

        store
            .settle_compensation(&handle)
            .await
            .expect("compensation already settled");
        let manifest = store
            .get(&test_suite_id("suite-compensated"))
            .expect("read")
            .expect("pending compensation visible");
        assert_eq!(manifest.runs[0].state, "pending");
        assert!(manifest.runs[0].session_id.is_none());
        assert!(
            backend
                .committed_manifests()
                .iter()
                .all(|manifest| manifest.runs[0].state != "launching")
        );
        store.close().await.expect("close store");
        cleanup(&root);
    }

    #[tokio::test]
    async fn failed_compensation_retains_exact_retry_and_unrelated_suite_progresses() {
        let (root, _, backend, _, store) = persistence_fixture("compensation-retry");
        backend.fail_next(2);
        let selected = selection(&store, "suite-failed", None).await;
        let error = store
            .reserve(selected, "session-failed", test_timestamp(), false)
            .await
            .expect_err("reservation and compensation fail");
        let handle = accepted_write_handle(error);
        assert!(
            store
                .get(&test_suite_id("suite-failed"))
                .expect("read")
                .is_none()
        );

        let unrelated = selection(&store, "suite-unrelated", None).await;
        store
            .reserve(unrelated, "session-unrelated", test_timestamp(), false)
            .await
            .expect("unrelated suite remains usable");
        store
            .settle_compensation(&handle)
            .await
            .expect("exact pending compensation retries");

        assert_eq!(
            store
                .get(&test_suite_id("suite-failed"))
                .expect("read")
                .expect("pending manifest")
                .runs[0]
                .state,
            "pending"
        );
        store.close().await.expect("close store");
        cleanup(&root);
    }

    #[tokio::test]
    async fn stale_compensation_handle_does_not_reconcile_newer_obligation() {
        let (root, _, backend, _, store) = persistence_fixture("stale-compensation-handle");
        backend.fail_next(2);
        let first = selection(&store, "suite-stale-handle", None).await;
        let first_error = store
            .reserve(first, "session-first", test_timestamp(), false)
            .await
            .expect_err("first reservation and compensation fail");
        let first_handle = accepted_write_handle(first_error);
        backend.wait_for_attempt(2).await;
        store
            .settle_compensation(&first_handle)
            .await
            .expect("first obligation settles");

        let attempts_before_second = backend.attempts.load(Ordering::SeqCst);
        backend.fail_next(2);
        let second = selection(&store, "suite-stale-handle", None).await;
        let second_error = store
            .reserve(second, "session-second", test_timestamp(), false)
            .await
            .expect_err("second reservation and compensation fail");
        let second_handle = accepted_write_handle(second_error);
        backend.wait_for_attempt(attempts_before_second + 2).await;
        assert!(second_handle.obligation_id > first_handle.obligation_id);

        let attempts_before_stale_settle = backend.attempts.load(Ordering::SeqCst);
        store
            .settle_compensation(&first_handle)
            .await
            .expect("stale handle is already settled");

        assert_eq!(
            backend.attempts.load(Ordering::SeqCst),
            attempts_before_stale_settle
        );
        assert_eq!(
            store
                .obligations
                .lock()
                .expect(SUITE_STORE_LOCK_INVARIANT)
                .get(&test_suite_id("suite-stale-handle"))
                .map(|obligation| obligation.obligation_id),
            Some(second_handle.obligation_id)
        );
        store
            .settle_compensation(&second_handle)
            .await
            .expect("newer obligation settles independently");
        store.close().await.expect("close store");
        cleanup(&root);
    }

    #[tokio::test]
    async fn existing_suite_compensation_restores_exact_manifest_and_generation() {
        let (root, _, backend, _, store) = persistence_fixture("existing-compensation");
        let initial = selection(&store, "suite-existing", None).await;
        store
            .reserve(initial, "session-old", test_timestamp(), false)
            .await
            .expect("initial reservation");
        store
            .update_run_state_for_session("session-old", "failed")
            .await
            .expect("terminal outcome");
        let before = store
            .get(&test_suite_id("suite-existing"))
            .expect("read")
            .expect("existing manifest");
        let before_generation = store
            .inner
            .read()
            .expect(SUITE_STORE_LOCK_INVARIANT)
            .suites
            .get(&test_suite_id("suite-existing"))
            .expect("versioned manifest")
            .generation;
        let replacement = selection(&store, "suite-existing", Some(0)).await;
        backend.fail_next(1);

        let error = store
            .reserve(replacement, "session-replacement", test_timestamp(), false)
            .await
            .expect_err("replacement reservation fails");
        let handle = accepted_write_handle(error);
        store
            .settle_compensation(&handle)
            .await
            .expect("restore exact previous manifest");

        assert_eq!(
            store
                .get(&test_suite_id("suite-existing"))
                .expect("read")
                .expect("restored manifest"),
            before
        );
        assert_eq!(
            store
                .inner
                .read()
                .expect(SUITE_STORE_LOCK_INVARIANT)
                .suites
                .get(&test_suite_id("suite-existing"))
                .expect("versioned manifest")
                .generation,
            before_generation
        );
        store.close().await.expect("close store");
        cleanup(&root);
    }

    #[tokio::test]
    async fn later_same_suite_selection_reconciles_failed_outcome_first() {
        let (root, _, backend, _, store) = persistence_fixture("outcome-retry");
        let first = selection(&store, "suite-outcome", None).await;
        store
            .reserve(first, "session-1", test_timestamp(), false)
            .await
            .expect("initial reservation");
        backend.fail_next(1);
        store
            .update_run_state_for_session("session-1", "failed")
            .await
            .expect_err("outcome persistence fails");
        assert_eq!(
            store
                .get(&test_suite_id("suite-outcome"))
                .expect("read")
                .expect("manifest")
                .runs[0]
                .state,
            "launching"
        );

        let next = selection(&store, "suite-outcome", None).await;

        assert_eq!(next.run_index(), 1);
        assert_eq!(
            store
                .get(&test_suite_id("suite-outcome"))
                .expect("read")
                .expect("manifest")
                .runs[0]
                .state,
            "failed"
        );
        store.close().await.expect("close store");
        cleanup(&root);
    }

    #[tokio::test]
    async fn stale_same_suite_reservation_cannot_overwrite_concurrent_outcome() {
        let store = BenchmarkSuiteStore::new();
        let first = selection(&store, "suite-concurrent", None).await;
        store
            .reserve(first, "session-1", test_timestamp(), false)
            .await
            .expect("initial reservation");
        store
            .update_run_state_for_session("session-1", "failed")
            .await
            .expect("first terminal outcome");
        let stale_next = selection(&store, "suite-concurrent", None).await;
        store
            .update_run_state_for_session("session-1", "stopped")
            .await
            .expect("concurrent outcome wins");

        assert!(matches!(
            store
                .reserve(stale_next, "session-2", test_timestamp(), false)
                .await,
            Err(BenchmarkSuiteReserveError::PreAccept(
                BenchmarkSuiteStoreError::AutoConflict
            ))
        ));
        assert_eq!(
            store
                .get(&test_suite_id("suite-concurrent"))
                .expect("read")
                .expect("manifest")
                .runs[0]
                .state,
            "stopped"
        );
    }

    #[tokio::test]
    async fn reconciled_retry_is_idempotent_without_extra_semantic_write() {
        let (root, _, backend, _, store) = persistence_fixture("retry-idempotent");
        let first = selection(&store, "suite-idempotent", None).await;
        store
            .reserve(first, "session-1", test_timestamp(), false)
            .await
            .expect("initial reservation");
        backend.fail_next(1);
        store
            .update_run_state_for_session("session-1", "failed")
            .await
            .expect_err("outcome write fails");

        let _ = selection(&store, "suite-idempotent", None).await;
        let attempts_after_retry = backend.attempts.load(Ordering::SeqCst);
        let _ = selection(&store, "suite-idempotent", None).await;

        assert_eq!(
            backend.attempts.load(Ordering::SeqCst),
            attempts_after_retry
        );
        assert_eq!(
            store
                .get(&test_suite_id("suite-idempotent"))
                .expect("read")
                .expect("manifest")
                .runs[0]
                .state,
            "failed"
        );
        store.close().await.expect("close store");
        cleanup(&root);
    }

    #[tokio::test]
    async fn lifecycle_retries_exact_obligation_and_releases_owner() {
        let (root, paths, backend, coordinator, store) = persistence_fixture("lifecycle");
        let selected = selection(&store, "suite-lifecycle", None).await;
        store
            .reserve(selected, "session-1", test_timestamp(), false)
            .await
            .expect("reservation");
        backend.fail_next(1);
        store
            .update_run_state_for_session("session-1", "failed")
            .await
            .expect_err("outcome write fails");

        store.close().await.expect("close reconciles exact outcome");
        drop(store);
        let reloaded =
            BenchmarkSuiteStore::try_load_from_paths_with_coordinator(&paths, coordinator)
                .expect("owner and state reload");
        assert_eq!(
            reloaded
                .get(&test_suite_id("suite-lifecycle"))
                .expect("read")
                .expect("manifest")
                .runs[0]
                .state,
            "failed"
        );
        reloaded.close().await.expect("close reloaded");
        cleanup(&root);
    }

    #[tokio::test]
    async fn failed_close_reopens_store_and_keeps_owner_until_retry() {
        let (root, paths, backend, coordinator, store) = persistence_fixture("close-retry");
        let first = selection(&store, "suite-close-retry", None).await;
        store
            .reserve(first, "session-1", test_timestamp(), false)
            .await
            .expect("reservation");
        backend.fail_next(1);
        store
            .update_run_state_for_session("session-1", "failed")
            .await
            .expect_err("outcome write fails");
        backend.fail_next(1);

        assert!(matches!(
            store.close().await,
            Err(BenchmarkSuiteStoreError::Persistence(_))
        ));
        assert!(matches!(
            BenchmarkSuiteStore::try_load_from_paths_with_coordinator(
                &paths,
                coordinator.clone(),
            ),
            Err(BenchmarkSuiteStoreError::Persistence(ref error))
                if error.kind() == io::ErrorKind::AlreadyExists
        ));
        selection(&store, "suite-still-open", None).await;

        store.close().await.expect("close retry succeeds");
        drop(store);
        let reloaded =
            BenchmarkSuiteStore::try_load_from_paths_with_coordinator(&paths, coordinator)
                .expect("owner released after successful close");
        reloaded.close().await.expect("close reloaded");
        cleanup(&root);
    }

    #[tokio::test]
    async fn persistence_owner_and_writer_are_unique_and_lazy() {
        let (root, paths, _, coordinator, store) = persistence_fixture("owner-lazy");
        assert_eq!(
            store
                .persistence
                .as_ref()
                .expect("persistence")
                .writer_count(),
            0
        );
        assert!(matches!(
            BenchmarkSuiteStore::try_load_from_paths_with_coordinator(
                &paths,
                coordinator.clone()
            ),
            Err(BenchmarkSuiteStoreError::Persistence(ref error))
                if error.kind() == io::ErrorKind::AlreadyExists
        ));
        let selected = selection(&store, "suite-writer", None).await;
        store
            .reserve(selected, "session-1", test_timestamp(), false)
            .await
            .expect("reservation opens writer");
        assert_eq!(
            store
                .persistence
                .as_ref()
                .expect("persistence")
                .writer_count(),
            1
        );
        store.close().await.expect("close store");
        cleanup(&root);
    }

    #[test]
    fn strict_loader_rejects_hostile_inputs_without_touching_bytes() {
        let root = test_root("hostile-loader");
        let paths = test_paths(&root);
        let dir = suite_dir(&paths);
        fs::create_dir_all(&dir).expect("create suite dir");
        let invalid_path = suite_path(&paths, &test_suite_id("suite-invalid"));
        let invalid_bytes = b"{not-json".to_vec();
        fs::write(&invalid_path, &invalid_bytes).expect("write invalid manifest");
        let noncanonical = valid_manifest("suite-copied", "session-copied");
        let noncanonical_path = dir.join("copied.json");
        fs::write(
            &noncanonical_path,
            serde_json::to_vec_pretty(&noncanonical).expect("encode copied"),
        )
        .expect("write copied manifest");
        let oversized_path = suite_path(&paths, &test_suite_id("suite-oversized"));
        fs::write(
            &oversized_path,
            vec![b'x'; usize::try_from(MAX_MANIFEST_BYTES + 1).expect("size")],
        )
        .expect("write oversized manifest");

        let load_state = load_persisted_suites(&dir);

        assert!(load_state.inner.suites.is_empty());
        assert!(
            load_state
                .inner
                .rejected_ids
                .contains(&test_suite_id("suite-invalid"))
        );
        assert!(
            load_state
                .inner
                .rejected_ids
                .contains(&test_suite_id("suite-copied"))
        );
        assert!(
            load_state
                .inner
                .rejected_ids
                .contains(&test_suite_id("suite-oversized"))
        );
        assert_eq!(
            fs::read(&invalid_path).expect("invalid bytes"),
            invalid_bytes
        );
        assert!(noncanonical_path.is_file());
        assert_eq!(
            fs::metadata(&oversized_path)
                .expect("oversized metadata")
                .len(),
            MAX_MANIFEST_BYTES + 1
        );
        cleanup(&root);
    }

    #[test]
    fn strict_loader_rejects_semantic_current_schema_ids_without_rewrite() {
        let root = test_root("semantic-identities");
        let paths = test_paths(&root);
        let dir = suite_dir(&paths);
        fs::create_dir_all(&dir).expect("create suite dir");

        let mut semantic_suite = valid_manifest("semantic-suite", "session-suite");
        semantic_suite.suite_id = "suite-release-validation-family-c".to_string();
        let semantic_suite_filename_id = test_suite_id("semantic-suite-filename");
        let semantic_suite_path = suite_path(&paths, &semantic_suite_filename_id);
        let semantic_suite_bytes =
            serde_json::to_vec_pretty(&semantic_suite).expect("encode semantic suite id");
        fs::write(&semantic_suite_path, &semantic_suite_bytes).expect("write semantic suite id");

        let mut semantic_run = valid_manifest("semantic-run", "session-run");
        semantic_run.runs[0].benchmark_id = "release-validation-family-c-baseline".to_string();
        let semantic_run_path = suite_path(&paths, &semantic_run.suite_id);
        let semantic_run_bytes =
            serde_json::to_vec_pretty(&semantic_run).expect("encode semantic benchmark id");
        fs::write(&semantic_run_path, &semantic_run_bytes).expect("write semantic benchmark id");

        let load_state = load_persisted_suites(&dir);

        assert!(load_state.inner.suites.is_empty());
        assert!(
            load_state
                .inner
                .rejected_ids
                .contains(&semantic_suite_filename_id)
        );
        assert_eq!(
            fs::read(&semantic_suite_path).expect("semantic suite bytes"),
            semantic_suite_bytes
        );
        assert_eq!(
            fs::read(&semantic_run_path).expect("semantic run bytes"),
            semantic_run_bytes
        );
        assert!(
            load_state
                .issues
                .iter()
                .any(|issue| issue.kind == BenchmarkSuiteLoadIssueKind::UnsafeSuiteId)
        );
        assert!(
            load_state
                .issues
                .iter()
                .any(|issue| issue.kind == BenchmarkSuiteLoadIssueKind::UnsafePublicField)
        );
        cleanup(&root);
    }

    #[tokio::test]
    async fn noncanonical_copy_reserves_filename_and_manifest_ids_without_rewrite() {
        let root = test_root("copied-identities");
        let paths = test_paths(&root);
        let dir = suite_dir(&paths);
        fs::create_dir_all(&dir).expect("create suite dir");
        let path = dir.join("copied.json");
        let bytes = serde_json::to_vec_pretty(&valid_manifest("suite-copied", "session-copied"))
            .expect("encode copied manifest");
        fs::write(&path, &bytes).expect("write copied manifest");
        let backend = Arc::new(RecordingFileBackend::new());
        let coordinator = PersistenceCoordinator::for_test(
            backend,
            Duration::from_millis(20),
            Duration::from_millis(100),
        );
        let store = BenchmarkSuiteStore::try_load_from_paths_with_coordinator(&paths, coordinator)
            .expect("load copied manifest");

        assert!(matches!(
            store.get("copied"),
            Err(BenchmarkSuiteStoreError::InvalidSuiteId)
        ));
        let manifest_id = test_suite_id("suite-copied");
        assert!(matches!(
            store.get(&manifest_id),
            Err(BenchmarkSuiteStoreError::RejectedManifest)
        ));
        assert!(matches!(
            store
                .select_reservation(&manifest_id, "instance", "development", &test_plan(), None,)
                .await,
            Err(BenchmarkSuiteStoreError::RejectedManifest)
        ));
        assert_eq!(fs::read(&path).expect("copied bytes"), bytes);
        assert_eq!(
            store
                .load_issues()
                .iter()
                .find(|issue| issue.kind == BenchmarkSuiteLoadIssueKind::NonCanonicalFilename)
                .map(|issue| issue.count),
            Some(1)
        );
        store.close().await.expect("close store");
        cleanup(&root);
    }

    #[test]
    fn parsed_invalid_copy_reserves_filename_and_manifest_ids_once() {
        let root = test_root("parsed-invalid-identities");
        let paths = test_paths(&root);
        let dir = suite_dir(&paths);
        fs::create_dir_all(&dir).expect("create suite dir");
        let path = dir.join("invalid-copy.json");
        let mut manifest = valid_manifest("suite-invalid-copy", "session-invalid-copy");
        manifest.updated_at = "2026-07-09T00:00:00.000Z".to_string();
        let bytes = serde_json::to_vec_pretty(&manifest).expect("encode invalid copy");
        fs::write(&path, &bytes).expect("write invalid copy");

        let load_state = load_persisted_suites(&dir);

        assert!(load_state.inner.suites.is_empty());
        assert!(
            load_state
                .inner
                .rejected_ids
                .contains(&test_suite_id("suite-invalid-copy"))
        );
        assert_eq!(
            load_state
                .issues
                .iter()
                .find(|issue| issue.kind == BenchmarkSuiteLoadIssueKind::IncoherentManifest)
                .map(|issue| issue.count),
            Some(1)
        );
        assert_eq!(
            load_state
                .issues
                .iter()
                .map(|issue| issue.count)
                .sum::<usize>(),
            1
        );
        assert_eq!(fs::read(&path).expect("invalid copy bytes"), bytes);
        cleanup(&root);
    }

    #[test]
    fn duplicate_alias_cohort_reserves_every_canonical_filename_id() {
        let root = test_root("duplicate-alias-identities");
        let paths = test_paths(&root);
        let dir = suite_dir(&paths);
        fs::create_dir_all(&dir).expect("create suite dir");
        for (filename_id, session_id) in [
            ("suite-shared", "session-primary"),
            ("suite-alias-one", "session-alias-one"),
            ("suite-alias-two", "session-alias-two"),
        ] {
            fs::write(
                suite_path_in_dir(&dir, &test_suite_id(filename_id)),
                serde_json::to_vec_pretty(&valid_manifest("suite-shared", session_id))
                    .expect("encode duplicate alias"),
            )
            .expect("write duplicate alias");
        }

        let load_state = load_persisted_suites(&dir);

        assert!(load_state.inner.suites.is_empty());
        for suite_id in ["suite-shared", "suite-alias-one", "suite-alias-two"] {
            assert!(
                load_state
                    .inner
                    .rejected_ids
                    .contains(&test_suite_id(suite_id))
            );
        }
        assert_eq!(
            load_state
                .issues
                .iter()
                .find(|issue| issue.kind == BenchmarkSuiteLoadIssueKind::NonCanonicalFilename)
                .map(|issue| issue.count),
            Some(2)
        );
        assert_eq!(
            load_state
                .issues
                .iter()
                .find(|issue| issue.kind == BenchmarkSuiteLoadIssueKind::DuplicateSuiteId)
                .map(|issue| issue.count),
            Some(2)
        );
        cleanup(&root);
    }

    #[test]
    fn strict_loader_rejects_all_manifests_with_ambiguous_session_id() {
        let root = test_root("ambiguous-session");
        let paths = test_paths(&root);
        let dir = suite_dir(&paths);
        fs::create_dir_all(&dir).expect("create suite dir");
        for suite_id in ["suite-first", "suite-second"] {
            let manifest = valid_manifest(suite_id, "shared-session");
            fs::write(
                suite_path(&paths, &test_suite_id(suite_id)),
                serde_json::to_vec_pretty(&manifest).expect("encode manifest"),
            )
            .expect("write manifest");
        }

        let load_state = load_persisted_suites(&dir);

        assert!(load_state.inner.suites.is_empty());
        assert!(
            load_state
                .inner
                .rejected_ids
                .contains(&test_suite_id("suite-first"))
        );
        assert!(
            load_state
                .inner
                .rejected_ids
                .contains(&test_suite_id("suite-second"))
        );
        assert_eq!(
            load_state
                .issues
                .iter()
                .find(|issue| issue.kind == BenchmarkSuiteLoadIssueKind::AmbiguousSessionId)
                .map(|issue| issue.count),
            Some(2)
        );
        cleanup(&root);
    }

    #[test]
    fn strict_loader_rejects_launched_timestamp_outside_manifest_window() {
        let root = test_root("launched-ordering");
        let paths = test_paths(&root);
        let dir = suite_dir(&paths);
        fs::create_dir_all(&dir).expect("create suite dir");
        let mut manifest = valid_manifest("suite-time-order", "session-time");
        manifest.runs[0].launched_at = Some("2026-07-11T00:00:00.000Z".to_string());
        let path = suite_path(&paths, &manifest.suite_id);
        fs::write(
            &path,
            serde_json::to_vec_pretty(&manifest).expect("encode manifest"),
        )
        .expect("write manifest");

        let load_state = load_persisted_suites(&dir);

        assert!(load_state.inner.suites.is_empty());
        assert!(
            load_state
                .inner
                .rejected_ids
                .contains(&test_suite_id("suite-time-order"))
        );
        assert!(path.is_file());
        cleanup(&root);
    }

    #[test]
    fn strict_loader_allows_launch_prepared_before_manifest_creation() {
        let root = test_root("prepared-before-created");
        let paths = test_paths(&root);
        let dir = suite_dir(&paths);
        fs::create_dir_all(&dir).expect("create suite dir");
        let mut manifest = valid_manifest("suite-prepared-first", "session-prepared-first");
        manifest.created_at = "2026-07-10T00:01:00.000Z".to_string();
        manifest.updated_at = "2026-07-10T00:01:00.000Z".to_string();
        manifest.runs[0].launched_at = Some("2026-07-10T00:00:00.000Z".to_string());
        fs::write(
            suite_path(&paths, &manifest.suite_id),
            serde_json::to_vec_pretty(&manifest).expect("encode manifest"),
        )
        .expect("write manifest");

        let load_state = load_persisted_suites(&dir);

        assert!(load_state.issues.is_empty());
        assert!(
            load_state
                .inner
                .suites
                .contains_key(&test_suite_id("suite-prepared-first"))
        );
        cleanup(&root);
    }

    #[tokio::test]
    async fn rejected_exact_id_fails_reads_and_mutations_without_rewrite() {
        let root = test_root("rejected-exact-id");
        let paths = test_paths(&root);
        let path = suite_path(&paths, &test_suite_id("suite-rejected"));
        fs::create_dir_all(path.parent().expect("suite parent")).expect("create suite dir");
        let bytes = b"{not-json".to_vec();
        fs::write(&path, &bytes).expect("write invalid manifest");
        let backend = Arc::new(RecordingFileBackend::new());
        let coordinator = PersistenceCoordinator::for_test(
            backend,
            Duration::from_millis(20),
            Duration::from_millis(100),
        );
        let store = BenchmarkSuiteStore::try_load_from_paths_with_coordinator(&paths, coordinator)
            .expect("load store");

        assert!(matches!(
            store.get(&test_suite_id("suite-rejected")),
            Err(BenchmarkSuiteStoreError::RejectedManifest)
        ));
        assert!(matches!(
            store
                .select_reservation(
                    &test_suite_id("suite-rejected"),
                    "instance",
                    "development",
                    &test_plan(),
                    None,
                )
                .await,
            Err(BenchmarkSuiteStoreError::RejectedManifest)
        ));
        assert_eq!(fs::read(&path).expect("rejected bytes"), bytes);
        store.close().await.expect("close store");
        cleanup(&root);
    }

    #[test]
    fn incomplete_directory_scan_latches_mutation_but_preserves_reads() {
        let root = test_root("directory-latch");
        let paths = test_paths(&root);
        let dir = suite_dir(&paths);
        fs::create_dir_all(dir.parent().expect("suite parent")).expect("create parent");
        fs::write(&dir, b"not-a-directory").expect("block directory scan");

        let load_state = load_persisted_suites(&dir);

        assert!(load_state.mutation_latched);
        assert!(load_state.inner.suites.is_empty());
        assert_eq!(
            load_state.issues,
            vec![BenchmarkSuiteLoadIssue {
                kind: BenchmarkSuiteLoadIssueKind::DirectoryUnreadable,
                count: 1,
            }]
        );
        cleanup(&root);
    }

    #[cfg(unix)]
    #[test]
    fn strict_loader_rejects_symlink_manifest_without_following_it() {
        use std::os::unix::fs::symlink;

        let root = test_root("symlink-loader");
        let paths = test_paths(&root);
        let dir = suite_dir(&paths);
        fs::create_dir_all(&dir).expect("create suite dir");
        let target = root.join("outside.json");
        let bytes = serde_json::to_vec_pretty(&valid_manifest("suite-link", "session-link"))
            .expect("encode target");
        fs::write(&target, &bytes).expect("write target");
        symlink(&target, suite_path(&paths, &test_suite_id("suite-link"))).expect("create symlink");

        let load_state = load_persisted_suites(&dir);

        assert!(load_state.inner.suites.is_empty());
        assert!(
            load_state
                .inner
                .rejected_ids
                .contains(&test_suite_id("suite-link"))
        );
        assert_eq!(fs::read(&target).expect("target bytes"), bytes);
        cleanup(&root);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_suite_directory_sets_global_mutation_latch() {
        use std::os::unix::fs::symlink;

        let root = test_root("symlink-directory");
        let paths = test_paths(&root);
        let target = root.join("suite-target");
        fs::create_dir_all(&target).expect("create target dir");
        fs::create_dir_all(suite_dir(&paths).parent().expect("benchmark parent"))
            .expect("create benchmark dir");
        symlink(&target, suite_dir(&paths)).expect("create suite directory symlink");

        let load_state = load_persisted_suites(&suite_dir(&paths));

        assert!(load_state.mutation_latched);
        assert!(load_state.inner.suites.is_empty());
        assert_eq!(
            load_state.issues,
            vec![BenchmarkSuiteLoadIssue {
                kind: BenchmarkSuiteLoadIssueKind::DirectoryUnreadable,
                count: 1,
            }]
        );
        cleanup(&root);
    }

    async fn selection(
        store: &BenchmarkSuiteStore,
        suite_id: &str,
        run_index: Option<usize>,
    ) -> BenchmarkSuiteSelection {
        let suite_id = test_suite_id(suite_id);
        store
            .select_reservation(
                &suite_id,
                "instance",
                "development",
                &test_plan(),
                run_index,
            )
            .await
            .expect("select reservation")
    }

    fn test_plan() -> Vec<BenchmarkSuiteRunInput> {
        vec![
            run_input(0, "vanilla_baseline", "coldish"),
            run_input(1, "managed_default", "repeat"),
        ]
    }

    fn run_input(run_index: usize, profile: &str, run_type: &str) -> BenchmarkSuiteRunInput {
        BenchmarkSuiteRunInput {
            run_index,
            profile: profile.to_string(),
            run_type: run_type.to_string(),
            target_id: None,
            benchmark_id: test_benchmark_id(&format!("run-{run_index}")),
        }
    }

    fn valid_manifest(suite_id: &str, session_id: &str) -> BenchmarkSuiteManifest {
        BenchmarkSuiteManifest {
            schema: BENCHMARK_SUITE_SCHEMA.to_string(),
            schema_version: BENCHMARK_SUITE_SCHEMA_VERSION,
            suite_id: test_suite_id(suite_id),
            instance_id: "instance".to_string(),
            mode: "development".to_string(),
            created_at: test_timestamp().to_string(),
            updated_at: test_timestamp().to_string(),
            runs: vec![BenchmarkSuiteManifestRun {
                run_index: 0,
                profile: "vanilla_baseline".to_string(),
                run_type: "coldish".to_string(),
                target_id: String::new(),
                benchmark_id: test_benchmark_id("manifest-run-0"),
                session_id: Some(session_id.to_string()),
                launched_at: Some(test_timestamp().to_string()),
                state: "launching".to_string(),
            }],
        }
    }

    fn test_suite_id(label: &str) -> String {
        normalize_suite_id(label).expect("test suite label is nonempty")
    }

    fn test_benchmark_id(label: &str) -> String {
        format!("benchmark-{:016x}", stable_hash(&[label]))
    }

    fn accepted_write_handle(
        error: BenchmarkSuiteReserveError,
    ) -> BenchmarkSuiteCompensationHandle {
        match error {
            BenchmarkSuiteReserveError::AcceptedWriteFailed { handle, .. } => handle,
            BenchmarkSuiteReserveError::PreAccept(error) => {
                panic!("expected accepted write failure, got {error}")
            }
        }
    }

    fn test_timestamp() -> &'static str {
        "2026-07-10T00:00:00.000Z"
    }

    fn test_root(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "axial-benchmark-suite-{name}-{}-{nanos}",
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
