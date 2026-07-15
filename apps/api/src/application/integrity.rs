use crate::application::registered_artifact_recovery::{
    RegisteredArtifactComponentRebuildSource, prepare_tier2_registered_artifact_recovery,
};
use crate::execution::integrity::{
    IntegrityTier2OwnedWork, IntegrityTier2OwnedWorkRejection, IntegrityTier2Report,
    IntegrityTier2Status,
};
use crate::guardian::{
    TIER2_INTEGRITY_COUNTER_TOKEN_COUNT, Tier2IntegrityGuardianEvidence,
    tier2_integrity_guardian_evidence,
};
use crate::state::contracts::{
    CommandKind, JournalId, OperationId, OperationJournalEntry, OperationJournalStep,
    OperationOutcome, OperationPhase, OperationStatus, OperationStepResult, OwnershipClass,
    RollbackState, StabilizationSystem, TargetDescriptor, TargetKind,
};
use crate::state::{
    AppState, IdleSweepCancellation, IdleSweepReserveError, IdleSweepSettlement,
    IdleSweepSettlementOwner, IdleSweepTerminal, IntegrityIdleEpoch,
    KnownGoodVerificationUnavailable, OperationJournalReconciliation, OperationJournalStore,
    OperationJournalStoreError, ProducerLease,
};
use axial_config::is_canonical_instance_id;
use std::time::Duration;

const TIER2_INTEGRITY_OPERATION_PREFIX: &str = "integrity-sweep-";
const TIER2_INTEGRITY_STEP: &str = "tier2_integrity_sweep";
const TIER2_INTEGRITY_FAILURE: &str = "tier2_integrity_refused";
const JOURNAL_RETRY_INITIAL_DELAY: Duration = Duration::from_millis(50);
const JOURNAL_RETRY_MAX_DELAY: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum IdleIntegrityTerminal {
    Succeeded,
    Cancelled,
    Refused,
}

#[must_use = "a planned integrity sweep must execute or be cancelled"]
pub(super) struct PlannedIntegritySweep {
    state: AppState,
    producer: ProducerLease,
    instance_id: String,
    journal: OperationJournalEntry,
}

#[must_use = "a reserved integrity sweep must execute or be cancelled"]
pub(super) struct ReservedIntegritySweep {
    planned: PlannedIntegritySweep,
    settlement: IdleSweepSettlementOwner,
}

#[must_use = "a started integrity sweep must be awaited or deliberately detached"]
pub(super) struct IntegritySweepExecution {
    completion: tokio::task::JoinHandle<Result<IdleIntegrityTerminal, Tier2IntegritySweepError>>,
}

#[must_use = "a failed integrity reservation must cancel its durable plan"]
pub(super) struct IntegritySweepReservationFailure {
    planned: Box<PlannedIntegritySweep>,
    error: IdleSweepReserveError,
}

#[derive(Debug, thiserror::Error)]
pub(super) enum Tier2IntegritySweepError {
    #[error("Tier 2 integrity target is not a canonical instance")]
    InvalidInstanceId,
    #[error("Tier 2 integrity target is not registered")]
    InstanceNotRegistered,
    #[error(transparent)]
    Journal(#[from] OperationJournalStoreError),
}

impl Tier2IntegritySweepError {
    pub(super) const fn class(&self) -> &'static str {
        match self {
            Self::InvalidInstanceId => "invalid_instance_id",
            Self::InstanceNotRegistered => "instance_not_registered",
            Self::Journal(error) => error.class(),
        }
    }
}

pub(super) async fn plan_tier2_integrity_sweep(
    state: AppState,
    producer: ProducerLease,
    instance_id: String,
) -> Result<PlannedIntegritySweep, Tier2IntegritySweepError> {
    let operation_id = OperationId::new(format!(
        "{TIER2_INTEGRITY_OPERATION_PREFIX}{}",
        uuid::Uuid::new_v4()
    ));
    plan_tier2_integrity_sweep_with_id(state, producer, instance_id, operation_id).await
}

async fn plan_tier2_integrity_sweep_with_id(
    state: AppState,
    producer: ProducerLease,
    instance_id: String,
    operation_id: OperationId,
) -> Result<PlannedIntegritySweep, Tier2IntegritySweepError> {
    if !is_canonical_instance_id(&instance_id) {
        return Err(Tier2IntegritySweepError::InvalidInstanceId);
    }
    if state.instances().get(&instance_id).is_none() {
        return Err(Tier2IntegritySweepError::InstanceNotRegistered);
    }
    let journal = planned_tier2_integrity_journal(operation_id, &instance_id);
    create_plan_reconciled(state.journals(), &journal).await?;
    Ok(PlannedIntegritySweep {
        state,
        producer,
        instance_id,
        journal,
    })
}

impl PlannedIntegritySweep {
    #[cfg(test)]
    fn operation_id(&self) -> &OperationId {
        &self.journal.operation_id
    }

    pub(super) fn reserve(
        self,
        expected_epoch: IntegrityIdleEpoch,
    ) -> Result<ReservedIntegritySweep, IntegritySweepReservationFailure> {
        match self
            .state
            .try_reserve_idle_sweep(expected_epoch, self.producer.claim_child())
        {
            Ok(reservation) => Ok(ReservedIntegritySweep {
                planned: self,
                settlement: IdleSweepSettlementOwner::new(reservation),
            }),
            Err(error) => Err(IntegritySweepReservationFailure {
                planned: Box::new(self),
                error,
            }),
        }
    }

    pub(super) async fn cancel(self) -> Result<IdleIntegrityTerminal, Tier2IntegritySweepError> {
        let transition = Tier2TerminalTransition::cancelled(Tier2IntegrityCounters::default());
        self.record_terminal(&transition).await?;
        Ok(IdleIntegrityTerminal::Cancelled)
    }

    fn start_execute_reserved(
        self,
        settlement: IdleSweepSettlementOwner,
    ) -> IntegritySweepExecution {
        let Self {
            state,
            producer,
            instance_id,
            journal,
        } = self;
        IntegritySweepExecution {
            completion: producer.spawn_joinable(execute_reserved_owned(
                state,
                instance_id,
                journal,
                settlement,
                |_| {},
            )),
        }
    }

    #[cfg(test)]
    fn start_execute_reserved_with<AfterSpawn>(
        self,
        settlement: IdleSweepSettlementOwner,
        after_spawn: AfterSpawn,
    ) -> IntegritySweepExecution
    where
        AfterSpawn: FnOnce(IdleSweepCancellation) + Send + 'static,
    {
        let Self {
            state,
            producer,
            instance_id,
            journal,
        } = self;
        IntegritySweepExecution {
            completion: producer.spawn_joinable(execute_reserved_owned(
                state,
                instance_id,
                journal,
                settlement,
                after_spawn,
            )),
        }
    }

    #[cfg(test)]
    fn start_execute_reserved_with_source<AfterSpawn>(
        self,
        settlement: IdleSweepSettlementOwner,
        after_spawn: AfterSpawn,
        rebuild_source: RegisteredArtifactComponentRebuildSource,
    ) -> IntegritySweepExecution
    where
        AfterSpawn: FnOnce(IdleSweepCancellation) + Send + 'static,
    {
        let Self {
            state,
            producer,
            instance_id,
            journal,
        } = self;
        IntegritySweepExecution {
            completion: producer.spawn_joinable(execute_reserved_owned_with_source(
                state,
                instance_id,
                journal,
                settlement,
                after_spawn,
                rebuild_source,
            )),
        }
    }

