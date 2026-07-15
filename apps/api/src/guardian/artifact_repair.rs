//! Guardian artifact repair execution.
//!
//! The executor consumes a State-minted registered-artifact admission. It does
//! not discover providers, accept paths from callers, or decide policy.

use super::DiagnosisId;
use crate::execution::ExecutionFact;
use crate::observability::{RedactionAudience, sanitize_evidence_token};
use crate::state::contracts::{
    CommandKind, JournalId, OperationId, OperationJournalEntry, OperationJournalStep,
    OperationOutcome, OperationPhase, OperationStatus, OperationStepResult, ReconciliationAttempt,
    ReconciliationScope, ReconciliationTerminal, ReconciliationTerminalOutcome, RollbackState,
    StabilizationSystem, TargetDescriptor,
};
use crate::state::failure_memory::GuardianFailureMemoryStore;
use crate::state::{
    OperationJournalReconciliation, OperationJournalStore, OperationJournalStoreError,
    ReconciliationAttemptReservation, RegisteredArtifactFailedRepair,
    RegisteredArtifactRepairAdmission, RegisteredArtifactRepairEffect,
    RegisteredArtifactRepairMemoryReceipt, operation_journal_completed_step_is_visible,
    operation_journal_plan_is_visible, reconciliation_attempt_key, reconciliation_instance_target,
    reconciliation_journal_attempt, record_reconciliation_journal_failure,
    record_reconciliation_journal_success, reserve_reconciliation_attempt,
    settle_reconciliation_memory,
};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::time::Duration as StdDuration;

const ARTIFACT_JOURNAL_RETRY_INITIAL_DELAY: StdDuration = StdDuration::from_millis(20);
const ARTIFACT_JOURNAL_RETRY_MAX_DELAY: StdDuration = StdDuration::from_secs(1);

enum ArtifactJournalReconciliation {
    MutationCommitted,
    AcceptedFailure(OperationJournalStoreError),
    RetryMutation,
}

#[must_use]
#[derive(Debug, Eq, PartialEq)]
pub(crate) struct GuardianArtifactRepairReceipt {
    diagnosis_id: DiagnosisId,
    status: GuardianArtifactRepairStatus,
}

#[must_use]
pub(crate) struct GuardianArtifactRepairFailure {
    outcome: GuardianArtifactRepairReceipt,
    continuation: RegisteredArtifactFailedRepair,
}

#[must_use]
pub(crate) enum GuardianArtifactRepairSettlement {
    Completed(GuardianArtifactRepairReceipt),
    Failed(GuardianArtifactRepairFailure),
}

impl GuardianArtifactRepairReceipt {
    pub(crate) const fn diagnosis_id(&self) -> DiagnosisId {
        self.diagnosis_id
    }

    pub(crate) const fn status(&self) -> GuardianArtifactRepairStatus {
        self.status
    }
}

impl GuardianArtifactRepairFailure {
    pub(crate) const fn outcome(&self) -> &GuardianArtifactRepairReceipt {
        &self.outcome
    }

    pub(crate) fn into_continuation(self) -> RegisteredArtifactFailedRepair {
        self.continuation
    }
}

