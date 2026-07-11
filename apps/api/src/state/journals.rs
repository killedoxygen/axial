use super::contracts::{
    CommandKind, OperationId, OperationJournalEntry, OperationJournalStep, OperationOutcome,
    OperationPhase, OperationStatus, OperationStepResult, TargetDescriptor,
};
use super::ownership::{CurrentArtifact, classify_current_artifact};
#[cfg(test)]
use crate::execution::persistence::PersistenceCoordinator;
use crate::execution::persistence::{
    AcceptedWrite, AtomicSnapshotWriter, PersistenceOwnerLease, WriteUrgency,
};
use crate::observability::{
    RedactionAudience, evidence_text_looks_sensitive, sanitize_evidence_text,
};
use axial_config::AppPaths;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::sync::Mutex as AsyncMutex;
use tracing::warn;

pub const OPERATION_JOURNAL_SCHEMA: &str = "axial.state.operation_journals.v1";
pub const DEFAULT_OPERATION_JOURNAL_LIMIT: usize = 128;
const OPERATION_JOURNAL_FILE: &str = "operation-journals.json";
const OPERATION_JOURNAL_LOCK_INVARIANT: &str =
    "operation journal records lock poisoned; in-memory and persisted state may diverge";

#[derive(Debug, thiserror::Error)]
pub enum OperationJournalStoreError {
    #[error("invalid operation journal entry: {0:?}")]
    Validation(OperationJournalValidationError),
    #[error("invalid operation journal snapshot: {0:?}")]
    Snapshot(OperationJournalLoadError),
    #[error("operation journal record does not exist")]
    MissingOperation,
    #[error("operation journal is already terminal")]
    AlreadyTerminal,
    #[error("operation journal record already exists with different contents")]
    AlreadyExists,
    #[error("operation journal has a failed critical commit that must be retried")]
    RetryRequired,
    #[error("operation journal capacity is exhausted by active operations")]
    CapacityExhausted,
    #[error("operation journal persistence failed: {0}")]
    Persistence(#[source] io::Error),
}

impl OperationJournalStoreError {
    pub const fn class(&self) -> &'static str {
        match self {
            Self::Validation(_) => "validation",
            Self::Snapshot(_) => "snapshot",
            Self::MissingOperation => "missing_operation",
            Self::AlreadyTerminal => "already_terminal",
            Self::AlreadyExists => "already_exists",
            Self::RetryRequired => "retry_required",
            Self::CapacityExhausted => "capacity_exhausted",
            Self::Persistence(_) => "persistence",
        }
    }
}

impl From<OperationJournalValidationError> for OperationJournalStoreError {
    fn from(error: OperationJournalValidationError) -> Self {
        Self::Validation(error)
    }
}

impl From<OperationJournalLoadError> for OperationJournalStoreError {
    fn from(error: OperationJournalLoadError) -> Self {
        Self::Snapshot(error)
    }
}

#[derive(Debug)]
pub(crate) enum OperationJournalReconciliation {
    CommittedAfterPersistenceFailure(OperationJournalStoreError),
    RequestedTransitionAlreadyCommitted,
    RetryRequestedTransition,
}

pub(crate) fn operation_journal_plan_is_visible(
    entry: &OperationJournalEntry,
    expected: &OperationJournalEntry,
) -> bool {
    operation_journal_identity_and_plan_match(entry, expected)
        && entry.status == expected.status
        && entry.completed_steps == expected.completed_steps
        && entry.failure_point == expected.failure_point
        && entry.guardian_diagnosis_ids == expected.guardian_diagnosis_ids
        && entry.outcome == expected.outcome
}

fn operation_journal_identity_and_plan_match(
    entry: &OperationJournalEntry,
    expected: &OperationJournalEntry,
) -> bool {
    entry.journal_id == expected.journal_id
        && entry.operation_id == expected.operation_id
        && entry.command == expected.command
        && entry.owner == expected.owner
        && entry.ownership == expected.ownership
        && entry.targets == expected.targets
        && entry.planned_steps == expected.planned_steps
        && entry.rollback == expected.rollback
}

pub(crate) fn operation_journal_completed_step_is_visible(
    entry: &OperationJournalEntry,
    expected: &OperationJournalStep,
) -> bool {
    entry.completed_steps.iter().any(|step| {
        step.step_id == expected.step_id
            && step.phase == expected.phase
            && step.result == expected.result
            && step.changed_target == expected.changed_target
            && step.rollback == expected.rollback
            && expected
                .generated_facts
                .iter()
                .all(|fact| step.generated_facts.contains(fact))
    })
}

pub(crate) fn operation_journal_terminal_is_visible(
    entry: &OperationJournalEntry,
    expected: &OperationJournalEntry,
) -> bool {
    operation_journal_identity_and_plan_match(entry, expected)
        && entry.status == expected.status
        && entry.failure_point == expected.failure_point
        && expected
            .guardian_diagnosis_ids
            .iter()
            .all(|diagnosis_id| entry.guardian_diagnosis_ids.contains(diagnosis_id))
        && entry.outcome == expected.outcome
        && expected
            .completed_steps
            .iter()
            .all(|step| operation_journal_completed_step_is_visible(entry, step))
}

struct OperationJournalPersistence {
    owner: PersistenceOwnerLease,
    writer: AtomicSnapshotWriter,
}

impl OperationJournalPersistence {
    fn claim(storage_path: &Path) -> Result<Self, OperationJournalStoreError> {
        let owner = PersistenceOwnerLease::claim(storage_path)
            .map_err(|error| OperationJournalStoreError::Persistence(error.into()))?;
        Self::writer_for_owner(storage_path, owner)
    }

    #[cfg(test)]
    fn claim_with_coordinator(
        storage_path: &Path,
        coordinator: PersistenceCoordinator,
    ) -> Result<Self, OperationJournalStoreError> {
        let owner = coordinator
            .claim_owner(storage_path)
            .map_err(|error| OperationJournalStoreError::Persistence(error.into()))?;
        Self::writer_for_owner(storage_path, owner)
    }

    fn writer_for_owner(
        storage_path: &Path,
        owner: PersistenceOwnerLease,
    ) -> Result<Self, OperationJournalStoreError> {
        let writer = owner
            .writer(storage_path, operation_journal_target())
            .map_err(|error| OperationJournalStoreError::Persistence(error.into()))?;
        Ok(Self { owner, writer })
    }
}

pub struct OperationJournalStore {
    records: Arc<RwLock<OperationJournalRecords>>,
    mutation_gate: Arc<AsyncMutex<()>>,
    max_entries: usize,
    persistence: Option<OperationJournalPersistence>,
}

#[derive(Default)]
struct OperationJournalRecords {
    visible: BTreeMap<String, OperationJournalEntry>,
    visible_revision: u64,
    retry_candidate: Option<(u64, BTreeMap<String, OperationJournalEntry>)>,
}

struct PendingJournalCommit {
    ticket: AcceptedWrite,
    revision: u64,
    candidate: BTreeMap<String, OperationJournalEntry>,
}

impl OperationJournalStore {
    pub fn new() -> Self {
        Self::with_max_entries(DEFAULT_OPERATION_JOURNAL_LIMIT)
    }

    pub fn with_max_entries(max_entries: usize) -> Self {
        Self {
            records: Arc::new(RwLock::new(OperationJournalRecords::default())),
            mutation_gate: Arc::new(AsyncMutex::new(())),
            max_entries: max_entries.clamp(1, DEFAULT_OPERATION_JOURNAL_LIMIT),
            persistence: None,
        }
    }

    pub fn load_from_paths(paths: &AppPaths) -> Self {
        Self::try_load_from_paths(paths).unwrap_or_else(|error| {
            panic!("failed to initialize operation journal persistence: {error}")
        })
    }

    pub fn try_load_from_paths(paths: &AppPaths) -> Result<Self, OperationJournalStoreError> {
        let storage_path = operation_journal_path(paths);
        let store = Self::with_max_entries_and_persistence(
            DEFAULT_OPERATION_JOURNAL_LIMIT,
            Some(OperationJournalPersistence::claim(&storage_path)?),
        );

        store.load_from_path(&storage_path);
        Ok(store)
    }

    #[cfg(test)]
    pub(crate) fn try_load_from_paths_with_coordinator(
        paths: &AppPaths,
        coordinator: PersistenceCoordinator,
    ) -> Result<Self, OperationJournalStoreError> {
        let storage_path = operation_journal_path(paths);
        let store = Self::with_max_entries_and_persistence(
            DEFAULT_OPERATION_JOURNAL_LIMIT,
            Some(OperationJournalPersistence::claim_with_coordinator(
                &storage_path,
                coordinator,
            )?),
        );
        store.load_from_path(&storage_path);
        Ok(store)
    }