    async fn record_terminal(
        &self,
        transition: &Tier2TerminalTransition,
    ) -> Result<(), OperationJournalStoreError> {
        record_terminal_reconciled(self.state.journals(), &self.journal, transition).await
    }
}

async fn execute_reserved_owned<AfterSpawn>(
    state: AppState,
    instance_id: String,
    journal: OperationJournalEntry,
    settlement: IdleSweepSettlementOwner,
    after_spawn: AfterSpawn,
) -> Result<IdleIntegrityTerminal, Tier2IntegritySweepError>
where
    AfterSpawn: FnOnce(IdleSweepCancellation),
{
    execute_reserved_owned_with_source(
        state,
        instance_id,
        journal,
        settlement,
        after_spawn,
        RegisteredArtifactComponentRebuildSource::Production,
    )
    .await
}

async fn execute_reserved_owned_with_source<AfterSpawn>(
    state: AppState,
    instance_id: String,
    journal: OperationJournalEntry,
    settlement: IdleSweepSettlementOwner,
    after_spawn: AfterSpawn,
    rebuild_source: RegisteredArtifactComponentRebuildSource,
) -> Result<IdleIntegrityTerminal, Tier2IntegritySweepError>
where
    AfterSpawn: FnOnce(IdleSweepCancellation),
{
    let authority = settlement.authority();
    let ticket = match state
        .mint_known_good_tier2_ticket(&authority, &instance_id)
        .await
    {
        Ok(ticket) => ticket,
        Err(KnownGoodVerificationUnavailable::SweepAuthorityUnavailable) => {
            settlement.settle(IdleSweepTerminal::Cancelled);
            let transition = Tier2TerminalTransition::cancelled(Tier2IntegrityCounters::default());
            record_terminal_reconciled(state.journals(), &journal, &transition).await?;
            return Ok(IdleIntegrityTerminal::Cancelled);
        }
        Err(
            KnownGoodVerificationUnavailable::InstanceNotRegistered
            | KnownGoodVerificationUnavailable::LibraryRootUnavailable
            | KnownGoodVerificationUnavailable::LiveAuthorityUnavailable,
        ) => {
            settlement.settle(IdleSweepTerminal::Refused);
            let transition = Tier2TerminalTransition::failed(
                Tier2IntegrityCounters::default(),
                Tier2IntegrityGuardianEvidence::empty(),
            );
            record_terminal_reconciled(state.journals(), &journal, &transition).await?;
            return Ok(IdleIntegrityTerminal::Refused);
        }
    };

    let cancellation = settlement.cancellation();
    let work = match IntegrityTier2OwnedWork::new(state.clone(), ticket, settlement) {
        Ok(work) => work,
        Err(mismatch) => {
            let (transition, terminal) = match mismatch.settle() {
                IntegrityTier2OwnedWorkRejection::Cancelled => (
                    Tier2TerminalTransition::cancelled(Tier2IntegrityCounters::default()),
                    IdleIntegrityTerminal::Cancelled,
                ),
                IntegrityTier2OwnedWorkRejection::Refused => (
                    Tier2TerminalTransition::failed(
                        Tier2IntegrityCounters::default(),
                        Tier2IntegrityGuardianEvidence::empty(),
                    ),
                    IdleIntegrityTerminal::Refused,
                ),
            };
            record_terminal_reconciled(state.journals(), &journal, &transition).await?;
            return Ok(terminal);
        }
    };
    let worker = work.spawn();
    after_spawn(cancellation);
    let result = worker.join().await;
    let (transition, terminal) = match result {
        Ok(result) => {
            let recovery_state = state.clone();
            let recovery_operation_id = journal.operation_id.clone();
            let client = reqwest::Client::new();
            let (report, settlement, recovery_error) = result
                .settle_after_registered_artifact_recovery(move |report, findings| {
                    let recovery = prepare_tier2_registered_artifact_recovery(
                        recovery_state,
                        &recovery_operation_id,
                        report,
                        findings,
                        client,
                        rebuild_source,
                    );
                    async move { recovery.execute().await.map(drop) }
                })
                .await;
            if let Some(error) = recovery_error {
                tracing::warn!(
                    operation_id = journal.operation_id.as_str(),
                    error_kind = error.class(),
                    "Tier 2 registered artifact recovery failed"
                );
            }
            match report.status {
                IntegrityTier2Status::Complete => {
                    debug_assert_eq!(settlement, IdleSweepSettlement::Authoritative);
                    let counters = Tier2IntegrityCounters::from(&report);
                    let evidence =
                        tier2_integrity_guardian_evidence(&journal.operation_id, &report.facts);
                    (
                        Tier2TerminalTransition::succeeded(counters, evidence),
                        IdleIntegrityTerminal::Succeeded,
                    )
                }
                IntegrityTier2Status::Cancelled => (
                    Tier2TerminalTransition::cancelled(Tier2IntegrityCounters::from(&report)),
                    IdleIntegrityTerminal::Cancelled,
                ),
                IntegrityTier2Status::Refused => {
                    debug_assert_eq!(settlement, IdleSweepSettlement::Superseded);
                    let counters = Tier2IntegrityCounters::from(&report);
                    let evidence =
                        tier2_integrity_guardian_evidence(&journal.operation_id, &report.facts);
                    (
                        Tier2TerminalTransition::failed(counters, evidence),
                        IdleIntegrityTerminal::Refused,
                    )
                }
            }
        }
        Err(_) => (
            Tier2TerminalTransition::failed(
                Tier2IntegrityCounters::default(),
                Tier2IntegrityGuardianEvidence::empty(),
            ),
            IdleIntegrityTerminal::Refused,
        ),
    };
    record_terminal_reconciled(state.journals(), &journal, &transition).await?;
    Ok(terminal)
}

impl ReservedIntegritySweep {
    pub(super) fn is_current(&self) -> bool {
        self.settlement.is_current()
    }

    pub(super) fn start(self) -> IntegritySweepExecution {
        self.planned.start_execute_reserved(self.settlement)
    }

    #[cfg(test)]
    pub(super) async fn execute(self) -> Result<IdleIntegrityTerminal, Tier2IntegritySweepError> {
        self.start().wait().await
    }

    #[cfg(test)]
    async fn execute_with_rebuild_source(
        self,
        rebuild_source: RegisteredArtifactComponentRebuildSource,
    ) -> Result<IdleIntegrityTerminal, Tier2IntegritySweepError> {
        self.planned
            .start_execute_reserved_with_source(self.settlement, |_| {}, rebuild_source)
            .wait()
            .await
    }

    pub(super) async fn cancel(self) -> Result<IdleIntegrityTerminal, Tier2IntegritySweepError> {
        self.settlement.settle(IdleSweepTerminal::Cancelled);
        self.planned.cancel().await
    }

    #[cfg(test)]
    async fn execute_cancelling_after_spawn(
        self,
    ) -> Result<IdleIntegrityTerminal, Tier2IntegritySweepError> {
        self.planned
            .start_execute_reserved_with(self.settlement, |cancellation| cancellation.cancel())
            .wait()
            .await
    }
}

