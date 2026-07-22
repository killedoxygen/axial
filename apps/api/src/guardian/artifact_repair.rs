//! Guardian artifact repair execution.
//!
//! The executor consumes a State-minted registered-artifact admission. It does
//! not discover providers, accept paths from callers, or decide policy.

use super::DiagnosisId;
use crate::execution::registered_artifact::{
    RegisteredArtifactEffectPreservationError, RegisteredArtifactMutationError,
    RegisteredArtifactMutationProof, RegisteredArtifactObservedExactProof,
    RegisteredArtifactObservedExactValidationError, RegisteredArtifactPhysicalState,
    RegisteredArtifactQuarantineOutcome, RegisteredArtifactQuarantinePreservation,
};
use crate::execution::ExecutionFact;
use crate::observability::{RedactionAudience, sanitize_evidence_token};
use crate::state::contracts::{
    CommandKind, JournalId, OperationId, OperationJournalEntry, OperationJournalStep,
    OperationOutcome, OperationPhase, OperationStatus, OperationStepResult, ReconciliationAttempt,
    ReconciliationQuarantineCheckpoint, ReconciliationQuarantineRecord, ReconciliationScope,
    ReconciliationTerminal, ReconciliationTerminalOutcome, RollbackState, StabilizationSystem,
    TargetDescriptor,
};
use crate::state::failure_memory::GuardianFailureMemoryStore;
use crate::state::{
    OperationJournalReconciliation, OperationJournalStore, OperationJournalStoreError,
    ReconciliationAttemptReservation, RegisteredArtifactFailedRepair,
    RegisteredArtifactRepairAdmission, RegisteredArtifactRepairEffect,
    RegisteredArtifactRepairMemoryReceipt, RegisteredArtifactRepairPlanRef,
    operation_journal_completed_step_is_visible, operation_journal_plan_is_visible,
    reconciliation_attempt_key, reconciliation_instance_target, reconciliation_journal_attempt,
    record_reconciliation_journal_failure, record_reconciliation_journal_success,
    reserve_reconciliation_attempt, settle_reconciliation_memory,
};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::io;
use std::time::Duration as StdDuration;

const ARTIFACT_JOURNAL_RETRY_INITIAL_DELAY: StdDuration = StdDuration::from_millis(20);
const ARTIFACT_JOURNAL_RETRY_MAX_DELAY: StdDuration = StdDuration::from_secs(1);

enum ArtifactJournalReconciliation {
    MutationCommitted,
    AcceptedFailure(OperationJournalStoreError),
    RetryMutation,
}

#[must_use = "registered-artifact completion proof must reach durable settlement"]
enum ArtifactCompletionProof {
    ObservedExact(RegisteredArtifactObservedExactProof),
    Published(RegisteredArtifactMutationProof),
}

impl ArtifactCompletionProof {
    fn settle(self) {
        match self {
            Self::ObservedExact(proof) => drop(proof),
            Self::Published(proof) => drop(proof),
        }
    }
}

#[must_use = "registered-artifact continuation cause must reach durable settlement"]
enum ArtifactContinuationCause {
    MutationFailure(RegisteredArtifactMutationError),
    ObservationFailure(RegisteredArtifactObservedExactValidationError),
    ObservedExact(RegisteredArtifactObservedExactProof),
}

impl ArtifactContinuationCause {
    fn try_no_effect_mutation(
        error: RegisteredArtifactMutationError,
    ) -> Result<Self, RegisteredArtifactMutationError> {
        if error.has_unsettled_effect() {
            Err(error)
        } else {
            Ok(Self::MutationFailure(error))
        }
    }

    fn settle(self) {
        match self {
            Self::MutationFailure(error) => drop(error),
            Self::ObservationFailure(error) => drop(error),
            Self::ObservedExact(proof) => drop(proof),
        }
    }
}

#[must_use = "registered-artifact propagation authority must remain retained"]
enum ArtifactPropagationOwner {
    Published(RegisteredArtifactMutationProof),
    PreservationFailure(RegisteredArtifactEffectPreservationError),
    PendingQuarantine(RegisteredArtifactQuarantinePreservation),
}

impl ArtifactPropagationOwner {
    fn release(self) {
        match self {
            Self::Published(proof) => drop(proof),
            Self::PreservationFailure(error) => drop(error),
            Self::PendingQuarantine(preservation) => drop(preservation),
        }
    }
}

#[must_use = "registered-artifact disposition owns runtime authority through settlement"]
enum ArtifactFinishDisposition {
    Complete(ArtifactCompletionProof),
    Continue(Option<ArtifactContinuationCause>),
    Propagate {
        error: OperationJournalStoreError,
        owner: Option<ArtifactPropagationOwner>,
    },
}

enum RetainedArtifactOwner {
    Completion(ArtifactCompletionProof),
    Continuation(ArtifactContinuationCause),
    Propagation(ArtifactPropagationOwner),
}

impl RetainedArtifactOwner {
    fn release(self) {
        match self {
            Self::Completion(proof) => proof.settle(),
            Self::Continuation(cause) => cause.settle(),
            Self::Propagation(owner) => owner.release(),
        }
    }
}

struct RetainedArtifactRepairError {
    source: OperationJournalStoreError,
    secondary: Option<OperationJournalStoreError>,
    _owner: Option<RetainedArtifactOwner>,
}

impl Drop for RetainedArtifactRepairError {
    fn drop(&mut self) {
        if let Some(owner) = self._owner.take() {
            owner.release();
        }
    }
}

#[must_use]
#[derive(Debug, Eq, PartialEq)]
pub(crate) struct GuardianArtifactRepairReceipt {
    diagnosis_id: DiagnosisId,
    status: GuardianArtifactRepairStatus,
}

#[must_use]
pub(crate) struct GuardianArtifactRepairFailure {
    continuation: RegisteredArtifactFailedRepair,
}