    fn load_from_path(&self, storage_path: &Path) {
        match fs::read_to_string(storage_path) {
            Ok(data) => match OperationJournalSnapshot::from_json(&data) {
                Ok(snapshot) => {
                    if let Err(error) = self.load_snapshot(snapshot) {
                        warn!(
                            error = ?error,
                            "failed to load persisted operation journal snapshot"
                        );
                    }
                }
                Err(error) => {
                    warn!(
                        error = ?error,
                        "failed to parse persisted operation journal snapshot"
                    );
                }
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                warn!(
                    error = %error,
                    "failed to read persisted operation journal snapshot"
                );
            }
        }
    }

    fn with_max_entries_and_persistence(
        max_entries: usize,
        persistence: Option<OperationJournalPersistence>,
    ) -> Self {
        Self {
            records: Arc::new(RwLock::new(OperationJournalRecords::default())),
            mutation_gate: Arc::new(AsyncMutex::new(())),
            max_entries: max_entries.clamp(1, DEFAULT_OPERATION_JOURNAL_LIMIT),
            persistence,
        }
    }

    pub async fn create(
        &self,
        entry: OperationJournalEntry,
    ) -> Result<(), OperationJournalStoreError> {
        let mutation = self.mutation_gate.clone().lock_owned().await;
        validate_entry(&entry)?;
        let ticket = {
            let mut records = self
                .records
                .write()
                .expect(OPERATION_JOURNAL_LOCK_INVARIANT);
            if records.retry_candidate.is_some() {
                return Err(OperationJournalStoreError::RetryRequired);
            }
            if let Some(existing) = records.visible.get(entry.operation_id.as_str()) {
                if existing == &entry {
                    return Ok(());
                }
                return Err(OperationJournalStoreError::AlreadyExists);
            }
            let operation_key = entry.operation_id.as_str().to_string();
            let mut candidate = records.visible.clone();
            candidate.insert(operation_key.clone(), entry);
            if !prune_records(&mut candidate, self.max_entries, Some(&operation_key)) {
                return Err(OperationJournalStoreError::CapacityExhausted);
            }
            self.accept_candidate(&mut records, candidate, WriteUrgency::Immediate)?
        };
        self.await_commit(ticket, mutation).await
    }

    pub fn get(&self, operation_id: &OperationId) -> Option<OperationJournalEntry> {
        self.records
            .read()
            .expect(OPERATION_JOURNAL_LOCK_INVARIANT)
            .visible
            .get(operation_id.as_str())
            .cloned()
    }

    pub fn latest_for_command(&self, command: CommandKind) -> Option<OperationJournalEntry> {
        self.records
            .read()
            .expect(OPERATION_JOURNAL_LOCK_INVARIANT)
            .visible
            .values()
            .filter(|entry| entry.command == command)
            .max_by(|left, right| left.operation_id.as_str().cmp(right.operation_id.as_str()))
            .cloned()
    }

    pub async fn record_success(
        &self,
        operation_id: &OperationId,
        completed_step: OperationJournalStep,
        outcome: OperationOutcome,
    ) -> Result<(), OperationJournalStoreError> {
        let mutation = self.mutation_gate.clone().lock_owned().await;
        let ticket = self.update(operation_id, WriteUrgency::Immediate, |entry| {
            if operation_journal_status_is_terminal(entry.status) {
                return Err(OperationJournalStoreError::AlreadyTerminal);
            }
            entry.status = OperationStatus::Succeeded;
            entry.completed_steps.push(completed_step);
            entry.failure_point = None;
            entry.outcome = Some(outcome);
            Ok(())
        })?;
        self.await_commit(ticket, mutation).await
    }

    pub async fn record_failure(
        &self,
        operation_id: &OperationId,
        failure_step: OperationJournalStep,
        failure_point: impl Into<String>,
        outcome: OperationOutcome,
    ) -> Result<(), OperationJournalStoreError> {
        let mutation = self.mutation_gate.clone().lock_owned().await;
        let ticket = self.update(operation_id, WriteUrgency::Immediate, |entry| {
            if operation_journal_status_is_terminal(entry.status) {
                return Err(OperationJournalStoreError::AlreadyTerminal);
            }
            entry.status = OperationStatus::Failed;
            entry.completed_steps.push(failure_step);
            entry.failure_point = Some(failure_point.into());
            entry.outcome = Some(outcome);
            Ok(())
        })?;
        self.await_commit(ticket, mutation).await
    }

    pub async fn record_failure_with_guardian_evidence(
        &self,
        operation_id: &OperationId,
        failure_step: OperationJournalStep,
        failure_point: impl Into<String>,
        outcome: OperationOutcome,
        fact_ids: Vec<String>,
        diagnosis_ids: Vec<String>,
    ) -> Result<(), OperationJournalStoreError> {
        let mutation = self.mutation_gate.clone().lock_owned().await;
        let ticket = self.update(operation_id, WriteUrgency::Immediate, |entry| {
            if operation_journal_status_is_terminal(entry.status) {
                return Err(OperationJournalStoreError::AlreadyTerminal);
            }
            entry.status = OperationStatus::Failed;
            entry.completed_steps.push(failure_step);
            entry.failure_point = Some(failure_point.into());
            entry.outcome = Some(outcome);
            apply_guardian_evidence(entry, fact_ids, diagnosis_ids);
            Ok(())
        })?;
        self.await_commit(ticket, mutation).await
    }

    pub async fn record_progress(
        &self,
        operation_id: &OperationId,
        progress_step: OperationJournalStep,
    ) -> Result<(), OperationJournalStoreError> {
        let _mutation = self.mutation_gate.lock().await;
        self.update(operation_id, WriteUrgency::Debounced, |entry| {
            if operation_journal_status_is_terminal(entry.status) {
                return Err(OperationJournalStoreError::AlreadyTerminal);
            }
            entry.status = OperationStatus::Running;
            entry.completed_steps.push(progress_step);
            Ok(())
        })?;
        Ok(())
    }

    pub async fn record_checkpoint(
        &self,
        operation_id: &OperationId,
        checkpoint: OperationJournalStep,
    ) -> Result<(), OperationJournalStoreError> {
        let mutation = self.mutation_gate.clone().lock_owned().await;
        let ticket = self.update(operation_id, WriteUrgency::Immediate, |entry| {
            if operation_journal_status_is_terminal(entry.status) {
                return Err(OperationJournalStoreError::AlreadyTerminal);
            }
            entry.status = OperationStatus::Running;
            entry.completed_steps.push(checkpoint);
            Ok(())
        })?;
        self.await_commit(ticket, mutation).await
    }

    pub async fn record_guardian_evidence(
        &self,
        operation_id: &OperationId,
        fact_ids: Vec<String>,
        diagnosis_ids: Vec<String>,
    ) -> Result<(), OperationJournalStoreError> {
        let mutation = self.mutation_gate.clone().lock_owned().await;
        let ticket = self.update(operation_id, WriteUrgency::Immediate, |entry| {
            apply_guardian_evidence(entry, fact_ids, diagnosis_ids);
            Ok(())
        })?;
        self.await_commit(ticket, mutation).await
    }

    fn update(
        &self,
        operation_id: &OperationId,
        urgency: WriteUrgency,
        update: impl FnOnce(&mut OperationJournalEntry) -> Result<(), OperationJournalStoreError>,
    ) -> Result<Option<PendingJournalCommit>, OperationJournalStoreError> {
        let mut records = self
            .records
            .write()
            .expect(OPERATION_JOURNAL_LOCK_INVARIANT);
        if records.retry_candidate.is_some() {
            return Err(OperationJournalStoreError::RetryRequired);
        }
        let mut candidate = records.visible.clone();
        let entry = candidate
            .get_mut(operation_id.as_str())
            .ok_or(OperationJournalStoreError::MissingOperation)?;
        update(entry)?;
        validate_entry(entry)?;
        if !prune_records(&mut candidate, self.max_entries, None) {
            return Err(OperationJournalStoreError::CapacityExhausted);
        }
        self.accept_candidate(&mut records, candidate, urgency)
    }