impl GuardianArtifactRepairSettlement {
    #[cfg(test)]
    pub(crate) const fn outcome(&self) -> &GuardianArtifactRepairReceipt {
        match self {
            Self::Completed(outcome) => outcome,
            Self::Failed(failure) => failure.outcome(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum GuardianArtifactRepairStatus {
    Repaired,
    Blocked,
    Failed,
}

impl GuardianArtifactRepairStatus {
    pub const fn as_persisted_id(self) -> &'static str {
        match self {
            Self::Repaired => "repaired",
            Self::Blocked => "blocked",
            Self::Failed => "failed",
        }
    }
}

enum ArtifactTerminal {
    Repaired {
        step_id: &'static str,
        facts: Vec<String>,
        quarantined_target: Option<TargetDescriptor>,
    },
    Failed {
        step_id: &'static str,
        rollback: RollbackState,
        facts: Vec<String>,
        quarantined_target: Option<TargetDescriptor>,
    },
}

struct ArtifactRepairContext<'a> {
    client: &'a Client,
    journals: &'a OperationJournalStore,
    failure_memory: &'a GuardianFailureMemoryStore,
    effect: RegisteredArtifactRepairEffect,
    attempt: ReconciliationAttempt,
    reservation: Option<ReconciliationAttemptReservation>,
    admission: &'a RegisteredArtifactRepairAdmission,
}

struct ArtifactRepairExecution {
    outcome: GuardianArtifactRepairReceipt,
    memory_receipt: RegisteredArtifactRepairMemoryReceipt,
}

pub(crate) async fn execute_registered_guardian_artifact_repair(
    admission: RegisteredArtifactRepairAdmission,
    client: &Client,
) -> Result<GuardianArtifactRepairSettlement, OperationJournalStoreError> {
    let attempt = admission.attempt().clone();
    let operation_id = attempt.operation_id().clone();
    let mut context = ArtifactRepairContext {
        client,
        journals: admission.authority().journals(),
        failure_memory: admission.authority().failure_memory(),
        effect: admission.effect(),
        attempt,
        reservation: None,
        admission: &admission,
    };

    settle_reconciliation_memory(context.failure_memory)
        .await
        .map_err(artifact_memory_error)?;
    let attempt_key = reconciliation_attempt_key(&context.attempt);
    context.reservation = Some(
        reserve_reconciliation_attempt(context.failure_memory, context.journals, attempt_key)
            .map_err(|_| {
                OperationJournalStoreError::Persistence(std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    "Guardian registered artifact reconciliation attempt is already active",
                ))
            })?,
    );
    let execution = execute_admitted_artifact_repair(context, operation_id).await?;
    if execution.outcome.status() != GuardianArtifactRepairStatus::Failed {
        return Ok(GuardianArtifactRepairSettlement::Completed(
            execution.outcome,
        ));
    }
    let continuation = admission
        .into_failed_continuation(execution.memory_receipt)
        .map_err(artifact_reconciliation_error)?;
    Ok(GuardianArtifactRepairSettlement::Failed(
        GuardianArtifactRepairFailure {
            outcome: execution.outcome,
            continuation,
        },
    ))
}

async fn execute_admitted_artifact_repair(
    context: ArtifactRepairContext<'_>,
    operation_id: OperationId,
) -> Result<ArtifactRepairExecution, OperationJournalStoreError> {
    let target = context.attempt.target().clone();
    if let Some(error) =
        create_planned_journal_reconciled(context.journals, &operation_id, &context).await?
    {
        finish_artifact_repair(
            &context,
            operation_id.clone(),
            ArtifactTerminal::Failed {
                step_id: "journal_repair_start",
                rollback: RollbackState::NotApplicable,
                facts: Vec::new(),
                quarantined_target: None,
            },
        )
        .await?;
        return Err(error);
    }

    {
        let admission = context.admission;
        if !admission.evidence_is_live() {
            return finish_artifact_repair(
                &context,
                operation_id,
                ArtifactTerminal::Failed {
                    step_id: "revalidate_registered_artifact_authority",
                    rollback: RollbackState::NotApplicable,
                    facts: Vec::new(),
                    quarantined_target: None,
                },
            )
            .await;
        }
        let state = admission.physical_state().await;
        if state
            == Some(crate::execution::registered_artifact::RegisteredArtifactPhysicalState::Exact)
            && admission.evidence_is_live()
        {
            return finish_artifact_repair(
                &context,
                operation_id,
                ArtifactTerminal::Repaired {
                    step_id: "registered_artifact_already_exact",
                    facts: Vec::new(),
                    quarantined_target: None,
                },
            )
            .await;
        }
        let expected = match admission.effect() {
            RegisteredArtifactRepairEffect::DownloadMissing => {
                crate::execution::registered_artifact::RegisteredArtifactPhysicalState::Missing
            }
            RegisteredArtifactRepairEffect::QuarantineRedownload => {
                crate::execution::registered_artifact::RegisteredArtifactPhysicalState::Corrupt
            }
            RegisteredArtifactRepairEffect::ComponentRebuildRequired => {
                crate::execution::registered_artifact::RegisteredArtifactPhysicalState::Corrupt
            }
        };
        if state != Some(expected) || !admission.evidence_is_live() {
            return finish_artifact_repair(
                &context,
                operation_id,
                ArtifactTerminal::Failed {
                    step_id: "revalidate_registered_artifact_condition",
                    rollback: RollbackState::NotApplicable,
                    facts: Vec::new(),
                    quarantined_target: None,
                },
            )
            .await;
        }
        if admission.effect() == RegisteredArtifactRepairEffect::ComponentRebuildRequired {
            return finish_artifact_repair(
                &context,
                operation_id,
                ArtifactTerminal::Failed {
                    step_id: "require_registered_artifact_component_rebuild",
                    rollback: RollbackState::NotApplicable,
                    facts: Vec::new(),
                    quarantined_target: None,
                },
            )
            .await;
        }
    }

    let quarantined_target = if context.quarantines_existing() {
        let quarantine_facts = match context
            .admission
            .mutation()
            .quarantine_existing(&operation_id, &target)
        {
            Ok(report) => fact_ids(&report.facts),
            Err(error) => {
                return finish_artifact_repair(
                    &context,
                    operation_id,
                    ArtifactTerminal::Failed {
                        step_id: "quarantine_launcher_managed_target",
                        rollback: RollbackState::Unavailable,
                        facts: fact_ids(&error.facts),
                        quarantined_target: None,
                    },
                )
                .await;
            }
        };
        let quarantined_target = TargetDescriptor::new(
            StabilizationSystem::Execution,
            target.kind,
            format!("quarantine-{}", target.id),
            target.ownership,
        );
        let quarantine_checkpoint = repair_step(
            "quarantine_launcher_managed_target",
            OperationStepResult::Completed,
            Some(target.clone()),
            quarantine_facts,
            RollbackState::Available,
        );
        loop {
            let result = context
                .journals
                .record_checkpoint(&operation_id, quarantine_checkpoint.clone())
                .await;
            match result {
                Ok(()) => break,
                Err(error) => match reconcile_artifact_journal_error(
                    context.journals,
                    &operation_id,
                    error,
                    |entry| {
                        artifact_journal_identity_matches(entry, &operation_id)
                            && entry.status == OperationStatus::Running
                            && quarantine_checkpoint
                                .changed_target
                                .as_ref()
                                .is_some_and(|target| {
                                    entry.targets.contains(target)
                                        && entry.ownership == target.ownership
                                })
                            && operation_journal_completed_step_is_visible(
                                entry,
                                &quarantine_checkpoint,
                            )
                    },
                )
                .await?
                {
                    ArtifactJournalReconciliation::MutationCommitted => break,
                    ArtifactJournalReconciliation::AcceptedFailure(error) => {
                        finish_artifact_repair(
                            &context,
                            operation_id.clone(),
                            ArtifactTerminal::Failed {
                                step_id: "record_quarantine_checkpoint",
                                rollback: RollbackState::Available,
                                facts: Vec::new(),
                                quarantined_target: Some(quarantined_target),
                            },
                        )
                        .await?;
                        return Err(error);
                    }
                    ArtifactJournalReconciliation::RetryMutation => {}
                },
            }
        }
        Some(quarantined_target)
    } else {
        None
    };

    let download_result = if !context.admission.evidence_is_live() {
        Err(Vec::new())
    } else {
        let (provider_url, expected_sha1, expected_size) = context.admission.download_contract();
        context
            .admission
            .mutation()
            .download_verify_promote(
                &operation_id,
                &target,
                provider_url,
                expected_sha1,
                expected_size,
                context.client,
            )
            .await
            .map(|report| report.facts)
            .map_err(|error| error.facts)
    };

    match download_result {
        Ok(facts) => {
            let fact_ids = fact_ids(&facts);
            let (_, expected_sha1, expected_size) = context.admission.download_contract();
            if !context
                .admission
                .mutation()
                .verify_exact(expected_sha1, expected_size)
                .await
                || !context.admission.evidence_is_live()
            {
                return finish_artifact_repair(
                    &context,
                    operation_id,
                    ArtifactTerminal::Failed {
                        step_id: "verify_registered_artifact_postcondition",
                        rollback: if context.quarantines_existing() {
                            RollbackState::Available
                        } else {
                            RollbackState::Unavailable
                        },
                        facts: fact_ids,
                        quarantined_target,
                    },
                )
                .await;
            }
            finish_artifact_repair(
                &context,
                operation_id,
                ArtifactTerminal::Repaired {
                    step_id: "promote_verified_artifact",
                    facts: fact_ids,
                    quarantined_target,
                },
            )
            .await
        }
        Err(facts) => {
            let fact_ids = fact_ids(&facts);
            if !context.admission.evidence_is_live() {
                return finish_artifact_repair(
                    &context,
                    operation_id,
                    ArtifactTerminal::Failed {
                        step_id: "revalidate_registered_artifact_authority",
                        rollback: if context.quarantines_existing() {
                            RollbackState::Available
                        } else {
                            RollbackState::Unavailable
                        },
                        facts: fact_ids,
                        quarantined_target,
                    },
                )
                .await;
            }
            finish_artifact_repair(
                &context,
                operation_id,
                ArtifactTerminal::Failed {
                    step_id: "download_artifact_to_temp",
                    rollback: if context.quarantines_existing() {
                        RollbackState::Available
                    } else {
                        RollbackState::Unavailable
                    },
                    facts: fact_ids,
                    quarantined_target,
                },
            )
            .await
        }
    }
}

async fn finish_artifact_repair(
    context: &ArtifactRepairContext<'_>,
    operation_id: OperationId,
    terminal: ArtifactTerminal,
) -> Result<ArtifactRepairExecution, OperationJournalStoreError> {
    let (step_id, rollback, failure_point, reconciliation_outcome, status, facts, quarantined) =
        match terminal {
            ArtifactTerminal::Repaired {
                step_id,
                facts,
                quarantined_target,
            } => (
                step_id,
                RollbackState::Available,
                None,
                ReconciliationTerminalOutcome::Succeeded,
                GuardianArtifactRepairStatus::Repaired,
                facts,
                quarantined_target,
            ),
            ArtifactTerminal::Failed {
                step_id,
                rollback,
                facts,
                quarantined_target,
            } => (
                step_id,
                rollback,
                Some(step_id),
                ReconciliationTerminalOutcome::Failed,
                GuardianArtifactRepairStatus::Failed,
                facts,
                quarantined_target,
            ),
        };
    let reconciliation_terminal = context
        .admission
        .terminal(
            context.attempt.clone(),
            reconciliation_outcome,
            quarantined.clone(),
        )
        .map_err(artifact_reconciliation_error)?;
    let step_result = if failure_point.is_some() {
        OperationStepResult::Failed
    } else {
        OperationStepResult::Completed
    };
    if let Some(error) = record_artifact_terminal_reconciled(
        context.journals,
        &operation_id,
        repair_step(
            step_id,
            step_result,
            Some(context.attempt.target().clone()),
            facts,
            rollback,
        ),
        failure_point,
        &reconciliation_terminal,
    )
    .await?
    {
        return Err(error);
    }
    let memory_receipt = context
        .admission
        .commit_terminal_memory(
            reconciliation_terminal,
            context
                .reservation
                .as_ref()
                .expect("attempted repair owns memory reservation"),
        )
        .await?;
    Ok(ArtifactRepairExecution {
        outcome: artifact_repair_outcome(context.attempt.diagnosis_id(), status),
        memory_receipt,
    })
}

fn artifact_memory_error(error: impl std::fmt::Display) -> OperationJournalStoreError {
    OperationJournalStoreError::Persistence(std::io::Error::other(format!(
        "Guardian artifact reconciliation memory failed: {error}"
    )))
}

fn artifact_reconciliation_error(_error: impl std::fmt::Debug) -> OperationJournalStoreError {
    OperationJournalStoreError::Persistence(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        "Guardian artifact reconciliation evidence is invalid",
    ))
}