#[must_use]
pub(crate) enum GuardianArtifactRepairSettlement {
    Completed(GuardianArtifactRepairReceipt),
    Failed(Box<GuardianArtifactRepairFailure>),
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
    pub(crate) fn into_continuation(self) -> RegisteredArtifactFailedRepair {
        self.continuation
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
        rollback: RollbackState,
        facts: Vec<String>,
        quarantine_checkpoint: ReconciliationQuarantineCheckpoint,
    },
    Failed {
        step_id: &'static str,
        rollback: RollbackState,
        facts: Vec<String>,
        quarantine_checkpoint: ReconciliationQuarantineCheckpoint,
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
    disposition: ArtifactFinishDisposition,
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
    match execution.disposition {
        ArtifactFinishDisposition::Complete(proof) => {
            proof.settle();
            Ok(GuardianArtifactRepairSettlement::Completed(
                execution.outcome,
            ))
        }
        ArtifactFinishDisposition::Continue(cause) => {
            let continuation = match admission.into_failed_continuation(execution.memory_receipt) {
                Ok(continuation) => continuation,
                Err(error) => {
                    return Err(retained_artifact_error(
                        artifact_reconciliation_error(error),
                        None,
                        cause.map(RetainedArtifactOwner::Continuation),
                    ));
                }
            };
            if let Some(cause) = cause {
                cause.settle();
            }
            Ok(GuardianArtifactRepairSettlement::Failed(Box::new(
                GuardianArtifactRepairFailure { continuation },
            )))
        }
        ArtifactFinishDisposition::Propagate { error, owner } => {
            match owner {
                Some(ArtifactPropagationOwner::PendingQuarantine(preservation)) => {
                    match preservation.acknowledge_preserved().await {
                        Ok(()) => Err(error),
                        Err(acknowledgement) => Err(retained_artifact_error(
                            error,
                            None,
                            Some(RetainedArtifactOwner::Propagation(
                                ArtifactPropagationOwner::PreservationFailure(acknowledgement),
                            )),
                        )),
                    }
                }
                Some(owner) => Err(retained_artifact_error(
                    error,
                    None,
                    Some(RetainedArtifactOwner::Propagation(owner)),
                )),
                None => Err(error),
            }
        }
    }
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
            operation_id,
            ArtifactTerminal::Failed {
                step_id: "journal_repair_start",
                rollback: RollbackState::NotApplicable,
                facts: Vec::new(),
                quarantine_checkpoint: ReconciliationQuarantineCheckpoint::default(),
            },
            ArtifactFinishDisposition::Propagate {
                error,
                owner: None,
            },
        )
        .await
    } else {
        execute_planned_artifact_repair(&context, operation_id, target).await
    }
}