    fn accept_candidate(
        &self,
        records: &mut OperationJournalRecords,
        candidate: BTreeMap<String, OperationJournalEntry>,
        urgency: WriteUrgency,
    ) -> Result<Option<PendingJournalCommit>, OperationJournalStoreError> {
        let snapshot = OperationJournalSnapshot::new(candidate.values().cloned().collect())?;
        let ticket = self
            .persistence
            .as_ref()
            .map(|persistence| {
                persistence
                    .writer
                    .accept(snapshot, urgency, encode_snapshot)
                    .map_err(|error| OperationJournalStoreError::Persistence(error.into()))
            })
            .transpose()?;
        records.retry_candidate = None;
        let Some(ticket) = ticket else {
            records.visible = candidate;
            return Ok(None);
        };
        let revision = ticket.revision().get();
        if urgency == WriteUrgency::Debounced {
            records.visible = candidate;
            records.visible_revision = revision;
            return Ok(None);
        }
        Ok(Some(PendingJournalCommit {
            ticket,
            revision,
            candidate,
        }))
    }

    pub fn list(&self) -> Vec<OperationJournalEntry> {
        self.records
            .read()
            .expect(OPERATION_JOURNAL_LOCK_INVARIANT)
            .visible
            .values()
            .cloned()
            .collect()
    }

    pub(crate) fn has_retry_candidate(&self) -> bool {
        self.records
            .read()
            .expect(OPERATION_JOURNAL_LOCK_INVARIANT)
            .retry_candidate
            .is_some()
    }

    pub fn snapshot(&self) -> Result<OperationJournalSnapshot, OperationJournalLoadError> {
        OperationJournalSnapshot::new(self.list())
    }

    pub fn load_snapshot(
        &self,
        snapshot: OperationJournalSnapshot,
    ) -> Result<(), OperationJournalLoadError> {
        snapshot.validate()?;
        let mut candidate = BTreeMap::new();
        for entry in snapshot.entries {
            candidate.insert(entry.operation_id.as_str().to_string(), entry);
        }
        if !prune_records(&mut candidate, self.max_entries, None) {
            return Err(OperationJournalLoadError::TooManyEntries);
        }
        let mut records = self
            .records
            .write()
            .expect(OPERATION_JOURNAL_LOCK_INVARIANT);
        records.visible = candidate;
        records.visible_revision = 0;
        records.retry_candidate = None;
        Ok(())
    }

    pub async fn flush(&self) -> Result<(), OperationJournalStoreError> {
        let _mutation = self.mutation_gate.lock().await;
        if let Some(persistence) = &self.persistence {
            persistence
                .owner
                .flush()
                .await
                .map_err(|error| OperationJournalStoreError::Persistence(error.into()))?;
        }
        Ok(())
    }

    pub async fn retry(&self) -> Result<(), OperationJournalStoreError> {
        let mutation = self.mutation_gate.clone().lock_owned().await;
        let mutation = self.retry_holding_gate(mutation).await?;
        drop(mutation);
        Ok(())
    }

    pub(crate) async fn reconcile_transition(
        &self,
        operation_id: &OperationId,
        mut error: OperationJournalStoreError,
        retry_initial_delay: Duration,
        retry_max_delay: Duration,
        expected: impl Fn(&OperationJournalEntry) -> bool,
    ) -> Result<OperationJournalReconciliation, OperationJournalStoreError> {
        let retry_requested = match &error {
            OperationJournalStoreError::Persistence(_) => false,
            OperationJournalStoreError::RetryRequired => true,
            _ => return Err(error),
        };
        let mut delay = retry_initial_delay;
        while self.has_retry_candidate() {
            match self.retry().await {
                Ok(()) => break,
                Err(next_error) => {
                    error = next_error;
                    if !self.has_retry_candidate() {
                        break;
                    }
                    warn!(
                        error_class = error.class(),
                        "operation journal transition reconciliation failed"
                    );
                    tokio::time::sleep(delay).await;
                    delay = delay.saturating_mul(2).min(retry_max_delay);
                }
            }
        }

        if self.get(operation_id).as_ref().is_some_and(expected) {
            return Ok(if retry_requested {
                OperationJournalReconciliation::RequestedTransitionAlreadyCommitted
            } else {
                OperationJournalReconciliation::CommittedAfterPersistenceFailure(error)
            });
        }
        if retry_requested {
            return Ok(OperationJournalReconciliation::RetryRequestedTransition);
        }
        Err(error)
    }

    pub async fn close(&self) -> Result<(), OperationJournalStoreError> {
        let mut mutation = self.mutation_gate.clone().lock_owned().await;
        if self.has_retry_candidate() {
            mutation = self.retry_holding_gate(mutation).await?;
        }
        if let Some(persistence) = &self.persistence {
            persistence
                .writer
                .settle()
                .await
                .map_err(|error| OperationJournalStoreError::Persistence(error.into()))?;
            persistence
                .owner
                .close()
                .await
                .map_err(|error| OperationJournalStoreError::Persistence(error.into()))?;
        }
        drop(mutation);
        Ok(())
    }

    async fn retry_holding_gate(
        &self,
        mutation: tokio::sync::OwnedMutexGuard<()>,
    ) -> Result<tokio::sync::OwnedMutexGuard<()>, OperationJournalStoreError> {
        let Some(persistence) = &self.persistence else {
            return Ok(mutation);
        };
        let ticket = persistence
            .writer
            .retry()
            .map_err(|error| OperationJournalStoreError::Persistence(error.into()))?;
        let revision = ticket.revision().get();
        let candidate = {
            let records = self.records.read().expect(OPERATION_JOURNAL_LOCK_INVARIANT);
            records
                .retry_candidate
                .as_ref()
                .filter(|(candidate_revision, _)| *candidate_revision == revision)
                .map(|(_, candidate)| candidate.clone())
                .unwrap_or_else(|| records.visible.clone())
        };
        self.await_commit_holding_gate(
            Some(PendingJournalCommit {
                ticket,
                revision,
                candidate,
            }),
            mutation,
        )
        .await
    }

    async fn await_commit(
        &self,
        commit: Option<PendingJournalCommit>,
        mutation: tokio::sync::OwnedMutexGuard<()>,
    ) -> Result<(), OperationJournalStoreError> {
        let mutation = self.await_commit_holding_gate(commit, mutation).await?;
        drop(mutation);
        Ok(())
    }

    async fn await_commit_holding_gate(
        &self,
        commit: Option<PendingJournalCommit>,
        mutation: tokio::sync::OwnedMutexGuard<()>,
    ) -> Result<tokio::sync::OwnedMutexGuard<()>, OperationJournalStoreError> {
        let Some(commit) = commit else {
            return Ok(mutation);
        };
        let records = self.records.clone();
        let (completed_tx, completed_rx) = tokio::sync::oneshot::channel();
        commit.ticket.observe(move |result| {
            let result = match result {
                Ok(_) => {
                    let mut records = records.write().expect(OPERATION_JOURNAL_LOCK_INVARIANT);
                    if records.visible_revision < commit.revision {
                        records.visible = commit.candidate;
                        records.visible_revision = commit.revision;
                    }
                    records.retry_candidate = None;
                    Ok(())
                }
                Err(error) => {
                    records
                        .write()
                        .expect(OPERATION_JOURNAL_LOCK_INVARIANT)
                        .retry_candidate = Some((commit.revision, commit.candidate));
                    Err(error)
                }
            };
            let _ = completed_tx.send((result, mutation));
        });
        let (result, mutation) = completed_rx.await.map_err(|_| {
            OperationJournalStoreError::Persistence(io::Error::other(
                "operation journal commit observer stopped",
            ))
        })?;
        result.map_err(|error| OperationJournalStoreError::Persistence(error.into()))?;
        Ok(mutation)
    }
}

fn apply_guardian_evidence(
    entry: &mut OperationJournalEntry,
    fact_ids: Vec<String>,
    diagnosis_ids: Vec<String>,
) {
    if !fact_ids.is_empty() && entry.completed_steps.is_empty() {
        let mut step = OperationJournalStep::new("guardian_evidence", OperationPhase::Running);
        step.result = OperationStepResult::Completed;
        entry.completed_steps.push(step);
    }
    if let Some(step) = entry.completed_steps.last_mut() {
        for fact_id in fact_ids {
            if !step.generated_facts.contains(&fact_id) {
                step.generated_facts.push(fact_id);
            }
        }
    }
    for diagnosis_id in diagnosis_ids {
        if !entry.guardian_diagnosis_ids.contains(&diagnosis_id) {
            entry.guardian_diagnosis_ids.push(diagnosis_id);
        }
    }
}

impl Default for OperationJournalStore {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperationJournalSnapshot {
    pub schema: String,
    pub entries: Vec<OperationJournalEntry>,
}

impl OperationJournalSnapshot {
    pub fn new(entries: Vec<OperationJournalEntry>) -> Result<Self, OperationJournalLoadError> {
        let snapshot = Self {
            schema: OPERATION_JOURNAL_SCHEMA.to_string(),
            entries,
        };
        snapshot.validate()?;
        Ok(snapshot)
    }