impl IntegritySweepExecution {
    pub(super) async fn wait(self) -> Result<IdleIntegrityTerminal, Tier2IntegritySweepError> {
        self.completion
            .await
            .expect("owned Tier 2 sweep task must return its terminal result")
    }
}

impl IntegritySweepReservationFailure {
    pub(super) const fn class(&self) -> &'static str {
        match self.error {
            IdleSweepReserveError::Closing => "closing",
            IdleSweepReserveError::EpochChanged => "epoch_changed",
            IdleSweepReserveError::ForegroundActive => "foreground_active",
            IdleSweepReserveError::SweepActive => "sweep_active",
        }
    }

    pub(super) async fn cancel(self) -> Result<IdleIntegrityTerminal, Tier2IntegritySweepError> {
        self.planned.cancel().await
    }
}

pub(super) async fn reconcile_interrupted_tier2_integrity_sweeps(
    state: &AppState,
    producer: ProducerLease,
) -> Result<(), OperationJournalStoreError> {
    let _producer = producer;
    let interrupted = state
        .journals()
        .list()
        .into_iter()
        .filter(tier2_restart_journal_is_exact)
        .collect::<Vec<_>>();
    for journal in interrupted {
        record_terminal_reconciled(
            state.journals(),
            &journal,
            &Tier2TerminalTransition::cancelled(Tier2IntegrityCounters::default()),
        )
        .await?;
    }
    Ok(())
}

