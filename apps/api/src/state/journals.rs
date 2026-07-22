use super::contracts::{
    CommandKind, OperationId, OperationJournalEntry, OperationJournalStep, OperationOutcome,
    OperationPhase, OperationStatus, OperationStepResult, OwnershipClass,
    PersistedStateRepairAttempt, PersistedStateRepairTerminal, PersistedStateRepairTerminalOutcome,
    RECONCILIATION_EVIDENCE_CAPACITY, ReconciliationAttempt, ReconciliationScope,
    ReconciliationTerminal, ReconciliationTerminalOutcome, RollbackState, StabilizationSystem,
    TargetDescriptor, TargetKind,
};
use crate::execution::anchored_record::{AnchoredRecordDirectory, AnchoredRecordObservation};
#[cfg(test)]
use crate::execution::persistence::PersistenceCoordinator;
use crate::execution::persistence::{
    AcceptedWrite, AtomicSnapshotWriter, PersistenceOwnerLease, WriteUrgency,
};
use crate::guardian::DiagnosisId;
use crate::observability::{
    RedactionAudience, evidence_text_looks_sensitive, sanitize_evidence_text,
};
use axial_config::AppPaths;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::sync::Mutex as AsyncMutex;
use tracing::warn;

pub const OPERATION_JOURNAL_SCHEMA: &str = "axial.state.operation_journals.v6";
pub const DEFAULT_OPERATION_JOURNAL_LIMIT: usize = RECONCILIATION_EVIDENCE_CAPACITY;
pub(crate) const MAX_OPERATION_JOURNAL_STEP_FACTS: usize = 64;
pub(crate) const PERFORMANCE_PLAN_GRAPH_SHA512_FACT_PREFIX: &str = "performance_plan_graph_sha512_";
const GUARDIAN_OUTCOME_MEMORY_BINDING_PREFIX: &str = "guardian_outcome_memory_binding:";
const OPERATION_JOURNAL_SNAPSHOT_NAME: &str = "operation-journals.json";
pub(crate) const MAX_OPERATION_JOURNAL_DIAGNOSES: usize = 32;
const MAX_OPERATION_JOURNAL_SNAPSHOT_BYTES: u64 = 8 * 1024 * 1024;
const OPERATION_JOURNAL_LOCK_INVARIANT: &str =
    "operation journal records lock poisoned; in-memory and persisted state may diverge";
const OPERATION_JOURNAL_TRANSITION_RETRY_ATTEMPTS: usize = 4;
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
    #[error("operation journal record already exists")]
    AlreadyExists,
    #[error("operation journal has a failed critical commit that must be retried")]
    RetryRequired,
    #[error("operation journal capacity is exhausted by active operations")]
    CapacityExhausted,
    #[error("operation journal sequence is exhausted")]
    SequenceExhausted,
    #[error("operation journal contains an invalid Guardian install outcome")]
    InvalidGuardianOutcome,
    #[error("Guardian install failure memory could not be settled")]
    GuardianFailureMemoryUnavailable,
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
            Self::SequenceExhausted => "sequence_exhausted",
            Self::InvalidGuardianOutcome => "invalid_guardian_outcome",
            Self::GuardianFailureMemoryUnavailable => "guardian_failure_memory_unavailable",
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
        && entry.reconciliation_attempt == expected.reconciliation_attempt
        && entry.reconciliation_terminal == expected.reconciliation_terminal
        && entry.persisted_state_repair_attempt == expected.persisted_state_repair_attempt
        && entry.persisted_state_repair_terminal == expected.persisted_state_repair_terminal
}

fn operation_journal_identity_and_plan_match(
    entry: &OperationJournalEntry,
    expected: &OperationJournalEntry,
) -> bool {
    entry.journal_id == expected.journal_id
        && entry.operation_id == expected.operation_id
        && entry.parent_operation_id == expected.parent_operation_id
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
        && entry.reconciliation_attempt == expected.reconciliation_attempt
        && entry.reconciliation_terminal == expected.reconciliation_terminal
        && entry.persisted_state_repair_attempt == expected.persisted_state_repair_attempt
        && entry.persisted_state_repair_terminal == expected.persisted_state_repair_terminal
        && expected
            .completed_steps
            .iter()
            .all(|step| operation_journal_completed_step_is_visible(entry, step))
}

pub(super) fn persisted_state_repair_plan_is_visible(
    entry: &OperationJournalEntry,
    attempt: &PersistedStateRepairAttempt,
) -> bool {
    entry.persisted_state_repair_attempt() == Some(attempt)
        && entry.persisted_state_repair_terminal().is_none()
        && validate_entry(entry).is_ok()
}

pub(super) fn persisted_state_repair_terminal_is_visible(
    entry: &OperationJournalEntry,
    terminal: &PersistedStateRepairTerminal,
) -> bool {
    entry.persisted_state_repair_terminal() == Some(terminal) && validate_entry(entry).is_ok()
}

struct OperationJournalPersistence {
    owner: PersistenceOwnerLease,
    writer: AtomicSnapshotWriter,
}

impl OperationJournalPersistence {
    fn claim(
        directory: AnchoredRecordDirectory,
    ) -> Result<Self, OperationJournalStoreError> {
        Self::claim_with_coordinator(directory, PersistenceCoordinator::global())
    }

    fn claim_with_coordinator(
        directory: AnchoredRecordDirectory,
        coordinator: PersistenceCoordinator,
    ) -> Result<Self, OperationJournalStoreError> {
        let record = directory
            .target(
                std::ffi::OsStr::new(OPERATION_JOURNAL_SNAPSHOT_NAME),
                MAX_OPERATION_JOURNAL_SNAPSHOT_BYTES,
            )
            .map_err(OperationJournalStoreError::Persistence)?;
        let owner = coordinator
            .claim_record(record.clone())
            .map_err(|error| OperationJournalStoreError::Persistence(error.into()))?;
        let writer = owner
            .writer(record)
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
    visible: BTreeMap<OperationId, OperationJournalEntry>,
    visible_revision: u64,
    retry_candidate: Option<(u64, BTreeMap<OperationId, OperationJournalEntry>)>,
}

struct PendingJournalCommit {
    ticket: AcceptedWrite,
    revision: u64,
    candidate: BTreeMap<OperationId, OperationJournalEntry>,
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

    pub(crate) fn try_load_from_directory(
        directory: AnchoredRecordDirectory,
    ) -> Result<Self, OperationJournalStoreError> {
        let mut store = Self::with_max_entries_and_persistence(
            DEFAULT_OPERATION_JOURNAL_LIMIT,
            Some(OperationJournalPersistence::claim(directory.clone())?),
        );

        store.load_from_directory(&directory)?;
        Ok(store)
    }

    #[cfg(test)]
    pub fn try_load_from_paths(paths: &AppPaths) -> Result<Self, OperationJournalStoreError> {
        let directory = test_journal_record_directory(paths)?;
        Self::try_load_from_directory(directory)
    }

    #[cfg(test)]
    pub(crate) fn try_load_from_paths_with_coordinator(
        paths: &AppPaths,
        coordinator: PersistenceCoordinator,
    ) -> Result<Self, OperationJournalStoreError> {
        let directory = test_journal_record_directory(paths)?;
        let mut store = Self::with_max_entries_and_persistence(
            DEFAULT_OPERATION_JOURNAL_LIMIT,
            Some(OperationJournalPersistence::claim_with_coordinator(
                directory.clone(),
                coordinator,
            )?),
        );
        store.load_from_directory(&directory)?;
        Ok(store)
    }