    pub fn from_json(value: &str) -> Result<Self, OperationJournalLoadError> {
        let snapshot = serde_json::from_str::<Self>(value)?;
        snapshot.validate()?;
        Ok(snapshot)
    }

    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }

    fn validate(&self) -> Result<(), OperationJournalLoadError> {
        if self.schema != OPERATION_JOURNAL_SCHEMA {
            return Err(OperationJournalLoadError::InvalidSchema);
        }
        if self.entries.len() > DEFAULT_OPERATION_JOURNAL_LIMIT {
            return Err(OperationJournalLoadError::TooManyEntries);
        }
        for entry in &self.entries {
            validate_entry(entry)?;
        }
        Ok(())
    }
}

#[derive(Debug)]
pub enum OperationJournalLoadError {
    Json(serde_json::Error),
    InvalidSchema,
    TooManyEntries,
    InvalidEntry(OperationJournalValidationError),
}

impl From<serde_json::Error> for OperationJournalLoadError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

impl From<OperationJournalValidationError> for OperationJournalLoadError {
    fn from(error: OperationJournalValidationError) -> Self {
        Self::InvalidEntry(error)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OperationJournalValidationError {
    UnsafeJournalId,
    UnsafeOperationId,
    UnsafeTargetId,
    UnsafeStepId,
    UnsafeGeneratedFact,
    UnsafeFailurePoint,
    UnsafeDiagnosisId,
    EmptyJournal,
    TooManyTargets,
    TooManyPlannedSteps,
    TooManyCompletedSteps,
    TooManyFacts,
    TooManyDiagnoses,
}

fn validate_entry(entry: &OperationJournalEntry) -> Result<(), OperationJournalValidationError> {
    if !safe_token(entry.journal_id.as_str(), 128) {
        return Err(OperationJournalValidationError::UnsafeJournalId);
    }
    if !safe_token(entry.operation_id.as_str(), 128) {
        return Err(OperationJournalValidationError::UnsafeOperationId);
    }
    if entry.targets.len() > 16 {
        return Err(OperationJournalValidationError::TooManyTargets);
    }
    for target in &entry.targets {
        validate_target(target)?;
    }
    if entry.planned_steps.len() > 128 {
        return Err(OperationJournalValidationError::TooManyPlannedSteps);
    }
    for step in &entry.planned_steps {
        validate_step(step)?;
    }
    if entry.completed_steps.len() > 256 {
        return Err(OperationJournalValidationError::TooManyCompletedSteps);
    }
    for step in &entry.completed_steps {
        validate_step(step)?;
    }
    if let Some(failure_point) = &entry.failure_point
        && !safe_token(failure_point, 96)
    {
        return Err(OperationJournalValidationError::UnsafeFailurePoint);
    }
    if entry.guardian_diagnosis_ids.len() > 32 {
        return Err(OperationJournalValidationError::TooManyDiagnoses);
    }
    for diagnosis_id in &entry.guardian_diagnosis_ids {
        if !safe_token(diagnosis_id, 96) {
            return Err(OperationJournalValidationError::UnsafeDiagnosisId);
        }
    }
    Ok(())
}

fn operation_journal_status_is_terminal(status: OperationStatus) -> bool {
    matches!(
        status,
        OperationStatus::Succeeded
            | OperationStatus::Failed
            | OperationStatus::Blocked
            | OperationStatus::Cancelled
    )
}

fn validate_target(target: &TargetDescriptor) -> Result<(), OperationJournalValidationError> {
    if !safe_token(&target.id, 96) {
        return Err(OperationJournalValidationError::UnsafeTargetId);
    }
    Ok(())
}

fn validate_step(step: &OperationJournalStep) -> Result<(), OperationJournalValidationError> {
    if !safe_token(&step.step_id, 96) {
        return Err(OperationJournalValidationError::UnsafeStepId);
    }
    if let Some(target) = &step.changed_target {
        validate_target(target)?;
    }
    if step.generated_facts.len() > 64 {
        return Err(OperationJournalValidationError::TooManyFacts);
    }
    for fact in &step.generated_facts {
        if !safe_public_fragment(fact, 320) {
            return Err(OperationJournalValidationError::UnsafeGeneratedFact);
        }
    }
    Ok(())
}

fn safe_token(value: &str, max_chars: usize) -> bool {
    let value = value.trim();
    !value.is_empty()
        && !value.chars().any(char::is_control)
        && value.chars().count() <= max_chars
        && value.chars().all(|value| {
            value.is_ascii_alphanumeric() || matches!(value, '-' | '_' | '.' | '+' | ':')
        })
        && !structured_token_looks_sensitive(value)
}

fn safe_public_fragment(value: &str, max_chars: usize) -> bool {
    let value = value.trim();
    !value.is_empty()
        && !value.chars().any(char::is_control)
        && !evidence_text_looks_sensitive(value)
        && sanitize_evidence_text(value, RedactionAudience::UserVisible, max_chars).is_some()
}

fn structured_token_looks_sensitive(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    if value.contains('/') || value.contains('\\') || contains_windows_drive_path(value) {
        return true;
    }
    if lower.contains(".jar")
        || lower.contains(".exe")
        || lower.contains(".dll")
        || lower.contains(".dylib")
        || lower.contains(".so")
        || lower.contains("-xmx")
        || lower.contains("-xms")
        || lower.contains("-xx:")
        || lower.starts_with("-d")
        || lower.contains("--")
        || lower.contains("token")
        || lower.contains("secret")
        || lower.contains("password")
        || lower.contains("provider_payload")
        || lower.contains("account_id")
        || lower.contains("username=")
        || lower.contains("xuid=")
        || lower.contains("authorization")
        || lower.contains("credential")
        || lower.contains("bearer")
    {
        return true;
    }
    if value.contains('@') && value.contains('.') {
        return true;
    }
    looks_like_jwt_token(value) || has_long_secret_like_segment(value)
}

fn contains_windows_drive_path(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.windows(3).any(|window| {
        window[0].is_ascii_alphabetic() && window[1] == b':' && matches!(window[2], b'\\' | b'/')
    })
}

fn looks_like_jwt_token(value: &str) -> bool {
    let parts = value.split('.').collect::<Vec<_>>();
    parts.len() >= 3
        && parts.iter().take(3).all(|part| {
            part.len() >= 12
                && part
                    .chars()
                    .all(|value| value.is_ascii_alphanumeric() || matches!(value, '-' | '_'))
        })
}

fn has_long_secret_like_segment(value: &str) -> bool {
    value
        .split(|value: char| !value.is_ascii_alphanumeric())
        .any(|part| {
            part.len() >= 48
                && part.chars().any(|value| value.is_ascii_alphabetic())
                && part.chars().any(|value| value.is_ascii_digit())
        })
}

fn prune_records(
    records: &mut BTreeMap<String, OperationJournalEntry>,
    max_entries: usize,
    protected_key: Option<&str>,
) -> bool {
    while records.len() > max_entries {
        let Some(key) = records.iter().find_map(|(key, entry)| {
            (protected_key != Some(key.as_str())
                && operation_journal_status_is_terminal(entry.status))
            .then(|| key.clone())
        }) else {
            return false;
        };
        records.remove(&key);
    }
    true
}

pub fn operation_journal_path(paths: &AppPaths) -> PathBuf {
    paths.config_dir.join("state").join(OPERATION_JOURNAL_FILE)
}

fn operation_journal_target() -> TargetDescriptor {
    classify_current_artifact(
        CurrentArtifact::OperationJournalSnapshot,
        "operation_journal",
    )
    .target
}

fn encode_snapshot(snapshot: OperationJournalSnapshot) -> io::Result<Vec<u8>> {
    snapshot
        .to_json()
        .map(String::into_bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

#[cfg(test)]
mod tests {
    use super::{
        OPERATION_JOURNAL_LOCK_INVARIANT, OperationJournalReconciliation, OperationJournalSnapshot,
        OperationJournalStore, OperationJournalStoreError, operation_journal_path,
        operation_journal_plan_is_visible,
    };
    use crate::execution::file::{FileWriteRequest, write_file_atomically};
    use crate::execution::persistence::{AtomicWriteBackend, PersistenceCoordinator};
    use crate::state::contracts::{
        CommandKind, JournalId, OperationId, OperationJournalEntry, OperationJournalStep,
        OperationOutcome, OperationPhase, OperationStatus, OwnershipClass, RollbackState,
        StabilizationSystem, TargetDescriptor, TargetKind,
    };
    use axial_config::AppPaths;
    use std::fs;
    use std::io;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::Duration;
    use std::time::{SystemTime, UNIX_EPOCH};
    use tokio::sync::Notify;

    const OPERATION_JOURNALS_V1_FIXTURE: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/guardian/operation-journals-v1.json"
    ));

    struct RecordingFileBackend {
        attempts: AtomicUsize,
        failures: AtomicUsize,
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
                started: Notify::new(),
                gate: Mutex::new(None),
            }
        }

        fn fail_next(&self) {
            self.failures.fetch_add(1, Ordering::SeqCst);
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

        fn gate_next(&self) -> WriteGateHandle {
            let gate = Arc::new(WriteGate {
                released: Mutex::new(false),
                changed: Condvar::new(),
            });
            *self.gate.lock().expect("backend gate lock") = Some(gate.clone());
            WriteGateHandle(gate)
        }
    }

    impl WriteGate {
        fn release(&self) {
            *self.released.lock().expect("write gate lock") = true;
            self.changed.notify_all();
        }

        fn wait(&self) {
            let mut released = self.released.lock().expect("write gate lock");
            while !*released {
                released = self.changed.wait(released).expect("wait on write gate");
            }
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
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |failures| {
                    (failures > 0).then(|| failures - 1)
                })
                .is_ok()
            {
                return Err(io::Error::other("injected operation-journal write failure"));
            }
            write_file_atomically(FileWriteRequest::new(target.clone(), destination, contents))
                .map(|_| ())
                .map_err(io::Error::from)
        }
    }