async fn execute_planned_artifact_repair(
    context: &ArtifactRepairContext<'_>,
    operation_id: OperationId,
    target: TargetDescriptor,
) -> Result<ArtifactRepairExecution, OperationJournalStoreError> {
    let download_plan = {
        let admission = context.admission;
        if !admission.evidence_is_live() {
            return finish_artifact_repair(
                context,
                operation_id,
                ArtifactTerminal::Failed {
                    step_id: "revalidate_registered_artifact_authority",
                    rollback: RollbackState::NotApplicable,
                    facts: Vec::new(),
                    quarantine_checkpoint: ReconciliationQuarantineCheckpoint::default(),
                },
                ArtifactFinishDisposition::Continue(None),
            )
            .await;
        }
        let Some(state) = admission.physical_state().await else {
            return finish_artifact_repair(
                context,
                operation_id,
                ArtifactTerminal::Failed {
                    step_id: "revalidate_registered_artifact_condition",
                    rollback: RollbackState::NotApplicable,
                    facts: Vec::new(),
                    quarantine_checkpoint: ReconciliationQuarantineCheckpoint::default(),
                },
                ArtifactFinishDisposition::Continue(None),
            )
            .await;
        };
        match state {
            RegisteredArtifactPhysicalState::Exact(proof) => {
                return settle_observed_exact(context, operation_id, proof).await;
            }
            state if admission.physical_state_matches_finding(&state) => {}
            _ => {
                return finish_artifact_repair(
                    context,
                    operation_id,
                    ArtifactTerminal::Failed {
                        step_id: "revalidate_registered_artifact_condition",
                        rollback: RollbackState::NotApplicable,
                        facts: Vec::new(),
                        quarantine_checkpoint: ReconciliationQuarantineCheckpoint::default(),
                    },
                    ArtifactFinishDisposition::Continue(None),
                )
                .await;
            }
        }
        if !admission.evidence_is_live() {
            return finish_artifact_repair(
                context,
                operation_id,
                ArtifactTerminal::Failed {
                    step_id: "revalidate_registered_artifact_authority",
                    rollback: RollbackState::NotApplicable,
                    facts: Vec::new(),
                    quarantine_checkpoint: ReconciliationQuarantineCheckpoint::default(),
                },
                ArtifactFinishDisposition::Continue(None),
            )
            .await;
        }
        match admission.plan() {
            RegisteredArtifactRepairPlanRef::Download(plan) => plan,
            RegisteredArtifactRepairPlanRef::ComponentRebuildRequired => {
                return finish_artifact_repair(
                    context,
                    operation_id,
                    ArtifactTerminal::Failed {
                        step_id: crate::state::REGISTERED_ARTIFACT_COMPONENT_REBUILD_FAILURE_POINT,
                        rollback: RollbackState::NotApplicable,
                        facts: Vec::new(),
                        quarantine_checkpoint: ReconciliationQuarantineCheckpoint::default(),
                    },
                    ArtifactFinishDisposition::Continue(None),
                )
                .await;
            }
        }
    };

    let _mutation = match context.admission.admit_managed_artifact_mutation() {
        Ok(mutation) => mutation,
        Err(_) => {
            return finish_artifact_repair(
                context,
                operation_id,
                ArtifactTerminal::Failed {
                    step_id: "admit_managed_artifact_mutation",
                    rollback: RollbackState::NotApplicable,
                    facts: Vec::new(),
                    quarantine_checkpoint: ReconciliationQuarantineCheckpoint::default(),
                },
                ArtifactFinishDisposition::Continue(None),
            )
            .await;
        }
    };

    let quarantine_checkpoint = if context.quarantines_existing() {
        let quarantine_outcome = context
            .admission
            .mutation()
            .quarantine_existing(
                &operation_id,
                &target,
                download_plan.expected_sha1(),
                download_plan.expected_size(),
            )
            .await;
        let report = match quarantine_outcome {
            Ok(RegisteredArtifactQuarantineOutcome::AlreadyExact(proof)) => {
                return settle_observed_exact(context, operation_id, proof).await;
            }
            Ok(RegisteredArtifactQuarantineOutcome::Quarantined(report)) => report,
            Err(error) => {
                let facts = fact_ids(error.facts());
                let unsettled = error.has_unsettled_effect();
                let disposition = match ArtifactContinuationCause::try_no_effect_mutation(error) {
                    Ok(cause) => ArtifactFinishDisposition::Continue(Some(cause)),
                    Err(error) => ArtifactFinishDisposition::Propagate {
                        error: artifact_execution_error(error),
                        owner: None,
                    },
                };
                return finish_artifact_repair(
                    context,
                    operation_id,
                    ArtifactTerminal::Failed {
                        step_id: "quarantine_launcher_managed_target",
                        rollback: if unsettled {
                            RollbackState::Unavailable
                        } else {
                            RollbackState::NotApplicable
                        },
                        facts,
                        quarantine_checkpoint: ReconciliationQuarantineCheckpoint::default(),
                    },
                    disposition,
                )
                .await;
            }
        };
        let (quarantine_facts, preservation) = report.into_parts();
        let quarantine_target = TargetDescriptor::new(
            StabilizationSystem::Execution,
            target.kind,
            format!("quarantine-{}", target.id),
            target.ownership,
        );
        let quarantine_checkpoint = repair_step(
            "quarantine_launcher_managed_target",
            OperationStepResult::Completed,
            Some(target.clone()),
            fact_ids(&quarantine_facts),
            RollbackState::Unavailable,
        );
        let durable_checkpoint = ReconciliationQuarantineCheckpoint::new(vec![
            ReconciliationQuarantineRecord::artifact(quarantine_target),
        ]);
        let mut checkpoint_error = None;
        loop {
            let result = context
                .journals
                .record_checkpoint(&operation_id, quarantine_checkpoint.clone())
                .await;
            match result {
                Ok(()) => break,
                Err(error) => {
                    let reconciliation = reconcile_artifact_journal_error(
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
                    .await;
                    match reconciliation {
                        Ok(ArtifactJournalReconciliation::MutationCommitted) => break,
                        Ok(ArtifactJournalReconciliation::AcceptedFailure(error)) => {
                            checkpoint_error = Some(error);
                            break;
                        }
                        Ok(ArtifactJournalReconciliation::RetryMutation) => {}
                        Err(error) => {
                            return finish_artifact_repair(
                                context,
                                operation_id,
                                ArtifactTerminal::Failed {
                                    step_id: "record_quarantine_checkpoint",
                                    rollback: RollbackState::Unavailable,
                                    facts: Vec::new(),
                                    quarantine_checkpoint: durable_checkpoint,
                                },
                                ArtifactFinishDisposition::Propagate {
                                    error,
                                    owner: Some(ArtifactPropagationOwner::PendingQuarantine(
                                        preservation,
                                    )),
                                },
                            )
                            .await;
                        }
                    }
                }
            }
        }
        if let Err(error) = preservation.acknowledge_preserved().await {
            let (error, owner) = match checkpoint_error {
                Some(primary) => (
                    primary,
                    Some(ArtifactPropagationOwner::PreservationFailure(error)),
                ),
                None => (artifact_execution_error(error), None),
            };
            return finish_artifact_repair(
                context,
                operation_id,
                ArtifactTerminal::Failed {
                    step_id: "acknowledge_quarantined_artifact",
                    rollback: RollbackState::Unavailable,
                    facts: Vec::new(),
                    quarantine_checkpoint: durable_checkpoint,
                },
                ArtifactFinishDisposition::Propagate {
                    error,
                    owner,
                },
            )
            .await;
        }
        if let Some(error) = checkpoint_error {
            return finish_artifact_repair(
                context,
                operation_id,
                ArtifactTerminal::Failed {
                    step_id: "record_quarantine_checkpoint",
                    rollback: RollbackState::Unavailable,
                    facts: Vec::new(),
                    quarantine_checkpoint: durable_checkpoint,
                },
                ArtifactFinishDisposition::Propagate { error, owner: None },
            )
            .await;
        }
        durable_checkpoint
    } else {
        ReconciliationQuarantineCheckpoint::default()
    };

    if !context.admission.evidence_is_live() {
        let disposition = if quarantine_checkpoint.is_empty() {
            ArtifactFinishDisposition::Continue(None)
        } else {
            ArtifactFinishDisposition::Propagate {
                error: artifact_runtime_error(
                    "registered artifact authority changed after quarantine",
                ),
                owner: None,
            }
        };
        return finish_artifact_repair(
            context,
            operation_id,
            ArtifactTerminal::Failed {
                step_id: "revalidate_registered_artifact_authority",
                rollback: if quarantine_checkpoint.is_empty() {
                    RollbackState::NotApplicable
                } else {
                    RollbackState::Unavailable
                },
                facts: Vec::new(),
                quarantine_checkpoint,
            },
            disposition,
        )
        .await;
    }
    let download_result = context
        .admission
        .mutation()
        .download_verify_promote(
            &operation_id,
            &target,
            download_plan.provider_url(),
            download_plan.expected_sha1(),
            download_plan.expected_size(),
            context.client,
        )
        .await;

    match download_result {
        Ok(report) => {
            let facts = fact_ids(report.facts());
            match report.validate().await {
                Ok(proof) if context.admission.evidence_is_live() => {
                    finish_artifact_repair(
                        context,
                        operation_id,
                        ArtifactTerminal::Repaired {
                            step_id: "promote_verified_artifact",
                            rollback: RollbackState::Unavailable,
                            facts,
                            quarantine_checkpoint,
                        },
                        ArtifactFinishDisposition::Complete(ArtifactCompletionProof::Published(
                            proof,
                        )),
                    )
                    .await
                }
                Ok(proof) => {
                    finish_artifact_repair(
                        context,
                        operation_id,
                        ArtifactTerminal::Failed {
                            step_id: "revalidate_registered_artifact_authority",
                            rollback: RollbackState::Unavailable,
                            facts,
                            quarantine_checkpoint,
                        },
                        ArtifactFinishDisposition::Propagate {
                            error: artifact_runtime_error(
                                "registered artifact authority changed after publication",
                            ),
                            owner: Some(ArtifactPropagationOwner::Published(proof)),
                        },
                    )
                    .await
                }
                Err(error) => {
                    finish_artifact_repair(
                        context,
                        operation_id,
                        ArtifactTerminal::Failed {
                            step_id: "verify_registered_artifact_postcondition",
                            rollback: RollbackState::Unavailable,
                            facts,
                            quarantine_checkpoint,
                        },
                        ArtifactFinishDisposition::Propagate {
                            error: artifact_execution_error(error),
                            owner: None,
                        },
                    )
                    .await
                }
            }
        }
        Err(error) => {
            let facts = fact_ids(error.facts());
            let unsettled = error.has_unsettled_effect();
            let target_effect = !quarantine_checkpoint.is_empty();
            let disposition = if target_effect {
                ArtifactFinishDisposition::Propagate {
                    error: artifact_execution_error(error),
                    owner: None,
                }
            } else {
                match ArtifactContinuationCause::try_no_effect_mutation(error) {
                    Ok(cause) => ArtifactFinishDisposition::Continue(Some(cause)),
                    Err(error) => ArtifactFinishDisposition::Propagate {
                        error: artifact_execution_error(error),
                        owner: None,
                    },
                }
            };
            finish_artifact_repair(
                context,
                operation_id,
                ArtifactTerminal::Failed {
                    step_id: "download_artifact_to_temp",
                    rollback: if unsettled || target_effect {
                        RollbackState::Unavailable
                    } else {
                        RollbackState::NotApplicable
                    },
                    facts,
                    quarantine_checkpoint,
                },
                disposition,
            )
            .await
        }
    }
}

async fn settle_observed_exact(
    context: &ArtifactRepairContext<'_>,
    operation_id: OperationId,
    proof: RegisteredArtifactObservedExactProof,
) -> Result<ArtifactRepairExecution, OperationJournalStoreError> {
    match proof.validate().await {
        Ok(proof) if context.admission.evidence_is_live() => {
            finish_artifact_repair(
                context,
                operation_id,
                ArtifactTerminal::Repaired {
                    step_id: "registered_artifact_already_exact",
                    rollback: RollbackState::NotApplicable,
                    facts: Vec::new(),
                    quarantine_checkpoint: ReconciliationQuarantineCheckpoint::default(),
                },
                ArtifactFinishDisposition::Complete(ArtifactCompletionProof::ObservedExact(proof)),
            )
            .await
        }
        Ok(proof) => {
            finish_artifact_repair(
                context,
                operation_id,
                ArtifactTerminal::Failed {
                    step_id: "revalidate_registered_artifact_authority",
                    rollback: RollbackState::NotApplicable,
                    facts: Vec::new(),
                    quarantine_checkpoint: ReconciliationQuarantineCheckpoint::default(),
                },
                ArtifactFinishDisposition::Continue(Some(
                    ArtifactContinuationCause::ObservedExact(proof),
                )),
            )
            .await
        }
        Err(error) => {
            finish_artifact_repair(
                context,
                operation_id,
                ArtifactTerminal::Failed {
                    step_id: "validate_registered_artifact_observation",
                    rollback: RollbackState::NotApplicable,
                    facts: Vec::new(),
                    quarantine_checkpoint: ReconciliationQuarantineCheckpoint::default(),
                },
                ArtifactFinishDisposition::Continue(Some(
                    ArtifactContinuationCause::ObservationFailure(error),
                )),
            )
            .await
        }
    }
}

async fn finish_artifact_repair(
    context: &ArtifactRepairContext<'_>,
    operation_id: OperationId,
    terminal: ArtifactTerminal,
    disposition: ArtifactFinishDisposition,
) -> Result<ArtifactRepairExecution, OperationJournalStoreError> {
    let pairing_is_valid = matches!(
        (&terminal, &disposition),
        (
            ArtifactTerminal::Repaired { .. },
            ArtifactFinishDisposition::Complete(_)
        ) | (
            ArtifactTerminal::Failed { .. },
            ArtifactFinishDisposition::Continue(_)
        ) | (
            ArtifactTerminal::Failed { .. },
            ArtifactFinishDisposition::Propagate { .. }
        )
    );
    if !pairing_is_valid {
        return Err(retained_disposition_error(
            artifact_runtime_error(
                "registered artifact terminal and runtime disposition are inconsistent",
            ),
            disposition,
        ));
    }
    let (
        step_id,
        rollback,
        failure_point,
        reconciliation_outcome,
        status,
        facts,
        quarantine_checkpoint,
    ) = match terminal {
        ArtifactTerminal::Repaired {
            step_id,
            rollback,
            facts,
            quarantine_checkpoint,
        } => (
            step_id,
            rollback,
            None,
            ReconciliationTerminalOutcome::Succeeded,
            GuardianArtifactRepairStatus::Repaired,
            facts,
            quarantine_checkpoint,
        ),
        ArtifactTerminal::Failed {
            step_id,
            rollback,
            facts,
            quarantine_checkpoint,
        } => (
            step_id,
            rollback,
            Some(step_id),
            ReconciliationTerminalOutcome::Failed,
            GuardianArtifactRepairStatus::Failed,
            facts,
            quarantine_checkpoint,
        ),
    };
    let reconciliation_terminal = match context
        .admission
        .terminal(
            context.attempt.clone(),
            reconciliation_outcome,
            quarantine_checkpoint,
        )
    {
        Ok(terminal) => terminal,
        Err(error) => {
            return Err(retained_disposition_error(
                artifact_reconciliation_error(error),
                disposition,
            ));
        }
    };
    let step_result = if failure_point.is_some() {
        OperationStepResult::Failed
    } else {
        OperationStepResult::Completed
    };
    let journal_persistence_error = match record_artifact_terminal_reconciled(
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
    .await
    {
        Ok(error) => error,
        Err(error) => return Err(retained_disposition_error(error, disposition)),
    };
    let memory_receipt = match context
        .admission
        .commit_terminal_memory(
            reconciliation_terminal,
            context
                .reservation
                .as_ref()
                .expect("attempted repair owns memory reservation"),
        )
        .await
    {
        Ok(receipt) => receipt,
        Err(error) => {
            return Err(retained_disposition_error(error, disposition));
        }
    };
    if let Some(error) = journal_persistence_error {
        return Err(retained_disposition_error(error, disposition));
    }
    Ok(ArtifactRepairExecution {
        outcome: artifact_repair_outcome(context.attempt.diagnosis_id(), status),
        memory_receipt,
        disposition,
    })
}

fn retained_disposition_error(
    source: OperationJournalStoreError,
    disposition: ArtifactFinishDisposition,
) -> OperationJournalStoreError {
    match disposition {
        ArtifactFinishDisposition::Complete(proof) => retained_artifact_error(
            source,
            None,
            Some(RetainedArtifactOwner::Completion(proof)),
        ),
        ArtifactFinishDisposition::Continue(cause) => retained_artifact_error(
            source,
            None,
            cause.map(RetainedArtifactOwner::Continuation),
        ),
        ArtifactFinishDisposition::Propagate { error, owner } => {
            retained_artifact_error(
                source,
                Some(error),
                owner.map(RetainedArtifactOwner::Propagation),
            )
        }
    }
}

fn retained_artifact_error(
    source: OperationJournalStoreError,
    secondary: Option<OperationJournalStoreError>,
    owner: Option<RetainedArtifactOwner>,
) -> OperationJournalStoreError {
    OperationJournalStoreError::Persistence(io::Error::other(RetainedArtifactRepairError {
        source,
        secondary,
        _owner: owner,
    }))
}

fn artifact_runtime_error(message: &'static str) -> OperationJournalStoreError {
    OperationJournalStoreError::Persistence(io::Error::other(message))
}

fn artifact_execution_error(
    error: impl std::error::Error + Send + Sync + 'static,
) -> OperationJournalStoreError {
    OperationJournalStoreError::Persistence(io::Error::other(error))
}

impl std::fmt::Debug for RetainedArtifactRepairError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RetainedArtifactRepairError")
            .field("has_secondary", &self.secondary.is_some())
            .field("has_runtime_owner", &self._owner.is_some())
            .finish_non_exhaustive()
    }
}

