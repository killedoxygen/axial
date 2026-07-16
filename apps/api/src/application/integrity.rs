use crate::application::registered_artifact_recovery::{
    RegisteredArtifactComponentRebuildSource, prepare_tier2_registered_artifact_recovery,
};
#[cfg(all(test, target_os = "linux"))]
use crate::execution::integrity::IntegrityTier2ProgressObserver;
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
                #[cfg(target_os = "linux")]
                None,
            )),
        }
    }

    #[cfg(all(test, target_os = "linux"))]
    fn start_execute_reserved_with_progress_observer(
        self,
        settlement: IdleSweepSettlementOwner,
        progress_observer: IntegrityTier2ProgressObserver,
    ) -> IntegritySweepExecution {
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
                |_| {},
                RegisteredArtifactComponentRebuildSource::Production,
                Some(progress_observer),
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
        #[cfg(all(test, target_os = "linux"))]
        None,
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
    #[cfg(all(test, target_os = "linux"))] progress_observer: Option<
        IntegrityTier2ProgressObserver,
    >,
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
    #[cfg(all(test, target_os = "linux"))]
    let work = match &progress_observer {
        Some(observer) => work.observe_progress_for_test(observer.clone()),
        None => work,
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
            #[cfg(all(test, target_os = "linux"))]
            if let Some(observer) = &progress_observer {
                observer.settlement_finished();
            }
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

    #[cfg(all(test, target_os = "linux"))]
    fn start_with_progress_observer(
        self,
        progress_observer: IntegrityTier2ProgressObserver,
    ) -> IntegritySweepExecution {
        self.planned
            .start_execute_reserved_with_progress_observer(self.settlement, progress_observer)
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
    #[cfg(target_os = "linux")]
    use std::io::Write as _;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Condvar, Mutex};
    #[cfg(target_os = "linux")]
    use std::time::Instant;
    use std::time::{SystemTime, UNIX_EPOCH};
    use tokio::sync::Notify;

    const FIRST_OPERATION_ID: &str = "integrity-sweep-00000000-0000-4000-8000-000000000001";
    const SECOND_OPERATION_ID: &str = "integrity-sweep-00000000-0000-4000-8000-000000000002";
    #[cfg(target_os = "linux")]
    const R5_REPRESENTATIVE_ENTRY_COUNT: usize = 442;
    #[cfg(target_os = "linux")]
    const R5_REPRESENTATIVE_CONTENT_BYTES: u64 = 344_363_465;
    #[cfg(target_os = "linux")]
    const R5_LAUNCH_SAMPLE_COUNT: usize = 21;
    #[cfg(target_os = "linux")]
    const R5_LAUNCH_IMPACT_CEILING: Duration = Duration::from_millis(10);

    #[cfg(target_os = "linux")]
    struct R5MeasurementRoot(PathBuf);

    #[cfg(target_os = "linux")]
    impl Drop for R5MeasurementRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

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
        state_fixture_at(root)
    }

    fn state_fixture_at(root: PathBuf) -> (AppState, PathBuf, AppPaths) {
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

    fn activate_corrupt_inventory(
        state: &AppState,
        instance_id: &str,
        version_id: &str,
        paths: &AppPaths,
    ) -> (Vec<u8>, String, String) {
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
        let version_json = serde_json::to_vec(&serde_json::json!({
            "id": version_id,
            "type": "release",
            "mainClass": "org.axial.GuardianFixture",
            "assetIndex": { "id": "fixture-assets" },
            "libraries": []
        }))
        .expect("Assets launch version metadata");
        let version_dir = paths.library_dir.join("versions").join(version_id);
        fs::create_dir_all(&version_dir).expect("create Assets launch version directory");
        fs::write(
            version_dir.join(format!("{version_id}.json")),
            &version_json,
        )
        .expect("write Assets launch version metadata");
        fs::write(version_dir.join(format!("{version_id}.jar")), b"client jar")
            .expect("write Assets launch client");
        let parent = paths.library_dir.join("assets/indexes");
        fs::create_dir_all(&parent).expect("create corrupt Assets parent");
        fs::write(
            parent.join("fixture-assets.json"),
            vec![7_u8; index_bytes.len()],
        )
        .expect("write corrupt Assets index");
        let inventory = KnownGoodInventory::from_test_entries([
            TestKnownGoodEntry {
                root: TestKnownGoodRoot::Versions,
                path: format!("{version_id}/{version_id}.json"),
                kind: KnownGoodArtifactKind::VersionMetadata,
                integrity: TestKnownGoodIntegrity::Sha1 {
                    digest: format!("{:x}", Sha1::digest(&version_json)),
                    size: version_json.len() as u64,
                },
            },
            TestKnownGoodEntry {
                root: TestKnownGoodRoot::Versions,
                path: format!("{version_id}/{version_id}.jar"),
                kind: KnownGoodArtifactKind::ClientJar,
                integrity: TestKnownGoodIntegrity::Sha1 {
                    digest: format!("{:x}", Sha1::digest(b"client jar")),
                    size: b"client jar".len() as u64,
                },
            },
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
                    digest: empty_digest.clone(),
                    size: 0,
                },
            },
        ])
        .expect("corrupt Assets inventory");
        let index_ordinal = inventory
            .entries()
            .iter()
            .position(|entry| entry.kind() == KnownGoodArtifactKind::AssetIndex)
            .expect("Assets index inventory ordinal");
        let object_ordinal = inventory
            .entries()
            .iter()
            .position(|entry| {
                entry.kind() == KnownGoodArtifactKind::AssetObject
                    && entry.path().as_str().ends_with(&object_digest)
            })
            .expect("Assets object inventory ordinal");
        let inventory = inventory
            .with_test_standalone_leaf_repair_source(
                index_ordinal,
                "https://example.invalid/fixture-assets.json",
            )
            .expect("Assets index fixture source")
            .with_test_standalone_leaf_repair_source(
                object_ordinal,
                &format!(
                    "https://resources.download.minecraft.net/{}/{}",
                    &object_digest[..2],
                    object_digest
                ),
            )
            .expect("Assets object fixture source");
        state.activate_known_good_inventory_for_test(instance_id, inventory);
        (index_bytes, object_digest, empty_digest)
    }

    fn activate_corrupt_version_bundle_inventory(
        state: &AppState,
        instance_id: &str,
        version_id: &str,
        paths: &AppPaths,
    ) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        const CLIENT_BYTES: &[u8] = b"axial managed VersionBundle client fixture";
        const LOG_ID: &str = "guardian-version-bundle.xml";
        const LOG_BYTES: &[u8] = b"<Configuration/>";

        let version_json = serde_json::to_vec(&serde_json::json!({
            "id": version_id,
            "type": "release",
            "mainClass": "org.axial.GuardianFixture"
        }))
        .expect("VersionBundle fixture metadata");
        let version_dir = paths.library_dir.join("versions").join(version_id);
        fs::create_dir_all(&version_dir).expect("create VersionBundle fixture directory");
        fs::write(
            version_dir.join(format!("{version_id}.json")),
            &version_json,
        )
        .expect("write exact VersionBundle metadata");
        fs::write(
            version_dir.join(format!("{version_id}.jar")),
            &CLIENT_BYTES[..CLIENT_BYTES.len() / 2],
        )
        .expect("write truncated VersionBundle client");
        let log_dir = paths.library_dir.join("assets/log_configs");
        fs::create_dir_all(&log_dir).expect("create VersionBundle log directory");
        fs::write(log_dir.join(LOG_ID), LOG_BYTES).expect("write exact VersionBundle log config");

        let inventory = KnownGoodInventory::from_test_entries([
            TestKnownGoodEntry {
                root: TestKnownGoodRoot::Versions,
                path: format!("{version_id}/{version_id}.json"),
                kind: KnownGoodArtifactKind::VersionMetadata,
                integrity: TestKnownGoodIntegrity::Sha1 {
                    digest: format!("{:x}", Sha1::digest(&version_json)),
                    size: version_json.len() as u64,
                },
            },
            TestKnownGoodEntry {
                root: TestKnownGoodRoot::Versions,
                path: format!("{version_id}/{version_id}.jar"),
                kind: KnownGoodArtifactKind::ClientJar,
                integrity: TestKnownGoodIntegrity::Sha1 {
                    digest: format!("{:x}", Sha1::digest(CLIENT_BYTES)),
                    size: CLIENT_BYTES.len() as u64,
                },
            },
            TestKnownGoodEntry {
                root: TestKnownGoodRoot::Assets,
                path: format!("log_configs/{LOG_ID}"),
                kind: KnownGoodArtifactKind::LogConfig,
                integrity: TestKnownGoodIntegrity::Sha1 {
                    digest: format!("{:x}", Sha1::digest(LOG_BYTES)),
                    size: LOG_BYTES.len() as u64,
                },
            },
        ])
        .expect("corrupt VersionBundle inventory");
        state.activate_known_good_inventory_for_test(instance_id, inventory);

        (version_json, CLIENT_BYTES.to_vec(), LOG_BYTES.to_vec())
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

    fn write_user_owned_sentinels(paths: &AppPaths, instance_id: &str) -> Vec<(PathBuf, Vec<u8>)> {
        [
            ("saves/world/level.dat", b"world".as_slice()),
            ("mods/user.jar", b"mod".as_slice()),
            ("config/user.toml", b"config".as_slice()),
            ("resourcepacks/user.zip", b"resourcepack".as_slice()),
        ]
        .into_iter()
        .map(|(relative, contents)| {
            let path = paths.instances_dir.join(instance_id).join(relative);
            fs::create_dir_all(path.parent().expect("user-owned sentinel parent"))
                .expect("create user-owned sentinel parent");
            fs::write(&path, contents).expect("write user-owned sentinel");
            (path, contents.to_vec())
        })
        .collect()
    }

    async fn assert_clean_tier1(state: &AppState, instance_id: &str, library_root: &Path) {
        let foreground = state
            .register_integrity_foreground()
            .expect("register repaired component postcheck")
            .wait_for_settlement()
            .await;
        let lifecycle = state.acquire_instance_lifecycle(instance_id).await;
        let postcheck = crate::execution::integrity::sense_integrity_tier1(
            state,
            &foreground,
            &lifecycle,
            library_root,
        )
        .await
        .expect("repaired component Tier1 postcheck");
        assert!(postcheck.facts.is_empty());
        drop((postcheck, lifecycle, foreground));
    }

    #[cfg(unix)]
    async fn assert_repaired_instance_launches_once(
        state: &AppState,
        root: &Path,
        instance_id: &str,
        label: &str,
    ) {
        use std::os::unix::fs::PermissionsExt as _;

        let java_dir = root.join(format!("{label}-java/bin"));
        fs::create_dir_all(&java_dir).expect("booting Java fixture directory");
        let java_path = java_dir.join("java");
        fs::write(
            &java_path,
            r#"#!/bin/sh
if [ "$1" = "-XshowSettings:property" ]; then
  echo 'openjdk version "21.0.3"' >&2
  exit 0
fi
count=0
if [ -f guardian-component-process-count ]; then
  count=$(cat guardian-component-process-count)
fi
count=$((count + 1))
printf '%s' "$count" > guardian-component-process-count
printf '%s\n' '[Render thread/INFO]: Created: 1024x512x4 minecraft:textures/atlas/blocks.png-atlas' >&2
sleep 1
exit 0
"#,
        )
        .expect("booting Java fixture");
        let mut permissions = fs::metadata(&java_path)
            .expect("booting Java metadata")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&java_path, permissions).expect("booting Java executable");
        let mut instance = state.instances().get(instance_id).expect("launch instance");
        instance.java_path = java_path.to_string_lossy().into_owned();
        state
            .instances()
            .replace_for_test(instance)
            .expect("set booting Java override");
        let producer = state
            .try_claim_producer()
            .expect("claim repaired component launch owner");
        let prepared = crate::application::launch::prepare_launch_session_owned(
            state,
            crate::application::launch::LaunchRequest {
                instance_id: instance_id.to_string(),
                username: None,
                max_memory_mb: None,
                min_memory_mb: None,
                client_started_at_ms: None,
            },
            &producer,
        )
        .await
        .unwrap_or_else(|(_, payload)| panic!("prepare repaired component launch: {payload:?}"));
        let session_id = prepared.task.intent.session_id.clone();
        let launched = tokio::time::timeout(
            Duration::from_secs(10),
            crate::application::launch::launch_session(state.clone(), prepared.task, producer),
        )
        .await
        .expect("repaired component launch deadline")
        .unwrap_or_else(|error| panic!("repaired component launch: {}", error.message));
        assert_eq!(launched.instance_id, instance_id);
        let game_dir = state.instances().game_dir(instance_id);
        assert_eq!(
            fs::read_to_string(game_dir.join("guardian-component-process-count"))
                .expect("repaired component process count"),
            "1"
        );
        let running = state
            .sessions()
            .get(&session_id)
            .await
            .expect("repaired component running session");
        assert_eq!(running.state, axial_launcher::LaunchState::Running);
        assert!(running.boot_completed_at_ms.is_some());
        let _ = state.sessions().kill(&session_id).await;
    }

    #[cfg(target_os = "linux")]
    fn r5_measurement_root() -> R5MeasurementRoot {
        let supplied_parent = PathBuf::from(
            std::env::var_os("AXIAL_R5_MEASUREMENT_ROOT")
                .expect("AXIAL_R5_MEASUREMENT_ROOT is required"),
        );
        let metadata =
            fs::symlink_metadata(&supplied_parent).expect("R5 measurement root metadata");
        assert!(metadata.is_dir(), "R5 measurement root must be a directory");
        assert!(
            !metadata.file_type().is_symlink(),
            "R5 measurement root must not be a symlink"
        );
        let parent = fs::canonicalize(supplied_parent).expect("canonical R5 measurement root");
        let root = parent.join(format!(
            "axial-r5-launch-impact-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        fs::create_dir(&root).expect("create isolated R5 measurement fixture");
        R5MeasurementRoot(root)
    }

    #[cfg(target_os = "linux")]
    fn r5_entry_size(index: usize) -> u64 {
        let base = R5_REPRESENTATIVE_CONTENT_BYTES / R5_REPRESENTATIVE_ENTRY_COUNT as u64;
        let remainder = R5_REPRESENTATIVE_CONTENT_BYTES % R5_REPRESENTATIVE_ENTRY_COUNT as u64;
        base + u64::from(index < remainder as usize)
    }

    #[cfg(target_os = "linux")]
    fn r5_write_patterned_file(path: &Path, size: u64, pattern: u8) -> String {
        fs::create_dir_all(path.parent().expect("R5 fixture parent"))
            .expect("create R5 fixture parent");
        let mut file = fs::File::create(path).expect("create R5 fixture file");
        let chunk = vec![pattern; 64 * 1024];
        let mut remaining = size;
        let mut hasher = Sha1::new();
        while remaining > 0 {
            let count = remaining.min(chunk.len() as u64) as usize;
            file.write_all(&chunk[..count])
                .expect("write R5 fixture file");
            hasher.update(&chunk[..count]);
            remaining -= count as u64;
        }
        format!("{:x}", hasher.finalize())
    }

    #[cfg(target_os = "linux")]
    fn r5_write_version_json(path: &Path, version_id: &str, size: u64) -> String {
        let mut bytes = serde_json::to_vec(&serde_json::json!({
            "id": version_id,
            "type": "release",
            "mainClass": "org.axial.GuardianR5Fixture",
            "assetIndex": {},
            "libraries": []
        }))
        .expect("encode R5 version metadata");
        assert!(bytes.len() < size as usize);
        bytes.resize(size as usize, b' ');
        fs::create_dir_all(path.parent().expect("R5 version parent"))
            .expect("create R5 version parent");
        fs::write(path, &bytes).expect("write R5 version metadata");
        format!("{:x}", Sha1::digest(&bytes))
    }

    #[cfg(target_os = "linux")]
    fn r5_activate_representative_inventory(
        state: &AppState,
        paths: &AppPaths,
        instance_id: &str,
        version_id: &str,
    ) {
        let version_dir = paths.library_dir.join("versions").join(version_id);
        let version_path = version_dir.join(format!("{version_id}.json"));
        let version_digest = r5_write_version_json(&version_path, version_id, r5_entry_size(0));
        let client_path = version_dir.join(format!("{version_id}.jar"));
        let client_digest = r5_write_patterned_file(&client_path, r5_entry_size(1), 0x51);
        let mut entries = vec![
            TestKnownGoodEntry {
                root: TestKnownGoodRoot::Versions,
                path: format!("{version_id}/{version_id}.json"),
                kind: KnownGoodArtifactKind::VersionMetadata,
                integrity: TestKnownGoodIntegrity::Sha1 {
                    digest: version_digest,
                    size: r5_entry_size(0),
                },
            },
            TestKnownGoodEntry {
                root: TestKnownGoodRoot::Versions,
                path: format!("{version_id}/{version_id}.jar"),
                kind: KnownGoodArtifactKind::ClientJar,
                integrity: TestKnownGoodIntegrity::Sha1 {
                    digest: client_digest,
                    size: r5_entry_size(1),
                },
            },
        ];
        for index in 2..R5_REPRESENTATIVE_ENTRY_COUNT {
            let relative = format!("guardian-r5/{index:04}.jar");
            let size = r5_entry_size(index);
            let digest = r5_write_patterned_file(
                &paths.library_dir.join("libraries").join(&relative),
                size,
                (index % 251) as u8,
            );
            entries.push(TestKnownGoodEntry {
                root: TestKnownGoodRoot::Libraries,
                path: relative,
                kind: KnownGoodArtifactKind::Library,
                integrity: TestKnownGoodIntegrity::Sha1 { digest, size },
            });
        }
        let inventory = KnownGoodInventory::from_test_entries(entries)
            .expect("synthetic representative R5 inventory");
        assert_eq!(inventory.entries().len(), R5_REPRESENTATIVE_ENTRY_COUNT);
        state.activate_known_good_inventory_for_test(instance_id, inventory);
    }

    #[cfg(target_os = "linux")]
    fn r5_install_booting_java(state: &AppState, root: &Path, instance_id: &str) {
        use std::os::unix::fs::PermissionsExt as _;

        let java_dir = root.join("guardian-r5-java/bin");
        fs::create_dir_all(&java_dir).expect("create R5 Java fixture directory");
        let java_path = java_dir.join("java");
        fs::write(
            &java_path,
            r#"#!/bin/sh
if [ "$1" = "-XshowSettings:property" ]; then
  echo 'openjdk version "21.0.3"' >&2
  exit 0
fi
count=0
if [ -f guardian-r5-process-count ]; then
  count=$(cat guardian-r5-process-count)
fi
count=$((count + 1))
printf '%s' "$count" > guardian-r5-process-count
printf '%s\n' '[Render thread/INFO]: Created: 1024x512x4 minecraft:textures/atlas/blocks.png-atlas' >&2
exec sleep 30
"#,
        )
        .expect("write R5 Java fixture");
        let mut permissions = fs::metadata(&java_path)
            .expect("R5 Java fixture metadata")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&java_path, permissions).expect("make R5 Java fixture executable");
        let mut instance = state.instances().get(instance_id).expect("R5 instance");
        instance.java_path = java_path.to_string_lossy().into_owned();
        state
            .instances()
            .replace_for_test(instance)
            .expect("set R5 Java fixture");
    }

    #[cfg(target_os = "linux")]
    fn r5_process_count(path: &Path) -> usize {
        fs::read_to_string(path)
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or_default()
    }

    #[cfg(target_os = "linux")]
    async fn r5_wait_until_stably_idle(state: &AppState) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !state.subscribe_integrity_idle().borrow().is_stably_idle() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("R5 fixture returns to stable idle");
    }

    #[cfg(target_os = "linux")]
    async fn r5_launch_once(state: &AppState, instance_id: &str) -> Duration {
        let started_at = Instant::now();
        let producer = state.try_claim_producer().expect("claim R5 launch owner");
        let prepared = crate::application::launch::prepare_launch_session_owned(
            state,
            crate::application::launch::LaunchRequest {
                instance_id: instance_id.to_string(),
                username: None,
                max_memory_mb: None,
                min_memory_mb: None,
                client_started_at_ms: None,
            },
            &producer,
        )
        .await
        .unwrap_or_else(|(_, payload)| panic!("prepare R5 launch: {payload:?}"));
        let session_id = prepared.task.intent.session_id.clone();
        tokio::time::timeout(
            Duration::from_secs(10),
            crate::application::launch::launch_session(state.clone(), prepared.task, producer),
        )
        .await
        .expect("R5 launch deadline")
        .unwrap_or_else(|error| panic!("R5 launch: {}", error.message));
        let elapsed = started_at.elapsed();
        let running = state
            .sessions()
            .get(&session_id)
            .await
            .expect("R5 running session");
        assert!(running.boot_completed_at_ms.is_some());
        state
            .sessions()
            .kill(&session_id)
            .await
            .expect("stop R5 launch");
        tokio::time::timeout(Duration::from_secs(5), async {
            while state.sessions().has_active_instance(instance_id).await {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("R5 launch session becomes terminal");
        r5_wait_until_stably_idle(state).await;
        elapsed
    }

    #[cfg(target_os = "linux")]
    async fn r5_wait_for_content_read(observer: &IntegrityTier2ProgressObserver) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while observer.content_read_count() == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("R5 Tier 2 worker enters physical content sensing");
    }

    #[cfg(target_os = "linux")]
    async fn r5_assert_process_effect_after_settlement(
        process_count_path: &Path,
        expected_count: usize,
        observer: &IntegrityTier2ProgressObserver,
    ) {
        tokio::time::timeout(Duration::from_secs(10), async {
            while r5_process_count(process_count_path) < expected_count {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("R5 launch reaches its process effect");
        assert!(
            observer.settled_at().is_some(),
            "Tier 2 physical ownership must settle before the launch process effect"
        );
    }

    #[cfg(target_os = "linux")]
    fn r5_timing_summary(samples: &[Duration]) -> (Duration, Duration, Duration) {
        assert_eq!(samples.len(), R5_LAUNCH_SAMPLE_COUNT);
        let mut sorted = samples.to_vec();
        sorted.sort_unstable();
        let p50 = sorted[sorted.len() / 2];
        let p95_index = (sorted.len() * 95).div_ceil(100) - 1;
        (p50, sorted[p95_index], *sorted.last().expect("R5 samples"))
    }

    #[cfg(target_os = "linux")]
    fn r5_timing_json(summary: (Duration, Duration, Duration)) -> serde_json::Value {
        serde_json::json!({
            "p50_micros": summary.0.as_micros(),
            "p95_micros": summary.1.as_micros(),
            "max_micros": summary.2.as_micros()
        })
    }

    #[cfg(target_os = "linux")]
    fn r5_release_build() -> bool {
        !cfg!(debug_assertions)
    }

    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires a release build and explicit AXIAL_R5_* physical evidence bindings"]
    async fn representative_idle_sweep_launch_preemption_measurement() {
        assert!(
            r5_release_build(),
            "R5 physical evidence must use cargo test --release"
        );
        let device_evidence = std::env::var("AXIAL_R5_DEVICE_EVIDENCE")
            .expect("AXIAL_R5_DEVICE_EVIDENCE is required");
        let filesystem_evidence = std::env::var("AXIAL_R5_FILESYSTEM_EVIDENCE")
            .expect("AXIAL_R5_FILESYSTEM_EVIDENCE is required");
        let source_binding =
            std::env::var("AXIAL_R5_SOURCE_BINDING").expect("AXIAL_R5_SOURCE_BINDING is required");
        assert!(
            !device_evidence.trim().is_empty(),
            "device evidence is empty"
        );
        assert!(
            !filesystem_evidence.trim().is_empty(),
            "filesystem evidence is empty"
        );
        assert!(!source_binding.trim().is_empty(), "source binding is empty");
        let kernel_release = fs::read_to_string("/proc/sys/kernel/osrelease")
            .expect("read Linux kernel release")
            .trim()
            .to_string();
        assert!(
            kernel_release.to_ascii_lowercase().contains("microsoft"),
            "this evidence contract is scoped to the current WSL2 host"
        );

        let measurement_root = r5_measurement_root();
        let (state, root, paths) = state_fixture_at(measurement_root.0.clone());
        let version_id = "guardian-r5-representative";
        let instance = state
            .instances()
            .insert_for_test("R5 synthetic representative", version_id)
            .expect("register R5 representative instance");
        r5_activate_representative_inventory(&state, &paths, &instance.id, version_id);
        r5_install_booting_java(&state, &root, &instance.id);
        let process_count_path = state
            .instances()
            .game_dir(&instance.id)
            .join("guardian-r5-process-count");

        let full_sweep_started = Instant::now();
        let full_sweep = reserve(
            fixed_plan(
                &state,
                &instance.id,
                "integrity-sweep-00000000-0000-4000-8000-000000000500",
            )
            .await,
            idle_epoch(&state),
        )
        .execute()
        .await
        .expect("execute full representative R5 sweep");
        let full_sweep_elapsed = full_sweep_started.elapsed();
        assert_eq!(full_sweep, IdleIntegrityTerminal::Succeeded);
        let full_journal = state
            .journals()
            .get(&OperationId::new(
                "integrity-sweep-00000000-0000-4000-8000-000000000500",
            ))
            .expect("full representative R5 terminal journal");
        assert_eq!(full_journal.status, OperationStatus::Succeeded);
        assert_eq!(full_journal.outcome, Some(OperationOutcome::Succeeded));
        let full_facts = &full_journal.completed_steps[0].generated_facts;
        for expected in [
            format!("integrity_counter:selected_entry_count:{R5_REPRESENTATIVE_ENTRY_COUNT}"),
            format!("integrity_counter:verified_entry_count:{R5_REPRESENTATIVE_ENTRY_COUNT}"),
            format!("integrity_counter:processed_entry_count:{R5_REPRESENTATIVE_ENTRY_COUNT}"),
            format!("integrity_counter:hashed_entry_count:{R5_REPRESENTATIVE_ENTRY_COUNT}"),
            format!(
                "integrity_counter:expected_content_byte_count:{R5_REPRESENTATIVE_CONTENT_BYTES}"
            ),
            format!("integrity_counter:content_read_byte_count:{R5_REPRESENTATIVE_CONTENT_BYTES}"),
        ] {
            assert!(
                full_facts.contains(&expected),
                "missing R5 counter {expected}"
            );
        }

        r5_launch_once(&state, &instance.id).await;
        let mut baseline = Vec::with_capacity(R5_LAUNCH_SAMPLE_COUNT);
        let mut concurrent = Vec::with_capacity(R5_LAUNCH_SAMPLE_COUNT);
        let mut paired_impact = Vec::with_capacity(R5_LAUNCH_SAMPLE_COUNT);
        let mut preemption = Vec::with_capacity(R5_LAUNCH_SAMPLE_COUNT);
        for sample_index in 0..R5_LAUNCH_SAMPLE_COUNT {
            let baseline_elapsed = r5_launch_once(&state, &instance.id).await;
            baseline.push(baseline_elapsed);

            let operation_id = format!(
                "integrity-sweep-00000000-0000-4000-8000-{:012x}",
                0x600 + sample_index
            );
            let observer = IntegrityTier2ProgressObserver::default();
            let execution = reserve(
                fixed_plan(&state, &instance.id, &operation_id).await,
                idle_epoch(&state),
            )
            .start_with_progress_observer(observer.clone());
            r5_wait_for_content_read(&observer).await;
            assert!(observer.settled_at().is_none());

            let expected_process_count = r5_process_count(&process_count_path) + 1;
            let preemption_started = Instant::now();
            let (concurrent_elapsed, ()) = tokio::join!(
                r5_launch_once(&state, &instance.id),
                r5_assert_process_effect_after_settlement(
                    &process_count_path,
                    expected_process_count,
                    &observer,
                )
            );
            let settled_at = observer
                .settled_at()
                .expect("R5 sweep settles before launch effect");
            preemption.push(settled_at.saturating_duration_since(preemption_started));
            paired_impact.push(concurrent_elapsed.saturating_sub(baseline_elapsed));
            concurrent.push(concurrent_elapsed);
            assert_eq!(
                execution.wait().await.expect("R5 cancelled sweep terminal"),
                IdleIntegrityTerminal::Cancelled
            );
            let journal = state
                .journals()
                .get(&OperationId::new(operation_id))
                .expect("R5 cancelled sweep journal");
            assert_eq!(journal.status, OperationStatus::Cancelled);
            assert_eq!(journal.outcome, Some(OperationOutcome::Cancelled));
        }

        let baseline_summary = r5_timing_summary(&baseline);
        let concurrent_summary = r5_timing_summary(&concurrent);
        let paired_impact_summary = r5_timing_summary(&paired_impact);
        let preemption_summary = r5_timing_summary(&preemption);
        let launch_impact_within_ceiling = paired_impact_summary.1 <= R5_LAUNCH_IMPACT_CEILING;
        let preemption_within_ceiling = preemption_summary.1 <= R5_LAUNCH_IMPACT_CEILING;

        println!(
            "{}",
            serde_json::json!({
                "schema": "axial.guardian.r5.launch-impact.v1",
                "source_binding": source_binding,
                "host_scope": "linux_wsl2_virtual_disk_only",
                "kernel_release": kernel_release,
                "device_evidence": device_evidence,
                "filesystem_evidence": filesystem_evidence,
                "fixture_root_supplied": true,
                "fixture_kind": "synthetic_representative",
                "fixture_basis": "current_local_axial_runtime_footprint",
                "entry_count": R5_REPRESENTATIVE_ENTRY_COUNT,
                "content_bytes": R5_REPRESENTATIVE_CONTENT_BYTES,
                "full_sweep_elapsed_micros": full_sweep_elapsed.as_micros(),
                "full_sweep_status": "succeeded",
                "warmup_launch_samples": 1,
                "paired_launch_samples": R5_LAUNCH_SAMPLE_COUNT,
                "baseline_launch": r5_timing_json(baseline_summary),
                "concurrent_launch": r5_timing_json(concurrent_summary),
                "paired_launch_impact": r5_timing_json(paired_impact_summary),
                "physical_preemption_settlement": r5_timing_json(preemption_summary),
                "ceiling_ms": R5_LAUNCH_IMPACT_CEILING.as_millis(),
                "launch_impact_within_ceiling": launch_impact_within_ceiling,
                "preemption_within_ceiling": preemption_within_ceiling,
                "cache_condition": "warm_without_cache_flush",
                "limitations": [
                    "synthetic fixture, not a real installed Minecraft instance",
                    "WSL2 virtual disk, not native bare-metal or physical-HDD evidence",
                    "warm page-cache condition; cold cache was not measured"
                ],
                "measurement_status": "candidate_only_pending_review"
            })
        );
        assert!(
            launch_impact_within_ceiling,
            "R5 paired p95 launch impact exceeded the predeclared 10 ms ceiling"
        );
        assert!(
            preemption_within_ceiling,
            "R5 p95 physical sweep settlement exceeded the predeclared 10 ms ceiling"
        );

        close_fixture(state, &root).await;
        drop(measurement_root);
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
    async fn corrupt_assets_rebuilds_minimal_component_and_launches_once() {
        let (state, root, paths) = state_fixture("corrupt");
        let instance = state
            .instances()
            .insert_for_test("Corrupt", "1.21.5")
            .expect("register instance");
        let (index_bytes, object_digest, empty_digest) =
            activate_corrupt_inventory(&state, &instance.id, &instance.version_id, &paths);
        let user_owned = write_user_owned_sentinels(&paths, &instance.id);
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
        assert_eq!(
            leaf.0.failure_point.as_deref(),
            Some(crate::state::REGISTERED_ARTIFACT_COMPONENT_REBUILD_FAILURE_POINT)
        );
        assert!(leaf.0.planned_steps.iter().all(|step| {
            step.step_id != "download_artifact_to_temp"
                && step.step_id != "quarantine_launcher_managed_target"
        }));
        let component = child_journals
            .iter()
            .find(|(_, attempt)| attempt.rung() == ReconciliationRung::RebuildComponent)
            .expect("Assets component child journal");
        assert_eq!(component.0.status, OperationStatus::Succeeded);
        assert_eq!(component.1.component(), ReconciliationComponent::Assets);
        assert!(
            component
                .0
                .planned_steps
                .iter()
                .any(|step| { step.step_id == crate::state::ASSETS_COMPONENT_REBUILD_STEP })
        );
        assert_ne!(leaf.0.operation_id, component.0.operation_id);
        assert_ne!(leaf.0.operation_id, journal.operation_id);
        assert_ne!(component.0.operation_id, journal.operation_id);
        for (entry, attempt) in [leaf, component] {
            let terminal = entry
                .reconciliation_terminal()
                .expect("child reconciliation terminal");
            assert!(terminal.quarantine_checkpoint().is_empty());
            assert_eq!(
                state
                    .failure_memory()
                    .get(&reconciliation_attempt_key(attempt))
                    .and_then(|memory| memory.reconciliation_terminal().cloned()),
                Some(terminal.clone()),
                "each child terminal must reach memory before parent success",
            );
        }
        assert_eq!(
            fs::read(paths.library_dir.join("assets/indexes/fixture-assets.json"))
                .expect("rebuilt Assets index"),
            index_bytes
        );
        assert_eq!(
            fs::read(paths.library_dir.join(format!(
                "assets/objects/{}/{object_digest}",
                &object_digest[..2]
            )))
            .expect("rebuilt nonempty Assets object"),
            b"axial managed Assets fixture"
        );
        assert_eq!(
            fs::read(paths.library_dir.join(format!(
                "assets/objects/{}/{empty_digest}",
                &empty_digest[..2]
            )))
            .expect("rebuilt empty Assets object"),
            Vec::<u8>::new()
        );
        assert_clean_tier1(&state, &instance.id, &paths.library_dir).await;
        #[cfg(unix)]
        assert_repaired_instance_launches_once(&state, &root, &instance.id, "assets").await;
        for (path, contents) in &user_owned {
            assert_eq!(
                fs::read(path).expect("user-owned Assets sentinel"),
                contents.as_slice()
            );
        }
        close_fixture(state, &root).await;
    }

    #[tokio::test]
    async fn truncated_client_rebuilds_minimal_version_bundle_and_launches_once() {
        let (state, root, paths) = state_fixture("corrupt-version-bundle");
        let instance = state
            .instances()
            .insert_for_test("Corrupt VersionBundle", "1.21.5")
            .expect("register VersionBundle instance");
        let (version_json, client_jar, log_config) = activate_corrupt_version_bundle_inventory(
            &state,
            &instance.id,
            &instance.version_id,
            &paths,
        );
        let version_dir = paths
            .library_dir
            .join("versions")
            .join(&instance.version_id);
        let truncated = fs::read(version_dir.join(format!("{}.jar", instance.version_id)))
            .expect("truncated VersionBundle client");
        assert!(truncated.len() < client_jar.len());
        let user_owned = write_user_owned_sentinels(&paths, &instance.id);
        let plan = fixed_plan(&state, &instance.id, FIRST_OPERATION_ID).await;
        let reserved = reserve(plan, idle_epoch(&state));

        let terminal = reserved
            .execute_with_rebuild_source(RegisteredArtifactComponentRebuildSource::Fixture)
            .await
            .expect("execute VersionBundle sweep");

        assert_eq!(terminal, IdleIntegrityTerminal::Succeeded);
        let parent = state
            .journals()
            .get(&OperationId::new(FIRST_OPERATION_ID))
            .expect("VersionBundle parent journal");
        assert_eq!(parent.status, OperationStatus::Succeeded);
        assert!(journal_fact_ids(&parent).contains(&"guardian_fact:artifact_size_drift"));
        assert_eq!(
            parent.guardian_diagnosis_ids,
            vec![crate::guardian::DiagnosisId::LauncherManagedArtifactCorrupt]
        );
        let child_journals = state
            .journals()
            .list()
            .into_iter()
            .filter(|entry| entry.operation_id != parent.operation_id)
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
            .expect("VersionBundle leaf child journal");
        assert_eq!(leaf.0.status, OperationStatus::Failed);
        assert_eq!(leaf.1.component(), ReconciliationComponent::VersionBundle);
        assert_eq!(
            leaf.0.failure_point.as_deref(),
            Some(crate::state::REGISTERED_ARTIFACT_COMPONENT_REBUILD_FAILURE_POINT)
        );
        assert!(
            leaf.0
                .planned_steps
                .iter()
                .all(|step| step.step_id != "download_artifact_to_temp"
                    && step.step_id != "quarantine_launcher_managed_target")
        );
        let component = child_journals
            .iter()
            .find(|(_, attempt)| attempt.rung() == ReconciliationRung::RebuildComponent)
            .expect("VersionBundle component child journal");
        assert_eq!(component.0.status, OperationStatus::Succeeded);
        assert_eq!(
            component.1.component(),
            ReconciliationComponent::VersionBundle
        );
        assert!(
            component.0.planned_steps.iter().any(|step| {
                step.step_id == crate::state::VERSION_BUNDLE_COMPONENT_REBUILD_STEP
            })
        );
        assert_ne!(leaf.0.operation_id, component.0.operation_id);
        assert_ne!(leaf.0.operation_id, parent.operation_id);
        assert_ne!(component.0.operation_id, parent.operation_id);
        for (entry, attempt) in [leaf, component] {
            let child_terminal = entry
                .reconciliation_terminal()
                .expect("VersionBundle child reconciliation terminal");
            assert!(child_terminal.quarantine_checkpoint().is_empty());
            assert_eq!(
                state
                    .failure_memory()
                    .get(&reconciliation_attempt_key(attempt))
                    .and_then(|memory| memory.reconciliation_terminal().cloned()),
                Some(child_terminal.clone()),
                "each VersionBundle child terminal must reach memory before parent success",
            );
        }

        assert_eq!(
            fs::read(version_dir.join(format!("{}.json", instance.version_id)))
                .expect("rebuilt VersionBundle metadata"),
            version_json
        );
        assert_eq!(
            fs::read(version_dir.join(format!("{}.jar", instance.version_id)))
                .expect("rebuilt VersionBundle client"),
            client_jar
        );
        assert_eq!(
            fs::read(
                paths
                    .library_dir
                    .join("assets/log_configs/guardian-version-bundle.xml")
            )
            .expect("rebuilt VersionBundle log config"),
            log_config
        );
        assert_clean_tier1(&state, &instance.id, &paths.library_dir).await;
        #[cfg(unix)]
        assert_repaired_instance_launches_once(&state, &root, &instance.id, "version-bundle").await;
        for (path, contents) in &user_owned {
            assert_eq!(
                fs::read(path).expect("user-owned sentinel remains readable"),
                contents.as_slice(),
                "VersionBundle rebuild and launch must not mutate user-owned instance files",
            );
        }

        close_fixture(state, &root).await;
    }

    #[tokio::test]
    async fn superseded_ticket_cancels_atomically_without_findings() {
        let (state, root, paths) = state_fixture("superseded");
        let instance = state
            .instances()
            .insert_for_test("Superseded", "1.21.5")
            .expect("register instance");
        let _ = activate_corrupt_inventory(&state, &instance.id, &instance.version_id, &paths);
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