    fn persistence_fixture(
        name: &str,
    ) -> (
        PathBuf,
        AppPaths,
        Arc<RecordingFileBackend>,
        PersistenceCoordinator,
        OperationJournalStore,
    ) {
        let root = test_root(name);
        let paths = test_paths(&root);
        let backend = Arc::new(RecordingFileBackend::new());
        let coordinator = PersistenceCoordinator::for_test(
            backend.clone(),
            Duration::from_millis(20),
            Duration::from_millis(100),
        );
        let store = OperationJournalStore::try_load_from_paths_with_coordinator(
            &paths,
            coordinator.clone(),
        )
        .expect("claim operation journal persistence");
        (root, paths, backend, coordinator, store)
    }

    #[tokio::test]
    async fn journal_store_creates_updates_and_reads_records() {
        let store = OperationJournalStore::new();
        let operation_id = OperationId::new("operation-1");
        let mut entry = OperationJournalEntry::new(
            JournalId::new("journal-1"),
            operation_id.clone(),
            CommandKind::RefreshPerformanceRules,
            StabilizationSystem::Application,
            OwnershipClass::LauncherManaged,
            RollbackState::NotApplicable,
        );
        entry.planned_steps.push(OperationJournalStep::new(
            "refresh_remote_rules",
            OperationPhase::Running,
        ));

        store.create(entry).await.expect("create journal");

        let mut completed =
            OperationJournalStep::new("refresh_remote_rules", OperationPhase::Running);
        completed.result = crate::state::contracts::OperationStepResult::Completed;
        let mut progress =
            OperationJournalStep::new("refresh_remote_rules_progress", OperationPhase::Running);
        progress.result = crate::state::contracts::OperationStepResult::Completed;
        store
            .record_progress(&operation_id, progress)
            .await
            .expect("record progress");
        store
            .record_success(&operation_id, completed, OperationOutcome::Succeeded)
            .await
            .expect("record success");

        let stored = store.get(&operation_id).expect("journal record");
        assert_eq!(stored.status, OperationStatus::Succeeded);
        assert_eq!(stored.completed_steps.len(), 2);
        assert_eq!(
            stored.completed_steps[0].step_id,
            "refresh_remote_rules_progress"
        );
        assert_eq!(stored.outcome, Some(OperationOutcome::Succeeded));
        assert_eq!(
            store
                .latest_for_command(CommandKind::RefreshPerformanceRules)
                .expect("latest journal")
                .operation_id,
            operation_id
        );
    }

    #[tokio::test]
    async fn terminal_journal_outcome_is_immutable_after_success() {
        let store = OperationJournalStore::new();
        let operation_id = OperationId::new("operation-terminal-success");
        store
            .create(OperationJournalEntry::new(
                JournalId::new("journal-operation-terminal-success"),
                operation_id.clone(),
                CommandKind::InstallVersion,
                StabilizationSystem::Application,
                OwnershipClass::LauncherManaged,
                RollbackState::NotApplicable,
            ))
            .await
            .expect("create journal");

        let mut success = OperationJournalStep::new("install_done", OperationPhase::Completed);
        success.result = crate::state::contracts::OperationStepResult::Completed;
        store
            .record_success(&operation_id, success, OperationOutcome::Succeeded)
            .await
            .expect("record success");

        let mut failure = OperationJournalStep::new("install_failed", OperationPhase::Failed);
        failure.result = crate::state::contracts::OperationStepResult::Failed;
        store
            .record_failure(
                &operation_id,
                failure,
                "download_failed",
                OperationOutcome::Failed,
            )
            .await
            .expect_err("terminal journal rejects failure");

        let stored = store.get(&operation_id).expect("journal");
        assert_eq!(stored.status, OperationStatus::Succeeded);
        assert_eq!(stored.outcome, Some(OperationOutcome::Succeeded));
        assert_eq!(stored.failure_point, None);
        assert_eq!(stored.completed_steps.len(), 1);
        assert_eq!(stored.completed_steps[0].step_id, "install_done");
    }

    #[tokio::test]
    async fn terminal_journal_outcome_is_immutable_after_failure() {
        let store = OperationJournalStore::new();
        let operation_id = OperationId::new("operation-terminal-failure");
        store
            .create(OperationJournalEntry::new(
                JournalId::new("journal-operation-terminal-failure"),
                operation_id.clone(),
                CommandKind::InstallVersion,
                StabilizationSystem::Application,
                OwnershipClass::LauncherManaged,
                RollbackState::NotApplicable,
            ))
            .await
            .expect("create journal");

        let mut failure = OperationJournalStep::new("install_failed", OperationPhase::Failed);
        failure.result = crate::state::contracts::OperationStepResult::Failed;
        store
            .record_failure(
                &operation_id,
                failure,
                "download_failed",
                OperationOutcome::Failed,
            )
            .await
            .expect("record failure");

        let mut success = OperationJournalStep::new("install_done", OperationPhase::Completed);
        success.result = crate::state::contracts::OperationStepResult::Completed;
        store
            .record_success(&operation_id, success, OperationOutcome::Succeeded)
            .await
            .expect_err("terminal journal rejects success");

        let stored = store.get(&operation_id).expect("journal");
        assert_eq!(stored.status, OperationStatus::Failed);
        assert_eq!(stored.outcome, Some(OperationOutcome::Failed));
        assert_eq!(stored.failure_point.as_deref(), Some("download_failed"));
        assert_eq!(stored.completed_steps.len(), 1);
        assert_eq!(stored.completed_steps[0].step_id, "install_failed");
    }

    #[test]
    fn operation_journal_snapshot_round_trips_strict_shape() {
        let entry = test_entry("operation-1");
        let snapshot = OperationJournalSnapshot::new(vec![entry.clone()]).expect("snapshot");
        let encoded = snapshot.to_json().expect("serialize snapshot");
        let decoded = OperationJournalSnapshot::from_json(&encoded).expect("deserialize snapshot");

        assert_eq!(decoded.entries, vec![entry]);

        let unknown_field = serde_json::json!({
            "schema": super::OPERATION_JOURNAL_SCHEMA,
            "entries": [{
                "journal_id": "journal-operation-1",
                "operation_id": "operation-1",
                "command": "InstallVersion",
                "status": "Succeeded",
                "owner": "Application",
                "ownership": "LauncherManaged",
                "targets": [],
                "planned_steps": [],
                "completed_steps": [],
                "failure_point": null,
                "rollback": "NotApplicable",
                "guardian_diagnosis_ids": [],
                "outcome": "Succeeded",
                "unexpected": true
            }]
        });
        assert!(OperationJournalSnapshot::from_json(&unknown_field.to_string()).is_err());
    }