fn planned_artifact_journal(
    operation_id: &OperationId,
    context: &ArtifactRepairContext<'_>,
) -> OperationJournalEntry {
    let mut entry = OperationJournalEntry::new(
        JournalId::new(format!("journal-{}", operation_id.as_str())),
        operation_id.clone(),
        CommandKind::RepairInstance,
        StabilizationSystem::Guardian,
        context.attempt.ownership(),
        RollbackState::Available,
    );
    append_artifact_journal_targets(&mut entry, context);
    entry.planned_steps = artifact_repair_steps(context)
        .iter()
        .map(|(step_id, rollback)| {
            repair_step(
                step_id,
                OperationStepResult::Planned,
                Some(context.attempt.target().clone()),
                Vec::new(),
                *rollback,
            )
        })
        .collect();
    entry
        .guardian_diagnosis_ids
        .push(context.attempt.diagnosis_id());
    reconciliation_journal_attempt(entry, context.attempt.clone())
}

async fn reconcile_artifact_journal_error(
    journals: &OperationJournalStore,
    operation_id: &OperationId,
    error: OperationJournalStoreError,
    expected: impl Fn(&OperationJournalEntry) -> bool,
) -> Result<ArtifactJournalReconciliation, OperationJournalStoreError> {
    match journals
        .reconcile_transition(
            operation_id,
            error,
            ARTIFACT_JOURNAL_RETRY_INITIAL_DELAY,
            ARTIFACT_JOURNAL_RETRY_MAX_DELAY,
            expected,
        )
        .await?
    {
        OperationJournalReconciliation::CommittedAfterPersistenceFailure(error) => {
            Ok(ArtifactJournalReconciliation::AcceptedFailure(error))
        }
        OperationJournalReconciliation::RequestedTransitionAlreadyCommitted => {
            Ok(ArtifactJournalReconciliation::MutationCommitted)
        }
        OperationJournalReconciliation::RetryRequestedTransition => {
            Ok(ArtifactJournalReconciliation::RetryMutation)
        }
    }
}