    fn load_from_directory(
        &mut self,
        directory: &AnchoredRecordDirectory,
    ) -> Result<(), OperationJournalStoreError> {
        let observation =
            match directory.read(std::ffi::OsStr::new(OPERATION_JOURNAL_SNAPSHOT_NAME), MAX_OPERATION_JOURNAL_SNAPSHOT_BYTES) {
                Ok(observation) => observation,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
                Err(error) => return Err(OperationJournalStoreError::Persistence(error)),
            };
        let bytes = observation
            .bytes()
            .ok_or(OperationJournalLoadError::TooLarge)?;
        let data = String::from_utf8(bytes.to_vec()).map_err(|error| {
            OperationJournalStoreError::Persistence(io::Error::new(
                io::ErrorKind::InvalidData,
                error,
            ))
        })?;
        self.load_snapshot(OperationJournalSnapshot::from_json(&data)?)?;
        observation
            .admit(MAX_OPERATION_JOURNAL_SNAPSHOT_BYTES)
            .map_err(OperationJournalStoreError::Persistence)?;
        Ok(())
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

    pub(crate) const fn load_issue_count(&self) -> usize {
        0
    }

    pub async fn create(
        &self,
        entry: OperationJournalEntry,
    ) -> Result<(), OperationJournalStoreError> {
        self.create_with_existing_policy(entry, true).await
    }

    pub async fn create_fresh(
        &self,
        entry: OperationJournalEntry,
    ) -> Result<(), OperationJournalStoreError> {
        self.create_with_existing_policy(entry, false).await
    }

    async fn create_with_existing_policy(
        &self,
        mut entry: OperationJournalEntry,
        allow_matching_existing: bool,
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
            if let Some(existing) = records.visible.get(&entry.operation_id) {
                if allow_matching_existing && existing.matches_store_entry(&entry) {
                    return Ok(());
                }
                return Err(OperationJournalStoreError::AlreadyExists);
            }
            entry.sequence = next_journal_sequence(&records.visible)?;
            let operation_key = entry.operation_id.clone();
            let mut candidate = records.visible.clone();
            candidate.insert(operation_key.clone(), entry);
            if !prune_records(&mut candidate, self.max_entries, Some(&operation_key)) {
                return Err(OperationJournalStoreError::CapacityExhausted);
            }
            self.accept_candidate(&mut records, candidate, WriteUrgency::Immediate)?
        };
        self.await_commit(ticket, mutation).await
    }

