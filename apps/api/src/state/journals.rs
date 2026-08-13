use super::contracts::{
    CommandKind, JournalId, OperationId, OperationIntent, OperationJournalEntry,
    OperationJournalStep, OperationOutcome, OperationPhase, OperationStatus, OperationStepResult,
    OwnershipClass, PerformanceOperationAction, PerformanceOperationIntent,
    PerformanceOperationLifecycle, PerformanceOperationPhase, PerformanceOperationPrepared,
    PerformanceOperationTerminal, PerformancePreparedProof, PersistedStateRepairAttempt,
    PersistedStateRepairTerminal, PersistedStateRepairTerminalOutcome,
    RECONCILIATION_EVIDENCE_CAPACITY, ReconciliationAttempt, ReconciliationLineage,
    ReconciliationScope, ReconciliationTerminal, ReconciliationTerminalOutcome, RollbackState,
    StabilizationSystem, TargetDescriptor, TargetKind,
};
use super::successors::OPERATION_JOURNAL_SUCCESSOR;
use crate::execution::anchored_record::AnchoredRecordDirectory;
use crate::execution::persistence::{
    AcceptedWrite, AtomicSnapshotWriter, PersistenceCoordinator, PersistenceOwnerLease,
    WriteUrgency,
};
use crate::guardian::DiagnosisId;
use crate::logging::timestamp_utc;
use crate::observability::{
    RedactionAudience, evidence_text_looks_sensitive, sanitize_evidence_text,
};
#[cfg(test)]
use axial_config::AppPaths;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::io;
#[cfg(test)]
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::sync::Mutex as AsyncMutex;
use tracing::warn;

pub const OPERATION_JOURNAL_SCHEMA: &str = "axial.state.operation_journals.v8";
pub const DEFAULT_OPERATION_JOURNAL_LIMIT: usize = RECONCILIATION_EVIDENCE_CAPACITY;
pub(crate) const MAX_OPERATION_JOURNAL_STEP_FACTS: usize = 64;
pub(crate) const PERFORMANCE_PLAN_GRAPH_SHA512_FACT_PREFIX: &str = "performance_plan_graph_sha512_";
const GUARDIAN_OUTCOME_MEMORY_BINDING_PREFIX: &str = "guardian_outcome_memory_binding:";
const INSTALL_PUBLICATION_EVIDENCE_FACT_PREFIX: &str = "install_publication_evidence:";
const INSTALL_PUBLICATION_VERSION_ID_FACT_PREFIX: &str = "install_publication_version_id:";
const INSTALL_ACTIVATION_CONTRACT_FACT_PREFIX: &str = "install_activation_contract:";
const INSTALL_VERSION_ID_FACT_PREFIX: &str = "install_version_id:";
const LOADER_BUILD_ID_FACT_PREFIX: &str = "loader_build_id:";
const OPERATION_JOURNAL_SNAPSHOT_NAME: &str = "operation-journals.json";
pub(crate) const MAX_OPERATION_JOURNAL_DIAGNOSES: usize = 32;
const MAX_OPERATION_JOURNAL_SNAPSHOT_BYTES: u64 = 8 * 1024 * 1024;
const OPERATION_JOURNAL_LOCK_INVARIANT: &str =
    "operation journal records lock poisoned; in-memory and persisted state may diverge";
const OPERATION_JOURNAL_TRANSITION_RETRY_ATTEMPTS: usize = 4;
const OPERATION_ID_MINT_ATTEMPTS: usize = 8;

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
    #[error("another operation already owns this target lifecycle")]
    Conflict,
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
            Self::Conflict => "conflict",
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