async fn create_planned_journal_reconciled(
    journals: &OperationJournalStore,
    operation_id: &OperationId,
    context: &ArtifactRepairContext<'_>,
) -> Result<Option<OperationJournalStoreError>, OperationJournalStoreError> {
    let expected = planned_artifact_journal(operation_id, context);
    loop {
        match journals.create(expected.clone()).await {
            Ok(()) => return Ok(None),
            Err(OperationJournalStoreError::AlreadyExists)
                if journals
                    .get(operation_id)
                    .is_some_and(|entry| operation_journal_plan_is_visible(&entry, &expected)) =>
            {
                return Ok(None);
            }
            Err(error) => {
                match reconcile_artifact_journal_error(journals, operation_id, error, |entry| {
                    operation_journal_plan_is_visible(entry, &expected)
                })
                .await?
                {
                    ArtifactJournalReconciliation::MutationCommitted => return Ok(None),
                    ArtifactJournalReconciliation::AcceptedFailure(error) => {
                        return Ok(Some(error));
                    }
                    ArtifactJournalReconciliation::RetryMutation => {}
                }
            }
        }
    }
}

async fn record_artifact_terminal_reconciled(
    journals: &OperationJournalStore,
    operation_id: &OperationId,
    step: OperationJournalStep,
    failure_point: Option<&str>,
    terminal: &ReconciliationTerminal,
) -> Result<Option<OperationJournalStoreError>, OperationJournalStoreError> {
    loop {
        let result = if let Some(failure_point) = failure_point {
            record_reconciliation_journal_failure(
                journals,
                operation_id,
                step.clone(),
                failure_point,
                terminal.clone(),
            )
            .await
        } else {
            record_reconciliation_journal_success(
                journals,
                operation_id,
                step.clone(),
                terminal.clone(),
            )
            .await
        };
        match result {
            Ok(()) => return Ok(None),
            Err(OperationJournalStoreError::AlreadyTerminal)
                if journals.get(operation_id).is_some_and(|entry| {
                    artifact_terminal_transition_matches(
                        &entry,
                        operation_id,
                        failure_point,
                        &step,
                        terminal,
                    )
                }) =>
            {
                return Ok(None);
            }
            Err(error) => {
                match reconcile_artifact_journal_error(journals, operation_id, error, |entry| {
                    artifact_terminal_transition_matches(
                        entry,
                        operation_id,
                        failure_point,
                        &step,
                        terminal,
                    )
                })
                .await?
                {
                    ArtifactJournalReconciliation::MutationCommitted => return Ok(None),
                    ArtifactJournalReconciliation::AcceptedFailure(error) => {
                        return Ok(Some(error));
                    }
                    ArtifactJournalReconciliation::RetryMutation => {}
                }
            }
        }
    }
}