    #[test]
    fn checked_in_operation_journals_v1_fixture_is_byte_stable() {
        let snapshot = OperationJournalSnapshot::from_json(OPERATION_JOURNALS_V1_FIXTURE)
            .expect("strict fixture");
        assert_eq!(snapshot.schema, super::OPERATION_JOURNAL_SCHEMA);
        let diagnosis_ids = snapshot
            .entries
            .iter()
            .flat_map(|entry| entry.guardian_diagnosis_ids.iter())
            .collect::<Vec<_>>();
        assert_eq!(diagnosis_ids.len(), 78);
        assert_eq!(
            diagnosis_ids
                .into_iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            78
        );

        let pretty = serde_json::to_string_pretty(&snapshot).expect("pretty fixture json");
        assert_eq!(format!("{pretty}\n"), OPERATION_JOURNALS_V1_FIXTURE);

        let compact = snapshot.to_json().expect("compact fixture json");
        let decoded =
            OperationJournalSnapshot::from_json(&compact).expect("decode compact fixture");
        assert_eq!(
            decoded.to_json().expect("re-encode compact fixture"),
            compact
        );
    }

    #[test]
    fn operation_journal_snapshot_rejects_raw_public_evidence() {
        let mut entry = test_entry("operation-raw");
        entry.completed_steps[0]
            .generated_facts
            .push(r"C:\Users\Alice\.minecraft --accessToken secret -Xmx8192M".to_string());

        assert!(OperationJournalSnapshot::new(vec![entry]).is_err());

        let mut unsafe_target = test_entry("operation-unsafe-target");
        unsafe_target.targets.push(TargetDescriptor {
            system: StabilizationSystem::State,
            kind: TargetKind::FilesystemPath,
            id: "/home/alice/.axial/libraries/secret.jar".to_string(),
            ownership: OwnershipClass::LauncherManaged,
        });
        assert!(OperationJournalSnapshot::new(vec![unsafe_target]).is_err());
    }

    #[test]
    fn operation_journal_error_class_never_exposes_raw_persistence_details() {
        let error = OperationJournalStoreError::Persistence(io::Error::other(
            r"failed at C:\Users\Alice\.axial with token secret",
        ));

        assert_eq!(error.class(), "persistence");
        assert!(!error.class().contains("Alice"));
        assert!(!error.class().contains("token"));
    }

    #[test]
    fn structured_tokens_accept_uuid_ids_without_allowing_secret_runs() {
        assert!(super::safe_token(
            "performance-rules-refresh-123e4567-e89b-12d3-a456-426614174000",
            128,
        ));
        assert!(super::safe_token(
            "guardian-artifact-repair:123e4567-e89b-12d3-a456-426614174000",
            128,
        ));
        assert!(!super::safe_token(
            "operation-abcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyz123456",
            128,
        ));
        assert!(!super::safe_token("operation-access-token-secret", 128));
    }

    #[tokio::test]
    async fn journal_store_persists_snapshot_for_restart_replay() {
        let root = test_root("persisted-journal");
        let paths = test_paths(&root);
        let store = OperationJournalStore::load_from_paths(&paths);
        let operation_id = OperationId::new("install-operation-restart-replay");
        let mut entry = test_entry(operation_id.as_str());
        entry.operation_id = operation_id.clone();
        entry.journal_id = JournalId::new(format!("journal-{}", operation_id.as_str()));

        store.create(entry).await.expect("create journal");
        store
            .record_guardian_evidence(
                &operation_id,
                vec![
                "guardian_outcome_decision:retry".to_string(),
                "guardian_outcome_summary:Guardian treated install download failure as retryable."
                    .to_string(),
            ],
                vec!["download_unavailable".to_string()],
            )
            .await
            .expect("record Guardian evidence");

        let path = operation_journal_path(&paths);
        assert!(path.is_file());
        let snapshot = OperationJournalSnapshot::from_json(
            &fs::read_to_string(&path).expect("persisted journal snapshot"),
        )
        .expect("valid persisted snapshot");
        assert_eq!(snapshot.entries.len(), 1);

        store.close().await.expect("close journal store");
        drop(store);
        let reloaded = OperationJournalStore::load_from_paths(&paths);
        let loaded = reloaded.get(&operation_id).expect("reloaded journal");
        assert_eq!(loaded.operation_id, operation_id);
        assert_eq!(loaded.status, OperationStatus::Succeeded);
        assert!(
            loaded
                .guardian_diagnosis_ids
                .iter()
                .any(|id| id == "download_unavailable")
        );

        cleanup(&root);
    }

    #[tokio::test]
    async fn journal_store_retention_evicts_only_terminal_entries() {
        let store = OperationJournalStore::with_max_entries(2);
        let pinned = OperationId::new("operation-pinned");
        store
            .create(planned_entry(&pinned))
            .await
            .expect("create pinned journal");

        for index in 0..16 {
            let operation_id = OperationId::new(format!("operation-terminal-{index:02}"));
            store
                .create(planned_entry(&operation_id))
                .await
                .expect("create terminal churn journal");
            store
                .record_success(
                    &operation_id,
                    completed_step("done"),
                    OperationOutcome::Succeeded,
                )
                .await
                .expect("terminalize churn journal");
            assert!(store.get(&pinned).is_some());
        }

        let entries = store.list();
        assert_eq!(entries.len(), 2);
        assert!(entries.iter().any(|entry| entry.operation_id == pinned));
    }

    #[tokio::test]
    async fn journal_store_rejects_capacity_exhausted_by_nonterminal_entries() {
        let store = OperationJournalStore::with_max_entries(2);
        for id in ["operation-active-1", "operation-active-2"] {
            store
                .create(planned_entry(&OperationId::new(id)))
                .await
                .expect("create journal");
        }
        let before = store.snapshot().expect("snapshot before rejection");

        assert!(matches!(
            store
                .create(planned_entry(&OperationId::new("operation-active-3")))
                .await,
            Err(OperationJournalStoreError::CapacityExhausted)
        ));
        assert_eq!(store.snapshot().expect("snapshot after rejection"), before);
        assert!(matches!(
            store.create(test_entry("operation-terminal-new")).await,
            Err(OperationJournalStoreError::CapacityExhausted)
        ));
        assert_eq!(
            store.snapshot().expect("snapshot after terminal rejection"),
            before
        );
    }

    #[tokio::test]
    async fn journal_store_accepts_exact_duplicate_create_without_rewriting() {
        let (root, _paths, backend, _coordinator, store) =
            persistence_fixture("exact-duplicate-create");
        let entry = test_entry("operation-exact-duplicate");
        store.create(entry.clone()).await.expect("create journal");
        let attempts = backend.attempts.load(Ordering::SeqCst);
        store
            .create(entry)
            .await
            .expect("exact duplicate is idempotent");

        assert_eq!(store.list().len(), 1);
        assert_eq!(backend.attempts.load(Ordering::SeqCst), attempts);
        store.close().await.expect("close journal store");
        cleanup(&root);
    }

    #[tokio::test]
    async fn journal_store_rejects_duplicate_create_over_terminal_record() {
        let store = OperationJournalStore::new();
        let operation_id = OperationId::new("operation-terminal-duplicate");
        let planned = planned_entry(&operation_id);
        store.create(planned.clone()).await.expect("create journal");
        store
            .record_success(
                &operation_id,
                completed_step("done"),
                OperationOutcome::Succeeded,
            )
            .await
            .expect("terminalize journal");

        assert!(matches!(
            store.create(planned).await,
            Err(OperationJournalStoreError::AlreadyExists)
        ));
        assert_eq!(
            store.get(&operation_id).expect("terminal journal").status,
            OperationStatus::Succeeded
        );
    }

    #[tokio::test]
    async fn journal_store_rejects_invalid_update_without_mutating_record() {
        let store = OperationJournalStore::new();
        let operation_id = OperationId::new("operation-invalid-update");
        store
            .create(test_entry(operation_id.as_str()))
            .await
            .expect("create journal");

        store
            .record_guardian_evidence(
                &operation_id,
                vec![r"C:\Users\Alice\.minecraft --accessToken secret -Xmx8192M".to_string()],
                Vec::new(),
            )
            .await
            .expect_err("reject unsafe evidence");

        let entry = store.get(&operation_id).expect("journal");
        let facts = &entry.completed_steps[0].generated_facts;
        assert!(!facts.iter().any(|fact| fact.contains("accessToken")));
        assert_eq!(facts, &vec!["install_phase:done", "install_done:true"]);
    }

    #[tokio::test]
    async fn initial_journal_is_hidden_until_the_physical_commit_finishes() {
        let (root, _paths, backend, _coordinator, store) =
            persistence_fixture("gated-initial-visibility");
        let store = Arc::new(store);
        let operation_id = OperationId::new("operation-gated-initial");
        let gate = backend.gate_next();
        let expected_attempt = backend.attempts.load(Ordering::SeqCst) + 1;
        let create_store = store.clone();
        let create_id = operation_id.clone();
        let create =
            tokio::spawn(async move { create_store.create(planned_entry(&create_id)).await });
        backend.wait_for_attempt(expected_attempt).await;

        assert!(store.get(&operation_id).is_none());
        gate.release();
        create
            .await
            .expect("create task")
            .expect("commit initial journal");
        assert_eq!(
            store.get(&operation_id).expect("visible journal").status,
            OperationStatus::Planned
        );
        cleanup(&root);
    }