impl std::fmt::Display for RetainedArtifactRepairError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("registered artifact repair retained unsettled runtime authority")
    }
}

impl std::error::Error for RetainedArtifactRepairError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
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
        JournalId::new(format!("journal-{operation_id}")),
        operation_id.clone(),
        CommandKind::RepairInstance,
        StabilizationSystem::Guardian,
        context.attempt.ownership(),
        RollbackState::NotApplicable,
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
        match journals.create_fresh(expected.clone()).await {
            Ok(()) => return Ok(None),
            Err(OperationJournalStoreError::AlreadyExists) => {
                return Err(OperationJournalStoreError::AlreadyExists);
            }
            Err(OperationJournalStoreError::RetryRequired) => {
                journals.retry().await?;
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
    &entry.operation_id == operation_id
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
    const QUARANTINE_REDOWNLOAD: [(&str, RollbackState); 9] = [
        ("journal_repair_start", RollbackState::NotApplicable),
        (
            "registered_artifact_already_exact",
            RollbackState::NotApplicable,
        ),
        (
            "quarantine_launcher_managed_target",
            RollbackState::Unavailable,
        ),
        ("record_quarantine_checkpoint", RollbackState::Unavailable),
        (
            "acknowledge_quarantined_artifact",
            RollbackState::Unavailable,
        ),
        ("download_artifact_to_temp", RollbackState::Unavailable),
        ("verify_artifact_checksum", RollbackState::NotApplicable),
        ("promote_verified_artifact", RollbackState::Unavailable),
        ("record_repair_outcome", RollbackState::NotApplicable),
    ];
    const MISSING_DOWNLOAD: [(&str, RollbackState); 6] = [
        ("journal_repair_start", RollbackState::NotApplicable),
        (
            "registered_artifact_already_exact",
            RollbackState::NotApplicable,
        ),
        ("download_artifact_to_temp", RollbackState::NotApplicable),
        ("verify_artifact_checksum", RollbackState::NotApplicable),
        ("promote_verified_artifact", RollbackState::Unavailable),
        ("record_repair_outcome", RollbackState::NotApplicable),
    ];
    const COMPONENT_REBUILD_REQUIRED: [(&str, RollbackState); 4] = [
        ("journal_repair_start", RollbackState::NotApplicable),
        (
            "registered_artifact_already_exact",
            RollbackState::NotApplicable,
        ),
        (
            crate::state::REGISTERED_ARTIFACT_COMPONENT_REBUILD_FAILURE_POINT,
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
mod persistence_contract_tests {
    use super::{GuardianArtifactRepairSettlement, execute_registered_guardian_artifact_repair};
    use crate::execution::persistence::{AtomicWriteBackend, PersistenceCoordinator};
    use crate::guardian::{
        ActionPlanPrerequisite, DiagnosisId, GuardianAction, GuardianActionKind,
        GuardianActionPlan, GuardianConfidence, GuardianDecision, GuardianMode,
    };
    use crate::state::contracts::{
        OperationId, OperationStatus, OwnershipClass, ReconciliationTerminalOutcome,
        StabilizationSystem, TargetDescriptor,
    };
    use crate::state::failure_memory::GuardianFailureMemoryStore;
    use crate::state::{
        AppState, AppStateInit, InstallStore, OperationJournalStore,
        REGISTERED_ARTIFACT_COMPONENT_REBUILD_FAILURE_POINT, RegisteredArtifactCondition,
        SessionStore, new_instance, reconciliation_attempt_key, reconciliation_memory_entry,
    };
    use axial_config::{AppPaths, InstanceRegistrySnapshot};
    use axial_minecraft::known_good::{
        KnownGoodArtifactKind, KnownGoodInventory, TestKnownGoodEntry, TestKnownGoodIntegrity,
        TestKnownGoodRoot,
    };
    use sha1::{Digest as _, Sha1};
    use std::fs;
    use std::io;
    use std::io::Write as _;
    use std::net::TcpListener;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
    use std::thread;
    use std::time::{Duration, Instant};

    const INSTANCE_ID: &str = "0000000000000001";
    const EXPECTED_ASSET: &[u8] = b"registered artifact persistence proof";

    struct ScriptedWriteBackend {
        attempts: AtomicUsize,
        fail_attempt: AtomicUsize,
        gated_attempt: AtomicUsize,
        release_gate: AtomicBool,
        failure_message: &'static str,
    }

    impl ScriptedWriteBackend {
        fn new(fail_attempt: Option<usize>, failure_message: &'static str) -> Self {
            Self {
                attempts: AtomicUsize::new(0),
                fail_attempt: AtomicUsize::new(fail_attempt.unwrap_or_default()),
                gated_attempt: AtomicUsize::new(0),
                release_gate: AtomicBool::new(true),
                failure_message,
            }
        }

        fn attempts(&self) -> usize {
            self.attempts.load(Ordering::SeqCst)
        }

        fn gate_attempt(&self, attempt: usize) {
            self.gated_attempt.store(attempt, Ordering::SeqCst);
            self.release_gate.store(false, Ordering::SeqCst);
        }

        fn release(&self) {
            self.release_gate.store(true, Ordering::SeqCst);
        }

        async fn wait_for_attempt(&self, expected: usize) {
            tokio::time::timeout(Duration::from_secs(2), async {
                while self.attempts() < expected {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("artifact persistence attempt");
        }
    }

    impl AtomicWriteBackend for ScriptedWriteBackend {
        fn write(
            &self,
            destination: &crate::execution::anchored_record::AnchoredRecordTarget,
            effects: &axial_fs::EffectOwner,
            contents: &[u8],
        ) -> io::Result<()> {
            let attempt = self.attempts.fetch_add(1, Ordering::SeqCst) + 1;
            if self.gated_attempt.load(Ordering::SeqCst) == attempt {
                while !self.release_gate.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
            if self
                .fail_attempt
                .compare_exchange(attempt, 0, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                return Err(io::Error::other(self.failure_message));
            }
            destination.write(effects, contents)
        }
    }

    struct Fixture {
        state: AppState,
        journals: Arc<OperationJournalStore>,
        failure_memory: Arc<GuardianFailureMemoryStore>,
        journal_backend: Arc<ScriptedWriteBackend>,
        memory_backend: Arc<ScriptedWriteBackend>,
        root: PathBuf,
    }

    fn fixture(
        label: &str,
        journal_failure_attempt: Option<usize>,
        memory_failure_attempt: Option<usize>,
    ) -> Fixture {
        artifact_fixture(
            label,
            journal_failure_attempt,
            memory_failure_attempt,
            TestKnownGoodRoot::Assets,
            "indexes/persistence-proof.json",
            KnownGoodArtifactKind::AssetIndex,
            "https://example.invalid/persistence-proof.json",
            "assets/indexes/persistence-proof.json",
            Some(&vec![b'x'; EXPECTED_ASSET.len()]),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn artifact_fixture(
        label: &str,
        journal_failure_attempt: Option<usize>,
        memory_failure_attempt: Option<usize>,
        artifact_root: TestKnownGoodRoot,
        artifact_path: &str,
        artifact_kind: KnownGoodArtifactKind,
        provider_url: &str,
        destination_relative: &str,
        initial_bytes: Option<&[u8]>,
    ) -> Fixture {
        static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(1);

        let root = std::env::temp_dir().join(format!(
            "axial-artifact-persistence-{label}-{}-{}",
            std::process::id(),
            NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&root);
        let paths = AppPaths::from_root(root.to_path_buf()).expect("absolute test app root");
        fs::create_dir_all(paths.instances_dir().join(INSTANCE_ID)).expect("instance root");
        fs::create_dir_all(paths.library_dir()).expect("library root");
        let root_session = crate::state::test_root_session(&paths);
        let config = Arc::new(
            axial_config::ConfigStore::load_from(paths.clone(), Arc::clone(&root_session))
                .expect("test config store"),
        );
        let instances = Arc::new(
            axial_config::InstanceStore::from_snapshot(
                paths.clone(),
                root_session,
                InstanceRegistrySnapshot::new(
                    vec![new_instance(
                        INSTANCE_ID.to_string(),
                        "Artifact Persistence Test".to_string(),
                        "1.21.1".to_string(),
                        String::new(),
                        String::new(),
                    )],
                    INSTANCE_ID.to_string(),
                    Vec::new(),
                )
                .expect("instance registry snapshot"),
            )
            .expect("test instance store"),
        );
        let journal_backend = Arc::new(ScriptedWriteBackend::new(
            journal_failure_attempt,
            "injected artifact journal persistence failure",
        ));
        let memory_backend = Arc::new(ScriptedWriteBackend::new(
            memory_failure_attempt,
            "injected artifact failure-memory persistence failure",
        ));
        let journals = Arc::new(
            OperationJournalStore::try_load_from_paths_with_coordinator(
                &paths,
                PersistenceCoordinator::for_test(
                    journal_backend.clone(),
                    Duration::from_millis(1),
                    Duration::from_millis(5),
                ),
            )
            .expect("persistent artifact journals"),
        );
        let failure_memory = Arc::new(
            GuardianFailureMemoryStore::try_load_from_paths_with_coordinator(
                &paths,
                PersistenceCoordinator::for_test(
                    memory_backend.clone(),
                    Duration::from_millis(1),
                    Duration::from_millis(5),
                ),
            )
            .expect("persistent artifact failure memory"),
        );
        let state = AppState::new(AppStateInit {
            app_name: "Axial".to_string(),
            version: "test".to_string(),
            config,
            instances,
            installs: Arc::new(InstallStore::new()),
            sessions: Arc::new(SessionStore::new()),
            performance: Arc::new(
                axial_performance::PerformanceManager::load_for_startup(paths.performance_dir())
                    .expect("test performance state"),
            ),
            startup_warnings: Vec::new(),
        })
        .with_reconciliation_stores(journals.clone(), failure_memory.clone());
        state.set_library_dir_for_test(paths.library_dir().to_string_lossy().into_owned());
        fs::create_dir_all(state.managed_runtime_cache().root()).expect("managed runtime root");

        let inventory = KnownGoodInventory::from_test_entries([TestKnownGoodEntry {
            root: artifact_root,
            path: artifact_path.to_string(),
            kind: artifact_kind,
            integrity: TestKnownGoodIntegrity::Sha1 {
                digest: format!("{:x}", Sha1::digest(EXPECTED_ASSET)),
                size: EXPECTED_ASSET.len() as u64,
            },
        }])
        .expect("artifact persistence inventory")
        .with_test_standalone_leaf_repair_source(0, provider_url)
        .expect("artifact persistence source");
        state.activate_known_good_inventory_for_test(INSTANCE_ID, inventory);
        let destination = paths.library_dir.join(destination_relative);
        fs::create_dir_all(destination.parent().expect("artifact destination parent"))
            .expect("artifact destination parent");
        if let Some(initial_bytes) = initial_bytes {
            fs::write(destination, initial_bytes).expect("write initial artifact destination");
        }

        Fixture {
            state,
            journals,
            failure_memory,
            journal_backend,
            memory_backend,
            root,
        }
    }

    async fn artifact_admission(
        fixture: &Fixture,
        operation_id: &str,
        condition: RegisteredArtifactCondition,
    ) -> crate::state::RegisteredArtifactRepairAdmission {
        let lifecycle = fixture.state.acquire_instance_lifecycle(INSTANCE_ID).await;
        let foreground = fixture
            .state
            .register_integrity_foreground()
            .expect("register artifact persistence foreground")
            .wait_for_settlement()
            .await;
        let verification = fixture
            .state
            .mint_known_good_verification_lease(
                &foreground,
                &lifecycle,
                &PathBuf::from(
                    fixture
                        .state
                        .library_dir()
                        .expect("artifact persistence library root"),
                ),
            )
            .expect("mint artifact persistence verification");
        let observation = verification
            .registered_artifact_observation(0, condition)
            .expect("registered artifact observation");
        let findings = fixture
            .state
            .seal_registered_artifact_findings(verification, vec![observation])
            .expect("seal corrupt Assets finding");
        let target = findings
            .repair_candidate()
            .map(|candidate| candidate.target())
            .expect("corrupt Assets repair target")
            .clone();
        let authorization = findings
            .authorize_repair(&registered_artifact_repair_decision(target))
            .expect("authorize corrupt Assets repair");
        let admission = fixture
            .state
            .admit_registered_artifact_repair(
                authorization,
                OperationId::deterministic_test(operation_id),
                chrono::Duration::minutes(15),
            )
            .await
            .expect("admit corrupt Assets repair");
        drop((foreground, lifecycle));
        admission
    }

    async fn corrupt_assets_admission(
        fixture: &Fixture,
        operation_id: &str,
    ) -> crate::state::RegisteredArtifactRepairAdmission {
        artifact_admission(fixture, operation_id, RegisteredArtifactCondition::Corrupt).await
    }

    fn registered_artifact_repair_decision(target: TargetDescriptor) -> GuardianDecision {
        GuardianDecision::for_test(
            None,
            GuardianMode::Managed,
            GuardianActionKind::Repair,
            vec![DiagnosisId::LauncherManagedArtifactCorrupt],
            Some(GuardianActionPlan::new(
                StabilizationSystem::Guardian,
                ActionPlanPrerequisite {
                    diagnosis_id: DiagnosisId::LauncherManagedArtifactCorrupt,
                    ownership: OwnershipClass::LauncherManaged,
                    confidence: GuardianConfidence::Confirmed,
                    affected_targets: vec![target.clone()],
                    candidate_actions: vec![GuardianActionKind::Repair],
                },
                vec![GuardianAction {
                    kind: GuardianActionKind::Repair,
                    target: Some(target),
                    reason: DiagnosisId::LauncherManagedArtifactCorrupt,
                }],
            )),
        )
    }

    async fn execute_for_error(
        fixture: &Fixture,
        operation_id: &str,
    ) -> crate::state::OperationJournalStoreError {
        let admission = corrupt_assets_admission(fixture, operation_id).await;
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            execute_registered_guardian_artifact_repair(admission, &reqwest::Client::new()),
        )
        .await
        .expect("artifact executor must remain bounded");
        match result {
            Err(error) => error,
            Ok(GuardianArtifactRepairSettlement::Completed(_)) => {
                panic!("persistence failure must not return a completed settlement")
            }
            Ok(GuardianArtifactRepairSettlement::Failed(_)) => {
                panic!("persistence failure must not return a typed continuation")
            }
        }
    }

    fn error_chain_contains(error: &dyn std::error::Error, expected: &str) -> bool {
        let mut current = Some(error);
        while let Some(error) = current {
            if error.to_string().contains(expected) {
                return true;
            }
            current = error.source();
        }
        false
    }

    async fn cleanup(fixture: Fixture) {
        fixture
            .state
            .close_known_good_inventories()
            .await
            .expect("close known-good store");
        fixture
            .state
            .close_instance_registry()
            .await
            .expect("close instance registry");
        let Fixture {
            state,
            journals,
            failure_memory,
            journal_backend,
            memory_backend,
            root,
        } = fixture;
        drop((
            state,
            journals,
            failure_memory,
            journal_backend,
            memory_backend,
        ));
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn accepted_plan_persistence_failure_terminalizes_without_returning_continuation() {
        let fixture = fixture("accepted-plan", Some(1), None);
        let operation_id = "artifact-accepted-plan";

        let error = execute_for_error(&fixture, operation_id).await;

        assert!(error_chain_contains(
            &error,
            "injected artifact journal persistence failure"
        ));
        assert_eq!(fixture.journal_backend.attempts(), 3);
        assert_eq!(fixture.memory_backend.attempts(), 1);
        let journal = fixture
            .journals
            .get(&OperationId::deterministic_test(operation_id))
            .expect("accepted plan and separate terminal are visible");
        assert_eq!(journal.status, OperationStatus::Failed);
        assert_eq!(
            journal.failure_point.as_deref(),
            Some("journal_repair_start")
        );
        let terminal = journal
            .reconciliation_terminal()
            .expect("separate failed terminal")
            .clone();
        assert_eq!(terminal.outcome(), ReconciliationTerminalOutcome::Failed);
        let expected_memory =
            reconciliation_memory_entry(terminal.clone()).expect("canonical plan-failure memory");
        assert_eq!(
            fixture
                .failure_memory
                .get(&reconciliation_attempt_key(terminal.attempt())),
            Some(expected_memory)
        );

        cleanup(fixture).await;
    }

    #[tokio::test]
    async fn accepted_terminal_persistence_failure_commits_memory_before_returning_error() {
        let fixture = fixture("accepted-terminal", Some(2), None);
        let operation_id = "artifact-accepted-terminal";

        let error = execute_for_error(&fixture, operation_id).await;

        assert!(error_chain_contains(
            &error,
            "injected artifact journal persistence failure"
        ));
        assert_eq!(fixture.journal_backend.attempts(), 3);
        assert_eq!(fixture.memory_backend.attempts(), 1);
        let journal = fixture
            .journals
            .get(&OperationId::deterministic_test(operation_id))
            .expect("accepted failed terminal is visible");
        assert_eq!(journal.status, OperationStatus::Failed);
        assert_eq!(
            journal.failure_point.as_deref(),
            Some(REGISTERED_ARTIFACT_COMPONENT_REBUILD_FAILURE_POINT)
        );
        let terminal = journal
            .reconciliation_terminal()
            .expect("accepted failed terminal")
            .clone();
        assert_eq!(terminal.outcome(), ReconciliationTerminalOutcome::Failed);
        assert_eq!(
            fixture
                .failure_memory
                .get(&reconciliation_attempt_key(terminal.attempt())),
            Some(reconciliation_memory_entry(terminal).expect("canonical terminal memory"))
        );

        cleanup(fixture).await;
    }

    #[tokio::test]
    async fn failure_memory_persistence_retries_while_retaining_admission() {
        let fixture = fixture("memory-failure", None, Some(1));
        let operation_id = "artifact-memory-failure";
        fixture.memory_backend.gate_attempt(2);
        let admission = corrupt_assets_admission(&fixture, operation_id).await;
        let admission_lifetime = admission.lifetime_for_test();
        let execution = tokio::spawn(async move {
            execute_registered_guardian_artifact_repair(admission, &reqwest::Client::new()).await
        });

        fixture.memory_backend.wait_for_attempt(2).await;

        assert!(!execution.is_finished());
        assert!(admission_lifetime.upgrade().is_some());
        assert_eq!(fixture.journal_backend.attempts(), 2);
        assert_eq!(fixture.memory_backend.attempts(), 2);
        let journal = fixture
            .journals
            .get(&OperationId::deterministic_test(operation_id))
            .expect("failed terminal remains visible");
        assert_eq!(
            journal.failure_point.as_deref(),
            Some(REGISTERED_ARTIFACT_COMPONENT_REBUILD_FAILURE_POINT)
        );
        assert!(fixture.failure_memory.list().is_empty());

        fixture.memory_backend.release();
        let settlement = execution
            .await
            .expect("artifact memory retry task")
            .expect("artifact memory retry settles");
        assert!(matches!(
            settlement,
            GuardianArtifactRepairSettlement::Failed(_)
        ));
        let terminal = journal
            .reconciliation_terminal()
            .expect("failed terminal")
            .clone();
        assert_eq!(
            fixture
                .failure_memory
                .get(&reconciliation_attempt_key(terminal.attempt())),
            Some(reconciliation_memory_entry(terminal).expect("canonical terminal memory"))
        );

        cleanup(fixture).await;
    }

    #[tokio::test]
    async fn exact_observation_survives_accepted_terminal_failure_and_memory_retry() {
        let fixture = fixture("exact-proof", Some(2), Some(1));
        let operation_id = "artifact-exact-proof";
        let destination = PathBuf::from(
            fixture
                .state
                .library_dir()
                .expect("exact proof library root"),
        )
        .join("assets/indexes/persistence-proof.json");
        fs::write(&destination, EXPECTED_ASSET).expect("make observed artifact exact");

        let error = execute_for_error(&fixture, operation_id).await;

        assert!(error_chain_contains(
            &error,
            "injected artifact journal persistence failure"
        ));
        assert_eq!(fixture.memory_backend.attempts(), 2);
        let journal = fixture
            .journals
            .get(&OperationId::deterministic_test(operation_id))
            .expect("accepted exact terminal is visible");
        assert_eq!(journal.status, OperationStatus::Succeeded);
        assert_eq!(journal.failure_point, None);
        let step = journal
            .completed_steps
            .last()
            .expect("already-exact terminal step");
        assert_eq!(step.step_id, "registered_artifact_already_exact");
        assert_eq!(step.rollback, crate::state::contracts::RollbackState::NotApplicable);
        let terminal = journal
            .reconciliation_terminal()
            .expect("exact reconciliation terminal")
            .clone();
        assert_eq!(terminal.outcome(), ReconciliationTerminalOutcome::Succeeded);
        assert_eq!(
            fixture
                .failure_memory
                .get(&reconciliation_attempt_key(terminal.attempt())),
            Some(reconciliation_memory_entry(terminal).expect("exact terminal memory"))
        );

        drop(error);
        cleanup(fixture).await;
    }

    #[tokio::test]
    async fn accepted_quarantine_checkpoint_is_acknowledged_then_terminalized() {
        let fixture = artifact_fixture(
            "accepted-quarantine",
            Some(2),
            None,
            TestKnownGoodRoot::Libraries,
            "example/persistence-proof.jar",
            KnownGoodArtifactKind::Library,
            "https://example.invalid/persistence-proof.jar",
            "libraries/example/persistence-proof.jar",
            Some(&vec![b'x'; EXPECTED_ASSET.len()]),
        );
        let operation_id = "artifact-accepted-quarantine";
        let error = execute_for_error(&fixture, operation_id).await;

        assert!(error_chain_contains(
            &error,
            "injected artifact journal persistence failure"
        ));
        let operation_id = OperationId::deterministic_test(operation_id);
        let journal = fixture
            .journals
            .get(&operation_id)
            .expect("accepted quarantine terminal is visible");
        assert_eq!(journal.status, OperationStatus::Failed);
        assert_eq!(
            journal.failure_point.as_deref(),
            Some("record_quarantine_checkpoint")
        );
        assert!(!journal
            .reconciliation_terminal()
            .expect("quarantine failure terminal")
            .quarantine_checkpoint()
            .is_empty());
        assert_eq!(
            journal
                .completed_steps
                .last()
                .expect("quarantine failure step")
                .rollback,
            crate::state::contracts::RollbackState::Unavailable
        );
        let parked = PathBuf::from(
            fixture
                .state
                .library_dir()
                .expect("quarantine library root"),
        )
        .join("libraries/example")
        .join(format!(".axial-quarantine-{operation_id}"));
        assert_eq!(fs::read(parked).expect("acknowledged quarantine"), vec![b'x'; EXPECTED_ASSET.len()]);

        drop(error);
        cleanup(fixture).await;
    }

    #[tokio::test]
    async fn provider_failure_without_effect_remains_eligible_for_continuation() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind provider failure server");
        listener
            .set_nonblocking(true)
            .expect("make provider failure server nonblocking");
        let address = listener.local_addr().expect("provider failure address");
        let server = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream
                            .write_all(
                                b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                            )
                            .expect("write provider failure");
                        break;
                    }
                    Err(error)
                        if error.kind() == io::ErrorKind::WouldBlock
                            && Instant::now() < deadline =>
                    {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        panic!("Guardian did not request the provider before the test deadline");
                    }
                    Err(error) => panic!("accept provider request: {error}"),
                }
            }
        });
        let fixture = artifact_fixture(
            "provider-no-effect",
            None,
            None,
            TestKnownGoodRoot::Assets,
            "indexes/persistence-proof.json",
            KnownGoodArtifactKind::AssetIndex,
            &format!("http://{address}/persistence-proof.json"),
            "assets/indexes/persistence-proof.json",
            None,
        );
        let operation_id = "artifact-provider-no-effect";
        let admission = artifact_admission(
            &fixture,
            operation_id,
            RegisteredArtifactCondition::Missing,
        )
        .await;

        let settlement = tokio::time::timeout(
            Duration::from_secs(2),
            execute_registered_guardian_artifact_repair(admission, &reqwest::Client::new()),
        )
        .await
        .expect("no-effect provider failure settlement deadline")
        .expect("no-effect provider failure settles");
        assert!(matches!(
            &settlement,
            GuardianArtifactRepairSettlement::Failed(_)
        ));
        server.join().expect("join provider failure server");
        let journal = fixture
            .journals
            .get(&OperationId::deterministic_test(operation_id))
            .expect("provider failure terminal");
        assert_eq!(
            journal.failure_point.as_deref(),
            Some("download_artifact_to_temp")
        );
        assert_eq!(
            journal
                .completed_steps
                .last()
                .expect("provider failure step")
                .rollback,
            crate::state::contracts::RollbackState::NotApplicable
        );
        assert!(journal
            .reconciliation_terminal()
            .expect("provider failure reconciliation terminal")
            .quarantine_checkpoint()
            .is_empty());

        drop(settlement);
        cleanup(fixture).await;
    }

    #[tokio::test]
    async fn quarantine_acknowledgement_failure_cannot_mint_a_continuation() {
        let fixture = artifact_fixture(
            "quarantine-ack-failure",
            None,
            None,
            TestKnownGoodRoot::Libraries,
            "example/persistence-proof.jar",
            KnownGoodArtifactKind::Library,
            "https://example.invalid/persistence-proof.jar",
            "libraries/example/persistence-proof.jar",
            Some(&vec![b'x'; EXPECTED_ASSET.len()]),
        );
        let operation_label = "artifact-quarantine-ack-failure";
        let operation_id = OperationId::deterministic_test(operation_label);
        fixture.journal_backend.gate_attempt(2);
        let admission = artifact_admission(
            &fixture,
            operation_label,
            RegisteredArtifactCondition::Corrupt,
        )
        .await;
        let execution = tokio::spawn(async move {
            execute_registered_guardian_artifact_repair(admission, &reqwest::Client::new()).await
        });
        fixture.journal_backend.wait_for_attempt(2).await;

        let parent = PathBuf::from(
            fixture
                .state
                .library_dir()
                .expect("ack failure library root"),
        )
        .join("libraries/example");
        let parked = parent.join(format!(".axial-quarantine-{operation_id}"));
        let displaced = parent.join("displaced-persistence-proof.jar");
        fs::rename(&parked, &displaced).expect("displace pending quarantine");
        fixture.journal_backend.release();

        let result = tokio::time::timeout(Duration::from_secs(2), execution)
            .await
            .expect("ack failure settlement deadline")
            .expect("ack failure task");
        let Err(error) = result else {
            panic!("acknowledgement failure must propagate");
        };
        assert!(error_chain_contains(
            &error,
            "registered artifact quarantine acknowledgement failed"
        ));
        let journal = fixture
            .journals
            .get(&operation_id)
            .expect("acknowledgement failure terminal");
        assert_eq!(journal.status, OperationStatus::Failed);
        assert_eq!(
            journal.failure_point.as_deref(),
            Some("acknowledge_quarantined_artifact")
        );
        assert_eq!(
            journal
                .completed_steps
                .last()
                .expect("acknowledgement failure step")
                .rollback,
            crate::state::contracts::RollbackState::Unavailable
        );
        assert!(!journal
            .reconciliation_terminal()
            .expect("acknowledgement failure reconciliation terminal")
            .quarantine_checkpoint()
            .is_empty());

        fs::rename(&displaced, &parked).expect("restore pending quarantine binding");
        drop(error);
        cleanup(fixture).await;
    }
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