#[derive(Debug, thiserror::Error)]
pub(crate) enum PerformanceOperationCreateError {
    #[error(transparent)]
    BeforeAdmission(#[from] OperationJournalStoreError),
    #[error("performance operation was admitted but its commit failed")]
    AfterAdmission {
        operation_id: OperationId,
        #[source]
        source: OperationJournalStoreError,
    },
}

impl From<OperationJournalValidationError> for PerformanceOperationCreateError {
    fn from(error: OperationJournalValidationError) -> Self {
        Self::BeforeAdmission(error.into())
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
        && entry.intent == expected.intent
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
    fn claim(directory: AnchoredRecordDirectory) -> Result<Self, OperationJournalStoreError> {
        Self::claim_with_coordinator(directory, PersistenceCoordinator::current())
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
            .and_then(|record| OPERATION_JOURNAL_SUCCESSOR.bind(record))
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OperationRestartDisposition {
    AbandonedBeforeEffect,
    Resumable,
    AppliedUnverified,
    TerminalIntent,
    Terminal,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum PerformanceOperationTransition {
    Planning,
    Prepared(PerformanceOperationPrepared),
    EffectStarted,
    MarkAppliedUnverified(String),
    RequestTerminal(PerformanceOperationTerminal),
    CommitTerminal(PerformanceOperationTerminal),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PerformanceOperationProjection {
    pub operation_id: OperationId,
    pub sequence: u64,
    pub intent: PerformanceOperationIntent,
    pub phase: PerformanceOperationPhase,
    pub state: &'static str,
    pub error: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub terminal: bool,
    pub restart: OperationRestartDisposition,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct PerformanceRestartPlan {
    pub resumable: Vec<PerformanceOperationProjection>,
    pub applied_unverified: Vec<PerformanceOperationProjection>,
}

struct OperationJournalRecords {
    visible: BTreeMap<OperationId, OperationJournalEntry>,
    visible_revision: u64,
    next_sequence: u64,
    retry_candidate: Option<(u64, BTreeMap<OperationId, OperationJournalEntry>, u64)>,
}

impl Default for OperationJournalRecords {
    fn default() -> Self {
        Self {
            visible: BTreeMap::new(),
            visible_revision: 0,
            next_sequence: 1,
            retry_candidate: None,
        }
    }
}

struct PendingJournalCommit {
    ticket: AcceptedWrite,
    revision: u64,
    candidate: BTreeMap<OperationId, OperationJournalEntry>,
    next_sequence: u64,
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
        Self::try_load_from_directory_with_coordinator(directory, coordinator)
    }

    #[cfg(test)]
    pub(crate) fn try_load_from_directory_with_coordinator(
        directory: AnchoredRecordDirectory,
        coordinator: PersistenceCoordinator,
    ) -> Result<Self, OperationJournalStoreError> {
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
        let observation = match directory.read(
            std::ffi::OsStr::new(OPERATION_JOURNAL_SNAPSHOT_NAME),
            MAX_OPERATION_JOURNAL_SNAPSHOT_BYTES,
        ) {
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

    pub(crate) async fn create_performance(
        &self,
        intent: PerformanceOperationIntent,
    ) -> Result<PerformanceOperationProjection, PerformanceOperationCreateError> {
        let mutation = self.mutation_gate.clone().lock_owned().await;
        validate_performance_intent(&intent)?;
        let operation_id = (0..OPERATION_ID_MINT_ATTEMPTS)
            .map(|_| OperationId::mint())
            .find(|candidate| {
                !self
                    .records
                    .read()
                    .expect(OPERATION_JOURNAL_LOCK_INVARIANT)
                    .visible
                    .contains_key(candidate)
            })
            .ok_or(OperationJournalStoreError::SequenceExhausted)?;
        let now = timestamp_utc();
        let mut entry = OperationJournalEntry::new(
            super::contracts::JournalId::new(format!("journal-{operation_id}")),
            operation_id.clone(),
            CommandKind::ApplyPerformancePlan,
            StabilizationSystem::Application,
            OwnershipClass::CompositionManaged,
            intent.rollback,
        );
        entry.targets = vec![
            TargetDescriptor::new(
                StabilizationSystem::State,
                TargetKind::Instance,
                &intent.instance_id,
                OwnershipClass::CompositionManaged,
            ),
            TargetDescriptor::new(
                StabilizationSystem::Performance,
                TargetKind::PerformanceComposition,
                &intent.base_target_id,
                OwnershipClass::CompositionManaged,
            ),
        ];
        entry.intent = OperationIntent::Performance(PerformanceOperationLifecycle {
            intent: intent.clone(),
            phase: PerformanceOperationPhase::Accepted {},
            created_at: now.clone(),
            updated_at: now,
        });
        validate_entry(&entry)?;
        let ticket = {
            let mut records = self
                .records
                .write()
                .expect(OPERATION_JOURNAL_LOCK_INVARIANT);
            if records.retry_candidate.is_some() {
                return Err(OperationJournalStoreError::RetryRequired.into());
            }
            if records.visible.values().any(|candidate| {
                !operation_journal_status_is_terminal(candidate.status)
                    && candidate
                        .performance_lifecycle()
                        .is_some_and(|candidate| candidate.intent.instance_id == intent.instance_id)
            }) {
                return Err(OperationJournalStoreError::Conflict.into());
            }
            if records.visible.contains_key(&operation_id) {
                return Err(OperationJournalStoreError::AlreadyExists.into());
            }
            let next_sequence = records
                .next_sequence
                .checked_add(1)
                .ok_or(OperationJournalStoreError::SequenceExhausted)?;
            entry.sequence = records.next_sequence;
            let mut candidate = records.visible.clone();
            candidate.insert(operation_id.clone(), entry);
            if !prune_records(&mut candidate, self.max_entries, Some(&operation_id)) {
                return Err(OperationJournalStoreError::CapacityExhausted.into());
            }
            self.accept_candidate(
                &mut records,
                candidate,
                next_sequence,
                WriteUrgency::Immediate,
            )?
        };
        if let Err(source) = self.await_commit(ticket, mutation).await {
            return self
                .reconcile_performance_create(&operation_id, &intent, source)
                .await
                .map_err(|source| PerformanceOperationCreateError::AfterAdmission {
                    operation_id,
                    source,
                });
        }
        self.performance_operation(&operation_id)
            .ok_or(OperationJournalStoreError::MissingOperation.into())
    }

    pub(crate) async fn reconcile_performance_create(
        &self,
        operation_id: &OperationId,
        intent: &PerformanceOperationIntent,
        source: OperationJournalStoreError,
    ) -> Result<PerformanceOperationProjection, OperationJournalStoreError> {
        validate_performance_intent(intent)?;
        self.reconcile_transition(
            operation_id,
            source,
            Duration::from_millis(20),
            Duration::from_secs(1),
            |entry| performance_create_is_visible(entry, intent),
        )
        .await?;
        self.get(operation_id)
            .filter(|entry| performance_create_is_visible(entry, intent))
            .and_then(|entry| performance_operation_projection(&entry))
            .ok_or(OperationJournalStoreError::MissingOperation)
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
            let next_sequence = records
                .next_sequence
                .checked_add(1)
                .ok_or(OperationJournalStoreError::SequenceExhausted)?;
            entry.sequence = records.next_sequence;
            let operation_key = entry.operation_id.clone();
            let mut candidate = records.visible.clone();
            candidate.insert(operation_key.clone(), entry);
            if !prune_records(&mut candidate, self.max_entries, Some(&operation_key)) {
                return Err(OperationJournalStoreError::CapacityExhausted);
            }
            self.accept_candidate(
                &mut records,
                candidate,
                next_sequence,
                WriteUrgency::Immediate,
            )?
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
            let next_sequence = records
                .next_sequence
                .checked_add(1)
                .ok_or(OperationJournalStoreError::SequenceExhausted)?;
            entry.sequence = records.next_sequence;
            let operation_key = entry.operation_id.clone();
            let mut candidate = records.visible.clone();
            candidate.insert(operation_key.clone(), entry);
            if !prune_records(&mut candidate, self.max_entries, Some(&operation_key)) {
                return Err(OperationJournalStoreError::CapacityExhausted);
            }
            self.accept_candidate(
                &mut records,
                candidate,
                next_sequence,
                WriteUrgency::Immediate,
            )?
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

    pub(crate) fn performance_operation(
        &self,
        operation_id: &OperationId,
    ) -> Option<PerformanceOperationProjection> {
        self.get(operation_id)
            .and_then(|entry| performance_operation_projection(&entry))
    }

    pub(crate) fn current_or_latest_performance_operation(
        &self,
        instance_id: &str,
    ) -> Option<PerformanceOperationProjection> {
        self.records
            .read()
            .expect(OPERATION_JOURNAL_LOCK_INVARIANT)
            .visible
            .values()
            .filter_map(performance_operation_projection)
            .filter(|projection| projection.intent.instance_id == instance_id)
            .max_by_key(|projection| (!projection.terminal, projection.sequence))
    }

    pub(crate) async fn transition_performance(
        &self,
        operation_id: &OperationId,
        transition: PerformanceOperationTransition,
    ) -> Result<(), OperationJournalStoreError> {
        let intent = self
            .get(operation_id)
            .and_then(|entry| {
                entry
                    .performance_lifecycle()
                    .map(|lifecycle| lifecycle.intent.clone())
            })
            .ok_or(OperationJournalStoreError::MissingOperation)?;
        let transition = sanitize_performance_transition(&intent, transition)?;
        let mut foreign_retries = 0;
        loop {
            let mutation = self.mutation_gate.clone().lock_owned().await;
            if self
                .get(operation_id)
                .as_ref()
                .is_some_and(|entry| performance_transition_is_visible(entry, &transition))
            {
                return Ok(());
            }
            let ticket = self.update_performance(operation_id, WriteUrgency::Immediate, |entry| {
                apply_performance_transition(entry, &transition)
            });
            let error = match ticket {
                Ok(ticket) => match self.await_commit(ticket, mutation).await {
                    Ok(()) => return Ok(()),
                    Err(error) => error,
                },
                Err(error) => {
                    drop(mutation);
                    error
                }
            };
            if !matches!(
                error,
                OperationJournalStoreError::Persistence(_)
                    | OperationJournalStoreError::RetryRequired
            ) {
                return Err(error);
            }
            match self
                .reconcile_transition(
                    operation_id,
                    error,
                    Duration::from_millis(20),
                    Duration::from_secs(1),
                    |entry| performance_transition_is_visible(entry, &transition),
                )
                .await?
            {
                OperationJournalReconciliation::RetryRequestedTransition
                    if foreign_retries < OPERATION_JOURNAL_TRANSITION_RETRY_ATTEMPTS =>
                {
                    foreign_retries += 1;
                }
                OperationJournalReconciliation::RetryRequestedTransition => {
                    return Err(OperationJournalStoreError::RetryRequired);
                }
                OperationJournalReconciliation::CommittedAfterPersistenceFailure(_)
                | OperationJournalReconciliation::RequestedTransitionAlreadyCommitted => {
                    return Ok(());
                }
            }
        }
    }

    pub(crate) async fn settle_performance_restarts(
        &self,
    ) -> Result<PerformanceRestartPlan, OperationJournalStoreError> {
        const APPLIED_UNVERIFIED_ERROR: &str =
            "performance operation outcome could not be confirmed after restart";

        loop {
            let mutation = self.mutation_gate.clone().lock_owned().await;
            if self.has_retry_candidate() {
                drop(mutation);
                self.retry().await?;
                continue;
            }
            let ticket = {
                let mut records = self
                    .records
                    .write()
                    .expect(OPERATION_JOURNAL_LOCK_INVARIANT);
                let mut candidate = records.visible.clone();
                let now = timestamp_utc();
                let mut changed = false;
                for entry in candidate.values_mut() {
                    let Some(phase) = entry
                        .performance_lifecycle()
                        .map(|lifecycle| lifecycle.phase.clone())
                    else {
                        continue;
                    };
                    let terminal = match phase {
                        PerformanceOperationPhase::Accepted {}
                        | PerformanceOperationPhase::Planning {} => {
                            Some(PerformanceOperationTerminal::AbandonedBeforeEffect {})
                        }
                        PerformanceOperationPhase::EffectStarted { prepared } => {
                            let OperationIntent::Performance(lifecycle) = &mut entry.intent else {
                                unreachable!("performance lifecycle disappeared")
                            };
                            lifecycle.phase = PerformanceOperationPhase::AppliedUnverified {
                                prepared,
                                error: APPLIED_UNVERIFIED_ERROR.to_string(),
                            };
                            lifecycle.updated_at = now.clone();
                            apply_performance_phase_shape(entry);
                            validate_entry(entry)?;
                            changed = true;
                            None
                        }
                        PerformanceOperationPhase::TerminalIntent { terminal } => Some(terminal),
                        PerformanceOperationPhase::Prepared { .. }
                        | PerformanceOperationPhase::AppliedUnverified { .. }
                        | PerformanceOperationPhase::Terminal { .. } => None,
                    };
                    let Some(terminal) = terminal else {
                        continue;
                    };
                    let OperationIntent::Performance(lifecycle) = &mut entry.intent else {
                        unreachable!("performance lifecycle disappeared")
                    };
                    lifecycle.phase = PerformanceOperationPhase::Terminal { terminal };
                    lifecycle.updated_at = now.clone();
                    apply_performance_phase_shape(entry);
                    validate_entry(entry)?;
                    changed = true;
                }
                if changed {
                    let next_sequence = records.next_sequence;
                    self.accept_candidate(
                        &mut records,
                        candidate,
                        next_sequence,
                        WriteUrgency::Immediate,
                    )?
                } else {
                    None
                }
            };
            match self.await_commit(ticket, mutation).await {
                Ok(()) => return Ok(self.performance_restart_plan()),
                Err(_error) if self.has_retry_candidate() => {
                    self.retry().await?;
                    return Ok(self.performance_restart_plan());
                }
                Err(error) => return Err(error),
            }
        }
    }

    fn performance_restart_plan(&self) -> PerformanceRestartPlan {
        let mut plan = PerformanceRestartPlan::default();
        for projection in self
            .records
            .read()
            .expect(OPERATION_JOURNAL_LOCK_INVARIANT)
            .visible
            .values()
            .filter_map(performance_operation_projection)
        {
            match projection.restart {
                OperationRestartDisposition::Resumable => plan.resumable.push(projection),
                OperationRestartDisposition::AppliedUnverified => {
                    plan.applied_unverified.push(projection)
                }
                OperationRestartDisposition::AbandonedBeforeEffect
                | OperationRestartDisposition::TerminalIntent
                | OperationRestartDisposition::Terminal => {}
            }
        }
        plan.resumable.sort_by_key(|projection| projection.sequence);
        plan.applied_unverified
            .sort_by_key(|projection| projection.sequence);
        plan
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

    pub(super) async fn acknowledge_reconciliation_version_bundle_publication(
        &self,
        expected: &ReconciliationTerminal,
    ) -> Result<ReconciliationTerminal, OperationJournalStoreError> {
        let publication = expected
            .version_bundle_publication()
            .ok_or(OperationJournalValidationError::ReconciliationTerminalMismatch)?;
        if !publication.is_pending() {
            return self
                .get(expected.operation_id())
                .and_then(|entry| entry.reconciliation_terminal().cloned())
                .filter(|current| current == expected)
                .ok_or(OperationJournalValidationError::ReconciliationTerminalMismatch.into());
        }
        let evidence = publication.evidence();
        let acknowledged = expected
            .clone()
            .with_acknowledged_version_bundle_publication(evidence)
            .map_err(|_| OperationJournalValidationError::ReconciliationTerminalMismatch)?;
        let mutation = self.mutation_gate.clone().lock_owned().await;
        if self
            .get(expected.operation_id())
            .as_ref()
            .and_then(OperationJournalEntry::reconciliation_terminal)
            == Some(&acknowledged)
        {
            return Ok(acknowledged);
        }
        let ticket = self.update_post_terminal_obligation(
            expected.operation_id(),
            WriteUrgency::Immediate,
            |entry| {
                match entry.reconciliation_terminal() {
                    Some(current) if current == expected || current == &acknowledged => {}
                    _ => {
                        return Err(
                            OperationJournalValidationError::ReconciliationTerminalMismatch.into(),
                        );
                    }
                }
                entry.reconciliation_terminal = Some(acknowledged.clone());
                Ok(())
            },
        )?;
        self.await_commit(ticket, mutation).await?;
        Ok(acknowledged)
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
        {
            let records = self.records.read().expect(OPERATION_JOURNAL_LOCK_INVARIANT);
            if records.retry_candidate.is_some() {
                return Err(OperationJournalStoreError::RetryRequired);
            }
            let entry = records
                .visible
                .get(operation_id)
                .ok_or(OperationJournalStoreError::MissingOperation)?;
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
        }
        let ticket = self.update(operation_id, WriteUrgency::Immediate, |entry| {
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
        let ticket =
            self.update_post_terminal_obligation(operation_id, WriteUrgency::Immediate, |entry| {
                apply_guardian_evidence(entry, fact_ids, diagnosis_ids);
                Ok(())
            })?;
        self.await_commit(ticket, mutation).await
    }

    pub(crate) async fn record_performance_guardian_evidence(
        &self,
        operation_id: &OperationId,
        mut fact_ids: Vec<String>,
        mut diagnosis_ids: Vec<DiagnosisId>,
    ) -> Result<(), OperationJournalStoreError> {
        dedup_preserving_order(&mut fact_ids);
        dedup_preserving_order(&mut diagnosis_ids);
        if fact_ids.is_empty() && diagnosis_ids.is_empty() {
            return Ok(());
        }
        let mut foreign_retries = 0;
        loop {
            let mutation = self.mutation_gate.clone().lock_owned().await;
            {
                let records = self.records.read().expect(OPERATION_JOURNAL_LOCK_INVARIANT);
                let entry = records
                    .visible
                    .get(operation_id)
                    .ok_or(OperationJournalStoreError::MissingOperation)?;
                let Some(lifecycle) = entry.performance_lifecycle() else {
                    return Err(OperationJournalStoreError::MissingOperation);
                };
                if operation_journal_status_is_terminal(entry.status) {
                    return Err(OperationJournalStoreError::AlreadyTerminal);
                }
                match lifecycle.phase {
                    PerformanceOperationPhase::Accepted {}
                    | PerformanceOperationPhase::Planning {} => {
                        if entry.completed_steps.is_empty()
                            && entry.guardian_diagnosis_ids.is_empty()
                        {
                            // The first evidence set is accepted below.
                        } else if performance_guardian_evidence_matches(
                            entry,
                            &fact_ids,
                            &diagnosis_ids,
                        ) {
                            return Ok(());
                        } else {
                            return Err(OperationJournalStoreError::AlreadyExists);
                        }
                    }
                    PerformanceOperationPhase::Prepared { .. }
                        if performance_guardian_evidence_matches(
                            entry,
                            &fact_ids,
                            &diagnosis_ids,
                        ) =>
                    {
                        return Ok(());
                    }
                    PerformanceOperationPhase::Prepared { .. }
                    | PerformanceOperationPhase::EffectStarted { .. }
                    | PerformanceOperationPhase::AppliedUnverified { .. }
                    | PerformanceOperationPhase::TerminalIntent { .. }
                    | PerformanceOperationPhase::Terminal { .. } => {
                        return Err(OperationJournalStoreError::AlreadyExists);
                    }
                }
            }
            let ticket = self.update_performance(operation_id, WriteUrgency::Immediate, |entry| {
                let OperationIntent::Performance(lifecycle) = &mut entry.intent else {
                    return Err(OperationJournalStoreError::MissingOperation);
                };
                lifecycle.updated_at = timestamp_utc();
                apply_idempotent_guardian_evidence(entry, fact_ids.clone(), diagnosis_ids.clone());
                Ok(())
            });
            let error = match ticket {
                Ok(ticket) => match self.await_commit(ticket, mutation).await {
                    Ok(()) => return Ok(()),
                    Err(error) => error,
                },
                Err(error) => {
                    drop(mutation);
                    error
                }
            };
            if !matches!(
                error,
                OperationJournalStoreError::Persistence(_)
                    | OperationJournalStoreError::RetryRequired
            ) {
                return Err(error);
            }
            match self
                .reconcile_transition(
                    operation_id,
                    error,
                    Duration::from_millis(20),
                    Duration::from_secs(1),
                    |entry| {
                        performance_guardian_evidence_is_visible(entry, &fact_ids, &diagnosis_ids)
                    },
                )
                .await?
            {
                OperationJournalReconciliation::RetryRequestedTransition
                    if foreign_retries < OPERATION_JOURNAL_TRANSITION_RETRY_ATTEMPTS =>
                {
                    foreign_retries += 1;
                }
                OperationJournalReconciliation::RetryRequestedTransition => {
                    return Err(OperationJournalStoreError::RetryRequired);
                }
                OperationJournalReconciliation::CommittedAfterPersistenceFailure(_)
                | OperationJournalReconciliation::RequestedTransitionAlreadyCommitted => {
                    return Ok(());
                }
            }
        }
    }

    fn update(
        &self,
        operation_id: &OperationId,
        urgency: WriteUrgency,
        update: impl FnOnce(&mut OperationJournalEntry) -> Result<(), OperationJournalStoreError>,
    ) -> Result<Option<PendingJournalCommit>, OperationJournalStoreError> {
        self.update_with_policy(operation_id, urgency, false, false, update)
    }

    fn update_performance(
        &self,
        operation_id: &OperationId,
        urgency: WriteUrgency,
        update: impl FnOnce(&mut OperationJournalEntry) -> Result<(), OperationJournalStoreError>,
    ) -> Result<Option<PendingJournalCommit>, OperationJournalStoreError> {
        self.update_with_policy(operation_id, urgency, false, true, update)
    }

    fn update_post_terminal_obligation(
        &self,
        operation_id: &OperationId,
        urgency: WriteUrgency,
        update: impl FnOnce(&mut OperationJournalEntry) -> Result<(), OperationJournalStoreError>,
    ) -> Result<Option<PendingJournalCommit>, OperationJournalStoreError> {
        self.update_with_policy(operation_id, urgency, true, false, update)
    }

    fn update_with_policy(
        &self,
        operation_id: &OperationId,
        urgency: WriteUrgency,
        post_terminal_obligation: bool,
        performance_transition: bool,
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
        if entry.performance_lifecycle().is_some() && !performance_transition {
            return Err(if operation_journal_status_is_terminal(entry.status) {
                OperationJournalStoreError::AlreadyTerminal
            } else {
                OperationJournalStoreError::AlreadyExists
            });
        }
        if operation_journal_status_is_terminal(entry.status) && !post_terminal_obligation {
            return Err(OperationJournalStoreError::AlreadyTerminal);
        }
        update(entry)?;
        validate_entry(entry)?;
        if !prune_records(&mut candidate, self.max_entries, None) {
            return Err(OperationJournalStoreError::CapacityExhausted);
        }
        let next_sequence = records.next_sequence;
        self.accept_candidate(&mut records, candidate, next_sequence, urgency)
    }

    fn accept_candidate(
        &self,
        records: &mut OperationJournalRecords,
        candidate: BTreeMap<OperationId, OperationJournalEntry>,
        next_sequence: u64,
        urgency: WriteUrgency,
    ) -> Result<Option<PendingJournalCommit>, OperationJournalStoreError> {
        let snapshot = OperationJournalSnapshot::with_next_sequence(
            candidate.values().cloned().collect(),
            next_sequence,
        )?;
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
            records.next_sequence = next_sequence;
            return Ok(None);
        };
        let revision = ticket.revision().get();
        if urgency == WriteUrgency::Debounced {
            records.visible = candidate;
            records.visible_revision = revision;
            records.next_sequence = next_sequence;
            return Ok(None);
        }
        Ok(Some(PendingJournalCommit {
            ticket,
            revision,
            candidate,
            next_sequence,
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
        let records = self.records.read().expect(OPERATION_JOURNAL_LOCK_INVARIANT);
        OperationJournalSnapshot::with_next_sequence(
            records.visible.values().cloned().collect(),
            records.next_sequence,
        )
    }

    pub fn load_snapshot(
        &self,
        snapshot: OperationJournalSnapshot,
    ) -> Result<(), OperationJournalLoadError> {
        snapshot.validate()?;
        let next_sequence = snapshot.next_sequence;
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
        records.next_sequence = next_sequence;
        records.retry_candidate = None;
        Ok(())
    }

    pub async fn flush(&self) -> Result<(), OperationJournalStoreError> {
        let mut mutation = self.mutation_gate.clone().lock_owned().await;
        if self.has_retry_candidate() {
            mutation = self.retry_holding_gate(mutation).await?;
        }
        if let Some(persistence) = &self.persistence {
            persistence
                .owner
                .flush()
                .await
                .map_err(|error| OperationJournalStoreError::Persistence(error.into()))?;
        }
        drop(mutation);
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
                .filter(|(candidate_revision, _, _)| *candidate_revision == revision)
                .map(|(_, candidate, next_sequence)| (candidate.clone(), *next_sequence))
                .unwrap_or_else(|| (records.visible.clone(), records.next_sequence))
        };
        let (candidate, next_sequence) = candidate;
        self.await_commit_holding_gate(
            Some(PendingJournalCommit {
                ticket,
                revision,
                candidate,
                next_sequence,
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
                        records.next_sequence = commit.next_sequence;
                    }
                    records.retry_candidate = None;
                    Ok(())
                }
                Err(error) => {
                    records
                        .write()
                        .expect(OPERATION_JOURNAL_LOCK_INVARIANT)
                        .retry_candidate =
                        Some((commit.revision, commit.candidate, commit.next_sequence));
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
    pub next_sequence: u64,
    pub entries: Vec<OperationJournalEntry>,
}

impl OperationJournalSnapshot {
    pub fn new(
        entries: Vec<OperationJournalEntry>,
        next_sequence: u64,
    ) -> Result<Self, OperationJournalLoadError> {
        let snapshot = Self {
            schema: OPERATION_JOURNAL_SCHEMA.to_string(),
            next_sequence,
            entries,
        };
        snapshot.validate()?;
        Ok(snapshot)
    }

    fn with_next_sequence(
        entries: Vec<OperationJournalEntry>,
        next_sequence: u64,
    ) -> Result<Self, OperationJournalLoadError> {
        let snapshot = Self {
            schema: OPERATION_JOURNAL_SCHEMA.to_string(),
            next_sequence,
            entries,
        };
        snapshot.validate()?;
        Ok(snapshot)
    }

    pub fn from_json(value: &str) -> Result<Self, OperationJournalLoadError> {
        if super::persisted_snapshot_schema(value)? != OPERATION_JOURNAL_SCHEMA {
            return Err(OperationJournalLoadError::InvalidSchema);
        }
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
        let mut active_performance_instances = BTreeSet::new();
        let mut maximum_sequence = 0;
        for entry in &self.entries {
            validate_entry(entry)?;
            if !operation_ids.insert(entry.operation_id.clone()) {
                return Err(OperationJournalLoadError::DuplicateOperationId);
            }
            if entry.sequence == 0 || !sequences.insert(entry.sequence) {
                return Err(OperationJournalLoadError::InvalidSequence);
            }
            if !operation_journal_status_is_terminal(entry.status)
                && let Some(lifecycle) = entry.performance_lifecycle()
                && !active_performance_instances.insert(&lifecycle.intent.instance_id)
            {
                return Err(OperationJournalLoadError::InvalidEntry(
                    OperationJournalValidationError::InvalidPerformanceLifecycle,
                ));
            }
            maximum_sequence = maximum_sequence.max(entry.sequence);
        }
        if self.next_sequence == 0 || self.next_sequence <= maximum_sequence {
            return Err(OperationJournalLoadError::InvalidSequence);
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
    InvalidPerformanceLifecycle,
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
    match &entry.intent {
        OperationIntent::Performance(lifecycle) => {
            validate_performance_lifecycle(entry, lifecycle)?
        }
        OperationIntent::Generic {} if entry.command == CommandKind::ApplyPerformancePlan => {
            return Err(OperationJournalValidationError::InvalidPerformanceLifecycle);
        }
        OperationIntent::Generic {} => {}
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

fn validate_performance_intent(
    intent: &PerformanceOperationIntent,
) -> Result<(), OperationJournalValidationError> {
    if !axial_config::is_canonical_instance_id(&intent.instance_id)
        || !safe_token(&intent.base_target_id, 96)
        || !performance_requested_action_is_consistent(intent)
        || !performance_intent_identity_is_consistent(intent)
        || intent
            .game_version
            .as_deref()
            .is_some_and(|value| !performance_public_fragment_is_canonical(value, 96))
        || intent
            .loader
            .as_deref()
            .is_some_and(|value| !performance_public_fragment_is_canonical(value, 96))
        || intent
            .mode
            .as_deref()
            .is_some_and(|value| !performance_public_fragment_is_canonical(value, 96))
        || intent
            .rollback_id
            .as_deref()
            .is_some_and(|value| !canonical_safe_token(value, 96))
    {
        return Err(OperationJournalValidationError::InvalidPerformanceLifecycle);
    }
    Ok(())
}

fn performance_requested_action_is_consistent(intent: &PerformanceOperationIntent) -> bool {
    matches!(
        (intent.requested_action, intent.action),
        (
            PerformanceOperationAction::Install,
            PerformanceOperationAction::Install | PerformanceOperationAction::Remove,
        ) | (
            PerformanceOperationAction::Remove,
            PerformanceOperationAction::Remove,
        ) | (
            PerformanceOperationAction::Rollback,
            PerformanceOperationAction::Rollback,
        )
    )
}

fn performance_intent_identity_is_consistent(intent: &PerformanceOperationIntent) -> bool {
    match (intent.action, intent.rollback) {
        (
            PerformanceOperationAction::Install,
            RollbackState::Available | RollbackState::Unavailable,
        ) => true,
        (PerformanceOperationAction::Remove, RollbackState::Available) => {
            intent.base_target_id != "performance_composition_lock"
        }
        (PerformanceOperationAction::Remove, RollbackState::Unavailable) => {
            intent.base_target_id == "performance_composition_lock"
        }
        (PerformanceOperationAction::Rollback, RollbackState::Available) => {
            intent.base_target_id != "performance_rollback_snapshot"
        }
        (PerformanceOperationAction::Rollback, RollbackState::Unavailable) => {
            intent.base_target_id == "performance_rollback_snapshot"
        }
        (_, RollbackState::NotApplicable | RollbackState::Applied) => false,
    }
}

fn validate_performance_prepared(
    intent: &PerformanceOperationIntent,
    prepared: &PerformanceOperationPrepared,
) -> Result<(), OperationJournalValidationError> {
    if !canonical_safe_token(&prepared.result_target_id, 96)
        || prepared.result_target_id != intent.base_target_id
    {
        return Err(OperationJournalValidationError::InvalidPerformanceLifecycle);
    }
    let valid = match (intent.action, intent.rollback, &prepared.proof) {
        (
            PerformanceOperationAction::Install,
            RollbackState::Available | RollbackState::Unavailable,
            PerformancePreparedProof::InstallPlan {
                graph_sha512,
                artifact_count,
                ..
            },
        ) => performance_graph_proof_is_valid(graph_sha512, *artifact_count),
        (
            PerformanceOperationAction::Remove,
            RollbackState::Available,
            PerformancePreparedProof::RemoveCurrent {
                graph_sha512,
                artifact_count,
            },
        ) => performance_graph_proof_is_valid(graph_sha512, *artifact_count),
        (
            PerformanceOperationAction::Remove,
            RollbackState::Unavailable,
            PerformancePreparedProof::ManagedStateAbsent {},
        ) => true,
        (
            PerformanceOperationAction::Rollback,
            RollbackState::Available,
            PerformancePreparedProof::RollbackSnapshot {
                snapshot_id,
                target,
                artifact_count,
            },
        ) => {
            canonical_safe_token(snapshot_id, 96)
                && *artifact_count <= 1_000_000
                && match target {
                    super::contracts::PerformanceRollbackTarget::ManagedStateAbsent => {
                        prepared.result_target_id == "performance_managed_state_absent"
                    }
                    super::contracts::PerformanceRollbackTarget::ManagedComposition => {
                        prepared.result_target_id != "performance_managed_state_absent"
                    }
                }
                && intent
                    .rollback_id
                    .as_deref()
                    .is_none_or(|requested| requested == snapshot_id)
        }
        _ => false,
    };
    if !valid {
        return Err(OperationJournalValidationError::InvalidPerformanceLifecycle);
    }
    Ok(())
}

fn performance_graph_proof_is_valid(graph_sha512: &str, artifact_count: u64) -> bool {
    artifact_count <= 1_000_000
        && graph_sha512.len() == 128
        && graph_sha512
            .bytes()
            .all(|value| value.is_ascii_digit() || (b'a'..=b'f').contains(&value))
}

fn performance_public_fragment_is_canonical(value: &str, max_chars: usize) -> bool {
    sanitize_evidence_text(value, RedactionAudience::UserVisible, max_chars)
        .is_some_and(|sanitized| sanitized == value)
}

fn canonical_performance_timestamp(value: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    let parsed = chrono::DateTime::parse_from_rfc3339(value).ok()?;
    let utc = parsed.with_timezone(&chrono::Utc);
    (utc.to_rfc3339_opts(chrono::SecondsFormat::Millis, true) == value).then_some(utc)
}

fn performance_terminal_prepared(
    terminal: &PerformanceOperationTerminal,
) -> Option<&PerformanceOperationPrepared> {
    match terminal {
        PerformanceOperationTerminal::Succeeded { prepared, .. }
        | PerformanceOperationTerminal::FailedAfterEffect { prepared, .. } => Some(prepared),
        PerformanceOperationTerminal::FailedBeforeEffect { .. }
        | PerformanceOperationTerminal::AbandonedBeforeEffect {} => None,
    }
}

fn performance_terminal_error(terminal: &PerformanceOperationTerminal) -> Option<String> {
    match terminal {
        PerformanceOperationTerminal::FailedBeforeEffect { error }
        | PerformanceOperationTerminal::FailedAfterEffect { error, .. } => Some(error.clone()),
        PerformanceOperationTerminal::AbandonedBeforeEffect {} => {
            Some("performance operation abandoned before effect".to_string())
        }
        PerformanceOperationTerminal::Succeeded { .. } => None,
    }
}

fn performance_phase_error(phase: &PerformanceOperationPhase) -> Option<String> {
    match phase {
        PerformanceOperationPhase::AppliedUnverified { error, .. } => Some(error.clone()),
        PerformanceOperationPhase::TerminalIntent { terminal }
        | PerformanceOperationPhase::Terminal { terminal } => performance_terminal_error(terminal),
        PerformanceOperationPhase::Accepted {}
        | PerformanceOperationPhase::Planning {}
        | PerformanceOperationPhase::Prepared { .. }
        | PerformanceOperationPhase::EffectStarted { .. } => None,
    }
}

fn validate_performance_terminal(
    intent: &PerformanceOperationIntent,
    terminal: &PerformanceOperationTerminal,
) -> Result<(), OperationJournalValidationError> {
    if let Some(prepared) = performance_terminal_prepared(terminal) {
        validate_performance_prepared(intent, prepared)?;
    }
    if performance_terminal_error(terminal)
        .as_deref()
        .is_some_and(|error| !performance_public_fragment_is_canonical(error, 160))
    {
        return Err(OperationJournalValidationError::InvalidPerformanceLifecycle);
    }
    let valid_shape = match terminal {
        PerformanceOperationTerminal::Succeeded {
            prepared,
            changed_target,
            rollback,
        } => match (&prepared.proof, intent.action, rollback) {
            (
                PerformancePreparedProof::ManagedStateAbsent {},
                PerformanceOperationAction::Remove,
                RollbackState::Unavailable,
            ) => !changed_target,
            (
                PerformancePreparedProof::InstallPlan { .. },
                PerformanceOperationAction::Install,
                rollback,
            ) => {
                (*changed_target && rollback == &RollbackState::Available)
                    || (!changed_target && rollback == &intent.rollback)
            }
            (
                PerformancePreparedProof::RemoveCurrent { .. },
                PerformanceOperationAction::Remove,
                RollbackState::Available,
            ) => *changed_target,
            (
                PerformancePreparedProof::RollbackSnapshot { .. },
                PerformanceOperationAction::Rollback,
                RollbackState::Applied,
            ) => *changed_target,
            _ => false,
        },
        PerformanceOperationTerminal::FailedAfterEffect {
            prepared, rollback, ..
        } => matches!(
            (&prepared.proof, intent.action, rollback),
            (
                PerformancePreparedProof::InstallPlan { .. },
                PerformanceOperationAction::Install,
                RollbackState::Available | RollbackState::Unavailable,
            ) | (
                PerformancePreparedProof::RemoveCurrent { .. },
                PerformanceOperationAction::Remove,
                RollbackState::Available | RollbackState::Unavailable,
            ) | (
                PerformancePreparedProof::RollbackSnapshot { .. },
                PerformanceOperationAction::Rollback,
                RollbackState::Available | RollbackState::Unavailable | RollbackState::Applied,
            )
        ),
        PerformanceOperationTerminal::FailedBeforeEffect { .. }
        | PerformanceOperationTerminal::AbandonedBeforeEffect {} => true,
    };
    if !valid_shape {
        return Err(OperationJournalValidationError::InvalidPerformanceLifecycle);
    }
    Ok(())
}

fn performance_terminal_rollback(
    admitted_rollback: RollbackState,
    terminal: &PerformanceOperationTerminal,
) -> RollbackState {
    match terminal {
        PerformanceOperationTerminal::Succeeded { rollback, .. }
        | PerformanceOperationTerminal::FailedAfterEffect { rollback, .. } => *rollback,
        PerformanceOperationTerminal::FailedBeforeEffect { .. }
        | PerformanceOperationTerminal::AbandonedBeforeEffect {} => admitted_rollback,
    }
}

fn validate_performance_lifecycle(
    entry: &OperationJournalEntry,
    lifecycle: &PerformanceOperationLifecycle,
) -> Result<(), OperationJournalValidationError> {
    validate_performance_intent(&lifecycle.intent)?;
    let created_at = canonical_performance_timestamp(&lifecycle.created_at)
        .ok_or(OperationJournalValidationError::InvalidPerformanceLifecycle)?;
    let updated_at = canonical_performance_timestamp(&lifecycle.updated_at)
        .ok_or(OperationJournalValidationError::InvalidPerformanceLifecycle)?;
    if updated_at < created_at
        || entry.journal_id != JournalId::new(format!("journal-{}", entry.operation_id))
        || entry.parent_operation_id.is_some()
        || entry.command != CommandKind::ApplyPerformancePlan
        || entry.owner != StabilizationSystem::Application
        || entry.ownership != OwnershipClass::CompositionManaged
        || entry.rollback
            != match &lifecycle.phase {
                PerformanceOperationPhase::Terminal { terminal } => {
                    performance_terminal_rollback(lifecycle.intent.rollback, terminal)
                }
                _ => lifecycle.intent.rollback,
            }
        || entry.targets.len() != 2
        || !entry.targets.iter().any(|target| {
            target.system == StabilizationSystem::State
                && target.kind == TargetKind::Instance
                && target.id == lifecycle.intent.instance_id
                && target.ownership == OwnershipClass::CompositionManaged
        })
        || !entry.targets.iter().any(|target| {
            target.system == StabilizationSystem::Performance
                && target.kind == TargetKind::PerformanceComposition
                && target.id == lifecycle.intent.base_target_id
                && target.ownership == OwnershipClass::CompositionManaged
        })
        || !entry.planned_steps.is_empty()
        || !performance_guardian_evidence_is_canonical(entry)
    {
        return Err(OperationJournalValidationError::InvalidPerformanceLifecycle);
    }
    let expected = match &lifecycle.phase {
        PerformanceOperationPhase::Accepted {} => (OperationStatus::Planned, None, None),
        PerformanceOperationPhase::Planning {} => (OperationStatus::Running, None, None),
        PerformanceOperationPhase::Prepared { prepared }
        | PerformanceOperationPhase::EffectStarted { prepared } => {
            validate_performance_prepared(&lifecycle.intent, prepared)?;
            (OperationStatus::Running, None, None)
        }
        PerformanceOperationPhase::AppliedUnverified { prepared, error } => {
            validate_performance_prepared(&lifecycle.intent, prepared)?;
            if !performance_public_fragment_is_canonical(error, 160) {
                return Err(OperationJournalValidationError::InvalidPerformanceLifecycle);
            }
            (OperationStatus::Running, None, None)
        }
        PerformanceOperationPhase::TerminalIntent { terminal } => {
            validate_performance_terminal(&lifecycle.intent, terminal)?;
            (OperationStatus::Running, None, None)
        }
        PerformanceOperationPhase::Terminal { terminal } => {
            validate_performance_terminal(&lifecycle.intent, terminal)?;
            match terminal {
                PerformanceOperationTerminal::Succeeded { .. } => (
                    OperationStatus::Succeeded,
                    Some(OperationOutcome::Succeeded),
                    None,
                ),
                PerformanceOperationTerminal::AbandonedBeforeEffect {} => (
                    OperationStatus::Cancelled,
                    Some(OperationOutcome::Cancelled),
                    Some("performance_operation_abandoned_before_effect"),
                ),
                PerformanceOperationTerminal::FailedBeforeEffect { .. }
                | PerformanceOperationTerminal::FailedAfterEffect { .. } => (
                    OperationStatus::Failed,
                    Some(OperationOutcome::Failed),
                    Some("performance_operation_failed"),
                ),
            }
        }
    };
    if entry.status != expected.0
        || entry.outcome != expected.1
        || entry.failure_point.as_deref() != expected.2
    {
        return Err(OperationJournalValidationError::InvalidPerformanceLifecycle);
    }
    Ok(())
}

fn performance_guardian_evidence_is_canonical(entry: &OperationJournalEntry) -> bool {
    let diagnoses_are_unique = entry
        .guardian_diagnosis_ids
        .iter()
        .enumerate()
        .all(|(index, diagnosis)| !entry.guardian_diagnosis_ids[..index].contains(diagnosis));
    diagnoses_are_unique
        && (entry.completed_steps.is_empty()
            || (entry.completed_steps.len() == 1
                && entry.completed_steps[0].step_id == "guardian_evidence"
                && entry.completed_steps[0].phase == OperationPhase::Running
                && entry.completed_steps[0].result == OperationStepResult::Completed
                && entry.completed_steps[0].changed_target.is_none()
                && entry.completed_steps[0].rollback == RollbackState::NotApplicable
                && !entry.completed_steps[0].generated_facts.is_empty()
                && entry.completed_steps[0]
                    .generated_facts
                    .iter()
                    .enumerate()
                    .all(|(index, fact)| {
                        !entry.completed_steps[0].generated_facts[..index].contains(fact)
                    })))
}

fn performance_guardian_evidence_matches(
    entry: &OperationJournalEntry,
    fact_ids: &[String],
    diagnosis_ids: &[DiagnosisId],
) -> bool {
    let existing_facts = entry
        .completed_steps
        .first()
        .map(|step| step.generated_facts.as_slice())
        .unwrap_or_default();
    existing_facts == fact_ids && entry.guardian_diagnosis_ids == diagnosis_ids
}

fn performance_guardian_evidence_is_visible(
    entry: &OperationJournalEntry,
    fact_ids: &[String],
    diagnosis_ids: &[DiagnosisId],
) -> bool {
    entry.performance_lifecycle().is_some_and(|lifecycle| {
        matches!(
            lifecycle.phase,
            PerformanceOperationPhase::Accepted {} | PerformanceOperationPhase::Planning {}
        ) && performance_guardian_evidence_matches(entry, fact_ids, diagnosis_ids)
    })
}

fn dedup_preserving_order<T: Eq>(values: &mut Vec<T>) {
    let mut index = 0;
    while index < values.len() {
        if values[..index].contains(&values[index]) {
            values.remove(index);
        } else {
            index += 1;
        }
    }
}

fn sanitize_performance_terminal(
    intent: &PerformanceOperationIntent,
    terminal: PerformanceOperationTerminal,
) -> Result<PerformanceOperationTerminal, OperationJournalValidationError> {
    let terminal = match terminal {
        PerformanceOperationTerminal::FailedBeforeEffect { error } => {
            PerformanceOperationTerminal::FailedBeforeEffect {
                error: sanitize_evidence_text(&error, RedactionAudience::UserVisible, 160)
                    .unwrap_or_else(|| "performance operation failed".to_string()),
            }
        }
        PerformanceOperationTerminal::FailedAfterEffect {
            prepared,
            changed_target,
            rollback,
            error,
        } => PerformanceOperationTerminal::FailedAfterEffect {
            prepared,
            changed_target,
            rollback,
            error: sanitize_evidence_text(&error, RedactionAudience::UserVisible, 160)
                .unwrap_or_else(|| "performance operation failed".to_string()),
        },
        terminal => terminal,
    };
    validate_performance_terminal(intent, &terminal)?;
    Ok(terminal)
}

fn sanitize_performance_transition(
    intent: &PerformanceOperationIntent,
    transition: PerformanceOperationTransition,
) -> Result<PerformanceOperationTransition, OperationJournalStoreError> {
    Ok(match transition {
        PerformanceOperationTransition::Prepared(prepared) => {
            validate_performance_prepared(intent, &prepared)?;
            PerformanceOperationTransition::Prepared(prepared)
        }
        PerformanceOperationTransition::MarkAppliedUnverified(error) => {
            PerformanceOperationTransition::MarkAppliedUnverified(
                sanitize_evidence_text(&error, RedactionAudience::UserVisible, 160)
                    .unwrap_or_else(|| "performance operation outcome is unverified".to_string()),
            )
        }
        PerformanceOperationTransition::RequestTerminal(terminal) => {
            PerformanceOperationTransition::RequestTerminal(sanitize_performance_terminal(
                intent, terminal,
            )?)
        }
        PerformanceOperationTransition::CommitTerminal(terminal) => {
            PerformanceOperationTransition::CommitTerminal(sanitize_performance_terminal(
                intent, terminal,
            )?)
        }
        transition => transition,
    })
}

fn performance_post_effect_terminal_matches(
    terminal: &PerformanceOperationTerminal,
    prepared: &PerformanceOperationPrepared,
) -> bool {
    matches!(
        terminal,
        PerformanceOperationTerminal::Succeeded {
            prepared: candidate,
            ..
        } | PerformanceOperationTerminal::FailedAfterEffect {
            prepared: candidate,
            ..
        } if candidate == prepared
    )
}

fn apply_performance_transition(
    entry: &mut OperationJournalEntry,
    transition: &PerformanceOperationTransition,
) -> Result<(), OperationJournalStoreError> {
    let OperationIntent::Performance(lifecycle) = &entry.intent else {
        return Err(OperationJournalStoreError::MissingOperation);
    };
    let next_phase = match (&lifecycle.phase, transition) {
        (PerformanceOperationPhase::Accepted {}, PerformanceOperationTransition::Planning) => {
            PerformanceOperationPhase::Planning {}
        }
        (
            PerformanceOperationPhase::Planning {},
            PerformanceOperationTransition::Prepared(prepared),
        ) => PerformanceOperationPhase::Prepared {
            prepared: prepared.clone(),
        },
        (
            PerformanceOperationPhase::Prepared { prepared },
            PerformanceOperationTransition::EffectStarted,
        ) if !matches!(
            prepared.proof,
            PerformancePreparedProof::ManagedStateAbsent {}
        ) =>
        {
            PerformanceOperationPhase::EffectStarted {
                prepared: prepared.clone(),
            }
        }
        (
            PerformanceOperationPhase::EffectStarted { prepared },
            PerformanceOperationTransition::MarkAppliedUnverified(error),
        ) => PerformanceOperationPhase::AppliedUnverified {
            prepared: prepared.clone(),
            error: error.clone(),
        },
        (
            PerformanceOperationPhase::Prepared { prepared },
            PerformanceOperationTransition::RequestTerminal(
                terminal @ PerformanceOperationTerminal::Succeeded {
                    prepared: requested,
                    changed_target: false,
                    ..
                },
            ),
        ) if matches!(
            prepared.proof,
            PerformancePreparedProof::ManagedStateAbsent {}
                | PerformancePreparedProof::InstallPlan { .. }
        ) && requested == prepared =>
        {
            PerformanceOperationPhase::TerminalIntent {
                terminal: terminal.clone(),
            }
        }
        (
            PerformanceOperationPhase::Accepted {}
            | PerformanceOperationPhase::Planning {}
            | PerformanceOperationPhase::Prepared { .. },
            PerformanceOperationTransition::RequestTerminal(
                terminal @ (PerformanceOperationTerminal::FailedBeforeEffect { .. }
                | PerformanceOperationTerminal::AbandonedBeforeEffect {}),
            ),
        ) => PerformanceOperationPhase::TerminalIntent {
            terminal: terminal.clone(),
        },
        (
            PerformanceOperationPhase::EffectStarted { prepared },
            PerformanceOperationTransition::RequestTerminal(terminal),
        ) if performance_post_effect_terminal_matches(terminal, prepared) => {
            PerformanceOperationPhase::TerminalIntent {
                terminal: terminal.clone(),
            }
        }
        (
            PerformanceOperationPhase::TerminalIntent { terminal: intended },
            PerformanceOperationTransition::CommitTerminal(requested),
        ) if intended == requested => PerformanceOperationPhase::Terminal {
            terminal: intended.clone(),
        },
        _ => return Err(OperationJournalStoreError::AlreadyExists),
    };
    let OperationIntent::Performance(lifecycle) = &mut entry.intent else {
        unreachable!("performance intent was matched above")
    };
    lifecycle.phase = next_phase;
    lifecycle.updated_at = timestamp_utc();
    apply_performance_phase_shape(entry);
    Ok(())
}

fn performance_transition_is_visible(
    entry: &OperationJournalEntry,
    transition: &PerformanceOperationTransition,
) -> bool {
    let Some(lifecycle) = entry.performance_lifecycle() else {
        return false;
    };
    match (transition, &lifecycle.phase) {
        (PerformanceOperationTransition::Planning, phase) => {
            matches!(phase, PerformanceOperationPhase::Planning {})
                || performance_phase_prepared(phase).is_some()
        }
        (PerformanceOperationTransition::Prepared(expected), phase) => {
            performance_phase_prepared(phase) == Some(expected)
        }
        (PerformanceOperationTransition::EffectStarted, phase) => {
            performance_phase_proves_effect_started(phase)
        }
        (
            PerformanceOperationTransition::MarkAppliedUnverified(expected),
            PerformanceOperationPhase::AppliedUnverified { error, .. },
        ) => expected == error,
        (
            PerformanceOperationTransition::RequestTerminal(expected),
            PerformanceOperationPhase::TerminalIntent { terminal }
            | PerformanceOperationPhase::Terminal { terminal },
        ) => expected == terminal,
        (
            PerformanceOperationTransition::CommitTerminal(expected),
            PerformanceOperationPhase::Terminal { terminal },
        ) => expected == terminal,
        _ => false,
    }
}

fn performance_phase_prepared(
    phase: &PerformanceOperationPhase,
) -> Option<&PerformanceOperationPrepared> {
    match phase {
        PerformanceOperationPhase::Prepared { prepared }
        | PerformanceOperationPhase::EffectStarted { prepared }
        | PerformanceOperationPhase::AppliedUnverified { prepared, .. } => Some(prepared),
        PerformanceOperationPhase::TerminalIntent { terminal }
        | PerformanceOperationPhase::Terminal { terminal } => {
            performance_terminal_prepared(terminal)
        }
        PerformanceOperationPhase::Accepted {} | PerformanceOperationPhase::Planning {} => None,
    }
}

fn performance_phase_proves_effect_started(phase: &PerformanceOperationPhase) -> bool {
    match phase {
        PerformanceOperationPhase::EffectStarted { .. }
        | PerformanceOperationPhase::AppliedUnverified { .. } => true,
        PerformanceOperationPhase::TerminalIntent { terminal }
        | PerformanceOperationPhase::Terminal { terminal } => matches!(
            terminal,
            PerformanceOperationTerminal::Succeeded {
                changed_target: true,
                ..
            } | PerformanceOperationTerminal::FailedAfterEffect { .. }
        ),
        PerformanceOperationPhase::Accepted {}
        | PerformanceOperationPhase::Planning {}
        | PerformanceOperationPhase::Prepared { .. } => false,
    }
}

fn performance_create_is_visible(
    entry: &OperationJournalEntry,
    intent: &PerformanceOperationIntent,
) -> bool {
    entry
        .performance_lifecycle()
        .is_some_and(|lifecycle| lifecycle.intent == *intent)
}

fn apply_performance_phase_shape(entry: &mut OperationJournalEntry) {
    let Some((phase, admitted_rollback)) = entry
        .performance_lifecycle()
        .map(|lifecycle| (lifecycle.phase.clone(), lifecycle.intent.rollback))
    else {
        return;
    };
    entry.failure_point = None;
    entry.outcome = None;
    match phase {
        PerformanceOperationPhase::Accepted {} => entry.status = OperationStatus::Planned,
        PerformanceOperationPhase::Planning {}
        | PerformanceOperationPhase::Prepared { .. }
        | PerformanceOperationPhase::EffectStarted { .. }
        | PerformanceOperationPhase::AppliedUnverified { .. }
        | PerformanceOperationPhase::TerminalIntent { .. } => {
            entry.status = OperationStatus::Running;
        }
        PerformanceOperationPhase::Terminal { terminal } => {
            entry.rollback = performance_terminal_rollback(admitted_rollback, &terminal);
            match terminal {
                PerformanceOperationTerminal::Succeeded { .. } => {
                    entry.status = OperationStatus::Succeeded;
                    entry.outcome = Some(OperationOutcome::Succeeded);
                }
                PerformanceOperationTerminal::AbandonedBeforeEffect {} => {
                    entry.status = OperationStatus::Cancelled;
                    entry.outcome = Some(OperationOutcome::Cancelled);
                    entry.failure_point =
                        Some("performance_operation_abandoned_before_effect".to_string());
                }
                PerformanceOperationTerminal::FailedBeforeEffect { .. }
                | PerformanceOperationTerminal::FailedAfterEffect { .. } => {
                    entry.status = OperationStatus::Failed;
                    entry.outcome = Some(OperationOutcome::Failed);
                    entry.failure_point = Some("performance_operation_failed".to_string());
                }
            }
        }
    }
}

fn performance_restart_disposition(
    entry: &OperationJournalEntry,
) -> Option<OperationRestartDisposition> {
    let lifecycle = entry.performance_lifecycle()?;
    Some(match lifecycle.phase {
        PerformanceOperationPhase::Accepted {} | PerformanceOperationPhase::Planning {} => {
            OperationRestartDisposition::AbandonedBeforeEffect
        }
        PerformanceOperationPhase::Prepared { .. } => OperationRestartDisposition::Resumable,
        PerformanceOperationPhase::EffectStarted { .. }
        | PerformanceOperationPhase::AppliedUnverified { .. } => {
            OperationRestartDisposition::AppliedUnverified
        }
        PerformanceOperationPhase::TerminalIntent { .. } => {
            OperationRestartDisposition::TerminalIntent
        }
        PerformanceOperationPhase::Terminal { .. } => OperationRestartDisposition::Terminal,
    })
}

fn performance_operation_projection(
    entry: &OperationJournalEntry,
) -> Option<PerformanceOperationProjection> {
    let lifecycle = entry.performance_lifecycle()?;
    let restart = performance_restart_disposition(entry)?;
    let state = match &lifecycle.phase {
        PerformanceOperationPhase::Accepted {} => "queued",
        PerformanceOperationPhase::Planning {} => "planning",
        PerformanceOperationPhase::Prepared { .. } => "planning",
        PerformanceOperationPhase::EffectStarted { .. } => match lifecycle.intent.action {
            PerformanceOperationAction::Install => "applying",
            PerformanceOperationAction::Remove => "removing",
            PerformanceOperationAction::Rollback => "rolling_back",
        },
        PerformanceOperationPhase::AppliedUnverified { .. } => "applied_unverified",
        PerformanceOperationPhase::TerminalIntent { terminal } => match terminal {
            PerformanceOperationTerminal::Succeeded { .. } => "committing_complete",
            _ => "committing_failed",
        },
        PerformanceOperationPhase::Terminal { terminal } => match terminal {
            PerformanceOperationTerminal::Succeeded { .. } => "complete",
            PerformanceOperationTerminal::AbandonedBeforeEffect {} => "interrupted",
            PerformanceOperationTerminal::FailedBeforeEffect { .. }
            | PerformanceOperationTerminal::FailedAfterEffect { .. } => "failed",
        },
    };
    Some(PerformanceOperationProjection {
        operation_id: entry.operation_id.clone(),
        sequence: entry.sequence,
        intent: lifecycle.intent.clone(),
        phase: lifecycle.phase.clone(),
        state,
        error: performance_phase_error(&lifecycle.phase),
        created_at: lifecycle.created_at.clone(),
        updated_at: lifecycle.updated_at.clone(),
        terminal: restart == OperationRestartDisposition::Terminal,
        restart,
    })
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
    if value.contains(INSTALL_PUBLICATION_EVIDENCE_FACT_PREFIX) {
        return safe_install_publication_evidence_fact(value);
    }
    if value.contains(INSTALL_ACTIVATION_CONTRACT_FACT_PREFIX) {
        return safe_install_activation_contract_fact(value);
    }
    if value.contains(INSTALL_PUBLICATION_VERSION_ID_FACT_PREFIX) {
        return safe_install_publication_version_id_fact(value);
    }
    if value.contains(INSTALL_VERSION_ID_FACT_PREFIX) {
        return safe_install_version_id_fact(value);
    }
    if value.contains(LOADER_BUILD_ID_FACT_PREFIX) {
        return safe_loader_build_id_fact(value);
    }
    if value.contains(PERFORMANCE_PLAN_GRAPH_SHA512_FACT_PREFIX) {
        return safe_performance_plan_graph_sha512_fact(value);
    }
    if value.contains(GUARDIAN_OUTCOME_MEMORY_BINDING_PREFIX) {
        return safe_guardian_outcome_memory_binding(value);
    }
    safe_public_fragment(value, 320)
}

fn safe_install_activation_contract_fact(value: &str) -> bool {
    value
        .strip_prefix(INSTALL_ACTIVATION_CONTRACT_FACT_PREFIX)
        .is_some_and(|contract| {
            axial_minecraft::ManagedInstallActivationContractId::parse(contract).is_ok()
        })
}

fn safe_install_version_id_fact(value: &str) -> bool {
    safe_version_id_fact(value, INSTALL_VERSION_ID_FACT_PREFIX)
}

fn safe_install_publication_version_id_fact(value: &str) -> bool {
    safe_version_id_fact(value, INSTALL_PUBLICATION_VERSION_ID_FACT_PREFIX)
}

fn safe_version_id_fact(value: &str, prefix: &str) -> bool {
    value.strip_prefix(prefix).is_some_and(|version_id| {
        if version_id.starts_with("loader-v2-") {
            axial_minecraft::is_canonical_installed_loader_id(version_id)
        } else {
            axial_config::instances::is_safe_version_id(version_id)
        }
    })
}

fn safe_loader_build_id_fact(value: &str) -> bool {
    value
        .strip_prefix(LOADER_BUILD_ID_FACT_PREFIX)
        .is_some_and(|build_id| axial_minecraft::parse_build_id(build_id).is_some())
}

fn safe_install_publication_evidence_fact(value: &str) -> bool {
    value
        .strip_prefix(INSTALL_PUBLICATION_EVIDENCE_FACT_PREFIX)
        .is_some_and(|evidence| {
            axial_minecraft::ManagedInstallPublicationEvidenceId::parse(evidence).is_ok()
        })
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

fn canonical_safe_token(value: &str, max_chars: usize) -> bool {
    value == value.trim() && safe_token(value, max_chars)
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
    let referenced_predecessors = records
        .values()
        .filter(|entry| {
            matches!(
                entry.status,
                OperationStatus::Planned | OperationStatus::Running
            ) && entry.reconciliation_terminal().is_none()
        })
        .filter_map(OperationJournalEntry::reconciliation_attempt)
        .filter_map(|attempt| match attempt.lineage() {
            ReconciliationLineage::Predecessor { operation_id } => Some(operation_id.clone()),
            ReconciliationLineage::Initial => None,
        })
        .collect::<BTreeSet<_>>();
    while records.len() > max_entries {
        let Some(key) = records
            .iter()
            .filter(|(key, entry)| {
                protected_key != Some(*key)
                    && !referenced_predecessors.contains(*key)
                    && operation_journal_status_is_terminal(entry.status)
                    && !active_reconciliation_terminal(entry)
            })
            .min_by_key(|(_, entry)| entry.sequence)
            .map(|(key, _)| key.clone())
        else {
            return false;
        };
        records.remove(&key);
    }
    true
}

fn active_reconciliation_terminal(entry: &OperationJournalEntry) -> bool {
    entry.reconciliation_terminal().is_some_and(|terminal| {
        terminal
            .version_bundle_publication()
            .is_some_and(|publication| publication.is_pending())
            || chrono::DateTime::parse_from_rfc3339(terminal.suppression_until())
                .is_ok_and(|until| until > chrono::Utc::now())
    }) || entry
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
        OPERATION_JOURNAL_LOCK_INVARIANT, OPERATION_JOURNAL_SCHEMA, OperationJournalReconciliation,
        OperationJournalSnapshot, OperationJournalStore, OperationJournalStoreError,
        OperationRestartDisposition, PerformanceOperationCreateError,
        PerformanceOperationTransition, apply_performance_transition, operation_journal_path,
        operation_journal_plan_is_visible, performance_guardian_evidence_matches,
        safe_generated_fact,
    };
    use crate::execution::persistence::{AtomicWriteBackend, PersistenceCoordinator, WriteUrgency};
    use crate::guardian::DiagnosisId;
    use crate::state::contracts::{
        CommandKind, JournalId, OperationId, OperationIntent, OperationJournalEntry,
        OperationJournalStep, OperationOutcome, OperationPhase, OperationStatus,
        OperationStepResult, OwnershipClass, PerformanceOperationAction,
        PerformanceOperationIntent, PerformanceOperationPhase, PerformanceOperationPrepared,
        PerformanceOperationTerminal, PerformancePreparedProof, PerformanceRollbackTarget,
        ReconciliationComponent, ReconciliationRung, ReconciliationScope,
        ReconciliationTerminalOutcome, RollbackState, StabilizationSystem, TargetDescriptor,
        TargetKind,
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

    const OPERATION_JOURNALS_V8_FIXTURE: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/guardian/operation-journals-v8.json"
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
    fn install_publication_evidence_generated_fact_has_exact_safe_shape() {
        let evidence = "managed-install-v1.T7ghN0PBffcxr4Rg08bVvTPOl9fRcUh9qyNnWZtd93c.Xsu8KmJnT7So_J1WS8rcqA.X-fR4EpDTc2mbfPpfNOFiA.JGoynsQN9LfT8e7hWyX1fknDskeaM7xQCAbFGATbD-I._FMcn_pUsOarv_sNtTJousevn4S1SMqttV6yiYdONOY";
        let fact = format!("install_publication_evidence:{evidence}");
        assert!(safe_generated_fact(&fact));
        assert!(!safe_generated_fact(&format!("x{fact}")));
        assert!(!safe_generated_fact(&format!("{fact}:suffix")));
        assert!(!safe_generated_fact(&fact.replacen(
            "managed-install-v1",
            "managed-install-v2",
            1
        )));
    }

    #[test]
    fn install_activation_contract_generated_fact_has_exact_safe_shape() {
        let contract = "managed-install-activation-v1.qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqo";
        let fact = format!("install_activation_contract:{contract}");
        assert!(safe_generated_fact(&fact));
        assert!(!safe_generated_fact(&format!("x{fact}")));
        assert!(!safe_generated_fact(&format!("{fact}:suffix")));
        assert!(!safe_generated_fact("install_activation_contract:"));
        assert!(!safe_generated_fact(&fact.replacen(
            "managed-install-activation-v1",
            "managed-install-activation-v2",
            1
        )));
        assert!(!safe_generated_fact(&fact[..fact.len() - 1]));
        assert!(!safe_generated_fact(&format!("{fact}!")));
    }

    #[test]
    fn loader_build_generated_fact_has_exact_safe_shape() {
        let build_id = axial_minecraft::build_id_for(
            axial_minecraft::LoaderComponentId::Fabric,
            "1.21.5",
            "0.16.10",
        );
        let fact = format!("loader_build_id:{build_id}");
        assert!(safe_generated_fact(&fact));
        assert!(!safe_generated_fact(&format!("x{fact}")));
        assert!(!safe_generated_fact(&format!("{fact}:suffix")));
        assert!(!safe_generated_fact("loader_build_id:opaque-token"));
    }

    #[test]
    fn install_version_generated_fact_has_exact_safe_shape() {
        assert!(safe_generated_fact("install_version_id:1.21.5"));
        assert!(!safe_generated_fact("install_version_id:"));
        assert!(!safe_generated_fact("install_version_id:\n"));
        assert!(!safe_generated_fact("install_version_id:../secrets"));
        assert!(!safe_generated_fact(r"install_version_id:C:\Users\player"));
        assert!(!safe_generated_fact("install_version_id:."));
        assert!(!safe_generated_fact("install_version_id:.."));
        assert!(!safe_generated_fact("install_version_id: 1.21.5"));
        assert!(!safe_generated_fact("install_version_id:1.21.5 "));
        assert!(!safe_generated_fact("install_version_id:1.21 5"));
        assert!(!safe_generated_fact("install_version_id:1.21.5-\u{03b2}"));
        assert!(!safe_generated_fact(&format!(
            "install_version_id:{}",
            "v".repeat(257)
        )));

        let version_id = axial_minecraft::installed_version_id_for(
            axial_minecraft::LoaderComponentId::Fabric,
            "1.21.5",
            "0.16.10",
        )
        .expect("canonical installed loader version");
        let fact = format!("install_version_id:{version_id}");
        assert!(safe_generated_fact(&fact));
        assert!(!safe_generated_fact(&format!("x{fact}")));
        assert!(!safe_generated_fact(&format!("{fact}:suffix")));
        assert!(!safe_generated_fact(
            "install_version_id:loader-v2-opaque-token"
        ));
    }

    #[test]
    fn install_publication_version_generated_fact_has_exact_safe_shape() {
        assert!(safe_generated_fact("install_publication_version_id:1.21.5"));
        assert!(!safe_generated_fact("install_publication_version_id:"));
        assert!(!safe_generated_fact("install_publication_version_id:\n"));
        assert!(!safe_generated_fact(
            "install_publication_version_id:../secrets"
        ));
        assert!(!safe_generated_fact(
            r"install_publication_version_id:C:\Users\player"
        ));
        assert!(!safe_generated_fact("install_publication_version_id:."));
        assert!(!safe_generated_fact("install_publication_version_id:.."));
        assert!(!safe_generated_fact(
            "install_publication_version_id: 1.21.5"
        ));
        assert!(!safe_generated_fact(
            "install_publication_version_id:1.21.5 "
        ));
        assert!(!safe_generated_fact(
            "install_publication_version_id:1.21 5"
        ));
        assert!(!safe_generated_fact(
            "install_publication_version_id:1.21.5-\u{03b2}"
        ));
        assert!(!safe_generated_fact(&format!(
            "install_publication_version_id:{}",
            "v".repeat(257)
        )));

        let version_id = axial_minecraft::installed_version_id_for(
            axial_minecraft::LoaderComponentId::Fabric,
            "1.21.5",
            "0.16.10",
        )
        .expect("canonical installed loader version");
        let fact = format!("install_publication_version_id:{version_id}");
        assert!(safe_generated_fact(&fact));
        assert!(!safe_generated_fact(&format!("x{fact}")));
        assert!(!safe_generated_fact(&format!("{fact}:suffix")));
        assert!(!safe_generated_fact(
            "install_publication_version_id:loader-v2-opaque-token"
        ));
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

        let snapshot = store.snapshot().expect("valid v8 snapshot");
        assert_eq!(snapshot.schema, OPERATION_JOURNAL_SCHEMA);
        assert_eq!(snapshot.entries.len(), 1);
        assert_eq!(snapshot.entries[0].sequence, 1);
        assert_eq!(snapshot.entries[0].operation_id, operation_id);
        let encoded = snapshot.to_json().expect("encode v8 snapshot");
        let decoded = OperationJournalSnapshot::from_json(&encoded).expect("decode v8 snapshot");
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
    async fn p03_b01_contract() {
        let store = OperationJournalStore::new();
        let status = store
            .create_performance(performance_intent("0123456789abcdef"))
            .await
            .expect("create sole durable performance lifecycle");
        assert_eq!(status.state, "queued");
        assert!(matches!(
            status.phase,
            PerformanceOperationPhase::Accepted {}
        ));
        assert!(matches!(
            store
                .transition_performance(
                    &status.operation_id,
                    PerformanceOperationTransition::EffectStarted,
                )
                .await,
            Err(OperationJournalStoreError::AlreadyExists)
        ));
        store
            .transition_performance(
                &status.operation_id,
                PerformanceOperationTransition::Planning,
            )
            .await
            .expect("Accepted advances to Planning");
        store
            .record_performance_guardian_evidence(
                &status.operation_id,
                vec![
                    "guardian_fact:performance_plan_allowed".to_string(),
                    "guardian_fact:performance_plan_allowed".to_string(),
                ],
                vec![
                    DiagnosisId::PerformanceFallbackSelected,
                    DiagnosisId::PerformanceFallbackSelected,
                ],
            )
            .await
            .expect("supporting Guardian evidence is journal-owned after Planning");
        let evidence_snapshot = store.snapshot().expect("evidence snapshot");
        store
            .record_performance_guardian_evidence(
                &status.operation_id,
                vec!["guardian_fact:performance_plan_allowed".to_string()],
                vec![DiagnosisId::PerformanceFallbackSelected],
            )
            .await
            .expect("deduplicated exact evidence retry is idempotent");
        assert_eq!(
            store.snapshot().expect("stable exact retry"),
            evidence_snapshot
        );
        assert!(matches!(
            store
                .record_performance_guardian_evidence(
                    &status.operation_id,
                    vec!["guardian_fact:late".to_string()],
                    vec![DiagnosisId::PerformanceFallbackSelected],
                )
                .await,
            Err(OperationJournalStoreError::AlreadyExists)
        ));
        let prepared = performance_prepared("composition-result");
        store
            .transition_performance(
                &status.operation_id,
                PerformanceOperationTransition::Prepared(prepared.clone()),
            )
            .await
            .expect("Planning advances to typed Prepared");
        store
            .record_performance_guardian_evidence(
                &status.operation_id,
                vec!["guardian_fact:performance_plan_allowed".to_string()],
                vec![DiagnosisId::PerformanceFallbackSelected],
            )
            .await
            .expect("Prepared restart may replay only the exact admitted Guardian evidence");
        assert!(matches!(
            store
                .record_performance_guardian_evidence(
                    &status.operation_id,
                    vec!["guardian_fact:different".to_string()],
                    vec![DiagnosisId::PerformanceFallbackSelected],
                )
                .await,
            Err(OperationJournalStoreError::AlreadyExists)
        ));
        assert_eq!(
            store
                .performance_operation(&status.operation_id)
                .expect("prepared projection")
                .restart,
            OperationRestartDisposition::Resumable
        );
        store
            .transition_performance(
                &status.operation_id,
                PerformanceOperationTransition::EffectStarted,
            )
            .await
            .expect("Prepared advances to EffectStarted");
        assert_ne!(
            store
                .performance_operation(&status.operation_id)
                .expect("effect-started projection")
                .restart,
            OperationRestartDisposition::Resumable
        );

        let terminal = PerformanceOperationTerminal::Succeeded {
            prepared,
            changed_target: true,
            rollback: RollbackState::Available,
        };
        store
            .transition_performance(
                &status.operation_id,
                PerformanceOperationTransition::RequestTerminal(terminal.clone()),
            )
            .await
            .expect("write terminal intent before terminal projection");
        assert_eq!(
            store
                .performance_operation(&status.operation_id)
                .expect("terminal intent projection")
                .state,
            "committing_complete"
        );
        store
            .transition_performance(
                &status.operation_id,
                PerformanceOperationTransition::CommitTerminal(terminal.clone()),
            )
            .await
            .expect("commit typed terminal");
        store
            .transition_performance(
                &status.operation_id,
                PerformanceOperationTransition::CommitTerminal(terminal),
            )
            .await
            .expect("exact terminal retry is idempotent");
        let stable = store.snapshot().expect("stable exact terminal");
        assert!(matches!(
            store
                .transition_performance(
                    &status.operation_id,
                    PerformanceOperationTransition::CommitTerminal(
                        PerformanceOperationTerminal::FailedAfterEffect {
                            prepared: performance_prepared("composition-result"),
                            changed_target: false,
                            rollback: RollbackState::Available,
                            error: "different terminal".to_string(),
                        },
                    ),
                )
                .await,
            Err(OperationJournalStoreError::AlreadyTerminal)
        ));
        assert_eq!(store.snapshot().expect("unchanged terminal"), stable);
        assert!(matches!(
            store
                .record_guardian_evidence(
                    &status.operation_id,
                    vec!["guardian_fact:late".to_string()],
                    vec![],
                )
                .await,
            Err(OperationJournalStoreError::AlreadyTerminal)
        ));
        let terminal = store
            .performance_operation(&status.operation_id)
            .expect("terminal projection");
        assert_eq!(terminal.state, "complete");
        assert!(terminal.terminal);
        assert_eq!(terminal.created_at, status.created_at);
    }

    #[tokio::test]
    async fn p03_b01_strict_load_binds_performance_constructor_identity() {
        let store = OperationJournalStore::new();
        let status = store
            .create_performance(performance_intent("abababababababab"))
            .await
            .expect("create typed operation");
        let snapshot = store.snapshot().expect("valid constructor snapshot");

        let mut invalid_journal = snapshot.clone();
        invalid_journal.entries[0].journal_id = JournalId::new("journal-different");
        assert!(matches!(
            OperationJournalSnapshot::from_json(
                &invalid_journal
                    .to_json()
                    .expect("encode invalid journal id")
            ),
            Err(super::OperationJournalLoadError::InvalidEntry(
                super::OperationJournalValidationError::InvalidPerformanceLifecycle
            ))
        ));

        let mut invalid_action = snapshot;
        let OperationIntent::Performance(lifecycle) = &mut invalid_action.entries[0].intent else {
            panic!("constructor creates a Performance lifecycle")
        };
        lifecycle.intent.requested_action = PerformanceOperationAction::Remove;
        assert_eq!(lifecycle.intent.action, PerformanceOperationAction::Install);
        assert!(matches!(
            OperationJournalSnapshot::from_json(
                &invalid_action
                    .to_json()
                    .expect("encode invalid action pair")
            ),
            Err(super::OperationJournalLoadError::InvalidEntry(
                super::OperationJournalValidationError::InvalidPerformanceLifecycle
            ))
        ));

        let mut invalid_parent = store.snapshot().expect("valid constructor snapshot");
        invalid_parent.entries[0].parent_operation_id = Some(OperationId::deterministic_test(
            "impossible-performance-parent",
        ));
        assert!(matches!(
            OperationJournalSnapshot::from_json(
                &invalid_parent
                    .to_json()
                    .expect("encode invalid parent identity")
            ),
            Err(super::OperationJournalLoadError::InvalidEntry(
                super::OperationJournalValidationError::InvalidPerformanceLifecycle
            ))
        ));

        let mut duplicate_evidence = store.snapshot().expect("valid constructor snapshot");
        let mut evidence = OperationJournalStep::new("guardian_evidence", OperationPhase::Running);
        evidence.result = OperationStepResult::Completed;
        evidence.generated_facts = vec![
            "guardian_fact:performance_plan_allowed".to_string(),
            "guardian_fact:performance_plan_allowed".to_string(),
        ];
        duplicate_evidence.entries[0].completed_steps = vec![evidence];
        assert!(matches!(
            OperationJournalSnapshot::from_json(
                &duplicate_evidence
                    .to_json()
                    .expect("encode duplicate Performance evidence")
            ),
            Err(super::OperationJournalLoadError::InvalidEntry(
                super::OperationJournalValidationError::InvalidPerformanceLifecycle
            ))
        ));
        assert_eq!(
            store
                .performance_operation(&status.operation_id)
                .expect("live constructor row remains valid")
                .operation_id,
            status.operation_id
        );
    }

    #[tokio::test]
    async fn p03_b01_restart_settlement_is_durable_and_second_restart_is_stable() {
        let root = test_root("performance-restart-settlement");
        let paths = test_paths(&root);
        let store = OperationJournalStore::try_load_from_paths(&paths)
            .expect("claim operation journal persistence");
        let accepted = store
            .create_performance(performance_intent("1111111111111111"))
            .await
            .expect("create restart-abandoned operation");
        let effect_started = store
            .create_performance(performance_intent("2222222222222222"))
            .await
            .expect("create effect-started operation");
        let resumable = store
            .create_performance(performance_intent("3333333333333333"))
            .await
            .expect("create resumable operation");
        let terminal_intent = store
            .create_performance(performance_intent("4444444444444444"))
            .await
            .expect("create terminal-intent operation");
        let prepared = performance_prepared("effect-result");
        for transition in [
            PerformanceOperationTransition::Planning,
            PerformanceOperationTransition::Prepared(prepared.clone()),
            PerformanceOperationTransition::EffectStarted,
        ] {
            store
                .transition_performance(&effect_started.operation_id, transition)
                .await
                .expect("advance effect-started fixture");
        }
        for (operation_id, target) in [
            (&resumable.operation_id, "resumable-result"),
            (&terminal_intent.operation_id, "terminal-intent-result"),
        ] {
            store
                .transition_performance(operation_id, PerformanceOperationTransition::Planning)
                .await
                .expect("advance restart fixture to Planning");
            store
                .transition_performance(
                    operation_id,
                    PerformanceOperationTransition::Prepared(performance_prepared(target)),
                )
                .await
                .expect("advance restart fixture to Prepared");
        }
        store
            .transition_performance(
                &terminal_intent.operation_id,
                PerformanceOperationTransition::EffectStarted,
            )
            .await
            .expect("start terminal-intent effect");
        let intended_failure = PerformanceOperationTerminal::FailedAfterEffect {
            prepared: performance_prepared("terminal-intent-result"),
            changed_target: false,
            rollback: RollbackState::Available,
            error: "durable intended failure".to_string(),
        };
        store
            .transition_performance(
                &terminal_intent.operation_id,
                PerformanceOperationTransition::RequestTerminal(intended_failure.clone()),
            )
            .await
            .expect("persist restart terminal intent");
        store.close().await.expect("close before restart");
        drop(store);

        let restarted = OperationJournalStore::try_load_from_paths(&paths)
            .expect("first restart loads strict v8");
        let plan = restarted
            .settle_performance_restarts()
            .await
            .expect("durably settle first restart");
        assert_eq!(plan.resumable.len(), 1);
        assert_eq!(plan.resumable[0].operation_id, resumable.operation_id);
        assert!(matches!(
            &plan.resumable[0].phase,
            PerformanceOperationPhase::Prepared { prepared }
                if matches!(
                    &prepared.proof,
                    PerformancePreparedProof::InstallPlan {
                        graph_sha512,
                        artifact_count: 3,
                        aggregate_bytes: 4096,
                    } if graph_sha512 == &"0123456789abcdef".repeat(8)
                )
        ));
        assert_eq!(plan.applied_unverified.len(), 1);
        assert_eq!(
            plan.applied_unverified[0].operation_id,
            effect_started.operation_id
        );
        assert_eq!(
            restarted
                .performance_operation(&accepted.operation_id)
                .expect("abandoned operation retained")
                .state,
            "interrupted"
        );
        assert_eq!(
            restarted
                .performance_operation(&terminal_intent.operation_id)
                .expect("terminal intent committed on restart")
                .state,
            "failed"
        );
        restarted.close().await.expect("persist restart settlement");
        drop(restarted);

        let restarted_again = OperationJournalStore::try_load_from_paths(&paths)
            .expect("second restart loads settled v8");
        let first = restarted_again.snapshot().expect("settled snapshot");
        let second_plan = restarted_again
            .settle_performance_restarts()
            .await
            .expect("second restart is idempotent");
        assert_eq!(second_plan.applied_unverified.len(), 1);
        assert_eq!(restarted_again.snapshot().expect("stable snapshot"), first);
        assert!(matches!(
            restarted_again
                .performance_operation(&effect_started.operation_id)
                .expect("applied-unverified row remains explicit")
                .phase,
            PerformanceOperationPhase::AppliedUnverified { prepared: current, .. }
                if current == prepared
        ));
        restarted_again.close().await.expect("close second restart");
        cleanup(&root);
    }

    #[tokio::test]
    async fn p03_b01_abandoned_capacity_and_sequence_high_water_survive_pruning() {
        let store = OperationJournalStore::with_max_entries(128);
        let mut first = None;
        for index in 0_u64..128 {
            let status = store
                .create_performance(performance_intent(&format!("{index:016x}")))
                .await
                .expect("fill capacity with pre-effect operation");
            first.get_or_insert(status.operation_id);
        }
        store
            .settle_performance_restarts()
            .await
            .expect("terminalize all abandoned pre-effect rows");
        let replacement = store
            .create_performance(performance_intent("ffffffffffffffff"))
            .await
            .expect("abandoned rows cannot block one new operation");
        assert_eq!(replacement.sequence, 129);
        assert!(store.get(&first.expect("first operation")).is_none());

        let snapshot = store.snapshot().expect("snapshot after terminal prune");
        assert_eq!(snapshot.next_sequence, 130);
        let restarted = OperationJournalStore::with_max_entries(128);
        restarted.load_snapshot(snapshot).expect("restart snapshot");
        let next = restarted
            .create_performance(performance_intent("eeeeeeeeeeeeeeee"))
            .await
            .expect("high-water survives prune and restart");
        assert_eq!(next.sequence, 130);
    }

    #[tokio::test]
    async fn p03_b01_applied_unverified_capacity_is_protected() {
        let store = OperationJournalStore::with_max_entries(128);
        for index in 0_u64..128 {
            let status = store
                .create_performance(performance_intent(&format!("{index:016x}")))
                .await
                .expect("create indeterminate operation");
            for transition in [
                PerformanceOperationTransition::Planning,
                PerformanceOperationTransition::Prepared(performance_prepared(&format!(
                    "target-{index}"
                ))),
                PerformanceOperationTransition::EffectStarted,
            ] {
                store
                    .transition_performance(&status.operation_id, transition)
                    .await
                    .expect("advance indeterminate operation");
            }
        }
        let plan = store
            .settle_performance_restarts()
            .await
            .expect("classify effect-started rows");
        assert_eq!(plan.applied_unverified.len(), 128);
        assert!(matches!(
            store
                .create_performance(performance_intent("ffffffffffffffff"))
                .await,
            Err(PerformanceOperationCreateError::BeforeAdmission(
                OperationJournalStoreError::CapacityExhausted
            ))
        ));
    }

    #[tokio::test]
    async fn p03_b01_snapshot_rejects_parallel_active_instance_lifecycles() {
        let store = OperationJournalStore::new();
        let status = store
            .create_performance(performance_intent("aaaaaaaaaaaaaaaa"))
            .await
            .expect("create active operation");
        let first = store.get(&status.operation_id).expect("active entry");
        let mut second = first.clone();
        second.operation_id = OperationId::deterministic_test("parallel-performance");
        second.journal_id = JournalId::new(format!("journal-{}", second.operation_id));
        second.sequence = 2;
        let snapshot = OperationJournalSnapshot {
            schema: OPERATION_JOURNAL_SCHEMA.to_string(),
            next_sequence: 3,
            entries: vec![first, second],
        };
        assert!(matches!(
            OperationJournalSnapshot::from_json(&snapshot.to_json().expect("encode parallel rows")),
            Err(super::OperationJournalLoadError::InvalidEntry(
                super::OperationJournalValidationError::InvalidPerformanceLifecycle
            ))
        ));
    }

    #[tokio::test]
    async fn p03_b01_rejects_inconsistent_action_target_and_rollback_identity() {
        let store = OperationJournalStore::new();
        let mut intent = performance_intent("bbbbbbbbbbbbbbbb");
        intent.action = PerformanceOperationAction::Remove;
        intent.rollback = RollbackState::Unavailable;
        assert!(matches!(
            store.create_performance(intent).await,
            Err(PerformanceOperationCreateError::BeforeAdmission(
                OperationJournalStoreError::Validation(
                    super::OperationJournalValidationError::InvalidPerformanceLifecycle
                )
            ))
        ));
    }

    #[tokio::test]
    async fn p03_b01_prepared_proof_is_bound_to_intent_target() {
        let store = OperationJournalStore::new();
        let status = store
            .create_performance(performance_intent("bcbcbcbcbcbcbcbc"))
            .await
            .expect("create target-bound operation");
        store
            .transition_performance(
                &status.operation_id,
                PerformanceOperationTransition::Planning,
            )
            .await
            .expect("advance target-bound operation");
        let mut mismatched = performance_prepared("ignored");
        mismatched.result_target_id = "foreign-composition".to_string();
        assert!(matches!(
            store
                .transition_performance(
                    &status.operation_id,
                    PerformanceOperationTransition::Prepared(mismatched.clone()),
                )
                .await,
            Err(OperationJournalStoreError::Validation(
                super::OperationJournalValidationError::InvalidPerformanceLifecycle
            ))
        ));

        let mut entry = store.get(&status.operation_id).expect("planning entry");
        let OperationIntent::Performance(lifecycle) = &mut entry.intent else {
            panic!("performance lifecycle")
        };
        lifecycle.phase = PerformanceOperationPhase::Prepared {
            prepared: mismatched,
        };
        entry.status = OperationStatus::Running;
        let invalid = OperationJournalSnapshot {
            schema: OPERATION_JOURNAL_SCHEMA.to_string(),
            next_sequence: 2,
            entries: vec![entry],
        };
        assert!(matches!(
            OperationJournalSnapshot::from_json(&invalid.to_json().expect("encode invalid row")),
            Err(super::OperationJournalLoadError::InvalidEntry(
                super::OperationJournalValidationError::InvalidPerformanceLifecycle
            ))
        ));
    }

    #[test]
    fn p03_b01_rollback_proof_target_class_matches_exact_target() {
        let proof = |target| PerformancePreparedProof::RollbackSnapshot {
            snapshot_id: "snapshot-safe".to_string(),
            target,
            artifact_count: 1,
        };
        let mut intent = performance_intent("cdcdcdcdcdcdcdcd");
        intent.requested_action = PerformanceOperationAction::Rollback;
        intent.action = PerformanceOperationAction::Rollback;
        intent.rollback = RollbackState::Available;
        intent.base_target_id = "performance_managed_state_absent".to_string();
        let absent = PerformanceOperationPrepared {
            result_target_id: intent.base_target_id.clone(),
            proof: proof(PerformanceRollbackTarget::ManagedStateAbsent),
        };
        assert!(super::validate_performance_prepared(&intent, &absent).is_ok());
        let mut wrong_absent = absent.clone();
        wrong_absent.proof = proof(PerformanceRollbackTarget::ManagedComposition);
        assert!(super::validate_performance_prepared(&intent, &wrong_absent).is_err());

        intent.base_target_id = "managed-composition-target".to_string();
        let composition = PerformanceOperationPrepared {
            result_target_id: intent.base_target_id.clone(),
            proof: proof(PerformanceRollbackTarget::ManagedComposition),
        };
        assert!(super::validate_performance_prepared(&intent, &composition).is_ok());
        let mut wrong_composition = composition;
        wrong_composition.proof = proof(PerformanceRollbackTarget::ManagedStateAbsent);
        assert!(super::validate_performance_prepared(&intent, &wrong_composition).is_err());
    }

    #[tokio::test]
    async fn p03_b01_remove_absent_commits_without_effect() {
        let store = OperationJournalStore::new();
        let mut intent = performance_intent("dddddddddddddddd");
        intent.action = PerformanceOperationAction::Remove;
        intent.base_target_id = "performance_composition_lock".to_string();
        intent.rollback = RollbackState::Unavailable;
        let status = store
            .create_performance(intent)
            .await
            .expect("create absent remove");
        store
            .transition_performance(
                &status.operation_id,
                PerformanceOperationTransition::Planning,
            )
            .await
            .expect("plan absent remove");
        let prepared = PerformanceOperationPrepared {
            result_target_id: "performance_composition_lock".to_string(),
            proof: PerformancePreparedProof::ManagedStateAbsent {},
        };
        store
            .transition_performance(
                &status.operation_id,
                PerformanceOperationTransition::Prepared(prepared.clone()),
            )
            .await
            .expect("prepare known absent remove");
        assert!(matches!(
            store
                .transition_performance(
                    &status.operation_id,
                    PerformanceOperationTransition::EffectStarted,
                )
                .await,
            Err(OperationJournalStoreError::AlreadyExists)
        ));
        assert!(matches!(
            store
                .transition_performance(
                    &status.operation_id,
                    PerformanceOperationTransition::RequestTerminal(
                        PerformanceOperationTerminal::Succeeded {
                            prepared: prepared.clone(),
                            changed_target: true,
                            rollback: RollbackState::Unavailable,
                        },
                    ),
                )
                .await,
            Err(OperationJournalStoreError::Validation(
                super::OperationJournalValidationError::InvalidPerformanceLifecycle
            ))
        ));
        assert!(matches!(
            store
                .transition_performance(
                    &status.operation_id,
                    PerformanceOperationTransition::RequestTerminal(
                        PerformanceOperationTerminal::FailedAfterEffect {
                            prepared: prepared.clone(),
                            changed_target: false,
                            rollback: RollbackState::Unavailable,
                            error: "absent state cannot fail after an effect".to_string(),
                        },
                    ),
                )
                .await,
            Err(OperationJournalStoreError::Validation(
                super::OperationJournalValidationError::InvalidPerformanceLifecycle
            ))
        ));
        let terminal = PerformanceOperationTerminal::Succeeded {
            prepared,
            changed_target: false,
            rollback: RollbackState::Unavailable,
        };
        store
            .transition_performance(
                &status.operation_id,
                PerformanceOperationTransition::RequestTerminal(terminal.clone()),
            )
            .await
            .expect("write known no-effect terminal intent");
        store
            .transition_performance(
                &status.operation_id,
                PerformanceOperationTransition::CommitTerminal(terminal),
            )
            .await
            .expect("commit known no-effect terminal");
    }

    #[tokio::test]
    async fn p03_b01_terminal_retains_effective_rollback_fact() {
        let store = OperationJournalStore::new();
        let mut intent = performance_intent("eeeeeeeeeeeeeeee");
        intent.rollback = RollbackState::Unavailable;
        let status = store
            .create_performance(intent)
            .await
            .expect("create first install");
        let prepared = performance_prepared("first-install-result");
        for transition in [
            PerformanceOperationTransition::Planning,
            PerformanceOperationTransition::Prepared(prepared.clone()),
            PerformanceOperationTransition::EffectStarted,
        ] {
            store
                .transition_performance(&status.operation_id, transition)
                .await
                .expect("advance first install");
        }
        let terminal = PerformanceOperationTerminal::Succeeded {
            prepared,
            changed_target: true,
            rollback: RollbackState::Available,
        };
        store
            .transition_performance(
                &status.operation_id,
                PerformanceOperationTransition::RequestTerminal(terminal.clone()),
            )
            .await
            .expect("write terminal intent");
        assert_eq!(
            store
                .get(&status.operation_id)
                .expect("terminal intent journal")
                .rollback,
            RollbackState::Unavailable
        );
        store
            .transition_performance(
                &status.operation_id,
                PerformanceOperationTransition::CommitTerminal(terminal),
            )
            .await
            .expect("commit effective rollback");
        let snapshot = store.snapshot().expect("strict terminal snapshot");
        assert_eq!(snapshot.entries[0].rollback, RollbackState::Available);
        OperationJournalSnapshot::from_json(&snapshot.to_json().expect("encode terminal snapshot"))
            .expect("reload effective rollback fact");
    }

    #[tokio::test]
    async fn p03_b01_exact_install_reapply_commits_without_effect_started() {
        let store = OperationJournalStore::new();
        let mut intent = performance_intent("edededededededed");
        intent.rollback = RollbackState::Unavailable;
        let status = store
            .create_performance(intent)
            .await
            .expect("create exact install reapply");
        let prepared = performance_prepared("ignored");
        for transition in [
            PerformanceOperationTransition::Planning,
            PerformanceOperationTransition::Prepared(prepared.clone()),
        ] {
            store
                .transition_performance(&status.operation_id, transition)
                .await
                .expect("prepare exact install reapply");
        }
        let terminal = PerformanceOperationTerminal::Succeeded {
            prepared,
            changed_target: false,
            rollback: RollbackState::Unavailable,
        };
        for transition in [
            PerformanceOperationTransition::RequestTerminal(terminal.clone()),
            PerformanceOperationTransition::CommitTerminal(terminal),
        ] {
            store
                .transition_performance(&status.operation_id, transition)
                .await
                .expect("commit exact no-effect install");
        }
        assert_eq!(
            store
                .performance_operation(&status.operation_id)
                .expect("terminal exact install")
                .state,
            "complete"
        );
    }

    #[test]
    fn p03_b01_strict_sequence_high_water_rejects_missing_reuse_and_overflow() {
        let mut entry = test_entry("strict-sequence");
        entry.sequence = 7;
        let snapshot = OperationJournalSnapshot::new(vec![entry], 8).expect("strict v8 snapshot");
        assert_eq!(snapshot.next_sequence, 8);

        let mut missing = serde_json::to_value(&snapshot).expect("snapshot JSON");
        missing
            .as_object_mut()
            .expect("snapshot object")
            .remove("next_sequence");
        assert!(OperationJournalSnapshot::from_json(&missing.to_string()).is_err());

        let mut reused = serde_json::to_value(&snapshot).expect("snapshot JSON");
        reused["next_sequence"] = serde_json::json!(7);
        assert!(matches!(
            OperationJournalSnapshot::from_json(&reused.to_string()),
            Err(super::OperationJournalLoadError::InvalidSequence)
        ));

        let mut maximum = snapshot.entries[0].clone();
        maximum.sequence = u64::MAX;
        assert!(matches!(
            OperationJournalSnapshot::new(vec![maximum], u64::MAX),
            Err(super::OperationJournalLoadError::InvalidSequence)
        ));
    }

    #[test]
    fn p03_b01_tagged_performance_enums_reject_unknown_fields() {
        assert!(
            serde_json::from_value::<crate::state::contracts::OperationIntent>(
                serde_json::json!({"kind":"generic","future":true})
            )
            .is_err()
        );
        assert!(
            serde_json::from_value::<PerformanceOperationPhase>(
                serde_json::json!({"phase":"accepted","future":true})
            )
            .is_err()
        );
        assert!(
            serde_json::from_value::<PerformanceOperationPhase>(
                serde_json::json!({"phase":"planning","future":true})
            )
            .is_err()
        );
        assert!(
            serde_json::from_value::<PerformanceOperationTerminal>(serde_json::json!({
                "outcome":"failed_before_effect",
                "error":"bounded failure",
                "future":true
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<PerformanceOperationTerminal>(serde_json::json!({
                "outcome":"abandoned_before_effect",
                "future":true
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<PerformancePreparedProof>(serde_json::json!({
                "proof":"install_plan",
                "graph_sha512":"0123456789abcdef".repeat(8),
                "artifact_count":1,
                "aggregate_bytes":1,
                "future":true
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<PerformancePreparedProof>(serde_json::json!({
                "proof":"managed_state_absent",
                "future":true
            }))
            .is_err()
        );
    }

    #[tokio::test]
    async fn p03_b01_flush_retries_semantic_candidate_before_success() {
        let (root, _paths, backend, _coordinator, store) =
            persistence_fixture("performance-strict-flush");
        let operation_id = OperationId::deterministic_test("strict-flush-candidate");
        backend.fail_next();
        assert!(matches!(
            store.create(planned_entry(&operation_id)).await,
            Err(OperationJournalStoreError::Persistence(_))
        ));
        assert!(store.has_retry_candidate());
        assert!(store.get(&operation_id).is_none());

        backend.fail_next();
        assert!(matches!(
            store.flush().await,
            Err(OperationJournalStoreError::Persistence(_))
        ));
        assert!(store.has_retry_candidate());
        assert!(store.get(&operation_id).is_none());

        store.flush().await.expect("retry candidate then flush");
        assert!(!store.has_retry_candidate());
        assert!(store.get(&operation_id).is_some());
        store.close().await.expect("close strict flush store");
        cleanup(&root);
    }

    #[tokio::test]
    async fn p03_b01_create_failure_retains_minted_identity_and_same_candidate() {
        let (root, _paths, backend, _coordinator, store) =
            persistence_fixture("performance-create-identity");
        let intent = performance_intent("cccccccccccccccc");
        backend.failures.store(5, Ordering::SeqCst);
        let error = store
            .create_performance(intent.clone())
            .await
            .expect_err("initial commit and bounded reconciliation fail");
        let (operation_id, source) = match error {
            PerformanceOperationCreateError::AfterAdmission {
                operation_id,
                source,
            } if matches!(source, OperationJournalStoreError::Persistence(_)) => {
                (operation_id, source)
            }
            other => panic!("unexpected create error: {other:?}"),
        };
        assert!(store.get(&operation_id).is_none());
        assert!(store.has_retry_candidate());

        let projection = store
            .reconcile_performance_create(&operation_id, &intent, source)
            .await
            .expect("reconcile exact admitted performance candidate");
        assert_eq!(projection.operation_id, operation_id);
        assert_eq!(projection.sequence, 1);
        assert_eq!(store.list().len(), 1);
        store.close().await.expect("close create identity store");
        cleanup(&root);
    }

    #[tokio::test]
    async fn p03_b01_create_reconciliation_accepts_an_exact_planning_descendant() {
        let (root, _paths, backend, _coordinator, store) =
            persistence_fixture("performance-create-descendant");
        let store = Arc::new(store);
        let intent = performance_intent("cfcfcfcfcfcfcfcf");
        let attempts = backend.attempts.load(Ordering::SeqCst);
        let gate = backend.gate_next();
        backend.failures.store(5, Ordering::SeqCst);
        let first_store = store.clone();
        let first_intent = intent.clone();
        let first = tokio::spawn(async move { first_store.create_performance(first_intent).await });
        backend.wait_for_attempt(attempts + 1).await;
        gate.release();
        let error = first
            .await
            .expect("create task")
            .expect_err("initial create reconciliation remains bounded");
        let (operation_id, source) = match error {
            PerformanceOperationCreateError::AfterAdmission {
                operation_id,
                source,
            } => (operation_id, source),
            other => panic!("unexpected create error: {other:?}"),
        };
        assert!(store.has_retry_candidate());

        let second_store = store.clone();
        let second_operation_id = operation_id.clone();
        let second = tokio::spawn(async move {
            let mutation = second_store.mutation_gate.clone().lock_owned().await;
            let mutation = second_store.retry_holding_gate(mutation).await?;
            let ticket = second_store.update_performance(
                &second_operation_id,
                WriteUrgency::Immediate,
                |entry| {
                    apply_performance_transition(entry, &PerformanceOperationTransition::Planning)
                },
            )?;
            second_store.await_commit(ticket, mutation).await
        });
        second
            .await
            .expect("second create owner task")
            .expect("second owner drains Accepted and advances Planning");
        let projection = store
            .reconcile_performance_create(&operation_id, &intent, source)
            .await
            .expect("creator accepts exact Planning descendant");
        assert!(matches!(
            projection.phase,
            PerformanceOperationPhase::Planning {}
        ));
        store.close().await.expect("close create descendant store");
        cleanup(&root);
    }

    #[tokio::test]
    async fn p03_b01_performance_evidence_converges_own_and_foreign_failed_commits() {
        let (root, _paths, backend, _coordinator, store) =
            persistence_fixture("performance-evidence-convergence");
        let status = store
            .create_performance(performance_intent("dddddddddddddddd"))
            .await
            .expect("create performance operation");
        store
            .transition_performance(
                &status.operation_id,
                PerformanceOperationTransition::Planning,
            )
            .await
            .expect("advance before Guardian evidence");

        let fact = "guardian_fact:performance_plan_allowed".to_string();
        let diagnosis = DiagnosisId::PerformanceFallbackSelected;
        let attempts_before_failure = backend.attempts.load(Ordering::SeqCst);
        backend.fail_next();
        store
            .record_performance_guardian_evidence(
                &status.operation_id,
                vec![fact.clone(), fact.clone()],
                vec![diagnosis, diagnosis],
            )
            .await
            .expect("accepted evidence commit converges after persistence failure");
        assert_eq!(
            backend.attempts.load(Ordering::SeqCst),
            attempts_before_failure + 2
        );
        let entry = store.get(&status.operation_id).expect("visible evidence");
        assert_eq!(entry.completed_steps.len(), 1);
        assert_eq!(entry.completed_steps[0].generated_facts, vec![fact.clone()]);
        assert_eq!(entry.guardian_diagnosis_ids, vec![diagnosis]);

        let exact_snapshot = store.snapshot().expect("snapshot exact evidence");
        let exact_attempts = backend.attempts.load(Ordering::SeqCst);
        store
            .record_performance_guardian_evidence(
                &status.operation_id,
                vec![fact.clone()],
                vec![diagnosis],
            )
            .await
            .expect("exact evidence retry is stable");
        assert_eq!(backend.attempts.load(Ordering::SeqCst), exact_attempts);
        assert_eq!(
            store.snapshot().expect("stable exact retry"),
            exact_snapshot
        );

        let second = store
            .create_performance(performance_intent("abababababababab"))
            .await
            .expect("create second performance operation");
        let foreign_id = OperationId::deterministic_test("foreign-evidence-candidate");
        let foreign_attempts = backend.attempts.load(Ordering::SeqCst);
        backend.fail_next();
        assert!(matches!(
            store.create(planned_entry(&foreign_id)).await,
            Err(OperationJournalStoreError::Persistence(_))
        ));
        store
            .record_performance_guardian_evidence(&second.operation_id, vec![fact], vec![diagnosis])
            .await
            .expect("foreign candidate drains before typed evidence is reapplied");
        assert_eq!(
            backend.attempts.load(Ordering::SeqCst),
            foreign_attempts + 3
        );
        assert!(store.get(&foreign_id).is_some());
        assert!(performance_guardian_evidence_matches(
            &store
                .get(&second.operation_id)
                .expect("second evidence visible"),
            &["guardian_fact:performance_plan_allowed".to_string()],
            &[diagnosis],
        ));
        store.close().await.expect("close evidence store");
        cleanup(&root);
    }

    #[tokio::test]
    async fn p03_b01_transitions_and_restart_settlement_converge_failed_commits() {
        let (root, _paths, backend, _coordinator, store) =
            persistence_fixture("performance-transition-convergence");
        let status = store
            .create_performance(performance_intent("eeeeeeeeeeeeeeee"))
            .await
            .expect("create performance operation");
        backend.fail_next();
        store
            .transition_performance(
                &status.operation_id,
                PerformanceOperationTransition::Planning,
            )
            .await
            .expect("nonterminal transition reconciles exact candidate");
        let before_retry = store.snapshot().expect("stable Planning");
        store
            .transition_performance(
                &status.operation_id,
                PerformanceOperationTransition::Planning,
            )
            .await
            .expect("exact transition replay");
        assert_eq!(store.snapshot().expect("stable replay"), before_retry);

        backend.fail_next();
        store
            .transition_performance(
                &status.operation_id,
                PerformanceOperationTransition::RequestTerminal(
                    PerformanceOperationTerminal::FailedBeforeEffect {
                        error: "bounded failure".to_string(),
                    },
                ),
            )
            .await
            .expect("terminal intent reconciles exact candidate");
        let terminal = PerformanceOperationTerminal::FailedBeforeEffect {
            error: "bounded failure".to_string(),
        };
        backend.fail_next();
        store
            .transition_performance(
                &status.operation_id,
                PerformanceOperationTransition::CommitTerminal(terminal),
            )
            .await
            .expect("terminal commit reconciles exact candidate");

        let abandoned = store
            .create_performance(performance_intent("ffffffffffffffff"))
            .await
            .expect("create restart-abandoned row");
        backend.fail_next();
        store
            .settle_performance_restarts()
            .await
            .expect("restart settlement retries its exact candidate");
        assert_eq!(
            store
                .performance_operation(&abandoned.operation_id)
                .expect("settled restart row")
                .state,
            "interrupted"
        );
        store.close().await.expect("close convergence store");
        cleanup(&root);
    }

    #[tokio::test]
    async fn p03_b01_prepared_commit_accepts_a_proven_effect_started_descendant() {
        let (root, _paths, backend, _coordinator, store) =
            persistence_fixture("performance-transition-descendant");
        let store = Arc::new(store);
        let status = store
            .create_performance(performance_intent("dededededededede"))
            .await
            .expect("create descendant convergence operation");
        store
            .transition_performance(
                &status.operation_id,
                PerformanceOperationTransition::Planning,
            )
            .await
            .expect("advance before contested prepared transition");

        let attempts = backend.attempts.load(Ordering::SeqCst);
        let gate = backend.gate_next();
        backend.fail_next();
        let first_store = store.clone();
        let first_operation_id = status.operation_id.clone();
        let prepared = performance_prepared("ignored");
        let first_prepared = prepared.clone();
        let first = tokio::spawn(async move {
            first_store
                .transition_performance(
                    &first_operation_id,
                    PerformanceOperationTransition::Prepared(first_prepared),
                )
                .await
        });
        backend.wait_for_attempt(attempts + 1).await;

        let second_store = store.clone();
        let second_operation_id = status.operation_id.clone();
        let second = tokio::spawn(async move {
            let mutation = second_store.mutation_gate.clone().lock_owned().await;
            let mutation = second_store.retry_holding_gate(mutation).await?;
            let ticket = second_store.update_performance(
                &second_operation_id,
                WriteUrgency::Immediate,
                |entry| {
                    apply_performance_transition(
                        entry,
                        &PerformanceOperationTransition::EffectStarted,
                    )
                },
            )?;
            second_store.await_commit(ticket, mutation).await
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        gate.release();

        second
            .await
            .expect("second transition task")
            .expect("second owner drains Prepared and advances EffectStarted");
        first
            .await
            .expect("first transition task")
            .expect("first owner accepts proven Prepared descendant");
        assert!(matches!(
            store
                .performance_operation(&status.operation_id)
                .expect("descendant projection")
                .phase,
            PerformanceOperationPhase::EffectStarted { prepared: current }
                if current == prepared
        ));
        store.close().await.expect("close descendant store");
        cleanup(&root);
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

        for (operation_id, step_id) in [
            (first_id.clone(), "first_done"),
            (second_id.clone(), "second_done"),
        ] {
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
        let snapshot = OperationJournalSnapshot::new(vec![entry], 2).expect("snapshot");
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
    fn checked_in_operation_journals_v8_fixture_is_strict() {
        let snapshot = OperationJournalSnapshot::from_json(OPERATION_JOURNALS_V8_FIXTURE)
            .expect("strict fixture");
        assert_eq!(
            super::OPERATION_JOURNAL_SCHEMA,
            "axial.state.operation_journals.v8"
        );
        assert_eq!(snapshot.schema, "axial.state.operation_journals.v8");
        assert_eq!(snapshot.next_sequence, 8);
        assert_eq!(
            snapshot
                .entries
                .iter()
                .map(|entry| entry.sequence)
                .collect::<Vec<_>>(),
            vec![1, 2, 3, 4, 5, 7],
            "fixture must retain sequence 6 only through the durable high-water"
        );
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
        assert!(persisted_state_terminals.is_empty());

        let performance_lifecycles = snapshot
            .entries
            .iter()
            .filter_map(OperationJournalEntry::performance_lifecycle)
            .collect::<Vec<_>>();
        assert_eq!(
            performance_lifecycles.len(),
            1,
            "fixture must exercise the typed Performance lifecycle"
        );
        assert_eq!(
            performance_lifecycles[0].intent.action,
            PerformanceOperationAction::Install
        );
        assert!(matches!(
            performance_lifecycles[0].phase,
            PerformanceOperationPhase::Accepted {}
        ));

        let mut unknown_snapshot =
            serde_json::from_str::<serde_json::Value>(OPERATION_JOURNALS_V8_FIXTURE)
                .expect("fixture value");
        unknown_snapshot["entries"][0]["guardian_diagnosis_ids"][0] =
            serde_json::Value::String("future_diagnosis".to_string());
        let error = OperationJournalSnapshot::from_json(&unknown_snapshot.to_string())
            .expect_err("embedded unknown diagnosis must be rejected");
        let error = format!("{error:?}");
        assert!(!error.contains("future_diagnosis"));

        let pretty = serde_json::to_string_pretty(&snapshot).expect("pretty fixture json");
        assert_eq!(format!("{pretty}\n"), OPERATION_JOURNALS_V8_FIXTURE);

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

        assert!(OperationJournalSnapshot::new(vec![entry], 2).is_err());

        let mut unsafe_target = test_entry("operation-unsafe-target");
        unsafe_target.targets.push(TargetDescriptor {
            system: StabilizationSystem::State,
            kind: TargetKind::FilesystemPath,
            id: "/home/alice/.axial/libraries/secret.jar".to_string(),
            ownership: OwnershipClass::LauncherManaged,
        });
        assert!(OperationJournalSnapshot::new(vec![unsafe_target], 2).is_err());
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
        let future =
            r#"{"schema":"axial.state.operation_journals.v9","next_sequence":1,"entries":[]}"#;
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
        let legacy = OPERATION_JOURNALS_V8_FIXTURE
            .replacen(
                "axial.state.operation_journals.v8",
                "axial.state.operation_journals.v7",
                1,
            )
            .replacen("artifact_ownership_unsafe", "launch_command_prepared", 1);
        assert!(matches!(
            OperationJournalSnapshot::from_json(&legacy),
            Err(super::OperationJournalLoadError::InvalidSchema)
        ));

        let root = test_root("preserve-v7-schema");
        let paths = test_paths(&root);
        let path = operation_journal_path(&paths);
        fs::create_dir_all(path.parent().expect("journal parent")).expect("create journal parent");
        fs::write(&path, legacy.as_bytes()).expect("write v7 journal snapshot");

        assert!(matches!(
            OperationJournalStore::try_load_from_paths(&paths),
            Err(OperationJournalStoreError::Snapshot(
                super::OperationJournalLoadError::InvalidSchema
            ))
        ));
        assert_eq!(
            fs::read(&path).expect("v6 journal remains"),
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
                    OperationJournalSnapshot::new(vec![test_entry("operation-invalid-current")], 2)
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
            next_sequence: 1,
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
            let operation_id =
                OperationId::deterministic_test(format!("operation-terminal-{index:02}"));
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
                .create(planned_entry(&OperationId::deterministic_test(
                    "operation-active-3"
                )))
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
    async fn exact_idempotent_checkpoint_duplicate_does_not_submit_persistence() {
        let (root, _paths, backend, _coordinator, store) =
            persistence_fixture("exact-duplicate-idempotent-checkpoint");
        let operation_id = OperationId::deterministic_test("operation-idempotent-checkpoint");
        store
            .create(planned_entry(&operation_id))
            .await
            .expect("create journal");
        let checkpoint = completed_step("durable_checkpoint");
        store
            .record_idempotent_checkpoint(&operation_id, checkpoint.clone())
            .await
            .expect("record checkpoint");
        let attempts = backend.attempts.load(Ordering::SeqCst);

        store
            .record_idempotent_checkpoint(&operation_id, checkpoint)
            .await
            .expect("exact checkpoint duplicate is a no-op");

        assert_eq!(backend.attempts.load(Ordering::SeqCst), attempts);
        assert!(!store.has_retry_candidate());
        assert_eq!(
            store
                .get(&operation_id)
                .expect("checkpoint journal")
                .completed_steps
                .len(),
            1
        );
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
        let operation_label = "operation-invalid-update";
        let operation_id = OperationId::deterministic_test(operation_label);
        store
            .create(test_entry(operation_label))
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
        drop(store);

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
        drop(store);

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
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                OperationJournalStore::try_load_from_paths_with_coordinator(&paths, coordinator)
            }))
            .is_err()
        );
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
                    OperationJournalSnapshot::new(vec![planned_entry(&operation_id)], 2)
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
        let mut entry = OperationJournalEntry::new(
            JournalId::new(format!("journal-{operation_id}")),
            operation_id.clone(),
            CommandKind::InstallVersion,
            StabilizationSystem::Application,
            OwnershipClass::LauncherManaged,
            RollbackState::NotApplicable,
        );
        entry.sequence = 1;
        entry
    }

    fn completed_step(id: &str) -> OperationJournalStep {
        let mut step = OperationJournalStep::new(id, OperationPhase::Running);
        step.result = crate::state::contracts::OperationStepResult::Completed;
        step
    }

    fn performance_intent(instance_id: &str) -> PerformanceOperationIntent {
        PerformanceOperationIntent {
            instance_id: instance_id.to_string(),
            requested_action: PerformanceOperationAction::Install,
            action: PerformanceOperationAction::Install,
            base_target_id: "composition-base".to_string(),
            rollback: RollbackState::Available,
            game_version: Some("1.21.5".to_string()),
            loader: Some("fabric".to_string()),
            mode: Some("balanced".to_string()),
            rollback_id: None,
        }
    }

    fn performance_prepared(_result_target_id: &str) -> PerformanceOperationPrepared {
        PerformanceOperationPrepared {
            result_target_id: "composition-base".to_string(),
            proof: PerformancePreparedProof::InstallPlan {
                graph_sha512: "0123456789abcdef".repeat(8),
                artifact_count: 3,
                aggregate_bytes: 4096,
            },
        }
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
        entry.sequence = 1;
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