    #[tokio::test]
    async fn cancelled_terminal_caller_cannot_cancel_committed_visibility() {
        let (root, _paths, backend, _coordinator, store) =
            persistence_fixture("cancelled-terminal-visibility");
        let store = Arc::new(store);
        let operation_id = OperationId::new("operation-cancelled-terminal");
        store
            .create(planned_entry(&operation_id))
            .await
            .expect("commit initial journal");
        let gate = backend.gate_next();
        let expected_attempt = backend.attempts.load(Ordering::SeqCst) + 1;
        let terminal_store = store.clone();
        let terminal_id = operation_id.clone();
        let terminal = tokio::spawn(async move {
            terminal_store
                .record_success(
                    &terminal_id,
                    completed_step("operation_done"),
                    OperationOutcome::Succeeded,
                )
                .await
        });
        backend.wait_for_attempt(expected_attempt).await;
        assert_eq!(
            store.get(&operation_id).expect("planned journal").status,
            OperationStatus::Planned
        );
        terminal.abort();
        assert!(
            terminal
                .await
                .expect_err("cancel terminal caller")
                .is_cancelled()
        );

        gate.release();
        store.flush().await.expect("flush observed terminal commit");
        assert_eq!(
            store.get(&operation_id).expect("terminal journal").status,
            OperationStatus::Succeeded
        );
        cleanup(&root);
    }

    #[tokio::test]
    async fn gated_terminal_serializes_later_progress_without_visibility_regression() {
        let (root, _paths, backend, _coordinator, store) =
            persistence_fixture("terminal-progress-order");
        let store = Arc::new(store);
        let terminal_id = OperationId::new("operation-terminal-a");
        let progress_id = OperationId::new("operation-progress-b");
        store
            .create(planned_entry(&terminal_id))
            .await
            .expect("create terminal operation");
        store
            .create(planned_entry(&progress_id))
            .await
            .expect("create progress operation");

        let gate = backend.gate_next();
        let expected_attempt = backend.attempts.load(Ordering::SeqCst) + 1;
        let terminal_store = store.clone();
        let terminal_operation = terminal_id.clone();
        let terminal = tokio::spawn(async move {
            terminal_store
                .record_success(
                    &terminal_operation,
                    completed_step("terminal_done"),
                    OperationOutcome::Succeeded,
                )
                .await
        });
        backend.wait_for_attempt(expected_attempt).await;
        let progress_store = store.clone();
        let progress_operation = progress_id.clone();
        let progress = tokio::spawn(async move {
            progress_store
                .record_progress(&progress_operation, completed_step("progress_update"))
                .await
        });
        tokio::task::yield_now().await;
        assert_eq!(
            store.get(&terminal_id).expect("terminal hidden").status,
            OperationStatus::Planned
        );
        assert_eq!(
            store
                .get(&progress_id)
                .expect("progress still planned")
                .completed_steps
                .len(),
            0
        );

        gate.release();
        terminal
            .await
            .expect("terminal task")
            .expect("terminal commit");
        progress
            .await
            .expect("progress task")
            .expect("progress accept");
        assert_eq!(
            store.get(&terminal_id).expect("terminal visible").status,
            OperationStatus::Succeeded
        );
        assert_eq!(
            store
                .get(&progress_id)
                .expect("progress visible")
                .completed_steps
                .len(),
            1
        );
        cleanup(&root);
    }

    #[tokio::test]
    async fn progress_burst_coalesces_and_reloads_the_latest_snapshot() {
        let (root, paths, backend, coordinator, store) =
            persistence_fixture("progress-burst-reload");
        let operation_id = OperationId::new("operation-progress-burst");
        store
            .create(planned_entry(&operation_id))
            .await
            .expect("create journal");
        let writes_before = backend.attempts.load(Ordering::SeqCst);
        for index in 0..100 {
            store
                .record_progress(&operation_id, completed_step(&format!("progress_{index}")))
                .await
                .expect("accept progress");
        }
        store.flush().await.expect("flush progress burst");
        assert!(backend.attempts.load(Ordering::SeqCst) - writes_before < 10);
        store.close().await.expect("close journal owner");
        drop(store);

        let reloaded =
            OperationJournalStore::try_load_from_paths_with_coordinator(&paths, coordinator)
                .expect("reload journal store");
        assert_eq!(
            reloaded
                .get(&operation_id)
                .expect("reloaded journal")
                .completed_steps
                .len(),
            100
        );
        cleanup(&root);
    }

    #[tokio::test]
    async fn close_retries_latest_failed_debounced_progress_and_reloads_it() {
        let (root, paths, backend, coordinator, store) =
            persistence_fixture("close-retries-debounced-progress");
        let operation_id = OperationId::new("operation-close-progress-retry");
        store
            .create(planned_entry(&operation_id))
            .await
            .expect("create journal");
        backend.fail_next();
        store
            .record_progress(&operation_id, completed_step("progress_first"))
            .await
            .expect("accept first progress");
        store
            .record_progress(&operation_id, completed_step("progress_latest"))
            .await
            .expect("accept latest progress");
        assert!(matches!(
            store.flush().await,
            Err(OperationJournalStoreError::Persistence(_))
        ));
        assert!(!store.has_retry_candidate());

        store
            .close()
            .await
            .expect("close retries exact debounced snapshot");

        let reloaded =
            OperationJournalStore::try_load_from_paths_with_coordinator(&paths, coordinator)
                .expect("closed owner is released");
        let journal = reloaded.get(&operation_id).expect("progress reloads");
        assert_eq!(journal.status, OperationStatus::Running);
        assert_eq!(journal.completed_steps.len(), 2);
        assert_eq!(journal.completed_steps[1].step_id, "progress_latest");
        reloaded.close().await.expect("close reloaded store");
        cleanup(&root);
    }

    #[tokio::test]
    async fn physical_failure_stays_hidden_and_retry_publishes_latest_candidate() {
        let (root, _paths, backend, _coordinator, store) =
            persistence_fixture("failure-retry-latest");
        let store = Arc::new(store);
        let operation_id = OperationId::new("operation-failure-retry");
        store
            .create(planned_entry(&operation_id))
            .await
            .expect("create journal");
        backend.fail_next();
        assert!(matches!(
            store
                .record_success(
                    &operation_id,
                    completed_step("operation_done"),
                    OperationOutcome::Succeeded,
                )
                .await,
            Err(OperationJournalStoreError::Persistence(_))
        ));
        assert_eq!(
            store
                .get(&operation_id)
                .expect("nonterminal journal")
                .status,
            OperationStatus::Planned
        );
        let attempts = backend.attempts.load(Ordering::SeqCst);
        assert!(matches!(
            store
                .record_progress(&operation_id, completed_step("late_progress"))
                .await,
            Err(OperationJournalStoreError::RetryRequired)
        ));
        assert_eq!(backend.attempts.load(Ordering::SeqCst), attempts);
        let gate = backend.gate_next();
        let expected_attempt = backend.attempts.load(Ordering::SeqCst) + 1;
        let retry_store = store.clone();
        let retry = tokio::spawn(async move { retry_store.retry().await });
        backend.wait_for_attempt(expected_attempt).await;
        retry.abort();
        assert!(retry.await.expect_err("cancel retry caller").is_cancelled());
        gate.release();
        store.flush().await.expect("flush observed retry");
        assert_eq!(
            store.get(&operation_id).expect("retried journal").status,
            OperationStatus::Succeeded
        );
        cleanup(&root);
    }

    #[tokio::test]
    async fn close_retries_hidden_candidate_and_releases_owner() {
        let (root, paths, backend, coordinator, store) =
            persistence_fixture("close-retries-hidden-candidate");
        let operation_id = OperationId::new("operation-close-retry");
        store
            .create(planned_entry(&operation_id))
            .await
            .expect("create journal");
        backend.fail_next();
        assert!(matches!(
            store
                .record_success(
                    &operation_id,
                    completed_step("operation_done"),
                    OperationOutcome::Succeeded,
                )
                .await,
            Err(OperationJournalStoreError::Persistence(_))
        ));
        assert_eq!(
            store.get(&operation_id).expect("visible journal").status,
            OperationStatus::Planned
        );

        store
            .close()
            .await
            .expect("close retries the exact hidden candidate");
        store.close().await.expect("close is idempotent");

        let reloaded =
            OperationJournalStore::try_load_from_paths_with_coordinator(&paths, coordinator)
                .expect("closed owner is released");
        assert_eq!(
            reloaded
                .get(&operation_id)
                .expect("retried journal reloads")
                .status,
            OperationStatus::Succeeded
        );
        reloaded.close().await.expect("close reloaded store");
        cleanup(&root);
    }