    pub(super) async fn create_persisted_state_repair_plan(
        &self,
        attempt: PersistedStateRepairAttempt,
    ) -> Result<(), OperationJournalStoreError> {
        let mut entry = OperationJournalEntry::new(
            attempt.journal_id(),
            attempt.operation_id().clone(),
            CommandKind::RepairPersistedState,
            StabilizationSystem::Guardian,
            OwnershipClass::LauncherManaged,
            RollbackState::NotApplicable,
        );
        entry.targets.push(attempt.target().clone());
        entry.planned_steps.push(OperationJournalStep::new(
            "quarantine_rejected_restart_record",
            OperationPhase::Repairing,
        ));
        entry
            .guardian_diagnosis_ids
            .push(DiagnosisId::PersistedStateSchemaInvalid);
        entry.persisted_state_repair_attempt = Some(attempt);
        validate_entry(&entry)?;
        let mutation = self.mutation_gate.clone().lock_owned().await;
        let ticket = {
            let mut records = self
                .records
                .write()
                .expect(OPERATION_JOURNAL_LOCK_INVARIANT);
            if records.retry_candidate.is_some() {
                return Err(OperationJournalStoreError::RetryRequired);
            }
            if records.visible.contains_key(&entry.operation_id) {
                return Err(OperationJournalStoreError::AlreadyExists);
            }
            entry.sequence = next_journal_sequence(&records.visible)?;
            let operation_key = entry.operation_id.clone();
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
            .get(operation_id)
            .cloned()
    }

    pub fn latest_for_command(&self, command: CommandKind) -> Option<OperationJournalEntry> {
        self.records
            .read()
            .expect(OPERATION_JOURNAL_LOCK_INVARIANT)
            .visible
            .values()
            .filter(|entry| entry.command == command)
            .max_by_key(|entry| entry.sequence)
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

    pub(crate) async fn record_success_with_guardian_evidence(
        &self,
        operation_id: &OperationId,
        completed_step: OperationJournalStep,
        fact_ids: Vec<String>,
        diagnosis_ids: Vec<DiagnosisId>,
    ) -> Result<(), OperationJournalStoreError> {
        let mutation = self.mutation_gate.clone().lock_owned().await;
        let ticket = self.update(operation_id, WriteUrgency::Immediate, |entry| {
            if operation_journal_status_is_terminal(entry.status) {
                return Err(OperationJournalStoreError::AlreadyTerminal);
            }
            entry.status = OperationStatus::Succeeded;
            entry.completed_steps.push(completed_step);
            entry.failure_point = None;
            entry.outcome = Some(OperationOutcome::Succeeded);
            apply_guardian_evidence(entry, fact_ids, diagnosis_ids);
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

    pub(super) async fn record_reconciliation_success(
        &self,
        operation_id: &OperationId,
        completed_step: OperationJournalStep,
        terminal: ReconciliationTerminal,
    ) -> Result<(), OperationJournalStoreError> {
        if terminal.outcome() != ReconciliationTerminalOutcome::Succeeded {
            return Err(OperationJournalValidationError::ReconciliationTerminalMismatch.into());
        }
        let mutation = self.mutation_gate.clone().lock_owned().await;
        let ticket = self.update(operation_id, WriteUrgency::Immediate, |entry| {
            if operation_journal_status_is_terminal(entry.status) {
                return Err(OperationJournalStoreError::AlreadyTerminal);
            }
            if entry.reconciliation_attempt.as_ref() != Some(terminal.attempt()) {
                return Err(OperationJournalValidationError::ReconciliationTerminalMismatch.into());
            }
            entry.status = OperationStatus::Succeeded;
            entry.completed_steps.push(completed_step);
            entry.failure_point = None;
            entry.outcome = Some(OperationOutcome::Succeeded);
            entry.reconciliation_terminal = Some(terminal);
            Ok(())
        })?;
        self.await_commit(ticket, mutation).await
    }

    pub(super) async fn record_reconciliation_failure(
        &self,
        operation_id: &OperationId,
        failure_step: OperationJournalStep,
        failure_point: impl Into<String>,
        terminal: ReconciliationTerminal,
    ) -> Result<(), OperationJournalStoreError> {
        if terminal.outcome() != ReconciliationTerminalOutcome::Failed {
            return Err(OperationJournalValidationError::ReconciliationTerminalMismatch.into());
        }
        let mutation = self.mutation_gate.clone().lock_owned().await;
        let ticket = self.update(operation_id, WriteUrgency::Immediate, |entry| {
            if operation_journal_status_is_terminal(entry.status) {
                return Err(OperationJournalStoreError::AlreadyTerminal);
            }
            if entry.reconciliation_attempt.as_ref() != Some(terminal.attempt()) {
                return Err(OperationJournalValidationError::ReconciliationTerminalMismatch.into());
            }
            entry.status = OperationStatus::Failed;
            entry.completed_steps.push(failure_step);
            entry.failure_point = Some(failure_point.into());
            entry.outcome = Some(OperationOutcome::Failed);
            entry.reconciliation_terminal = Some(terminal);
            Ok(())
        })?;
        self.await_commit(ticket, mutation).await
    }

    pub(super) async fn record_persisted_state_repair_terminal(
        &self,
        operation_id: &OperationId,
        terminal: PersistedStateRepairTerminal,
    ) -> Result<(), OperationJournalStoreError> {
        let mutation = self.mutation_gate.clone().lock_owned().await;
        let ticket = self.update(operation_id, WriteUrgency::Immediate, |entry| {
            if operation_journal_status_is_terminal(entry.status) {
                return Err(OperationJournalStoreError::AlreadyTerminal);
            }
            if entry.persisted_state_repair_attempt.as_ref() != Some(terminal.attempt()) {
                return Err(OperationJournalValidationError::PersistedStateRepairMismatch.into());
            }
            let shape = persisted_state_repair_terminal_shape(terminal.outcome());
            let mut completed_step = OperationJournalStep::new(
                "quarantine_rejected_restart_record",
                OperationPhase::Repairing,
            );
            completed_step.result = shape.step_result;
            completed_step.changed_target = Some(terminal.attempt().target().clone());
            completed_step.generated_facts = vec![shape.fact.to_string()];
            entry.status = shape.status;
            entry.completed_steps.push(completed_step);
            entry.failure_point = shape.failure_point.map(str::to_string);
            entry.outcome = Some(shape.outcome);
            entry.persisted_state_repair_terminal = Some(terminal);
            Ok(())
        })?;
        self.await_commit(ticket, mutation).await
    }

    pub(super) async fn record_guardian_repair_refusal(
        &self,
        operation_id: &OperationId,
        skipped_step: OperationJournalStep,
    ) -> Result<(), OperationJournalStoreError> {
        if skipped_step.result != OperationStepResult::Skipped {
            return Err(OperationJournalValidationError::ReconciliationTerminalMismatch.into());
        }
        let mutation = self.mutation_gate.clone().lock_owned().await;
        let ticket = self.update(operation_id, WriteUrgency::Immediate, |entry| {
            if operation_journal_status_is_terminal(entry.status) {
                return Err(OperationJournalStoreError::AlreadyTerminal);
            }
            if entry.command != CommandKind::RepairInstance
                || entry.owner != StabilizationSystem::Guardian
            {
                return Err(OperationJournalValidationError::ReconciliationTerminalMismatch.into());
            }
            entry.status = OperationStatus::Blocked;
            entry.completed_steps.push(skipped_step);
            entry.failure_point = None;
            entry.outcome = Some(OperationOutcome::Blocked);
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
        diagnosis_ids: Vec<DiagnosisId>,
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

    pub(crate) async fn record_cancellation(
        &self,
        operation_id: &OperationId,
        cancellation_step: OperationJournalStep,
    ) -> Result<(), OperationJournalStoreError> {
        let mutation = self.mutation_gate.clone().lock_owned().await;
        let ticket = self.update(operation_id, WriteUrgency::Immediate, |entry| {
            if operation_journal_status_is_terminal(entry.status) {
                return Err(OperationJournalStoreError::AlreadyTerminal);
            }
            entry.status = OperationStatus::Cancelled;
            entry.completed_steps.clear();
            entry.completed_steps.push(cancellation_step);
            entry.failure_point = None;
            entry.guardian_diagnosis_ids.clear();
            entry.outcome = Some(OperationOutcome::Cancelled);
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

    pub(crate) async fn record_idempotent_checkpoint(
        &self,
        operation_id: &OperationId,
        checkpoint: OperationJournalStep,
    ) -> Result<(), OperationJournalStoreError> {
        let mutation = self.mutation_gate.clone().lock_owned().await;
        let ticket = self.update(operation_id, WriteUrgency::Immediate, |entry| {
            if operation_journal_status_is_terminal(entry.status) {
                return Err(OperationJournalStoreError::AlreadyTerminal);
            }
            if entry.completed_steps.iter().any(|step| step == &checkpoint) {
                return Ok(());
            }
            if entry
                .completed_steps
                .iter()
                .any(|step| step.step_id == checkpoint.step_id)
            {
                return Err(OperationJournalStoreError::AlreadyExists);
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
        diagnosis_ids: Vec<DiagnosisId>,
    ) -> Result<(), OperationJournalStoreError> {
        let mutation = self.mutation_gate.clone().lock_owned().await;
        let ticket = self.update(operation_id, WriteUrgency::Immediate, |entry| {
            apply_guardian_evidence(entry, fact_ids, diagnosis_ids);
            Ok(())
        })?;
        self.await_commit(ticket, mutation).await
    }

    pub(crate) async fn record_idempotent_guardian_evidence(
        &self,
        operation_id: &OperationId,
        fact_ids: Vec<String>,
        diagnosis_ids: Vec<DiagnosisId>,
    ) -> Result<(), OperationJournalStoreError> {
        let mutation = self.mutation_gate.clone().lock_owned().await;
        let ticket = self.update(operation_id, WriteUrgency::Immediate, |entry| {
            if operation_journal_status_is_terminal(entry.status) {
                return Err(OperationJournalStoreError::AlreadyTerminal);
            }
            apply_idempotent_guardian_evidence(entry, fact_ids, diagnosis_ids);
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
            .get_mut(operation_id)
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
        candidate: BTreeMap<OperationId, OperationJournalEntry>,
        urgency: WriteUrgency,
    ) -> Result<Option<PendingJournalCommit>, OperationJournalStoreError> {
        let snapshot = OperationJournalSnapshot::new(candidate.values().cloned().collect())?;
        let encoded = encode_snapshot(snapshot).map_err(OperationJournalStoreError::Persistence)?;
        let ticket = self
            .persistence
            .as_ref()
            .map(|persistence| {
                persistence
                    .writer
                    .accept_encoded(encoded, urgency)
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
            candidate.insert(entry.operation_id.clone(), entry);
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
        let mut attempts = 0usize;
        while self.has_retry_candidate() && attempts < OPERATION_JOURNAL_TRANSITION_RETRY_ATTEMPTS {
            attempts += 1;
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
                    if attempts < OPERATION_JOURNAL_TRANSITION_RETRY_ATTEMPTS {
                        tokio::time::sleep(delay).await;
                    }
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

#[cfg(test)]
fn test_journal_record_directory(
    paths: &AppPaths,
) -> Result<AnchoredRecordDirectory, OperationJournalStoreError> {
    let root_session = crate::state::test_root_session(paths);
    let directory = root_session
        .prepare_persisted_state_directories()
        .map(|directories| directories.operation_journal_parent())
        .map_err(OperationJournalStoreError::Persistence)?;
    Ok(AnchoredRecordDirectory::from_directory(
        root_session,
        directory,
    ))
}

fn apply_guardian_evidence(
    entry: &mut OperationJournalEntry,
    fact_ids: Vec<String>,
    diagnosis_ids: Vec<DiagnosisId>,
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

fn apply_idempotent_guardian_evidence(
    entry: &mut OperationJournalEntry,
    fact_ids: Vec<String>,
    diagnosis_ids: Vec<DiagnosisId>,
) {
    if !fact_ids.is_empty() {
        let evidence_index = entry
            .completed_steps
            .iter()
            .position(|step| step.step_id == "guardian_evidence")
            .unwrap_or_else(|| {
                let mut step =
                    OperationJournalStep::new("guardian_evidence", OperationPhase::Running);
                step.result = OperationStepResult::Completed;
                entry.completed_steps.push(step);
                entry.completed_steps.len() - 1
            });
        let evidence = &mut entry.completed_steps[evidence_index];
        for fact_id in fact_ids {
            if !evidence.generated_facts.contains(&fact_id) {
                evidence.generated_facts.push(fact_id);
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
    pub fn new(mut entries: Vec<OperationJournalEntry>) -> Result<Self, OperationJournalLoadError> {
        if entries.iter().all(|entry| entry.sequence == 0) {
            for (index, entry) in entries.iter_mut().enumerate() {
                entry.sequence = u64::try_from(index)
                    .ok()
                    .and_then(|index| index.checked_add(1))
                    .ok_or(OperationJournalLoadError::InvalidSequence)?;
            }
        }
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
        let mut operation_ids = BTreeSet::new();
        let mut sequences = BTreeSet::new();
        for entry in &self.entries {
            validate_entry(entry)?;
            if !operation_ids.insert(entry.operation_id.clone()) {
                return Err(OperationJournalLoadError::DuplicateOperationId);
            }
            if entry.sequence == 0 || !sequences.insert(entry.sequence) {
                return Err(OperationJournalLoadError::InvalidSequence);
            }
        }
        Ok(())
    }
}

#[derive(Debug)]
pub enum OperationJournalLoadError {
    Json(serde_json::Error),
    InvalidSchema,
    TooLarge,
    TooManyEntries,
    InvalidEntry(OperationJournalValidationError),
    DuplicateOperationId,
    InvalidSequence,
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
    InvalidParentOperationId,
    UnsafeTargetId,
    UnsafeStepId,
    UnsafeGeneratedFact,
    UnsafeFailurePoint,
    EmptyJournal,
    TooManyTargets,
    TooManyPlannedSteps,
    TooManyCompletedSteps,
    TooManyFacts,
    TooManyDiagnoses,
    InvalidReconciliationTerminal,
    ReconciliationTerminalMismatch,
    InvalidPersistedStateRepair,
    PersistedStateRepairMismatch,
}

fn validate_entry(entry: &OperationJournalEntry) -> Result<(), OperationJournalValidationError> {
    if !safe_token(entry.journal_id.as_str(), 128) {
        return Err(OperationJournalValidationError::UnsafeJournalId);
    }
    if entry.parent_operation_id.as_ref() == Some(&entry.operation_id) {
        return Err(OperationJournalValidationError::InvalidParentOperationId);
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
    if entry.guardian_diagnosis_ids.len() > MAX_OPERATION_JOURNAL_DIAGNOSES {
        return Err(OperationJournalValidationError::TooManyDiagnoses);
    }
    if (entry.reconciliation_attempt().is_some() || entry.reconciliation_terminal().is_some())
        && (entry.persisted_state_repair_attempt().is_some()
            || entry.persisted_state_repair_terminal().is_some())
    {
        return Err(OperationJournalValidationError::PersistedStateRepairMismatch);
    }
    if (entry.command == CommandKind::RepairPersistedState)
        != entry.persisted_state_repair_attempt().is_some()
    {
        return Err(OperationJournalValidationError::PersistedStateRepairMismatch);
    }
    if let Some(attempt) = entry.reconciliation_attempt() {
        validate_reconciliation_attempt(entry, attempt)?;
    }
    if let Some(terminal) = entry.reconciliation_terminal() {
        terminal
            .validate()
            .map_err(|_| OperationJournalValidationError::InvalidReconciliationTerminal)?;
        if entry.reconciliation_attempt() != Some(terminal.attempt()) {
            return Err(OperationJournalValidationError::ReconciliationTerminalMismatch);
        }
        let completed_quarantine = entry.completed_steps.iter().any(|step| {
            step.step_id == "quarantine_launcher_managed_target"
                && step.result == OperationStepResult::Completed
        });
        let terminal_has_quarantine = !terminal.quarantine_checkpoint().is_empty();
        if completed_quarantine != terminal_has_quarantine {
            return Err(OperationJournalValidationError::ReconciliationTerminalMismatch);
        }
        let expected = match terminal.outcome() {
            ReconciliationTerminalOutcome::Succeeded => {
                (OperationStatus::Succeeded, OperationOutcome::Succeeded)
            }
            ReconciliationTerminalOutcome::Failed => {
                (OperationStatus::Failed, OperationOutcome::Failed)
            }
        };
        if (entry.status, entry.outcome) != (expected.0, Some(expected.1)) {
            return Err(OperationJournalValidationError::ReconciliationTerminalMismatch);
        }
        match terminal.outcome() {
            ReconciliationTerminalOutcome::Succeeded
                if entry.failure_point.is_some()
                    || !entry.completed_steps.last().is_some_and(|step| {
                        step.result == OperationStepResult::Completed
                            && step.changed_target.as_ref() == Some(terminal.target())
                    }) =>
            {
                return Err(OperationJournalValidationError::ReconciliationTerminalMismatch);
            }
            ReconciliationTerminalOutcome::Failed
                if entry.failure_point.is_none()
                    || !entry.completed_steps.last().is_some_and(|step| {
                        step.result == OperationStepResult::Failed
                            && step.changed_target.as_ref() == Some(terminal.target())
                    }) =>
            {
                return Err(OperationJournalValidationError::ReconciliationTerminalMismatch);
            }
            _ => {}
        }
    } else if entry.command == CommandKind::RepairInstance
        && entry.owner == StabilizationSystem::Guardian
        && matches!(
            entry.status,
            OperationStatus::Succeeded | OperationStatus::Failed
        )
    {
        return Err(OperationJournalValidationError::ReconciliationTerminalMismatch);
    }
    if let Some(attempt) = entry.persisted_state_repair_attempt() {
        attempt
            .validate()
            .map_err(|_| OperationJournalValidationError::InvalidPersistedStateRepair)?;
        if attempt.operation_id() != &entry.operation_id
            || entry.journal_id != attempt.journal_id()
            || entry.command != CommandKind::RepairPersistedState
            || entry.owner != StabilizationSystem::Guardian
            || entry.ownership != OwnershipClass::LauncherManaged
            || entry.targets != [attempt.target().clone()]
            || entry.guardian_diagnosis_ids != [DiagnosisId::PersistedStateSchemaInvalid]
            || entry.rollback != RollbackState::NotApplicable
            || entry.planned_steps.len() != 1
            || entry.planned_steps[0].step_id != "quarantine_rejected_restart_record"
            || entry.planned_steps[0].phase != OperationPhase::Repairing
            || entry.planned_steps[0].result != OperationStepResult::Planned
            || entry.planned_steps[0].changed_target.is_some()
            || !entry.planned_steps[0].generated_facts.is_empty()
            || entry.planned_steps[0].rollback != RollbackState::NotApplicable
        {
            return Err(OperationJournalValidationError::PersistedStateRepairMismatch);
        }
        if entry.persisted_state_repair_terminal().is_none()
            && (entry.status != OperationStatus::Planned
                || !entry.completed_steps.is_empty()
                || entry.failure_point.is_some()
                || entry.outcome.is_some())
        {
            return Err(OperationJournalValidationError::PersistedStateRepairMismatch);
        }
    }
    if let Some(terminal) = entry.persisted_state_repair_terminal() {
        terminal
            .validate()
            .map_err(|_| OperationJournalValidationError::InvalidPersistedStateRepair)?;
        if entry.persisted_state_repair_attempt() != Some(terminal.attempt())
            || entry.completed_steps.len() != 1
            || entry.completed_steps[0].step_id != "quarantine_rejected_restart_record"
            || entry.completed_steps[0].phase != OperationPhase::Repairing
            || entry.completed_steps[0].changed_target.as_ref() != Some(terminal.attempt().target())
            || entry.completed_steps[0].rollback != RollbackState::NotApplicable
        {
            return Err(OperationJournalValidationError::PersistedStateRepairMismatch);
        }
        let shape = persisted_state_repair_terminal_shape(terminal.outcome());
        if entry.status != shape.status
            || entry.outcome != Some(shape.outcome)
            || entry.completed_steps[0].result != shape.step_result
            || entry.completed_steps[0].generated_facts != [shape.fact]
            || entry.failure_point.as_deref() != shape.failure_point
        {
            return Err(OperationJournalValidationError::PersistedStateRepairMismatch);
        }
    } else if entry.command == CommandKind::RepairPersistedState
        && operation_journal_status_is_terminal(entry.status)
    {
        return Err(OperationJournalValidationError::PersistedStateRepairMismatch);
    }
    Ok(())
}

struct PersistedStateRepairJournalTerminalShape {
    status: OperationStatus,
    outcome: OperationOutcome,
    step_result: OperationStepResult,
    failure_point: Option<&'static str>,
    fact: &'static str,
}

fn persisted_state_repair_terminal_shape(
    outcome: PersistedStateRepairTerminalOutcome,
) -> PersistedStateRepairJournalTerminalShape {
    match outcome {
        PersistedStateRepairTerminalOutcome::Quarantined => {
            PersistedStateRepairJournalTerminalShape {
                status: OperationStatus::Succeeded,
                outcome: OperationOutcome::Succeeded,
                step_result: OperationStepResult::Completed,
                failure_point: None,
                fact: "persisted_state_record_quarantined",
            }
        }
        PersistedStateRepairTerminalOutcome::Refused => PersistedStateRepairJournalTerminalShape {
            status: OperationStatus::Failed,
            outcome: OperationOutcome::Failed,
            step_result: OperationStepResult::Failed,
            failure_point: Some("persisted_state_quarantine_refused"),
            fact: "persisted_state_quarantine_refused",
        },
        PersistedStateRepairTerminalOutcome::AppliedUnverified => {
            PersistedStateRepairJournalTerminalShape {
                status: OperationStatus::Failed,
                outcome: OperationOutcome::Failed,
                step_result: OperationStepResult::Failed,
                failure_point: Some("persisted_state_quarantine_applied_unverified"),
                fact: "persisted_state_quarantine_applied_unverified",
            }
        }
    }
}

fn validate_reconciliation_attempt(
    entry: &OperationJournalEntry,
    attempt: &ReconciliationAttempt,
) -> Result<(), OperationJournalValidationError> {
    attempt
        .validate()
        .map_err(|_| OperationJournalValidationError::InvalidReconciliationTerminal)?;
    if attempt.operation_id() != &entry.operation_id
        || entry.command != CommandKind::RepairInstance
        || entry.owner != StabilizationSystem::Guardian
        || attempt.ownership() != entry.ownership
        || !entry.targets.contains(attempt.target())
        || !entry
            .guardian_diagnosis_ids
            .contains(&attempt.diagnosis_id())
    {
        return Err(OperationJournalValidationError::ReconciliationTerminalMismatch);
    }
    let ReconciliationScope::RegisteredInstance { instance_id, .. } = attempt.scope();
    if !entry.targets.iter().any(|target| {
        target.system == StabilizationSystem::State
            && target.kind == TargetKind::Instance
            && target.id == *instance_id
            && target.ownership == attempt.ownership()
    }) {
        return Err(OperationJournalValidationError::ReconciliationTerminalMismatch);
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
    if step.generated_facts.len() > MAX_OPERATION_JOURNAL_STEP_FACTS {
        return Err(OperationJournalValidationError::TooManyFacts);
    }
    for fact in &step.generated_facts {
        if !safe_generated_fact(fact) {
            return Err(OperationJournalValidationError::UnsafeGeneratedFact);
        }
    }
    Ok(())
}

fn safe_generated_fact(value: &str) -> bool {
    if value.contains(PERFORMANCE_PLAN_GRAPH_SHA512_FACT_PREFIX) {
        return safe_performance_plan_graph_sha512_fact(value);
    }
    if value.contains(GUARDIAN_OUTCOME_MEMORY_BINDING_PREFIX) {
        return safe_guardian_outcome_memory_binding(value);
    }
    safe_public_fragment(value, 320)
}

fn safe_performance_plan_graph_sha512_fact(value: &str) -> bool {
    value
        .strip_prefix(PERFORMANCE_PLAN_GRAPH_SHA512_FACT_PREFIX)
        .is_some_and(|digest| {
            digest.len() == 128
                && digest
                    .bytes()
                    .all(|value| value.is_ascii_digit() || (b'a'..=b'f').contains(&value))
        })
}

fn safe_guardian_outcome_memory_binding(value: &str) -> bool {
    value
        .strip_prefix(GUARDIAN_OUTCOME_MEMORY_BINDING_PREFIX)
        .is_some_and(|binding| {
            binding.len() == 64
                && binding
                    .bytes()
                    .all(|value| value.is_ascii_digit() || (b'a'..=b'f').contains(&value))
        })
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
    records: &mut BTreeMap<OperationId, OperationJournalEntry>,
    max_entries: usize,
    protected_key: Option<&OperationId>,
) -> bool {
    while records.len() > max_entries {
        let Some(key) = records
            .iter()
            .filter(|(key, entry)| {
                protected_key != Some(*key)
                    && operation_journal_status_is_terminal(entry.status)
                    && !active_reconciliation_terminal(entry)
            })
            .min_by_key(|(_, entry)| entry.sequence)
            .map(|(key, _)| *key)
        else {
            return false;
        };
        records.remove(&key);
    }
    true
}

fn next_journal_sequence(
    records: &BTreeMap<OperationId, OperationJournalEntry>,
) -> Result<u64, OperationJournalStoreError> {
    records
        .values()
        .map(|entry| entry.sequence)
        .max()
        .unwrap_or(0)
        .checked_add(1)
        .ok_or(OperationJournalStoreError::SequenceExhausted)
}

fn active_reconciliation_terminal(entry: &OperationJournalEntry) -> bool {
    entry
        .reconciliation_terminal()
        .and_then(|terminal| {
            chrono::DateTime::parse_from_rfc3339(terminal.suppression_until()).ok()
        })
        .is_some_and(|until| until > chrono::Utc::now())
        || entry
            .persisted_state_repair_terminal()
            .and_then(|terminal| {
                chrono::DateTime::parse_from_rfc3339(terminal.suppression_until()).ok()
            })
            .is_some_and(|until| until > chrono::Utc::now())
}

#[cfg(test)]
pub(crate) fn operation_journal_path(paths: &AppPaths) -> PathBuf {
    paths.operation_journal_file().to_path_buf()
}

fn encode_snapshot(snapshot: OperationJournalSnapshot) -> io::Result<Vec<u8>> {
    let encoded = snapshot
        .to_json()
        .map(String::into_bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if encoded.len() as u64 > MAX_OPERATION_JOURNAL_SNAPSHOT_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "operation journal snapshot exceeds its persistence bound",
        ));
    }
    Ok(encoded)
}

#[cfg(test)]
mod tests {
    use super::{
        OPERATION_JOURNAL_LOCK_INVARIANT, OPERATION_JOURNAL_SCHEMA,
        OperationJournalReconciliation, OperationJournalSnapshot, OperationJournalStore,
        OperationJournalStoreError, operation_journal_path, operation_journal_plan_is_visible,
        safe_generated_fact,
    };
    use crate::execution::persistence::{AtomicWriteBackend, PersistenceCoordinator};
    use crate::guardian::DiagnosisId;
    use crate::state::contracts::{
        CommandKind, JournalId, OperationId, OperationJournalEntry, OperationJournalStep,
        OperationOutcome, OperationPhase, OperationStatus, OwnershipClass,
        PersistedStateRepairTerminalOutcome, ReconciliationComponent, ReconciliationRung,
        ReconciliationScope, ReconciliationTerminalOutcome, RollbackState, StabilizationSystem,
        TargetDescriptor, TargetKind,
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

    const OPERATION_JOURNALS_V6_FIXTURE: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/guardian/operation-journals-v6.json"
    ));

    #[test]
    fn guardian_memory_binding_generated_fact_has_exact_safe_shape() {
        let binding = "0123456789abcdef".repeat(4);
        let fact = format!("guardian_outcome_memory_binding:{binding}");
        assert!(safe_generated_fact(&fact));

        assert!(!safe_generated_fact(&format!(
            "guardian_outcome_memory_binding:{}",
            binding.to_ascii_uppercase()
        )));
        assert!(!safe_generated_fact(&format!(
            "guardian_outcome_memory_binding:{}",
            &binding[..63]
        )));
        assert!(!safe_generated_fact(&format!(
            "guardian_outcome_memory_binding:{binding}0"
        )));
        assert!(!safe_generated_fact(&format!(
            "guardian_outcome_memory_binding:{}g",
            &binding[..63]
        )));
        assert!(!safe_generated_fact(&format!("x{fact}")));
        assert!(!safe_generated_fact(&format!("{fact}:suffix")));
    }

    #[test]
    fn performance_plan_graph_generated_fact_has_exact_safe_shape() {
        let digest = "0123456789abcdef".repeat(8);
        let fact = format!("performance_plan_graph_sha512_{digest}");
        assert!(safe_generated_fact(&fact));

        assert!(!safe_generated_fact(&format!(
            "performance_plan_graph_sha512_{}",
            digest.to_ascii_uppercase()
        )));
        assert!(!safe_generated_fact(&format!(
            "performance_plan_graph_sha512_{}",
            &digest[..127]
        )));
        assert!(!safe_generated_fact(&format!(
            "performance_plan_graph_sha512_{digest}0"
        )));
        assert!(!safe_generated_fact(&format!(
            "performance_plan_graph_sha512_{}g",
            &digest[..127]
        )));
        assert!(!safe_generated_fact(&format!("x{fact}")));
        assert!(!safe_generated_fact(&format!("{fact}:suffix")));
        assert!(!safe_generated_fact(&format!("opaque_identity_{digest}")));
    }

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
            destination: &crate::execution::anchored_record::AnchoredRecordTarget,
            effects: &axial_fs::EffectOwner,
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
            destination.write(effects, contents)
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
    async fn p01_b01_contract() {
        let canonical = "op-123e4567-e89b-42d3-a456-426614174000";
        let operation_id = OperationId::try_from(canonical).expect("canonical operation id");
        assert_eq!(operation_id.to_string(), canonical);
        assert!(OperationId::try_from("op-123E4567-e89b-42d3-a456-426614174000").is_err());
        assert!(OperationId::try_from("op-123e4567-e89b-12d3-a456-426614174000").is_err());
        assert!(OperationId::try_from("123e4567-e89b-42d3-a456-426614174000").is_err());

        let store = OperationJournalStore::new();
        let entry = planned_entry(&operation_id);
        store
            .create_fresh(entry.clone())
            .await
            .expect("fresh identity is admitted once");
        assert!(matches!(
            store.create_fresh(entry.clone()).await,
            Err(OperationJournalStoreError::AlreadyExists)
        ));
        store
            .create(entry)
            .await
            .expect("explicit resume remains idempotent");

        let snapshot = store.snapshot().expect("valid v6 snapshot");
        assert_eq!(snapshot.schema, OPERATION_JOURNAL_SCHEMA);
        assert_eq!(snapshot.entries.len(), 1);
        assert_eq!(snapshot.entries[0].sequence, 1);
        assert_eq!(snapshot.entries[0].operation_id, operation_id);
        let encoded = snapshot.to_json().expect("encode v6 snapshot");
        let decoded = OperationJournalSnapshot::from_json(&encoded).expect("decode v6 snapshot");
        assert_eq!(decoded, snapshot);

        let concurrent_id = OperationId::try_from("op-123e4567-e89b-42d3-b456-426614174001")
            .expect("canonical concurrent operation id");
        let concurrent_entry = planned_entry(&concurrent_id);
        let (left, right) = tokio::join!(
            store.create_fresh(concurrent_entry.clone()),
            store.create_fresh(concurrent_entry),
        );
        assert_eq!(usize::from(left.is_ok()) + usize::from(right.is_ok()), 1);
        assert!(matches!(
            left.as_ref().err().or_else(|| right.as_ref().err()),
            Some(OperationJournalStoreError::AlreadyExists)
        ));
    }

    #[tokio::test]
    async fn journal_store_creates_updates_and_reads_records() {
        let store = OperationJournalStore::new();
        let operation_id = OperationId::deterministic_test("operation-1");
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
    async fn latest_and_retention_use_store_sequence_not_random_operation_identity() {
        let store = OperationJournalStore::with_max_entries(2);
        let first_id = OperationId::deterministic_test("sequence-first");
        let second_id = OperationId::deterministic_test("sequence-second");
        let third_id = OperationId::deterministic_test("sequence-third");

        for (operation_id, step_id) in
            [
                (first_id.clone(), "first_done"),
                (second_id.clone(), "second_done"),
            ]
        {
            store
                .create(planned_entry(&operation_id))
                .await
                .expect("create terminal candidate");
            store
                .record_success(
                    &operation_id,
                    completed_step(step_id),
                    OperationOutcome::Succeeded,
                )
                .await
                .expect("terminalize candidate");
        }
        assert_eq!(
            store
                .latest_for_command(CommandKind::InstallVersion)
                .expect("latest command")
                .operation_id,
            second_id
        );

        store
            .create(planned_entry(&third_id))
            .await
            .expect("create third");
        assert!(store.get(&first_id).is_none(), "oldest sequence is pruned");
        assert!(store.get(&second_id).is_some());
        assert!(store.get(&third_id).is_some());
    }

    #[tokio::test]
    async fn terminal_journal_outcome_is_immutable_after_success() {
        let store = OperationJournalStore::new();
        let operation_id = OperationId::deterministic_test("operation-terminal-success");
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
        let operation_id = OperationId::deterministic_test("operation-terminal-failure");
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

    #[tokio::test]
    async fn success_with_guardian_evidence_is_one_terminal_transition() {
        let store = OperationJournalStore::new();
        let operation_id = OperationId::deterministic_test("integrity-sweep-atomic-success");
        store
            .create(planned_entry(&operation_id))
            .await
            .expect("create planned journal");
        let mut step = completed_step("tier2_integrity_sweep");
        step.generated_facts
            .push("integrity_counter:processed_entry_count:1".to_string());

        store
            .record_success_with_guardian_evidence(
                &operation_id,
                step,
                vec!["guardian_fact:artifact_hash_mismatch".to_string()],
                vec![DiagnosisId::LauncherManagedArtifactCorrupt],
            )
            .await
            .expect("record atomic terminal evidence");

        let stored = store.get(&operation_id).expect("terminal journal");
        assert_eq!(stored.status, OperationStatus::Succeeded);
        assert_eq!(stored.outcome, Some(OperationOutcome::Succeeded));
        assert_eq!(stored.completed_steps.len(), 1);
        assert_eq!(
            stored.completed_steps[0].generated_facts,
            vec![
                "integrity_counter:processed_entry_count:1",
                "guardian_fact:artifact_hash_mismatch",
            ]
        );
        assert_eq!(
            stored.guardian_diagnosis_ids,
            vec![DiagnosisId::LauncherManagedArtifactCorrupt]
        );
    }

    #[tokio::test]
    async fn cancellation_atomically_replaces_nonterminal_findings() {
        let store = OperationJournalStore::new();
        let operation_id = OperationId::deterministic_test("integrity-sweep-atomic-cancel");
        store
            .create(planned_entry(&operation_id))
            .await
            .expect("create planned journal");
        let mut prior = completed_step("obsolete_progress");
        prior
            .generated_facts
            .push("guardian_fact:artifact_missing".to_string());
        store
            .record_checkpoint(&operation_id, prior)
            .await
            .expect("record nonterminal evidence");
        store
            .record_guardian_evidence(
                &operation_id,
                Vec::new(),
                vec![DiagnosisId::LauncherManagedArtifactCorrupt],
            )
            .await
            .expect("record nonterminal diagnosis");
        let mut cancelled =
            OperationJournalStep::new("tier2_integrity_sweep", OperationPhase::Validating);
        cancelled.result = crate::state::contracts::OperationStepResult::Skipped;
        cancelled
            .generated_facts
            .push("integrity_counter:processed_entry_count:0".to_string());

        store
            .record_cancellation(&operation_id, cancelled)
            .await
            .expect("record atomic cancellation");

        let stored = store.get(&operation_id).expect("cancelled journal");
        assert_eq!(stored.status, OperationStatus::Cancelled);
        assert_eq!(stored.outcome, Some(OperationOutcome::Cancelled));
        assert_eq!(stored.completed_steps.len(), 1);
        assert_eq!(
            stored.completed_steps[0].generated_facts,
            vec!["integrity_counter:processed_entry_count:0"]
        );
        assert!(stored.guardian_diagnosis_ids.is_empty());
        assert_eq!(stored.failure_point, None);
    }

    #[test]
    fn operation_journal_snapshot_round_trips_strict_shape() {
        let entry = test_entry("operation-1");
        let snapshot = OperationJournalSnapshot::new(vec![entry]).expect("snapshot");
        let assigned = snapshot.entries[0].clone();
        assert!(assigned.sequence > 0);
        let encoded = snapshot.to_json().expect("serialize snapshot");
        let decoded = OperationJournalSnapshot::from_json(&encoded).expect("deserialize snapshot");

        assert_eq!(decoded.entries, vec![assigned]);

        let mut unknown_field = serde_json::to_value(&snapshot).expect("snapshot value");
        unknown_field["entries"][0]
            .as_object_mut()
            .expect("journal entry object")
            .insert("unexpected".to_string(), serde_json::Value::Bool(true));
        assert!(OperationJournalSnapshot::from_json(&unknown_field.to_string()).is_err());
    }

    #[test]
    fn checked_in_operation_journals_v6_fixture_is_strict() {
        let snapshot = OperationJournalSnapshot::from_json(OPERATION_JOURNALS_V6_FIXTURE)
            .expect("strict fixture");
        assert_eq!(
            super::OPERATION_JOURNAL_SCHEMA,
            "axial.state.operation_journals.v6"
        );
        assert_eq!(snapshot.schema, "axial.state.operation_journals.v6");
        let diagnosis_ids = snapshot
            .entries
            .iter()
            .take(3)
            .flat_map(|entry| entry.guardian_diagnosis_ids.iter())
            .copied()
            .collect::<Vec<_>>();
        assert_eq!(diagnosis_ids.as_slice(), DiagnosisId::ALL.as_slice());

        let attempts = snapshot
            .entries
            .iter()
            .filter_map(OperationJournalEntry::reconciliation_attempt)
            .collect::<Vec<_>>();
        assert_eq!(attempts.len(), 2, "fixture must exercise both typed rungs");
        assert_eq!(
            attempts
                .iter()
                .map(|attempt| attempt.rung())
                .collect::<Vec<_>>(),
            vec![
                ReconciliationRung::RepairArtifact,
                ReconciliationRung::RebuildComponent,
            ]
        );
        assert_eq!(
            attempts
                .iter()
                .map(|attempt| attempt.component())
                .collect::<Vec<_>>(),
            vec![
                ReconciliationComponent::Libraries,
                ReconciliationComponent::Runtime,
            ]
        );
        let ReconciliationScope::RegisteredInstance {
            instance_id: artifact_instance_id,
            fingerprint: artifact_fingerprint,
            ..
        } = attempts[0].scope();
        assert_eq!(artifact_instance_id, "0123456789abcdef");
        assert_eq!(
            artifact_fingerprint.as_str(),
            "sha256.aaaaaaaa.bbbbbbbb.cccccccc.dddddddd.eeeeeeee.ffffffff.01234567.89abcdef"
        );
        let ReconciliationScope::RegisteredInstance {
            instance_id,
            fingerprint,
            ..
        } = attempts[1].scope();
        assert_eq!(instance_id, "0123456789abcdef");
        assert_eq!(
            fingerprint.as_str(),
            "sha256.aaaaaaaa.bbbbbbbb.cccccccc.dddddddd.eeeeeeee.ffffffff.01234567.89abcdef"
        );

        let terminals = snapshot
            .entries
            .iter()
            .filter_map(OperationJournalEntry::reconciliation_terminal)
            .collect::<Vec<_>>();
        assert_eq!(terminals.len(), 2, "fixture must exercise typed terminals");
        assert_eq!(
            terminals
                .iter()
                .map(|terminal| terminal.outcome())
                .collect::<Vec<_>>(),
            vec![
                ReconciliationTerminalOutcome::Failed,
                ReconciliationTerminalOutcome::Succeeded,
            ]
        );
        assert!(terminals[0].quarantine_checkpoint().is_empty());
        assert!(!terminals[1].quarantine_checkpoint().is_empty());

        let persisted_state_terminals = snapshot
            .entries
            .iter()
            .filter_map(OperationJournalEntry::persisted_state_repair_terminal)
            .collect::<Vec<_>>();
        assert_eq!(persisted_state_terminals.len(), 1);
        assert_eq!(
            persisted_state_terminals[0].outcome(),
            PersistedStateRepairTerminalOutcome::Refused
        );

        let mut unknown_snapshot =
            serde_json::from_str::<serde_json::Value>(OPERATION_JOURNALS_V6_FIXTURE)
                .expect("fixture value");
        unknown_snapshot["entries"][0]["guardian_diagnosis_ids"][0] =
            serde_json::Value::String("future_diagnosis".to_string());
        let error = OperationJournalSnapshot::from_json(&unknown_snapshot.to_string())
            .expect_err("embedded unknown diagnosis must be rejected");
        let error = format!("{error:?}");
        assert!(!error.contains("future_diagnosis"));

        let pretty = serde_json::to_string_pretty(&snapshot).expect("pretty fixture json");
        assert_eq!(format!("{pretty}\n"), OPERATION_JOURNALS_V6_FIXTURE);

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
        let store = OperationJournalStore::try_load_from_paths(&paths)
            .expect("load operation journal persistence");
        let operation_id = OperationId::deterministic_test("install-operation-restart-replay");
        let mut entry = test_entry(&operation_id.to_string());
        entry.operation_id = operation_id.clone();
        entry.journal_id = JournalId::new(format!("journal-{operation_id}"));

        store.create(entry).await.expect("create journal");
        store
            .record_guardian_evidence(
                &operation_id,
                vec![
                    "guardian_outcome_decision:retry".to_string(),
                    "guardian_outcome_summary:Guardian treated install download failure as retryable."
                        .to_string(),
                ],
                vec![DiagnosisId::DownloadUnavailable],
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
        let reloaded = OperationJournalStore::try_load_from_paths(&paths)
            .expect("reload operation journal persistence");
        let loaded = reloaded.get(&operation_id).expect("reloaded journal");
        assert_eq!(loaded.operation_id, operation_id);
        assert_eq!(loaded.status, OperationStatus::Succeeded);
        assert!(
            loaded
                .guardian_diagnosis_ids
                .contains(&DiagnosisId::DownloadUnavailable)
        );

        cleanup(&root);
    }

    #[test]
    fn journal_store_preserves_future_schema_and_fails_closed() {
        let root = test_root("preserve-future-schema");
        let paths = test_paths(&root);
        let path = operation_journal_path(&paths);
        fs::create_dir_all(path.parent().expect("journal parent")).expect("create journal parent");
        let future = r#"{"schema":"axial.state.operation_journals.v7","entries":[]}"#;
        fs::write(&path, future).expect("write future journal snapshot");

        let result = OperationJournalStore::try_load_from_paths(&paths);
        assert!(matches!(
            result,
            Err(OperationJournalStoreError::Snapshot(
                super::OperationJournalLoadError::InvalidSchema
            ))
        ));
        assert_eq!(
            fs::read_to_string(&path).expect("future snapshot remains available"),
            future
        );
        cleanup(&root);
    }

    #[test]
    fn previous_operation_journal_schema_is_strict_invalid_and_preserved_byte_exact() {
        let legacy = OPERATION_JOURNALS_V6_FIXTURE.replacen(
            "axial.state.operation_journals.v6",
            "axial.state.operation_journals.v5",
            1,
        );
        assert!(matches!(
            OperationJournalSnapshot::from_json(&legacy),
            Err(super::OperationJournalLoadError::InvalidSchema)
        ));

        let root = test_root("preserve-v5-schema");
        let paths = test_paths(&root);
        let path = operation_journal_path(&paths);
        fs::create_dir_all(path.parent().expect("journal parent")).expect("create journal parent");
        fs::write(&path, legacy.as_bytes()).expect("write v4 journal snapshot");

        assert!(matches!(
            OperationJournalStore::try_load_from_paths(&paths),
            Err(OperationJournalStoreError::Snapshot(
                super::OperationJournalLoadError::InvalidSchema
            ))
        ));
        assert_eq!(
            fs::read(&path).expect("v5 journal remains"),
            legacy.as_bytes()
        );
        cleanup(&root);
    }

    #[test]
    fn journal_store_preserves_malformed_and_invalid_current_snapshots() {
        let cases = [
            (
                "malformed-json",
                format!(r#"{{"schema":"{}""#, super::OPERATION_JOURNAL_SCHEMA),
            ),
            ("invalid-current", {
                let mut value = serde_json::to_value(
                    OperationJournalSnapshot::new(vec![test_entry("operation-invalid-current")])
                        .expect("valid snapshot"),
                )
                .expect("snapshot value");
                value["entries"][0]["operation_id"] =
                    serde_json::Value::String("../unsafe".to_string());
                value.to_string()
            }),
        ];

        for (name, rejected) in cases {
            let root = test_root(name);
            let paths = test_paths(&root);
            let path = operation_journal_path(&paths);
            fs::create_dir_all(path.parent().expect("journal parent"))
                .expect("create journal parent");
            fs::write(&path, &rejected).expect("write rejected current journal snapshot");

            assert!(OperationJournalStore::try_load_from_paths(&paths).is_err());
            assert_eq!(
                fs::read_to_string(&path).expect("rejected snapshot remains available"),
                rejected
            );
            cleanup(&root);
        }
    }

    #[test]
    fn journal_store_rejects_oversized_snapshot_without_reading_or_replacing_it() {
        let root = test_root("oversized-snapshot");
        let paths = test_paths(&root);
        let path = operation_journal_path(&paths);
        fs::create_dir_all(path.parent().expect("journal parent")).expect("create journal parent");
        let file = fs::File::create(&path).expect("create oversized journal snapshot");
        file.set_len(super::MAX_OPERATION_JOURNAL_SNAPSHOT_BYTES + 1)
            .expect("size oversized journal snapshot");
        drop(file);

        assert!(matches!(
            OperationJournalStore::try_load_from_paths(&paths),
            Err(OperationJournalStoreError::Snapshot(
                super::OperationJournalLoadError::TooLarge
            ))
        ));
        assert_eq!(
            fs::metadata(&path)
                .expect("oversized snapshot remains")
                .len(),
            super::MAX_OPERATION_JOURNAL_SNAPSHOT_BYTES + 1
        );
        cleanup(&root);
    }

    #[test]
    fn journal_snapshot_encoder_accepts_the_exact_size_bound_and_rejects_one_more_byte() {
        let mut snapshot = OperationJournalSnapshot {
            schema: String::new(),
            entries: Vec::new(),
        };
        let overhead = serde_json::to_vec(&snapshot)
            .expect("serialize empty snapshot")
            .len();
        snapshot.schema =
            "x".repeat(super::MAX_OPERATION_JOURNAL_SNAPSHOT_BYTES as usize - overhead);

        let encoded = super::encode_snapshot(snapshot.clone()).expect("accept exact size bound");
        assert_eq!(
            encoded.len() as u64,
            super::MAX_OPERATION_JOURNAL_SNAPSHOT_BYTES
        );

        snapshot.schema.push('x');

        assert_eq!(
            super::encode_snapshot(snapshot)
                .expect_err("oversized snapshots must not be persisted")
                .kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[tokio::test]
    async fn oversized_valid_candidate_is_rejected_before_acceptance_without_latching_retry() {
        let (root, _paths, backend, _coordinator, store) =
            persistence_fixture("oversized-candidate-preflight");
        let mut oversized = test_entry("operation-oversized-candidate");
        let fact = "\u{1f600}".repeat(320);
        oversized.completed_steps = (0..110)
            .map(|index| {
                let mut step = completed_step(&format!("oversized-step-{index}"));
                step.generated_facts = vec![fact.clone(); super::MAX_OPERATION_JOURNAL_STEP_FACTS];
                step
            })
            .collect();
        assert!(super::validate_entry(&oversized).is_ok());

        let error = store
            .create(oversized)
            .await
            .expect_err("reject oversized valid candidate before acceptance");
        assert!(matches!(
            error,
            OperationJournalStoreError::Persistence(error)
                if error.kind() == io::ErrorKind::InvalidData
        ));
        assert!(!store.has_retry_candidate());
        assert_eq!(backend.attempts.load(Ordering::SeqCst), 0);
        assert!(store.list().is_empty());

        let accepted = OperationId::deterministic_test("operation-after-oversized-candidate");
        store
            .create(planned_entry(&accepted))
            .await
            .expect("later bounded candidate remains writable");
        assert_eq!(backend.attempts.load(Ordering::SeqCst), 1);
        assert!(store.get(&accepted).is_some());
        store.close().await.expect("close recovered journal store");
        cleanup(&root);
    }

    #[tokio::test]
    async fn journal_store_retention_evicts_only_terminal_entries() {
        let store = OperationJournalStore::with_max_entries(2);
        let pinned = OperationId::deterministic_test("operation-pinned");
        store
            .create(planned_entry(&pinned))
            .await
            .expect("create pinned journal");

        for index in 0..16 {
            let operation_id = OperationId::deterministic_test(format!("operation-terminal-{index:02}"));
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
                .create(planned_entry(&OperationId::deterministic_test(id)))
                .await
                .expect("create journal");
        }
        let before = store.snapshot().expect("snapshot before rejection");

        assert!(matches!(
            store
                .create(planned_entry(&OperationId::deterministic_test("operation-active-3")))
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
        let operation_id = OperationId::deterministic_test("operation-terminal-duplicate");
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
        let operation_id = OperationId::deterministic_test("operation-invalid-update");
        store
            .create(test_entry(&operation_id.to_string()))
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
        let operation_id = OperationId::deterministic_test("operation-gated-initial");
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
        let operation_id = OperationId::deterministic_test("operation-cancelled-terminal");
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
        let terminal_id = OperationId::deterministic_test("operation-terminal-a");
        let progress_id = OperationId::deterministic_test("operation-progress-b");
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
        let operation_id = OperationId::deterministic_test("operation-progress-burst");
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
        let operation_id = OperationId::deterministic_test("operation-close-progress-retry");
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
        let operation_id = OperationId::deterministic_test("operation-failure-retry");
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
        let operation_id = OperationId::deterministic_test("operation-close-retry");
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
        let operation_id = OperationId::deterministic_test("operation-reconcile-own");
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

    #[tokio::test]
    async fn reconciliation_retains_a_permanent_candidate_without_retrying_forever() {
        let (root, _paths, backend, _coordinator, store) =
            persistence_fixture("reconcile-permanent-failure");
        let operation_id = OperationId::deterministic_test("operation-reconcile-permanent-failure");
        store
            .create(planned_entry(&operation_id))
            .await
            .expect("create journal");
        for _ in 0..=super::OPERATION_JOURNAL_TRANSITION_RETRY_ATTEMPTS {
            backend.fail_next();
        }
        let error = store
            .record_success(
                &operation_id,
                completed_step("never_persisted"),
                OperationOutcome::Succeeded,
            )
            .await
            .expect_err("initial terminal persistence fails");

        let result = tokio::time::timeout(
            Duration::from_secs(1),
            store.reconcile_transition(
                &operation_id,
                error,
                Duration::from_millis(1),
                Duration::from_millis(2),
                |entry| entry.status == OperationStatus::Succeeded,
            ),
        )
        .await
        .expect("bounded journal reconciliation");
        assert!(matches!(
            result,
            Err(OperationJournalStoreError::Persistence(_))
        ));
        assert!(store.has_retry_candidate());
        assert_eq!(
            store
                .get(&operation_id)
                .expect("last durable journal")
                .status,
            OperationStatus::Planned
        );

        drop(store);
        cleanup(&root);
    }

    #[test]
    fn planned_transition_rejects_an_advanced_journal_to_prevent_effect_replay() {
        let operation_id = OperationId::deterministic_test("operation-plan-visible-after-progress");
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
        let requested_id = OperationId::deterministic_test("operation-reconcile-requested");
        let foreign_id = OperationId::deterministic_test("operation-reconcile-foreign");
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
        let operation_id = OperationId::deterministic_test("operation-poisoned");
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
                .create(planned_entry(&OperationId::deterministic_test(
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
            JournalId::new(format!("journal-{operation_id}")),
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
        let operation_id = OperationId::deterministic_test(operation_id);
        let mut entry = OperationJournalEntry::new(
            JournalId::new(format!("journal-{operation_id}")),
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
        AppPaths::from_root(root.to_path_buf()).expect("absolute test app root")
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