fn planned_tier2_integrity_journal(
    operation_id: OperationId,
    instance_id: &str,
) -> OperationJournalEntry {
    let mut journal = OperationJournalEntry::new(
        JournalId::new(format!("journal-{}", operation_id.as_str())),
        operation_id,
        CommandKind::ValidateInstance,
        StabilizationSystem::Application,
        OwnershipClass::LauncherManaged,
        RollbackState::NotApplicable,
    );
    journal.targets.push(TargetDescriptor::new(
        StabilizationSystem::Application,
        TargetKind::Instance,
        instance_id,
        OwnershipClass::LauncherManaged,
    ));
    journal.planned_steps.push(tier2_integrity_step(
        OperationStepResult::Planned,
        Tier2IntegrityCounters::none(),
    ));
    journal
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct Tier2IntegrityCounters {
    selected_entry_count: usize,
    verified_entry_count: usize,
    processed_entry_count: usize,
    hashed_entry_count: usize,
    expected_content_byte_count: u64,
    content_read_byte_count: u64,
    metadata_lookup_count: usize,
    link_lookup_count: usize,
    suppressed_fact_count: usize,
}

impl Tier2IntegrityCounters {
    const fn none() -> Option<Self> {
        None
    }

    fn tokens(self) -> [String; TIER2_INTEGRITY_COUNTER_TOKEN_COUNT] {
        [
            format!(
                "integrity_counter:selected_entry_count:{}",
                self.selected_entry_count
            ),
            format!(
                "integrity_counter:verified_entry_count:{}",
                self.verified_entry_count
            ),
            format!(
                "integrity_counter:processed_entry_count:{}",
                self.processed_entry_count
            ),
            format!(
                "integrity_counter:hashed_entry_count:{}",
                self.hashed_entry_count
            ),
            format!(
                "integrity_counter:expected_content_byte_count:{}",
                self.expected_content_byte_count
            ),
            format!(
                "integrity_counter:content_read_byte_count:{}",
                self.content_read_byte_count
            ),
            format!(
                "integrity_counter:metadata_lookup_count:{}",
                self.metadata_lookup_count
            ),
            format!(
                "integrity_counter:link_lookup_count:{}",
                self.link_lookup_count
            ),
            format!(
                "integrity_counter:suppressed_fact_count:{}",
                self.suppressed_fact_count
            ),
        ]
    }
}

impl From<&IntegrityTier2Report> for Tier2IntegrityCounters {
    fn from(report: &IntegrityTier2Report) -> Self {
        Self {
            selected_entry_count: report.selected_entry_count,
            verified_entry_count: report.verified_entry_count,
            processed_entry_count: report.processed_entry_count,
            hashed_entry_count: report.hashed_entry_count,
            expected_content_byte_count: report.expected_content_byte_count,
            content_read_byte_count: report.content_read_byte_count,
            metadata_lookup_count: report.metadata_lookup_count,
            link_lookup_count: report.link_lookup_count,
            suppressed_fact_count: report.suppressed_fact_count,
        }
    }
}

#[derive(Clone)]
enum Tier2TerminalTransition {
    Succeeded {
        step: OperationJournalStep,
        fact_ids: Vec<String>,
        diagnosis_ids: Vec<crate::guardian::DiagnosisId>,
    },
    Failed {
        step: OperationJournalStep,
        fact_ids: Vec<String>,
        diagnosis_ids: Vec<crate::guardian::DiagnosisId>,
    },
    Cancelled {
        step: OperationJournalStep,
    },
}

impl Tier2TerminalTransition {
    fn succeeded(
        counters: Tier2IntegrityCounters,
        evidence: Tier2IntegrityGuardianEvidence,
    ) -> Self {
        Self::Succeeded {
            step: tier2_integrity_step(OperationStepResult::Completed, Some(counters)),
            fact_ids: evidence.fact_ids().to_vec(),
            diagnosis_ids: evidence.diagnosis_ids().to_vec(),
        }
    }

    fn failed(counters: Tier2IntegrityCounters, evidence: Tier2IntegrityGuardianEvidence) -> Self {
        Self::Failed {
            step: tier2_integrity_step(OperationStepResult::Failed, Some(counters)),
            fact_ids: evidence.fact_ids().to_vec(),
            diagnosis_ids: evidence.diagnosis_ids().to_vec(),
        }
    }

    fn cancelled(counters: Tier2IntegrityCounters) -> Self {
        Self::Cancelled {
            step: tier2_integrity_step(OperationStepResult::Skipped, Some(counters)),
        }
    }

    async fn apply(
        &self,
        journals: &OperationJournalStore,
        operation_id: &OperationId,
    ) -> Result<(), OperationJournalStoreError> {
        match self {
            Self::Succeeded {
                step,
                fact_ids,
                diagnosis_ids,
            } => {
                journals
                    .record_success_with_guardian_evidence(
                        operation_id,
                        step.clone(),
                        fact_ids.clone(),
                        diagnosis_ids.clone(),
                    )
                    .await
            }
            Self::Failed {
                step,
                fact_ids,
                diagnosis_ids,
            } => {
                journals
                    .record_failure_with_guardian_evidence(
                        operation_id,
                        step.clone(),
                        TIER2_INTEGRITY_FAILURE,
                        OperationOutcome::Failed,
                        fact_ids.clone(),
                        diagnosis_ids.clone(),
                    )
                    .await
            }
            Self::Cancelled { step } => {
                journals
                    .record_cancellation(operation_id, step.clone())
                    .await
            }
        }
    }

    fn expected(&self, planned: &OperationJournalEntry) -> OperationJournalEntry {
        let mut expected = planned.clone();
        match self {
            Self::Succeeded {
                step,
                fact_ids,
                diagnosis_ids,
            } => {
                expected.status = OperationStatus::Succeeded;
                expected.completed_steps.push(step.clone());
                append_unique(
                    &mut expected
                        .completed_steps
                        .last_mut()
                        .expect("Tier 2 success step")
                        .generated_facts,
                    fact_ids,
                );
                expected.guardian_diagnosis_ids = diagnosis_ids.clone();
                expected.outcome = Some(OperationOutcome::Succeeded);
            }
            Self::Failed {
                step,
                fact_ids,
                diagnosis_ids,
            } => {
                expected.status = OperationStatus::Failed;
                expected.completed_steps.push(step.clone());
                append_unique(
                    &mut expected
                        .completed_steps
                        .last_mut()
                        .expect("Tier 2 failure step")
                        .generated_facts,
                    fact_ids,
                );
                expected.failure_point = Some(TIER2_INTEGRITY_FAILURE.to_string());
                expected.guardian_diagnosis_ids = diagnosis_ids.clone();
                expected.outcome = Some(OperationOutcome::Failed);
            }
            Self::Cancelled { step } => {
                expected.status = OperationStatus::Cancelled;
                expected.completed_steps = vec![step.clone()];
                expected.failure_point = None;
                expected.guardian_diagnosis_ids.clear();
                expected.outcome = Some(OperationOutcome::Cancelled);
            }
        }
        expected
    }
}

fn tier2_integrity_step(
    result: OperationStepResult,
    counters: Option<Tier2IntegrityCounters>,
) -> OperationJournalStep {
    let mut step = OperationJournalStep::new(TIER2_INTEGRITY_STEP, OperationPhase::Validating);
    step.result = result;
    if let Some(counters) = counters {
        step.generated_facts.extend(counters.tokens());
    }
    step
}

fn append_unique(target: &mut Vec<String>, values: &[String]) {
    for value in values {
        if !target.contains(value) {
            target.push(value.clone());
        }
    }
}

async fn create_plan_reconciled(
    journals: &OperationJournalStore,
    expected: &OperationJournalEntry,
) -> Result<(), OperationJournalStoreError> {
    loop {
        match journals.create(expected.clone()).await {
            Ok(()) => return Ok(()),
            Err(OperationJournalStoreError::AlreadyExists)
                if journals
                    .get(&expected.operation_id)
                    .is_some_and(|entry| entry == *expected) =>
            {
                return Ok(());
            }
            Err(error) => match journals
                .reconcile_transition(
                    &expected.operation_id,
                    error,
                    JOURNAL_RETRY_INITIAL_DELAY,
                    JOURNAL_RETRY_MAX_DELAY,
                    |entry| entry == expected,
                )
                .await?
            {
                OperationJournalReconciliation::CommittedAfterPersistenceFailure(_)
                | OperationJournalReconciliation::RequestedTransitionAlreadyCommitted => {
                    return Ok(());
                }
                OperationJournalReconciliation::RetryRequestedTransition => {}
            },
        }
    }
}

async fn record_terminal_reconciled(
    journals: &OperationJournalStore,
    planned: &OperationJournalEntry,
    transition: &Tier2TerminalTransition,
) -> Result<(), OperationJournalStoreError> {
    let expected = transition.expected(planned);
    loop {
        match transition.apply(journals, &planned.operation_id).await {
            Ok(()) => {
                assert_eq!(
                    journals.get(&planned.operation_id).as_ref(),
                    Some(&expected),
                    "successful Tier 2 terminal journal write must be immediately visible"
                );
                return Ok(());
            }
            Err(OperationJournalStoreError::AlreadyTerminal)
                if journals
                    .get(&planned.operation_id)
                    .is_some_and(|entry| entry == expected) =>
            {
                return Ok(());
            }
            Err(error) => match journals
                .reconcile_transition(
                    &planned.operation_id,
                    error,
                    JOURNAL_RETRY_INITIAL_DELAY,
                    JOURNAL_RETRY_MAX_DELAY,
                    |entry| entry == &expected,
                )
                .await?
            {
                OperationJournalReconciliation::CommittedAfterPersistenceFailure(_)
                | OperationJournalReconciliation::RequestedTransitionAlreadyCommitted => {
                    return Ok(());
                }
                OperationJournalReconciliation::RetryRequestedTransition => {}
            },
        }
    }
}

fn tier2_operation_id_is_exact(value: &str) -> bool {
    let Some(suffix) = value.strip_prefix(TIER2_INTEGRITY_OPERATION_PREFIX) else {
        return false;
    };
    let Ok(uuid) = uuid::Uuid::parse_str(suffix) else {
        return false;
    };
    uuid.get_version() == Some(uuid::Version::Random)
        && uuid.get_variant() == uuid::Variant::RFC4122
        && uuid.hyphenated().to_string() == suffix
}

fn tier2_restart_journal_is_exact(entry: &OperationJournalEntry) -> bool {
    entry.command == CommandKind::ValidateInstance
        && tier2_operation_id_is_exact(entry.operation_id.as_str())
        && entry.journal_id.as_str() == format!("journal-{}", entry.operation_id.as_str())
        && entry.status == OperationStatus::Planned
        && entry.owner == StabilizationSystem::Application
        && entry.ownership == OwnershipClass::LauncherManaged
        && entry.targets.len() == 1
        && entry.targets[0].system == StabilizationSystem::Application
        && entry.targets[0].kind == TargetKind::Instance
        && is_canonical_instance_id(&entry.targets[0].id)
        && entry.targets[0].ownership == OwnershipClass::LauncherManaged
        && entry.planned_steps
            == vec![tier2_integrity_step(
                OperationStepResult::Planned,
                Tier2IntegrityCounters::none(),
            )]
        && entry.completed_steps.is_empty()
        && entry.failure_point.is_none()
        && entry.rollback == RollbackState::NotApplicable
        && entry.guardian_diagnosis_ids.is_empty()
        && entry.outcome.is_none()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::file::{FileWriteRequest, write_file_atomically};
    use crate::execution::persistence::{AtomicWriteBackend, PersistenceCoordinator};
    use crate::state::contracts::{ReconciliationComponent, ReconciliationRung};
    use crate::state::{AppStateInit, InstallStore, SessionStore, reconciliation_attempt_key};
    use axial_config::{AppPaths, ConfigStore, InstanceRegistrySnapshot, InstanceStore};
    use axial_minecraft::known_good::{
        KnownGoodArtifactKind, KnownGoodInventory, TestKnownGoodEntry, TestKnownGoodIntegrity,
        TestKnownGoodRoot,
    };
    use axial_performance::PerformanceManager;
    use sha1::{Digest as _, Sha1};
    use std::fs;
    use std::io;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::{SystemTime, UNIX_EPOCH};
    use tokio::sync::Notify;

    const FIRST_OPERATION_ID: &str = "integrity-sweep-00000000-0000-4000-8000-000000000001";
    const SECOND_OPERATION_ID: &str = "integrity-sweep-00000000-0000-4000-8000-000000000002";

    struct GatedJournalBackend {
        attempts: AtomicUsize,
        started: Notify,
        gate: Mutex<Option<Arc<JournalWriteGate>>>,
    }

    struct JournalWriteGate {
        released: Mutex<bool>,
        changed: Condvar,
    }

    struct JournalWriteGateHandle(Arc<JournalWriteGate>);

    impl GatedJournalBackend {
        fn new() -> Self {
            Self {
                attempts: AtomicUsize::new(0),
                started: Notify::new(),
                gate: Mutex::new(None),
            }
        }

        fn gate_next(&self) -> JournalWriteGateHandle {
            let gate = Arc::new(JournalWriteGate {
                released: Mutex::new(false),
                changed: Condvar::new(),
            });
            *self.gate.lock().expect("journal gate") = Some(gate.clone());
            JournalWriteGateHandle(gate)
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
    }

    impl JournalWriteGate {
        fn release(&self) {
            *self.released.lock().expect("journal gate") = true;
            self.changed.notify_all();
        }

        fn wait(&self) {
            let mut released = self.released.lock().expect("journal gate");
            while !*released {
                released = self.changed.wait(released).expect("journal gate wait");
            }
        }
    }

    impl JournalWriteGateHandle {
        fn release(&self) {
            self.0.release();
        }
    }

    impl Drop for JournalWriteGateHandle {
        fn drop(&mut self) {
            self.0.release();
        }
    }

    impl AtomicWriteBackend for GatedJournalBackend {
        fn write(
            &self,
            target: &TargetDescriptor,
            destination: &Path,
            contents: &[u8],
        ) -> io::Result<()> {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            self.started.notify_one();
            if let Some(gate) = self.gate.lock().expect("journal gate").take() {
                gate.wait();
            }
            write_file_atomically(FileWriteRequest::new(target.clone(), destination, contents))
                .map(|_| ())
                .map_err(io::Error::from)
        }
    }

    fn state_fixture(label: &str) -> (AppState, PathBuf, AppPaths) {
        let root = std::env::temp_dir().join(format!(
            "axial-application-integrity-{label}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let config_dir = root.join("config");
        let paths = AppPaths {
            config_file: config_dir.join("config.json"),
            instances_file: config_dir.join("instances.json"),
            instances_dir: root.join("instances"),
            music_dir: root.join("music"),
            library_dir: root.join("private-library-root"),
            config_dir,
        };
        let config = Arc::new(ConfigStore::load_from(paths.clone()).expect("load config"));
        let instances = Arc::new(
            InstanceStore::from_snapshot(paths.clone(), InstanceRegistrySnapshot::default())
                .expect("load instances"),
        );
        let state = AppState::new(AppStateInit {
            app_name: "Axial".to_string(),
            version: "test".to_string(),
            config,
            instances,
            installs: Arc::new(InstallStore::new()),
            sessions: Arc::new(SessionStore::new()),
            performance: Arc::new(
                PerformanceManager::load_for_startup(&paths.config_dir)
                    .expect("performance manager"),
            ),
            startup_warnings: Vec::new(),
            frontend_dir: root.join("frontend"),
        });
        fs::create_dir_all(&paths.library_dir).expect("create library root");
        state.set_library_dir_for_test(paths.library_dir.to_string_lossy().into_owned());
        (state, root, paths)
    }

    fn gated_state_fixture(label: &str) -> (AppState, PathBuf, AppPaths, Arc<GatedJournalBackend>) {
        let (state, root, paths) = state_fixture(label);
        let backend = Arc::new(GatedJournalBackend::new());
        let coordinator = PersistenceCoordinator::for_test(
            backend.clone(),
            Duration::from_millis(10),
            Duration::from_millis(50),
        );
        let mut journal_paths = paths.clone();
        journal_paths.config_dir = root.join("journal-config");
        let journals = Arc::new(
            OperationJournalStore::try_load_from_paths_with_coordinator(
                &journal_paths,
                coordinator,
            )
            .expect("claim gated journal store"),
        );
        let performance_operations = state.performance_operations().clone();
        let state = state.with_operation_stores(journals, performance_operations);
        (state, root, paths, backend)
    }

    async fn close_fixture(state: AppState, root: &Path) {
        state
            .close_known_good_inventories()
            .await
            .expect("close known-good store");
        state
            .close_instance_registry()
            .await
            .expect("close instance registry");
        drop(state);
        let _ = fs::remove_dir_all(root);
    }

    fn activate_empty_inventory(state: &AppState, instance_id: &str) {
        let inventory = KnownGoodInventory::from_test_entries(Vec::<TestKnownGoodEntry>::new())
            .expect("empty healthy inventory");
        state.activate_known_good_inventory_for_test(instance_id, inventory);
    }

    fn activate_corrupt_inventory(state: &AppState, instance_id: &str, paths: &AppPaths) {
        const OBJECT_BYTES: &[u8] = b"axial managed Assets fixture";
        let object_digest = format!("{:x}", Sha1::digest(OBJECT_BYTES));
        let empty_digest = format!("{:x}", Sha1::digest([]));
        let index_bytes = serde_json::to_vec(&serde_json::json!({
            "objects": {
                "fixture/object": {
                    "hash": object_digest.as_str(),
                    "size": OBJECT_BYTES.len()
                },
                "fixture/empty": {
                    "hash": empty_digest.as_str(),
                    "size": 0
                }
            }
        }))
        .expect("Assets fixture index");
        let parent = paths.library_dir.join("assets/indexes");
        fs::create_dir_all(&parent).expect("create corrupt Assets parent");
        fs::write(
            parent.join("fixture-assets.json"),
            vec![7_u8; index_bytes.len()],
        )
        .expect("write corrupt Assets index");
        let inventory = KnownGoodInventory::from_test_entries([
            TestKnownGoodEntry {
                root: TestKnownGoodRoot::Assets,
                path: "indexes/fixture-assets.json".to_string(),
                kind: KnownGoodArtifactKind::AssetIndex,
                integrity: TestKnownGoodIntegrity::Sha1 {
                    digest: format!("{:x}", Sha1::digest(&index_bytes)),
                    size: index_bytes.len() as u64,
                },
            },
            TestKnownGoodEntry {
                root: TestKnownGoodRoot::Assets,
                path: format!("objects/{}/{}", &object_digest[..2], object_digest),
                kind: KnownGoodArtifactKind::AssetObject,
                integrity: TestKnownGoodIntegrity::Sha1 {
                    digest: object_digest.clone(),
                    size: OBJECT_BYTES.len() as u64,
                },
            },
            TestKnownGoodEntry {
                root: TestKnownGoodRoot::Assets,
                path: format!("objects/{}/{}", &empty_digest[..2], empty_digest),
                kind: KnownGoodArtifactKind::AssetObject,
                integrity: TestKnownGoodIntegrity::Sha1 {
                    digest: empty_digest,
                    size: 0,
                },
            },
        ])
        .expect("corrupt Assets inventory")
        .with_test_standalone_leaf_repair_source(0, "https://example.invalid/fixture-assets.json")
        .expect("Assets index fixture source")
        .with_test_standalone_leaf_repair_source(
            1,
            &format!(
                "https://resources.download.minecraft.net/{}/{}",
                &object_digest[..2],
                object_digest
            ),
        )
        .expect("Assets object fixture source");
        state.activate_known_good_inventory_for_test(instance_id, inventory);
    }

    fn activate_large_corrupt_inventory(state: &AppState, instance_id: &str, paths: &AppPaths) {
        let parent = paths.library_dir.join("libraries/cancel");
        fs::create_dir_all(&parent).expect("create cancellation parent");
        fs::write(parent.join("large.jar"), vec![7_u8; 2 * 64 * 1024])
            .expect("write cancellation artifact");
        let inventory = KnownGoodInventory::from_test_entries([TestKnownGoodEntry {
            root: TestKnownGoodRoot::Libraries,
            path: "cancel/large.jar".to_string(),
            kind: KnownGoodArtifactKind::Library,
            integrity: TestKnownGoodIntegrity::File {
                size: 2 * 64 * 1024,
            },
        }])
        .expect("cancellation inventory");
        state.activate_known_good_inventory_for_test(instance_id, inventory);
    }

    async fn fixed_plan(
        state: &AppState,
        instance_id: &str,
        operation_id: &str,
    ) -> PlannedIntegritySweep {
        plan_tier2_integrity_sweep_with_id(
            state.clone(),
            state.try_claim_producer().expect("claim sweep owner"),
            instance_id.to_string(),
            OperationId::new(operation_id),
        )
        .await
        .expect("plan Tier 2 sweep")
    }

    fn reserve(plan: PlannedIntegritySweep, epoch: IntegrityIdleEpoch) -> ReservedIntegritySweep {
        match plan.reserve(epoch) {
            Ok(reserved) => reserved,
            Err(failure) => panic!("reserve Tier 2 sweep: {}", failure.class()),
        }
    }

    fn idle_epoch(state: &AppState) -> IntegrityIdleEpoch {
        let snapshot = *state.subscribe_integrity_idle().borrow();
        snapshot.epoch()
    }

    fn journal_fact_ids(entry: &OperationJournalEntry) -> Vec<&str> {
        entry
            .completed_steps
            .iter()
            .flat_map(|step| step.generated_facts.iter())
            .map(String::as_str)
            .filter(|value| value.starts_with("guardian_fact:"))
            .collect()
    }

    #[test]
    fn tier_two_plan_has_exact_durable_identity_and_target() {
        let operation_id = OperationId::new(FIRST_OPERATION_ID);
        let journal = planned_tier2_integrity_journal(operation_id.clone(), "0123456789abcdef");

        assert_eq!(
            journal.journal_id,
            JournalId::new(format!("journal-{FIRST_OPERATION_ID}"))
        );
        assert_eq!(journal.operation_id, operation_id);
        assert_eq!(journal.command, CommandKind::ValidateInstance);
        assert_eq!(journal.status, OperationStatus::Planned);
        assert_eq!(journal.owner, StabilizationSystem::Application);
        assert_eq!(journal.ownership, OwnershipClass::LauncherManaged);
        assert_eq!(journal.rollback, RollbackState::NotApplicable);
        assert_eq!(journal.targets.len(), 1);
        assert_eq!(journal.targets[0].system, StabilizationSystem::Application);
        assert_eq!(journal.targets[0].kind, TargetKind::Instance);
        assert_eq!(journal.targets[0].id, "0123456789abcdef");
        assert_eq!(
            journal.targets[0].ownership,
            OwnershipClass::LauncherManaged
        );
        assert_eq!(journal.planned_steps.len(), 1);
        assert_eq!(journal.planned_steps[0].step_id, TIER2_INTEGRITY_STEP);
        assert_eq!(journal.planned_steps[0].phase, OperationPhase::Validating);
        assert_eq!(
            journal.planned_steps[0].result,
            OperationStepResult::Planned
        );
        assert_eq!(
            journal.planned_steps[0].rollback,
            RollbackState::NotApplicable
        );
    }

    #[test]
    fn restart_identity_accepts_only_lowercase_hyphenated_rfc4122_v4() {
        assert!(tier2_operation_id_is_exact(FIRST_OPERATION_ID));
        assert!(!tier2_operation_id_is_exact(
            "integrity-sweep-AAAAAAAA-AAAA-4AAA-8AAA-AAAAAAAAAAAA"
        ));
        assert!(!tier2_operation_id_is_exact(
            "integrity-sweep-00000000-0000-3000-8000-000000000001"
        ));
        assert!(!tier2_operation_id_is_exact(
            "integrity-sweep-00000000-0000-4000-0000-000000000001"
        ));
        assert!(!tier2_operation_id_is_exact(
            "integrity-sweep-00000000000040008000000000000001"
        ));
    }

    #[tokio::test]
    async fn journal_plan_failure_creates_no_sweep_or_work() {
        let (state, root, _paths) = state_fixture("plan-gate");
        let instance = state
            .instances()
            .insert_for_test("Plan gate", "1.21.5")
            .expect("register instance");
        let journals = Arc::new(OperationJournalStore::with_max_entries(1));
        let blocker_id = OperationId::new("active-capacity-blocker");
        journals
            .create(OperationJournalEntry::new(
                JournalId::new("journal-active-capacity-blocker"),
                blocker_id,
                CommandKind::InstallVersion,
                StabilizationSystem::Application,
                OwnershipClass::LauncherManaged,
                RollbackState::NotApplicable,
            ))
            .await
            .expect("fill active journal capacity");
        let performance_operations = state.performance_operations().clone();
        let state = state.with_operation_stores(journals, performance_operations);
        let idle_before = *state.subscribe_integrity_idle().borrow();

        let result = plan_tier2_integrity_sweep_with_id(
            state.clone(),
            state.try_claim_producer().expect("claim plan owner"),
            instance.id,
            OperationId::new(FIRST_OPERATION_ID),
        )
        .await;

        let Err(error) = result else {
            panic!("capacity failure must reject the plan")
        };
        assert_eq!(error.class(), "capacity_exhausted");
        assert_eq!(*state.subscribe_integrity_idle().borrow(), idle_before);
        assert!(
            state
                .journals()
                .get(&OperationId::new(FIRST_OPERATION_ID))
                .is_none()
        );
        close_fixture(state, &root).await;
    }

    #[tokio::test]
    async fn unregistered_target_is_rejected_before_journaling() {
        let (state, root, _paths) = state_fixture("unregistered-plan");
        let idle_before = *state.subscribe_integrity_idle().borrow();

        let result = plan_tier2_integrity_sweep_with_id(
            state.clone(),
            state.try_claim_producer().expect("claim plan owner"),
            "0123456789abcdef".to_string(),
            OperationId::new(FIRST_OPERATION_ID),
        )
        .await;

        let Err(error) = result else {
            panic!("unregistered target must reject the plan")
        };
        assert_eq!(error.class(), "instance_not_registered");
        assert_eq!(*state.subscribe_integrity_idle().borrow(), idle_before);
        assert!(
            state
                .journals()
                .get(&OperationId::new(FIRST_OPERATION_ID))
                .is_none()
        );
        close_fixture(state, &root).await;
    }

    #[tokio::test]
    async fn healthy_sweep_records_counters_without_unknown_diagnosis() {
        let (state, root, _paths) = state_fixture("healthy");
        let instance = state
            .instances()
            .insert_for_test("Healthy", "1.21.5")
            .expect("register instance");
        activate_empty_inventory(&state, &instance.id);
        let plan = fixed_plan(&state, &instance.id, FIRST_OPERATION_ID).await;
        assert_eq!(plan.operation_id().as_str(), FIRST_OPERATION_ID);
        let reserved = reserve(plan, idle_epoch(&state));

        let terminal = reserved.execute().await.expect("execute healthy sweep");

        assert_eq!(terminal, IdleIntegrityTerminal::Succeeded);
        let journal = state
            .journals()
            .get(&OperationId::new(FIRST_OPERATION_ID))
            .expect("healthy terminal journal");
        assert_eq!(journal.status, OperationStatus::Succeeded);
        assert_eq!(journal.outcome, Some(OperationOutcome::Succeeded));
        assert!(journal_fact_ids(&journal).is_empty());
        assert!(journal.guardian_diagnosis_ids.is_empty());
        assert_eq!(journal.completed_steps[0].generated_facts.len(), 9);
        close_fixture(state, &root).await;
    }

    #[tokio::test]
    async fn terminal_journal_io_starts_after_settlement_and_retains_the_producer() {
        let (state, root, _paths, backend) = gated_state_fixture("terminal-order");
        let instance = state
            .instances()
            .insert_for_test("Terminal order", "1.21.5")
            .expect("register instance");
        activate_empty_inventory(&state, &instance.id);
        let plan = fixed_plan(&state, &instance.id, FIRST_OPERATION_ID).await;
        let reserved = reserve(plan, idle_epoch(&state));
        let gate = backend.gate_next();
        let terminal_attempt = backend.attempts.load(Ordering::SeqCst) + 1;
        let execution = tokio::spawn(reserved.execute());

        backend.wait_for_attempt(terminal_attempt).await;
        assert!(!execution.is_finished());
        assert_eq!(
            state
                .journals()
                .get(&OperationId::new(FIRST_OPERATION_ID))
                .expect("planned journal remains visible")
                .status,
            OperationStatus::Planned
        );
        drop(
            tokio::time::timeout(
                Duration::from_secs(1),
                state
                    .register_integrity_foreground()
                    .expect("foreground admission after sweep settlement")
                    .wait_for_settlement(),
            )
            .await
            .expect("foreground does not wait on terminal journal I/O"),
        );

        let quiesce_state = state.clone();
        let quiesce = tokio::spawn(async move { quiesce_state.quiesce().await });
        tokio::task::yield_now().await;
        assert!(!quiesce.is_finished());

        gate.release();
        assert_eq!(
            execution
                .await
                .expect("execution waiter")
                .expect("terminal journal commit"),
            IdleIntegrityTerminal::Succeeded
        );
        quiesce
            .await
            .expect("quiesce waiter")
            .expect("producer releases after terminal journal commit");
        assert_eq!(
            state
                .journals()
                .get(&OperationId::new(FIRST_OPERATION_ID))
                .expect("terminal journal")
                .status,
            OperationStatus::Succeeded
        );
        close_fixture(state, &root).await;
    }

    #[tokio::test]
    async fn corrupt_assets_sweep_repairs_lowest_source_backed_finding_before_completion() {
        let (state, root, paths) = state_fixture("corrupt");
        let instance = state
            .instances()
            .insert_for_test("Corrupt", "1.21.5")
            .expect("register instance");
        activate_corrupt_inventory(&state, &instance.id, &paths);
        let plan = fixed_plan(&state, &instance.id, FIRST_OPERATION_ID).await;
        let reserved = reserve(plan, idle_epoch(&state));

        let terminal = reserved
            .execute_with_rebuild_source(RegisteredArtifactComponentRebuildSource::Fixture)
            .await
            .expect("execute corrupt sweep");

        assert_eq!(terminal, IdleIntegrityTerminal::Succeeded);
        let journal = state
            .journals()
            .get(&OperationId::new(FIRST_OPERATION_ID))
            .expect("corrupt terminal journal");
        assert_eq!(journal.status, OperationStatus::Succeeded);
        assert!(journal_fact_ids(&journal).contains(&"guardian_fact:artifact_hash_mismatch"));
        assert_eq!(
            journal.guardian_diagnosis_ids,
            vec![crate::guardian::DiagnosisId::LauncherManagedArtifactCorrupt]
        );
        assert!(
            journal.completed_steps[0].generated_facts.len()
                <= crate::state::MAX_OPERATION_JOURNAL_STEP_FACTS
        );
        let child_journals = state
            .journals()
            .list()
            .into_iter()
            .filter(|entry| entry.operation_id != journal.operation_id)
            .filter_map(|entry| {
                entry
                    .reconciliation_attempt()
                    .cloned()
                    .map(|attempt| (entry, attempt))
            })
            .collect::<Vec<_>>();
        assert_eq!(child_journals.len(), 2);
        let leaf = child_journals
            .iter()
            .find(|(_, attempt)| attempt.rung() == ReconciliationRung::RepairArtifact)
            .expect("Assets leaf child journal");
        assert_eq!(leaf.0.status, OperationStatus::Failed);
        assert_eq!(leaf.1.component(), ReconciliationComponent::Assets);
        let component = child_journals
            .iter()
            .find(|(_, attempt)| attempt.rung() == ReconciliationRung::RebuildComponent)
            .expect("Assets component child journal");
        assert_eq!(component.0.status, OperationStatus::Succeeded);
        assert_eq!(component.1.component(), ReconciliationComponent::Assets);
        assert_ne!(leaf.0.operation_id, component.0.operation_id);
        assert_ne!(leaf.0.operation_id, journal.operation_id);
        assert_ne!(component.0.operation_id, journal.operation_id);
        for (entry, attempt) in [leaf, component] {
            let terminal = entry
                .reconciliation_terminal()
                .expect("child reconciliation terminal");
            assert_eq!(
                state
                    .failure_memory()
                    .get(&reconciliation_attempt_key(attempt))
                    .and_then(|memory| memory.reconciliation_terminal().cloned()),
                Some(terminal.clone()),
                "each child terminal must reach memory before parent success",
            );
        }
        assert!(
            fs::read(paths.library_dir.join("assets/indexes/fixture-assets.json"))
                .expect("rebuilt Assets index")
                .into_iter()
                .any(|byte| byte != 7)
        );
        close_fixture(state, &root).await;
    }

    #[tokio::test]
    async fn superseded_ticket_cancels_atomically_without_findings() {
        let (state, root, paths) = state_fixture("superseded");
        let instance = state
            .instances()
            .insert_for_test("Superseded", "1.21.5")
            .expect("register instance");
        activate_corrupt_inventory(&state, &instance.id, &paths);
        let plan = fixed_plan(&state, &instance.id, FIRST_OPERATION_ID).await;
        let reserved = reserve(plan, idle_epoch(&state));
        let foreground = state
            .register_integrity_foreground()
            .expect("supersede sweep before ticket mint");

        let terminal = reserved.execute().await.expect("cancel superseded sweep");
        drop(foreground.wait_for_settlement().await);

        assert_eq!(terminal, IdleIntegrityTerminal::Cancelled);
        let journal = state
            .journals()
            .get(&OperationId::new(FIRST_OPERATION_ID))
            .expect("cancelled terminal journal");
        assert_eq!(journal.status, OperationStatus::Cancelled);
        assert_eq!(journal.outcome, Some(OperationOutcome::Cancelled));
        assert!(journal_fact_ids(&journal).is_empty());
        assert!(journal.guardian_diagnosis_ids.is_empty());
        close_fixture(state, &root).await;
    }

    #[tokio::test]
    async fn spawned_worker_cancellation_settles_before_cancelled_journal_visibility() {
        let (state, root, paths, backend) = gated_state_fixture("worker-cancellation-order");
        let instance = state
            .instances()
            .insert_for_test("Worker cancellation", "1.21.5")
            .expect("register instance");
        activate_large_corrupt_inventory(&state, &instance.id, &paths);
        let plan = fixed_plan(&state, &instance.id, FIRST_OPERATION_ID).await;
        let reserved = reserve(plan, idle_epoch(&state));
        let gate = backend.gate_next();
        let terminal_attempt = backend.attempts.load(Ordering::SeqCst) + 1;
        let execution = tokio::spawn(reserved.execute_cancelling_after_spawn());

        backend.wait_for_attempt(terminal_attempt).await;
        assert!(!execution.is_finished());
        assert_eq!(
            state
                .journals()
                .get(&OperationId::new(FIRST_OPERATION_ID))
                .expect("planned journal remains atomically visible")
                .status,
            OperationStatus::Planned
        );
        drop(
            tokio::time::timeout(
                Duration::from_secs(1),
                state
                    .register_integrity_foreground()
                    .expect("foreground after cancelled sweep settlement")
                    .wait_for_settlement(),
            )
            .await
            .expect("sweep settlement precedes journal visibility"),
        );

        gate.release();
        assert_eq!(
            execution
                .await
                .expect("execution waiter")
                .expect("cancelled journal commit"),
            IdleIntegrityTerminal::Cancelled
        );
        let journal = state
            .journals()
            .get(&OperationId::new(FIRST_OPERATION_ID))
            .expect("cancelled terminal journal");
        assert_eq!(journal.status, OperationStatus::Cancelled);
        assert_eq!(journal.completed_steps[0].generated_facts.len(), 9);
        assert!(journal_fact_ids(&journal).is_empty());
        assert!(journal.guardian_diagnosis_ids.is_empty());
        close_fixture(state, &root).await;
    }

    #[tokio::test]
    async fn unavailable_ticket_refuses_without_spawning_or_artifact_findings() {
        let (state, root, _paths) = state_fixture("ticket-unavailable");
        let instance = state
            .instances()
            .insert_for_test("No inventory", "1.21.5")
            .expect("register instance");
        let plan = fixed_plan(&state, &instance.id, FIRST_OPERATION_ID).await;
        let reserved = reserve(plan, idle_epoch(&state));

        let terminal = reserved.execute().await.expect("record ticket refusal");

        assert_eq!(terminal, IdleIntegrityTerminal::Refused);
        let journal = state
            .journals()
            .get(&OperationId::new(FIRST_OPERATION_ID))
            .expect("refused terminal journal");
        assert_eq!(journal.status, OperationStatus::Failed);
        assert_eq!(
            journal.failure_point.as_deref(),
            Some(TIER2_INTEGRITY_FAILURE)
        );
        assert!(journal_fact_ids(&journal).is_empty());
        assert!(journal.guardian_diagnosis_ids.is_empty());
        assert!(
            journal.completed_steps[0]
                .generated_facts
                .iter()
                .all(|value| value.ends_with(":0"))
        );
        close_fixture(state, &root).await;
    }

    #[tokio::test]
    async fn reserve_race_uses_the_planned_cancellation_transition() {
        let (state, root, _paths) = state_fixture("reserve-race");
        let instance = state
            .instances()
            .insert_for_test("Reserve race", "1.21.5")
            .expect("register instance");
        let stale_epoch = state.subscribe_integrity_idle().borrow().epoch();
        let plan = fixed_plan(&state, &instance.id, FIRST_OPERATION_ID).await;
        let foreground = state
            .register_integrity_foreground()
            .expect("advance idle epoch");

        let failure = match plan.reserve(stale_epoch) {
            Ok(_) => panic!("stale epoch must not reserve"),
            Err(failure) => failure,
        };
        assert_eq!(failure.class(), "epoch_changed");
        drop(foreground);
        assert_eq!(
            failure.cancel().await.expect("cancel planned race"),
            IdleIntegrityTerminal::Cancelled
        );
        let journal = state
            .journals()
            .get(&OperationId::new(FIRST_OPERATION_ID))
            .expect("race cancellation journal");
        assert_eq!(journal.status, OperationStatus::Cancelled);
        assert!(journal_fact_ids(&journal).is_empty());
        assert!(journal.guardian_diagnosis_ids.is_empty());
        close_fixture(state, &root).await;
    }

    #[tokio::test]
    async fn restart_reconciliation_is_scoped_and_idempotent() {
        let (state, root, _paths) = state_fixture("restart-scope");
        let journals = Arc::new(OperationJournalStore::new());
        let interrupted = planned_tier2_integrity_journal(
            OperationId::new(FIRST_OPERATION_ID),
            "0123456789abcdef",
        );
        let terminal = planned_tier2_integrity_journal(
            OperationId::new(SECOND_OPERATION_ID),
            "0123456789abcdef",
        );
        let unrelated_id = OperationId::new("manual-validate-instance");
        let unrelated = planned_tier2_integrity_journal(unrelated_id.clone(), "0123456789abcdef");
        let foreign_id = OperationId::new("integrity-sweep-00000000-0000-4000-8000-000000000003");
        let foreign = OperationJournalEntry::new(
            JournalId::new("journal-foreign"),
            foreign_id.clone(),
            CommandKind::InstallVersion,
            StabilizationSystem::Application,
            OwnershipClass::LauncherManaged,
            RollbackState::NotApplicable,
        );
        let mismatched_id =
            OperationId::new("integrity-sweep-00000000-0000-4000-8000-000000000004");
        let mut mismatched =
            planned_tier2_integrity_journal(mismatched_id.clone(), "0123456789abcdef");
        mismatched.owner = StabilizationSystem::Guardian;
        for entry in [interrupted, terminal, unrelated, foreign, mismatched] {
            journals
                .create(entry)
                .await
                .expect("create restart fixture");
        }
        journals
            .record_success(
                &OperationId::new(SECOND_OPERATION_ID),
                tier2_integrity_step(
                    OperationStepResult::Completed,
                    Some(Tier2IntegrityCounters::default()),
                ),
                OperationOutcome::Succeeded,
            )
            .await
            .expect("terminalize completed fixture");
        let performance_operations = state.performance_operations().clone();
        let state = state.with_operation_stores(journals, performance_operations);

        for _ in 0..2 {
            reconcile_interrupted_tier2_integrity_sweeps(
                &state,
                state.try_claim_producer().expect("claim restart owner"),
            )
            .await
            .expect("reconcile interrupted sweep");
        }

        assert_eq!(
            state
                .journals()
                .get(&OperationId::new(FIRST_OPERATION_ID))
                .expect("reconciled journal")
                .status,
            OperationStatus::Cancelled
        );
        assert_eq!(
            state
                .journals()
                .get(&OperationId::new(SECOND_OPERATION_ID))
                .expect("terminal journal")
                .status,
            OperationStatus::Succeeded
        );
        assert_eq!(
            state
                .journals()
                .get(&unrelated_id)
                .expect("unrelated validate journal")
                .status,
            OperationStatus::Planned
        );
        assert_eq!(
            state
                .journals()
                .get(&foreign_id)
                .expect("foreign command journal")
                .status,
            OperationStatus::Planned
        );
        assert_eq!(
            state
                .journals()
                .get(&mismatched_id)
                .expect("shape-mismatched journal")
                .status,
            OperationStatus::Planned
        );
        close_fixture(state, &root).await;
    }
}