fn append_artifact_journal_targets(
    entry: &mut OperationJournalEntry,
    context: &ArtifactRepairContext<'_>,
) {
    entry.targets.push(context.attempt.target().clone());
    let ReconciliationScope::RegisteredInstance { instance_id, .. } = context.attempt.scope();
    entry
        .targets
        .push(reconciliation_instance_target(instance_id));
}

fn artifact_terminal_transition_matches(
    entry: &OperationJournalEntry,
    operation_id: &OperationId,
    failure_point: Option<&str>,
    step: &OperationJournalStep,
    terminal: &ReconciliationTerminal,
) -> bool {
    let (status, outcome) = if failure_point.is_some() {
        (OperationStatus::Failed, OperationOutcome::Failed)
    } else {
        (OperationStatus::Succeeded, OperationOutcome::Succeeded)
    };
    artifact_journal_identity_matches(entry, operation_id)
        && step.changed_target.as_ref().is_some_and(|target| {
            entry.targets.contains(target) && entry.ownership == target.ownership
        })
        && entry.status == status
        && entry.outcome == Some(outcome)
        && entry.failure_point.as_deref() == failure_point
        && entry.reconciliation_terminal() == Some(terminal)
        && operation_journal_completed_step_is_visible(entry, step)
}

fn artifact_journal_identity_matches(
    entry: &OperationJournalEntry,
    operation_id: &OperationId,
) -> bool {
    entry.operation_id == *operation_id
        && entry.command == CommandKind::RepairInstance
        && entry.owner == StabilizationSystem::Guardian
}