    #[tokio::test]
    async fn reconciliation_verifies_own_transition_after_another_owner_clears_candidate() {
        let (root, _paths, backend, _coordinator, store) =
            persistence_fixture("reconcile-own-cleared-candidate");
        let operation_id = OperationId::new("operation-reconcile-own");
        store
            .create(planned_entry(&operation_id))
            .await
            .expect("create journal");
        let terminal_step = completed_step("operation_done");
        backend.fail_next();
        let error = store
            .record_success(
                &operation_id,
                terminal_step.clone(),
                OperationOutcome::Succeeded,
            )
            .await
            .expect_err("terminal commit fails physically");

        store
            .retry()
            .await
            .expect("second owner commits accepted candidate");
        assert!(!store.has_retry_candidate());
        let reconciliation = store
            .reconcile_transition(
                &operation_id,
                error,
                Duration::from_millis(1),
                Duration::from_millis(5),
                |entry| {
                    entry.status == OperationStatus::Succeeded
                        && entry.outcome == Some(OperationOutcome::Succeeded)
                        && entry.failure_point.is_none()
                        && entry.completed_steps.contains(&terminal_step)
                },
            )
            .await
            .expect("visible requested transition is accepted");

        assert!(matches!(
            reconciliation,
            OperationJournalReconciliation::CommittedAfterPersistenceFailure(_)
        ));
        cleanup(&root);
    }

    #[test]
    fn planned_transition_rejects_an_advanced_journal_to_prevent_effect_replay() {
        let operation_id = OperationId::new("operation-plan-visible-after-progress");
        let expected = planned_entry(&operation_id);
        let mut advanced = expected.clone();
        advanced.status = OperationStatus::Running;
        advanced.completed_steps.push(completed_step("progress"));

        assert!(!operation_journal_plan_is_visible(&advanced, &expected));
        advanced = expected.clone();
        assert!(operation_journal_plan_is_visible(&advanced, &expected));
        advanced.owner = StabilizationSystem::Execution;
        assert!(!operation_journal_plan_is_visible(&advanced, &expected));
    }

    #[tokio::test]
    async fn reconciliation_reapplies_after_foreign_candidate_is_cleared() {
        let (root, _paths, backend, _coordinator, store) =
            persistence_fixture("reconcile-foreign-cleared-candidate");
        let requested_id = OperationId::new("operation-reconcile-requested");
        let foreign_id = OperationId::new("operation-reconcile-foreign");
        store
            .create(planned_entry(&requested_id))
            .await
            .expect("create requested journal");
        store
            .create(planned_entry(&foreign_id))
            .await
            .expect("create foreign journal");
        backend.fail_next();
        store
            .record_success(
                &foreign_id,
                completed_step("foreign_done"),
                OperationOutcome::Succeeded,
            )
            .await
            .expect_err("foreign terminal commit fails physically");
        let requested_step = completed_step("requested_checkpoint");
        let error = store
            .record_checkpoint(&requested_id, requested_step.clone())
            .await
            .expect_err("requested transition waits for foreign candidate");

        store
            .retry()
            .await
            .expect("second owner commits foreign candidate");
        let reconciliation = store
            .reconcile_transition(
                &requested_id,
                error,
                Duration::from_millis(1),
                Duration::from_millis(5),
                |entry| entry.completed_steps.contains(&requested_step),
            )
            .await
            .expect("foreign candidate requires requested transition reapply");

        assert!(matches!(
            reconciliation,
            OperationJournalReconciliation::RetryRequestedTransition
        ));
        assert!(
            !store
                .get(&requested_id)
                .expect("requested journal")
                .completed_steps
                .contains(&requested_step)
        );
        cleanup(&root);
    }

    #[tokio::test]
    async fn exact_snapshot_path_has_one_owner_and_poison_never_reports_success() {
        let (root, paths, _backend, coordinator, store) = persistence_fixture("owner-poison");
        let store = Arc::new(store);
        assert!(matches!(
            OperationJournalStore::try_load_from_paths_with_coordinator(&paths, coordinator,),
            Err(OperationJournalStoreError::Persistence(_))
        ));
        let operation_id = OperationId::new("operation-poisoned");
        store
            .create(planned_entry(&operation_id))
            .await
            .expect("create journal");
        let poison = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _records = store.records.write().expect("records lock");
            panic!("inject journal lock poison");
        }));
        assert!(poison.is_err());
        for panic in [
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _ = store.get(&operation_id);
            })),
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _ = store.list();
            })),
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _ = store.load_snapshot(
                    OperationJournalSnapshot::new(vec![planned_entry(&operation_id)])
                        .expect("snapshot"),
                );
            })),
        ] {
            let message = panic_message(panic.expect_err("poisoned access must panic"));
            assert!(message.contains(OPERATION_JOURNAL_LOCK_INVARIANT));
        }

        let create_store = store.clone();
        let create_panic = tokio::spawn(async move {
            create_store
                .create(planned_entry(&OperationId::new(
                    "operation-poisoned-create",
                )))
                .await
        })
        .await
        .expect_err("poisoned create must panic");
        assert!(
            panic_message(create_panic.into_panic()).contains(OPERATION_JOURNAL_LOCK_INVARIANT)
        );

        let update_store = store.clone();
        let update_panic = tokio::spawn(async move {
            update_store
                .record_progress(&operation_id, completed_step("poisoned-update"))
                .await
        })
        .await
        .expect_err("poisoned update must panic");
        assert!(
            panic_message(update_panic.into_panic()).contains(OPERATION_JOURNAL_LOCK_INVARIANT)
        );
        cleanup(&root);
    }

    fn planned_entry(operation_id: &OperationId) -> OperationJournalEntry {
        OperationJournalEntry::new(
            JournalId::new(format!("journal-{}", operation_id.as_str())),
            operation_id.clone(),
            CommandKind::InstallVersion,
            StabilizationSystem::Application,
            OwnershipClass::LauncherManaged,
            RollbackState::NotApplicable,
        )
    }

    fn completed_step(id: &str) -> OperationJournalStep {
        let mut step = OperationJournalStep::new(id, OperationPhase::Running);
        step.result = crate::state::contracts::OperationStepResult::Completed;
        step
    }

    fn panic_message(panic: Box<dyn std::any::Any + Send>) -> String {
        if let Some(message) = panic.downcast_ref::<&str>() {
            (*message).to_string()
        } else if let Some(message) = panic.downcast_ref::<String>() {
            message.clone()
        } else {
            "non-string panic".to_string()
        }
    }

    fn test_entry(operation_id: &str) -> OperationJournalEntry {
        let operation_id = OperationId::new(operation_id);
        let mut entry = OperationJournalEntry::new(
            JournalId::new(format!("journal-{}", operation_id.as_str())),
            operation_id,
            CommandKind::InstallVersion,
            StabilizationSystem::Application,
            OwnershipClass::LauncherManaged,
            RollbackState::NotApplicable,
        );
        entry.status = OperationStatus::Succeeded;
        entry.targets.push(TargetDescriptor::new(
            StabilizationSystem::Application,
            TargetKind::Version,
            "minecraft_1.21.5",
            OwnershipClass::LauncherManaged,
        ));
        let mut completed =
            OperationJournalStep::new("install_progress_done", OperationPhase::Completed);
        completed.result = crate::state::contracts::OperationStepResult::Completed;
        completed
            .generated_facts
            .push("install_phase:done".to_string());
        completed
            .generated_facts
            .push("install_done:true".to_string());
        entry.completed_steps.push(completed);
        entry.outcome = Some(OperationOutcome::Succeeded);
        entry
    }

    fn test_paths(root: &Path) -> AppPaths {
        let config_dir = root.join("config");
        AppPaths {
            config_file: config_dir.join("config.json"),
            instances_file: config_dir.join("instances.json"),
            instances_dir: root.join("instances"),
            music_dir: root.join("music"),
            library_dir: root.join("library"),
            config_dir,
        }
    }

    fn test_root(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or_default();
        std::env::temp_dir().join(format!(
            "axial-operation-journal-{prefix}-{}-{nanos:x}",
            std::process::id()
        ))
    }

    fn cleanup(root: &Path) {
        let _ = fs::remove_dir_all(root);
    }
}