fn repair_step(
    step_id: &str,
    result: OperationStepResult,
    target: Option<TargetDescriptor>,
    facts: Vec<String>,
    rollback: RollbackState,
) -> OperationJournalStep {
    let mut step =
        OperationJournalStep::new(safe_id(step_id, "repair_step"), OperationPhase::Repairing);
    step.result = result;
    step.changed_target = target;
    step.generated_facts = facts;
    step.rollback = rollback;
    step
}

fn artifact_repair_steps(
    context: &ArtifactRepairContext<'_>,
) -> &'static [(&'static str, RollbackState)] {
    const QUARANTINE_REDOWNLOAD: [(&str, RollbackState); 8] = [
        ("journal_repair_start", RollbackState::NotApplicable),
        (
            "registered_artifact_already_exact",
            RollbackState::NotApplicable,
        ),
        (
            "quarantine_launcher_managed_target",
            RollbackState::Available,
        ),
        ("record_quarantine_checkpoint", RollbackState::Available),
        ("download_artifact_to_temp", RollbackState::Available),
        ("verify_artifact_checksum", RollbackState::NotApplicable),
        ("promote_verified_artifact", RollbackState::Available),
        ("record_repair_outcome", RollbackState::NotApplicable),
    ];
    const MISSING_DOWNLOAD: [(&str, RollbackState); 6] = [
        ("journal_repair_start", RollbackState::NotApplicable),
        (
            "registered_artifact_already_exact",
            RollbackState::NotApplicable,
        ),
        ("download_artifact_to_temp", RollbackState::Available),
        ("verify_artifact_checksum", RollbackState::NotApplicable),
        ("promote_verified_artifact", RollbackState::Available),
        ("record_repair_outcome", RollbackState::NotApplicable),
    ];
    const COMPONENT_REBUILD_REQUIRED: [(&str, RollbackState); 4] = [
        ("journal_repair_start", RollbackState::NotApplicable),
        (
            "registered_artifact_already_exact",
            RollbackState::NotApplicable,
        ),
        (
            "require_registered_artifact_component_rebuild",
            RollbackState::NotApplicable,
        ),
        ("record_repair_outcome", RollbackState::NotApplicable),
    ];
    match context.effect {
        RegisteredArtifactRepairEffect::DownloadMissing => &MISSING_DOWNLOAD,
        RegisteredArtifactRepairEffect::QuarantineRedownload => &QUARANTINE_REDOWNLOAD,
        RegisteredArtifactRepairEffect::ComponentRebuildRequired => &COMPONENT_REBUILD_REQUIRED,
    }
}

impl ArtifactRepairContext<'_> {
    const fn quarantines_existing(&self) -> bool {
        matches!(
            self.effect,
            RegisteredArtifactRepairEffect::QuarantineRedownload
        )
    }
}

fn fact_ids(facts: &[ExecutionFact]) -> Vec<String> {
    facts
        .iter()
        .map(|fact| fact.kind.as_str())
        .map(|fact| safe_id(fact, "execution_fact"))
        .collect()
}

fn artifact_repair_outcome(
    diagnosis_id: DiagnosisId,
    status: GuardianArtifactRepairStatus,
) -> GuardianArtifactRepairReceipt {
    GuardianArtifactRepairReceipt {
        diagnosis_id,
        status,
    }
}

fn safe_id(value: &str, fallback: &str) -> String {
    sanitize_evidence_token(value, RedactionAudience::UserVisible, 96)
        .unwrap_or_else(|| fallback.to_string())
}

#[cfg(test)]
mod move_only_contract_tests {
    use super::{GuardianArtifactRepairFailure, GuardianArtifactRepairSettlement};

    trait AmbiguousIfClone<Marker> {
        fn assert_not_clone() {}
    }

    struct CloneMarker;

    impl<T: ?Sized> AmbiguousIfClone<()> for T {}
    impl<T: Clone> AmbiguousIfClone<CloneMarker> for T {}

    const _: fn() = || {
        let _ = <GuardianArtifactRepairFailure as AmbiguousIfClone<_>>::assert_not_clone;
        let _ = <GuardianArtifactRepairSettlement as AmbiguousIfClone<_>>::assert_not_clone;
    };
}
