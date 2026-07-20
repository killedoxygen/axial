use super::reconciliation_journal::{
    GuardianJournalReconciliation, reconcile_guardian_journal_error,
    record_reconciliation_terminal_reconciled, repair_step,
};
use super::{
    DiagnosisId, GuardianActionKind, GuardianDomain, GuardianMode, ReadyMarkerRepairAuthorization,
};
use crate::execution::ExecutionFact;
use crate::execution::runtime::{
    ManagedRuntimeRepairRequest, ManagedRuntimeRoot, repair_managed_runtime,
};
use crate::observability::{RedactionAudience, sanitize_evidence_token};
use crate::state::contracts::{
    CommandKind, JournalId, OperationId, OperationJournalEntry, OperationJournalStep,
    OperationOutcome, OperationStatus, OperationStepResult, OwnershipClass, ReconciliationAttempt,
    ReconciliationComponent, ReconciliationScope, ReconciliationTerminal,
    ReconciliationTerminalOutcome, RollbackState, StabilizationSystem, TargetDescriptor,
    TargetKind,
};
use crate::state::failure_memory::GuardianFailureMemoryStore;
use crate::state::{
    OperationJournalStore, OperationJournalStoreError, ReconciliationAttemptReservation,
    RegisteredReconciliationAuthority, commit_reconciliation_memory,
    operation_journal_completed_step_is_visible, operation_journal_plan_is_visible,
    operation_journal_terminal_is_visible, reconciliation_attempt_key,
    reconciliation_instance_target, reconciliation_journal_attempt, reconciliation_memory_entry,
    record_guardian_repair_refusal, reserve_reconciliation_attempt, settle_reconciliation_memory,
};
use chrono::{DateTime, Duration};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};

const READY_MARKER_REPAIR_STEP: &str = "recreate_managed_runtime_ready_marker";
const RUNTIME_REPAIR_START_STEP: &str = "journal_repair_start";
const DEFAULT_REPAIR_SUPPRESSION_MINUTES: i64 = 15;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct GuardianRepairOutcome {
    pub operation_id: OperationId,
    pub diagnosis_id: Option<DiagnosisId>,
    pub action: Option<GuardianActionKind>,
    pub status: GuardianRepairStatus,
    pub facts: Vec<String>,
    pub summary: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum GuardianRepairStatus {
    Repaired,
    Blocked,
    Failed,
}

struct RuntimeTerminalContext<'a> {
    authority: &'a RegisteredReconciliationAuthority,
    attempt: &'a ReconciliationAttempt,
    reservation: &'a ReconciliationAttemptReservation,
    journals: &'a OperationJournalStore,
    failure_memory: &'a GuardianFailureMemoryStore,
    diagnosis_id: &'a DiagnosisId,
    target: &'a TargetDescriptor,
    terminal_failure: Option<&'a tokio::sync::Notify>,
}

enum RuntimeTerminal {
    Repaired {
        action: GuardianActionKind,
        facts: Vec<String>,
    },
    Failed {
        step_id: &'static str,
        action: GuardianActionKind,
        facts: Vec<String>,
        summary: &'static str,
        suppression_until: Option<String>,
    },
}

enum RuntimeTerminalJournal {
    Record(&'static str, OperationStepResult, Option<&'static str>),
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn execute_managed_runtime_ready_marker_repair(
    authorization: ReadyMarkerRepairAuthorization,
    operation_id: Option<OperationId>,
    authority: RegisteredReconciliationAuthority,
    runtime_root: ManagedRuntimeRoot<'_>,
    abandoned: Option<&AtomicBool>,
    ready_for_effect: Option<tokio::sync::oneshot::Sender<()>>,
    terminal_failure: Option<&tokio::sync::Notify>,
) -> Result<GuardianRepairOutcome, OperationJournalStoreError> {
    let operation_id = operation_id
        .as_ref()
        .map(safe_operation_id)
        .unwrap_or_else(new_repair_operation_id);
    let authorization = authorization.into_parts();
    let journals = authority.journals();
    let failure_memory = authority.failure_memory();
    let runtime_root_target = public_safe_target(runtime_root.target());
    let target = public_safe_target(&authorization.target);
    let action = authorization.action;
    if authorization.ownership != target.ownership
        || !authority.owns_runtime_root(&runtime_root)
        || !ready_marker_repair_target_supported(&runtime_root_target)
        || target != runtime_root_target
    {
        return create_blocked_runtime_outcome(
            journals,
            operation_id,
            &authorization.diagnosis_id,
            &target,
            Some(action),
            "guardian_repair_blocked_unsupported_target",
        )
        .await;
    }
    if authorization.max_attempts == 0 {
        return create_blocked_runtime_outcome(
            journals,
            operation_id,
            &authorization.diagnosis_id,
            &target,
            None,
            "guardian_repair_blocked_by_policy",
        )
        .await;
    }

    settle_reconciliation_memory(failure_memory)
        .await
        .map_err(failure_memory_error)?;
    let attempt = authority
        .repair_artifact_attempt(
            operation_id.clone(),
            authorization.diagnosis_id,
            GuardianDomain::Runtime,
            ReconciliationComponent::Runtime,
            target.clone(),
            GuardianMode::Managed,
            Duration::minutes(DEFAULT_REPAIR_SUPPRESSION_MINUTES),
        )
        .map_err(reconciliation_evidence_error)?;
    let attempt_key = reconciliation_attempt_key(&attempt);
    let reservation = reserve_reconciliation_attempt(failure_memory, journals, attempt_key.clone())
        .map_err(|_| {
            OperationJournalStoreError::Persistence(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "managed-runtime reconciliation attempt is already active",
            ))
        })?;
    let terminal_context = RuntimeTerminalContext {
        authority: &authority,
        attempt: &attempt,
        reservation: &reservation,
        journals,
        failure_memory,
        diagnosis_id: &authorization.diagnosis_id,
        target: &target,
        terminal_failure,
    };
    if let Some(outcome) = recover_runtime_evidence(&terminal_context, &attempt_key, action).await?
    {
        return Ok(outcome);
    }
    if let Some(error) = create_planned_journal_reconciled(
        journals,
        &operation_id,
        &authorization.diagnosis_id,
        &target,
        &attempt,
    )
    .await?
    {
        finish_runtime_repair(
            &terminal_context,
            operation_id.clone(),
            RuntimeTerminal::Failed {
                step_id: RUNTIME_REPAIR_START_STEP,
                action,
                facts: Vec::new(),
                summary: "managed_runtime_repair_initialization_failed",
                suppression_until: Some(attempt.suppression_until().to_string()),
            },
        )
        .await?;
        return Err(error);
    }
    if abandoned.is_some_and(|abandoned| abandoned.load(Ordering::Acquire)) {
        terminalize_recovered_runtime_journal(journals, &operation_id, &target).await?;
        return Err(OperationJournalStoreError::Persistence(
            std::io::Error::other(
                "managed-runtime repair request ended before journal reconciliation",
            ),
        ));
    }
    if let Some(ready_for_effect) = ready_for_effect
        && ready_for_effect.send(()).is_err()
    {
        terminalize_recovered_runtime_journal(journals, &operation_id, &target).await?;
        return Err(OperationJournalStoreError::Persistence(
            std::io::Error::other("managed-runtime repair request ended before effect ownership"),
        ));
    }

    let _mutation = match authority.admit_managed_artifact_mutation() {
        Ok(mutation) => mutation,
        Err(_) => {
            return finish_runtime_repair(
                &terminal_context,
                operation_id,
                RuntimeTerminal::Failed {
                    step_id: READY_MARKER_REPAIR_STEP,
                    action,
                    facts: Vec::new(),
                    summary: "managed_runtime_mutation_epoch_exhausted",
                    suppression_until: Some(attempt.suppression_until().to_string()),
                },
            )
            .await;
        }
    };

    match repair_managed_runtime(
        ManagedRuntimeRepairRequest::new(target.clone(), runtime_root)
            .with_operation_id(operation_id.clone()),
    ) {
        Ok(report) => {
            let fact_ids = fact_ids(&report.facts);
            finish_runtime_repair(
                &terminal_context,
                operation_id,
                RuntimeTerminal::Repaired {
                    action,
                    facts: fact_ids,
                },
            )
            .await
        }
        Err(error) => {
            let fact_ids = fact_ids(&error.facts);
            finish_runtime_repair(
                &terminal_context,
                operation_id,
                RuntimeTerminal::Failed {
                    step_id: READY_MARKER_REPAIR_STEP,
                    action,
                    facts: fact_ids,
                    summary: "managed_runtime_ready_marker_repair_failed",
                    suppression_until: Some(attempt.suppression_until().to_string()),
                },
            )
            .await
        }
    }
}

async fn recover_runtime_evidence(
    context: &RuntimeTerminalContext<'_>,
    key: &crate::state::failure_memory::FailureMemoryKey,
    action: GuardianActionKind,
) -> Result<Option<GuardianRepairOutcome>, OperationJournalStoreError> {
    let now = chrono::Utc::now();
    let mut active_prior_terminal = None;
    for journal in context.journals.list() {
        let Some(attempt) = journal.reconciliation_attempt() else {
            continue;
        };
        if &reconciliation_attempt_key(attempt) != key
            || attempt.diagnosis_id() != *context.diagnosis_id
        {
            continue;
        }
        if let Some(terminal) = journal.reconciliation_terminal().cloned() {
            if journal.operation_id == *context.attempt.operation_id() {
                return reconcile_same_operation_runtime_terminal(
                    context, journal, terminal, action,
                )
                .await
                .map(Some);
            }
            let active = DateTime::parse_from_rfc3339(terminal.suppression_until())
                .is_ok_and(|until| until > now);
            if active
                && active_prior_terminal
                    .as_ref()
                    .is_none_or(|current: &ReconciliationTerminal| {
                        current.observed_at() < terminal.observed_at()
                    })
            {
                active_prior_terminal = Some(terminal);
            }
        }
    }
    let Some(active_prior_terminal) = active_prior_terminal else {
        return Ok(None);
    };
    let memory = reconciliation_memory_entry(active_prior_terminal)
        .map_err(|_| reconciliation_evidence_error(()))?;
    commit_reconciliation_memory(context.failure_memory, memory, context.reservation)
        .await
        .map_err(failure_memory_error)?;

    create_blocked_runtime_outcome(
        context.journals,
        context.attempt.operation_id().clone(),
        context.diagnosis_id,
        context.target,
        Some(action),
        "managed_runtime_repair_blocked_by_active_prior_attempt",
    )
    .await
    .map(Some)
}

async fn reconcile_same_operation_runtime_terminal(
    context: &RuntimeTerminalContext<'_>,
    journal: OperationJournalEntry,
    terminal: ReconciliationTerminal,
    action: GuardianActionKind,
) -> Result<GuardianRepairOutcome, OperationJournalStoreError> {
    let memory = reconciliation_memory_entry(terminal.clone())
        .map_err(|_| reconciliation_evidence_error(()))?;
    commit_reconciliation_memory(context.failure_memory, memory, context.reservation)
        .await
        .map_err(failure_memory_error)?;
    let (status, summary) = match terminal.outcome() {
        ReconciliationTerminalOutcome::Succeeded => (
            GuardianRepairStatus::Repaired,
            "managed_runtime_ready_marker_repaired",
        ),
        ReconciliationTerminalOutcome::Failed => (
            GuardianRepairStatus::Failed,
            match journal.failure_point.as_deref() {
                Some(RUNTIME_REPAIR_START_STEP) => "managed_runtime_repair_initialization_failed",
                _ => "managed_runtime_ready_marker_repair_failed",
            },
        ),
    };
    let facts = journal
        .completed_steps
        .last()
        .map(|step| step.generated_facts.clone())
        .unwrap_or_default();
    Ok(repair_outcome(
        journal.operation_id,
        Some(*context.diagnosis_id),
        Some(action),
        status,
        facts,
        summary,
    ))
}

async fn finish_runtime_repair(
    context: &RuntimeTerminalContext<'_>,
    operation_id: OperationId,
    terminal: RuntimeTerminal,
) -> Result<GuardianRepairOutcome, OperationJournalStoreError> {
    let (journal, action, status, facts, summary, suppression_until, reconciliation_outcome) =
        match terminal {
            RuntimeTerminal::Repaired { action, facts } => (
                RuntimeTerminalJournal::Record(
                    READY_MARKER_REPAIR_STEP,
                    OperationStepResult::Completed,
                    None,
                ),
                Some(action),
                GuardianRepairStatus::Repaired,
                facts,
                "managed_runtime_ready_marker_repaired",
                Some(context.attempt.suppression_until().to_string()),
                Some(ReconciliationTerminalOutcome::Succeeded),
            ),
            RuntimeTerminal::Failed {
                step_id,
                action,
                facts,
                summary,
                suppression_until,
            } => (
                RuntimeTerminalJournal::Record(step_id, OperationStepResult::Failed, Some(step_id)),
                Some(action),
                GuardianRepairStatus::Failed,
                facts,
                summary,
                suppression_until,
                Some(ReconciliationTerminalOutcome::Failed),
            ),
        };
    let reconciliation_terminal = match reconciliation_outcome {
        Some(outcome) => {
            let _suppression_until = suppression_until.as_deref().ok_or_else(|| {
                OperationJournalStoreError::Persistence(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "managed-runtime repair has no valid suppression window",
                ))
            })?;
            Some(
                context
                    .authority
                    .terminal(context.attempt.clone(), outcome)
                    .map_err(|_| {
                        OperationJournalStoreError::Persistence(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "managed-runtime repair terminal is invalid",
                        ))
                    })?,
            )
        }
        None => None,
    };
    let journal_operation_id = operation_id.clone();
    let journal_facts = facts.clone();
    let journal_terminal = reconciliation_terminal.clone();
    let complete_journal = async move {
        match journal {
            RuntimeTerminalJournal::Record(step_id, step_result, failure_point) => {
                record_reconciliation_terminal_reconciled(
                    context.journals,
                    &journal_operation_id,
                    repair_step(
                        step_id,
                        step_result,
                        Some(context.target.clone()),
                        journal_facts,
                    ),
                    failure_point,
                    journal_terminal
                        .as_ref()
                        .expect("attempted runtime repair has typed terminal"),
                    context.terminal_failure,
                )
                .await
            }
        }
    };
    if let Some(error) = complete_journal.await? {
        return Err(error);
    }
    if let Some(terminal) = reconciliation_terminal {
        let memory = reconciliation_memory_entry(terminal).map_err(|_| {
            OperationJournalStoreError::Persistence(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "managed-runtime repair memory terminal is invalid",
            ))
        })?;
        commit_reconciliation_memory(context.failure_memory, memory, context.reservation)
            .await
            .map_err(|error| {
                OperationJournalStoreError::Persistence(std::io::Error::other(format!(
                    "managed-runtime repair memory commit failed: {}",
                    error.class()
                )))
            })?;
    }
    Ok(repair_outcome(
        operation_id,
        Some(*context.diagnosis_id),
        action,
        status,
        facts,
        summary,
    ))
}

async fn create_blocked_runtime_outcome(
    journals: &OperationJournalStore,
    operation_id: OperationId,
    diagnosis_id: &DiagnosisId,
    target: &TargetDescriptor,
    action: Option<GuardianActionKind>,
    summary: &'static str,
) -> Result<GuardianRepairOutcome, OperationJournalStoreError> {
    if let Some(error) = create_terminal_journal_reconciled(
        journals,
        &operation_id,
        diagnosis_id,
        target,
        OperationStatus::Blocked,
        OperationOutcome::Blocked,
        OperationStepResult::Skipped,
        Vec::new(),
    )
    .await?
    {
        return Err(error);
    }
    Ok(repair_outcome(
        operation_id,
        Some(*diagnosis_id),
        action,
        GuardianRepairStatus::Blocked,
        Vec::new(),
        summary,
    ))
}

fn failure_memory_error(error: impl std::fmt::Display) -> OperationJournalStoreError {
    OperationJournalStoreError::Persistence(std::io::Error::other(format!(
        "managed-runtime reconciliation memory failed: {error}"
    )))
}

fn reconciliation_evidence_error(_error: impl std::fmt::Debug) -> OperationJournalStoreError {
    OperationJournalStoreError::Persistence(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        "managed-runtime reconciliation evidence is invalid",
    ))
}

fn planned_runtime_journal(
    operation_id: &OperationId,
    diagnosis_id: &DiagnosisId,
    target: &TargetDescriptor,
    attempt: &ReconciliationAttempt,
) -> OperationJournalEntry {
    let mut entry = OperationJournalEntry::new(
        JournalId::new(format!("journal-{}", operation_id.as_str())),
        operation_id.clone(),
        CommandKind::RepairInstance,
        StabilizationSystem::Guardian,
        target.ownership,
        RollbackState::NotApplicable,
    );
    entry.targets.push(target.clone());
    let ReconciliationScope::RegisteredInstance { instance_id, .. } = attempt.scope();
    entry
        .targets
        .push(reconciliation_instance_target(instance_id));
    entry.planned_steps.push(repair_step(
        RUNTIME_REPAIR_START_STEP,
        OperationStepResult::Planned,
        Some(target.clone()),
        Vec::new(),
    ));
    entry.planned_steps.push(repair_step(
        READY_MARKER_REPAIR_STEP,
        OperationStepResult::Planned,
        Some(target.clone()),
        Vec::new(),
    ));
    entry.guardian_diagnosis_ids.push(*diagnosis_id);
    reconciliation_journal_attempt(entry, attempt.clone())
}

async fn create_planned_journal_reconciled(
    journals: &OperationJournalStore,
    operation_id: &OperationId,
    diagnosis_id: &DiagnosisId,
    target: &TargetDescriptor,
    attempt: &ReconciliationAttempt,
) -> Result<Option<OperationJournalStoreError>, OperationJournalStoreError> {
    let expected = planned_runtime_journal(operation_id, diagnosis_id, target, attempt);
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
                match reconcile_guardian_journal_error(journals, operation_id, error, |entry| {
                    operation_journal_plan_is_visible(entry, &expected)
                })
                .await?
                {
                    GuardianJournalReconciliation::MutationCommitted => return Ok(None),
                    GuardianJournalReconciliation::AcceptedFailure(error) => {
                        return Ok(Some(error));
                    }
                    GuardianJournalReconciliation::RetryMutation => {}
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn create_terminal_journal_reconciled(
    journals: &OperationJournalStore,
    operation_id: &OperationId,
    diagnosis_id: &DiagnosisId,
    target: &TargetDescriptor,
    status: OperationStatus,
    outcome: OperationOutcome,
    step_result: OperationStepResult,
    facts: Vec<String>,
) -> Result<Option<OperationJournalStoreError>, OperationJournalStoreError> {
    let expected = terminal_runtime_journal(
        operation_id,
        diagnosis_id,
        target,
        status,
        outcome,
        step_result,
        facts,
    );
    loop {
        match journals.create(expected.clone()).await {
            Ok(()) => return Ok(None),
            Err(OperationJournalStoreError::AlreadyExists)
                if journals.get(operation_id).is_some_and(|entry| {
                    operation_journal_terminal_is_visible(&entry, &expected)
                }) =>
            {
                return Ok(None);
            }
            Err(error) => {
                match reconcile_guardian_journal_error(journals, operation_id, error, |entry| {
                    operation_journal_terminal_is_visible(entry, &expected)
                })
                .await?
                {
                    GuardianJournalReconciliation::MutationCommitted => return Ok(None),
                    GuardianJournalReconciliation::AcceptedFailure(error) => {
                        return Ok(Some(error));
                    }
                    GuardianJournalReconciliation::RetryMutation => {}
                }
            }
        }
    }
}

async fn terminalize_recovered_runtime_journal(
    journals: &OperationJournalStore,
    operation_id: &OperationId,
    target: &TargetDescriptor,
) -> Result<(), OperationJournalStoreError> {
    let step = repair_step(
        READY_MARKER_REPAIR_STEP,
        OperationStepResult::Skipped,
        Some(target.clone()),
        Vec::new(),
    );
    loop {
        match record_guardian_repair_refusal(journals, operation_id, step.clone()).await {
            Ok(()) => return Ok(()),
            Err(OperationJournalStoreError::AlreadyTerminal)
                if journals.get(operation_id).is_some_and(|entry| {
                    runtime_refusal_transition_matches(&entry, operation_id, &step)
                }) =>
            {
                return Ok(());
            }
            Err(error) => {
                match reconcile_guardian_journal_error(journals, operation_id, error, |entry| {
                    runtime_refusal_transition_matches(entry, operation_id, &step)
                })
                .await?
                {
                    GuardianJournalReconciliation::MutationCommitted => return Ok(()),
                    GuardianJournalReconciliation::AcceptedFailure(_) => return Ok(()),
                    GuardianJournalReconciliation::RetryMutation => {}
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
#[cfg(test)]
async fn create_terminal_journal(
    journals: &OperationJournalStore,
    operation_id: &OperationId,
    diagnosis_id: &DiagnosisId,
    target: &TargetDescriptor,
    status: OperationStatus,
    outcome: OperationOutcome,
    step_result: OperationStepResult,
    facts: Vec<String>,
) -> Result<(), OperationJournalStoreError> {
    journals
        .create(terminal_runtime_journal(
            operation_id,
            diagnosis_id,
            target,
            status,
            outcome,
            step_result,
            facts,
        ))
        .await
}

#[allow(clippy::too_many_arguments)]
fn terminal_runtime_journal(
    operation_id: &OperationId,
    diagnosis_id: &DiagnosisId,
    target: &TargetDescriptor,
    status: OperationStatus,
    outcome: OperationOutcome,
    step_result: OperationStepResult,
    facts: Vec<String>,
) -> OperationJournalEntry {
    let mut entry = OperationJournalEntry::new(
        JournalId::new(format!("journal-{}", operation_id.as_str())),
        operation_id.clone(),
        CommandKind::RepairInstance,
        StabilizationSystem::Guardian,
        target.ownership,
        RollbackState::NotApplicable,
    );
    entry.status = status;
    entry.targets.push(target.clone());
    entry.planned_steps.push(repair_step(
        READY_MARKER_REPAIR_STEP,
        OperationStepResult::Planned,
        Some(target.clone()),
        Vec::new(),
    ));
    entry.completed_steps.push(repair_step(
        READY_MARKER_REPAIR_STEP,
        step_result,
        Some(target.clone()),
        facts,
    ));
    entry.guardian_diagnosis_ids.push(*diagnosis_id);
    entry.outcome = Some(outcome);
    entry
}

fn runtime_refusal_transition_matches(
    entry: &OperationJournalEntry,
    operation_id: &OperationId,
    step: &OperationJournalStep,
) -> bool {
    entry.operation_id == *operation_id
        && entry.command == CommandKind::RepairInstance
        && entry.owner == StabilizationSystem::Guardian
        && entry.status == OperationStatus::Blocked
        && entry.outcome == Some(OperationOutcome::Blocked)
        && entry.failure_point.is_none()
        && entry.reconciliation_terminal().is_none()
        && operation_journal_completed_step_is_visible(entry, step)
}

fn fact_ids(facts: &[ExecutionFact]) -> Vec<String> {
    facts
        .iter()
        .map(|fact| format!("{:?}", fact.kind))
        .map(|fact| safe_id(&fact, "execution_fact"))
        .collect()
}

fn repair_outcome(
    operation_id: OperationId,
    diagnosis_id: Option<DiagnosisId>,
    action: Option<GuardianActionKind>,
    status: GuardianRepairStatus,
    facts: Vec<String>,
    summary: &str,
) -> GuardianRepairOutcome {
    GuardianRepairOutcome {
        operation_id,
        diagnosis_id,
        action,
        status,
        facts,
        summary: safe_id(summary, "guardian_repair_outcome"),
    }
}

fn public_safe_target(target: &TargetDescriptor) -> TargetDescriptor {
    TargetDescriptor::new(
        target.system,
        target.kind,
        target.id.as_str(),
        target.ownership,
    )
}

fn ready_marker_repair_target_supported(target: &TargetDescriptor) -> bool {
    target.system == StabilizationSystem::Execution
        && target.kind == TargetKind::Runtime
        && target.ownership == OwnershipClass::LauncherManaged
}

fn safe_operation_id(operation_id: &OperationId) -> OperationId {
    OperationId::new(safe_id(operation_id.as_str(), "operation"))
}

fn safe_id(value: &str, fallback: &str) -> String {
    sanitize_evidence_token(value, RedactionAudience::UserVisible, 96)
        .unwrap_or_else(|| fallback.to_string())
}

fn new_repair_operation_id() -> OperationId {
    OperationId::new(format!("guardian-repair-{}", uuid::Uuid::new_v4()))
}

#[cfg(test)]
mod tests {
    use super::{
        GuardianRepairOutcome, GuardianRepairStatus, READY_MARKER_REPAIR_STEP,
        RUNTIME_REPAIR_START_STEP, execute_managed_runtime_ready_marker_repair,
    };
    use crate::execution::persistence::{AtomicWriteBackend, PersistenceCoordinator};
    use crate::execution::runtime::{ManagedRuntimeRoot, ManagedRuntimeRootError};
    use crate::guardian::{
        ActionPlanPrerequisite, DiagnosisId, GuardianAction, GuardianActionKind,
        GuardianActionPlan, GuardianDecision, GuardianMode, RepairAuthorizationRejection,
        authorize_managed_runtime_ready_marker_repair,
    };
    use crate::state::contracts::{
        OperationId, OperationOutcome, OperationStatus, OperationStepResult, OwnershipClass,
        ReconciliationTerminalOutcome, StabilizationSystem, TargetDescriptor, TargetKind,
    };
    use crate::state::failure_memory::{
        FailureMemoryActionOutcome, FailureMemorySnapshot, GuardianFailureMemoryStore,
    };
    use crate::state::{
        AppState, AppStateInit, InstallStore, OperationJournalStore, SessionStore, new_instance,
        reconciliation_attempt_key,
    };
    use axial_config::{AppPaths, InstanceRegistrySnapshot};
    use axial_minecraft::ManagedRuntimeCache;
    use axial_minecraft::known_good::{KnownGoodInventory, TestKnownGoodEntry};
    use sha1::{Digest, Sha1};
    use std::fs;
    use std::io;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    trait ExpectErrorWithoutDebug<E> {
        fn expect_error_without_debug(self, message: &str) -> E;
    }

    impl<T, E> ExpectErrorWithoutDebug<E> for Result<T, E> {
        fn expect_error_without_debug(self, message: &str) -> E {
            match self {
                Ok(_) => panic!("{message}"),
                Err(error) => error,
            }
        }
    }

    struct FailingWriteBackend {
        fail_next: AtomicBool,
    }

    #[derive(Default)]
    struct ControlledWriteBackend {
        attempts: AtomicUsize,
        fail_all: AtomicBool,
        gate_attempt: AtomicUsize,
        release_gate: AtomicBool,
    }

    impl ControlledWriteBackend {
        fn gate_attempt(&self, attempt: usize) {
            self.gate_attempt.store(attempt, Ordering::SeqCst);
            self.release_gate.store(false, Ordering::SeqCst);
        }

        fn release(&self) {
            self.release_gate.store(true, Ordering::SeqCst);
        }

        async fn wait_for_attempt(&self, expected: usize) {
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                while self.attempts.load(Ordering::SeqCst) < expected {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("runtime persistence attempt");
        }
    }

    impl AtomicWriteBackend for ControlledWriteBackend {
        fn write(
            &self,
            target: &TargetDescriptor,
            destination: &Path,
            contents: &[u8],
        ) -> io::Result<()> {
            let attempt = self.attempts.fetch_add(1, Ordering::SeqCst) + 1;
            if self.gate_attempt.load(Ordering::SeqCst) == attempt {
                while !self.release_gate.load(Ordering::SeqCst) {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
            }
            if self.fail_all.load(Ordering::SeqCst) {
                return Err(io::Error::other(
                    "injected persistent managed-runtime journal failure",
                ));
            }
            crate::execution::file::write_file_atomically(
                crate::execution::file::FileWriteRequest::new(
                    target.clone(),
                    destination,
                    contents,
                ),
            )
            .map(|_| ())
            .map_err(io::Error::from)
        }
    }

    impl AtomicWriteBackend for FailingWriteBackend {
        fn write(
            &self,
            target: &TargetDescriptor,
            destination: &Path,
            contents: &[u8],
        ) -> io::Result<()> {
            if self.fail_next.swap(false, Ordering::SeqCst) {
                return Err(io::Error::other("injected managed-runtime journal failure"));
            }
            crate::execution::file::write_file_atomically(
                crate::execution::file::FileWriteRequest::new(
                    target.clone(),
                    destination,
                    contents,
                ),
            )
            .map(|_| ())
            .map_err(io::Error::from)
        }
    }

    #[test]
    fn managed_runtime_ready_marker_repair_records_journal_and_memory() {
        let root = test_root("success");
        let stores = stores();
        let runtime_root = managed_runtime_root(&stores, "java-runtime-delta");
        let java_executable = write_fake_java(&runtime_root);
        write_runtime_manifest_proof(&runtime_root, &java_executable);
        let decision = repair_decision(OwnershipClass::LauncherManaged);

        let outcome = execute_repair(&decision, &runtime_root, &java_executable, &stores);

        assert_eq!(outcome.status, GuardianRepairStatus::Repaired);
        assert!(runtime_root.join(".axial-ready").is_file());
        assert!(
            outcome
                .facts
                .iter()
                .any(|fact| fact == "RuntimeRepairApplied")
        );

        let journal = stores
            .journals
            .get(&outcome.operation_id)
            .expect("journal entry");
        assert_eq!(journal.status, OperationStatus::Succeeded);
        assert_eq!(journal.outcome, Some(OperationOutcome::Succeeded));
        assert_eq!(journal.planned_steps.len(), 2);
        assert_eq!(journal.planned_steps[0].step_id, RUNTIME_REPAIR_START_STEP);
        assert_eq!(journal.planned_steps[1].step_id, READY_MARKER_REPAIR_STEP);
        assert_eq!(journal.completed_steps.len(), 1);
        assert_eq!(journal.completed_steps[0].generated_facts, outcome.facts);

        let memory = stores.failure_memory.list();
        assert_eq!(memory.len(), 1);
        assert_eq!(memory[0].last_action_kind, Some(GuardianActionKind::Repair));
        assert_eq!(
            memory[0].last_action_outcome,
            Some(FailureMemoryActionOutcome::Repaired)
        );
        assert_eq!(memory[0].repair_attempt_count, 1);
        cleanup(&root);
    }

    #[test]
    fn renewed_missing_marker_is_blocked_by_active_successful_prior_attempt() {
        let root = test_root("active-success-blocks-renewed-repair");
        let stores = stores();
        let runtime_root = managed_runtime_root(&stores, "java-runtime-delta");
        let java_executable = write_fake_java(&runtime_root);
        write_runtime_manifest_proof(&runtime_root, &java_executable);
        let prior_decision = repair_decision(OwnershipClass::LauncherManaged);
        let prior = execute_repair(&prior_decision, &runtime_root, &java_executable, &stores);
        assert_eq!(prior.status, GuardianRepairStatus::Repaired);
        fs::remove_file(runtime_root.join(".axial-ready")).expect("renew missing ready marker");
        let prior_journals = stores.journals.list();
        let prior_memory = stores.failure_memory.list();
        let replayed = execute_repair(&prior_decision, &runtime_root, &java_executable, &stores);
        assert_eq!(replayed, prior);
        assert!(!runtime_root.join(".axial-ready").exists());
        assert_eq!(stores.journals.list(), prior_journals);
        assert_eq!(stores.failure_memory.list(), prior_memory);
        stores
            .failure_memory
            .load_snapshot(FailureMemorySnapshot::new(Vec::new()).expect("empty memory snapshot"))
            .expect("simulate missing terminal memory");
        assert!(stores.failure_memory.list().is_empty());
        let renewed_decision = repair_decision_with_operation_id(
            "operation-renewed-after-success",
            OwnershipClass::LauncherManaged,
        );

        let blocked = execute_repair(&renewed_decision, &runtime_root, &java_executable, &stores);

        assert_eq!(
            blocked.operation_id,
            OperationId::new("operation-renewed-after-success")
        );
        assert_eq!(blocked.status, GuardianRepairStatus::Blocked);
        assert_eq!(
            blocked.summary,
            "managed_runtime_repair_blocked_by_active_prior_attempt"
        );
        assert!(!runtime_root.join(".axial-ready").exists());
        assert_eq!(stores.journals.list().len(), prior_journals.len() + 1);
        for prior_journal in prior_journals {
            assert_eq!(
                stores.journals.get(&prior_journal.operation_id),
                Some(prior_journal)
            );
        }
        assert_blocked_runtime_refusal(
            &stores,
            &OperationId::new("operation-renewed-after-success"),
        );
        assert_eq!(stores.failure_memory.list(), prior_memory);
        cleanup(&root);
    }

    #[test]
    fn active_failed_prior_attempt_blocks_a_different_operation() {
        let root = test_root("active-failure-blocks-renewed-repair");
        let stores = stores();
        let runtime_root = managed_runtime_root(&stores, "java-runtime-delta");
        let java_executable = runtime_root.join("bin").join("java");
        fs::create_dir_all(runtime_root.parent().expect("runtime parent")).expect("test root");
        fs::write(&runtime_root, b"failed-runtime-sentinel").expect("invalid runtime root");
        let prior_decision = repair_decision(OwnershipClass::LauncherManaged);
        let prior = execute_repair(&prior_decision, &runtime_root, &java_executable, &stores);
        assert_eq!(prior.status, GuardianRepairStatus::Failed);
        let prior_journals = stores.journals.list();
        let prior_memory = stores.failure_memory.list();
        let replayed = execute_repair(&prior_decision, &runtime_root, &java_executable, &stores);
        assert_eq!(replayed, prior);
        assert_eq!(
            fs::read(&runtime_root).expect("invalid runtime root remains a file"),
            b"failed-runtime-sentinel"
        );
        assert_eq!(stores.journals.list(), prior_journals);
        assert_eq!(stores.failure_memory.list(), prior_memory);
        let renewed_decision = repair_decision_with_operation_id(
            "operation-renewed-after-failure",
            OwnershipClass::LauncherManaged,
        );

        let blocked = execute_repair(&renewed_decision, &runtime_root, &java_executable, &stores);

        assert_eq!(
            blocked.operation_id,
            OperationId::new("operation-renewed-after-failure")
        );
        assert_eq!(blocked.status, GuardianRepairStatus::Blocked);
        assert_eq!(
            fs::read(&runtime_root).expect("invalid runtime root remains a file"),
            b"failed-runtime-sentinel"
        );
        assert_eq!(stores.journals.list().len(), prior_journals.len() + 1);
        for prior_journal in prior_journals {
            assert_eq!(
                stores.journals.get(&prior_journal.operation_id),
                Some(prior_journal)
            );
        }
        assert_blocked_runtime_refusal(
            &stores,
            &OperationId::new("operation-renewed-after-failure"),
        );
        assert_eq!(stores.failure_memory.list(), prior_memory);
        cleanup(&root);
    }

    #[tokio::test]
    async fn planned_commit_failure_prevents_managed_runtime_mutation() {
        let root = test_root("planned-commit-gate");
        let paths = test_paths(&root);
        let stores = persistent_stores(&paths);
        let runtime_root = managed_runtime_root(&stores, "java-runtime-delta");
        let java_executable = write_fake_java(&runtime_root);
        write_runtime_manifest_proof(&runtime_root, &java_executable);
        let decision = repair_decision(OwnershipClass::LauncherManaged);
        let result =
            execute_repair_async(&decision, &runtime_root, &java_executable, &stores, None).await;

        assert!(result.is_err());
        assert!(!runtime_root.join(".axial-ready").exists());
        let operation_id = decision.operation_id().expect("repair operation id");
        let journal = stores
            .journals
            .get(operation_id)
            .expect("terminalized repair journal");
        assert_eq!(journal.status, OperationStatus::Failed);
        assert_eq!(journal.outcome, Some(OperationOutcome::Failed));
        assert_eq!(
            journal.failure_point.as_deref(),
            Some(RUNTIME_REPAIR_START_STEP)
        );
        assert_eq!(
            journal
                .completed_steps
                .last()
                .expect("initialization failure step")
                .step_id,
            RUNTIME_REPAIR_START_STEP
        );
        assert_eq!(
            journal
                .reconciliation_terminal()
                .expect("typed initialization terminal")
                .outcome(),
            ReconciliationTerminalOutcome::Failed
        );
        let memory = stores.failure_memory.list();
        assert_eq!(memory.len(), 1);
        assert_eq!(
            memory[0].last_action_outcome,
            Some(FailureMemoryActionOutcome::Failed)
        );
        assert!(!stores.journals.has_retry_candidate());
        cleanup(&root);
    }

    #[tokio::test]
    async fn terminal_commit_failure_signals_bounded_owner_and_retries_without_repair_replay() {
        let root = test_root("terminal-commit-retry");
        let paths = test_paths(&root);
        let backend = Arc::new(ControlledWriteBackend::default());
        backend.gate_attempt(2);
        let coordinator = PersistenceCoordinator::for_test(
            backend.clone(),
            std::time::Duration::from_millis(1),
            std::time::Duration::from_millis(5),
        );
        let stores = stores_with(
            OperationJournalStore::try_load_from_paths_with_coordinator(&paths, coordinator)
                .expect("claim runtime journal persistence"),
            GuardianFailureMemoryStore::new(),
        );
        let runtime_root = managed_runtime_root(&stores, "java-runtime-delta");
        let java_executable = write_fake_java(&runtime_root);
        write_runtime_manifest_proof(&runtime_root, &java_executable);
        let decision = repair_decision(OwnershipClass::LauncherManaged);
        let terminal_failure = tokio::sync::Notify::new();
        let repair = execute_repair_async(
            &decision,
            &runtime_root,
            &java_executable,
            &stores,
            Some(&terminal_failure),
        );
        let control = async {
            backend.wait_for_attempt(2).await;
            backend.fail_all.store(true, Ordering::SeqCst);
            backend.release();
            tokio::time::timeout(
                std::time::Duration::from_secs(2),
                terminal_failure.notified(),
            )
            .await
            .expect("bounded terminal persistence signal");
            let marker = runtime_root.join(".axial-ready");
            assert!(marker.is_file());
            fs::write(&marker, b"sentinel-after-effect").expect("write replay sentinel");
            backend.fail_all.store(false, Ordering::SeqCst);
        };
        let (result, ()) = tokio::join!(repair, control);

        assert!(result.is_err());
        assert_eq!(
            fs::read(runtime_root.join(".axial-ready")).expect("ready marker"),
            b"sentinel-after-effect"
        );
        let operation_id = decision.operation_id().expect("repair operation id");
        let journal = stores
            .journals
            .get(operation_id)
            .expect("reconciled terminal journal");
        assert_eq!(journal.status, OperationStatus::Succeeded);
        assert_eq!(journal.outcome, Some(OperationOutcome::Succeeded));
        assert!(!stores.journals.has_retry_candidate());
        assert!(stores.failure_memory.list().is_empty());

        let later_operation_id = OperationId::new("managed-runtime-later-mutation");
        super::create_terminal_journal(
            &stores.journals,
            &later_operation_id,
            &DiagnosisId::ManagedRuntimeCorrupt,
            &decision_target(&decision),
            OperationStatus::Blocked,
            OperationOutcome::Blocked,
            OperationStepResult::Skipped,
            Vec::new(),
        )
        .await
        .expect("later journal mutation");
        assert!(stores.journals.get(&later_operation_id).is_some());
        cleanup(&root);
    }

    #[test]
    fn user_owned_and_unknown_runtime_repairs_are_rejected() {
        for ownership in [OwnershipClass::UserOwned, OwnershipClass::Unknown] {
            let root = test_root("ownership-rejected");
            let stores = stores();
            let runtime_root = managed_runtime_root(&stores, "java-runtime-delta");
            let decision = repair_decision(ownership);

            let error = authorize_managed_runtime_ready_marker_repair(&decision)
                .expect_error_without_debug("unsafe ownership rejects before execution");

            assert_eq!(error, RepairAuthorizationRejection::UnsafeOwnership);
            assert!(!runtime_root.join(".axial-ready").exists());
            cleanup(&root);
        }
    }

    #[test]
    fn unsupported_runtime_repair_target_is_blocked_before_execution() {
        let root = test_root("unsupported-target");
        let stores = stores();
        let runtime_root = managed_runtime_root(&stores, "java-runtime-delta");
        let decision = repair_decision_for_target(TargetDescriptor::new(
            StabilizationSystem::Guardian,
            TargetKind::Runtime,
            "java-runtime-delta",
            OwnershipClass::LauncherManaged,
        ));

        let error = authorize_managed_runtime_ready_marker_repair(&decision)
            .expect_error_without_debug("unsupported target rejects before execution");

        assert_eq!(error, RepairAuthorizationRejection::UnsupportedDiagnosis);
        assert!(!runtime_root.join(".axial-ready").exists());
        cleanup(&root);
    }

    #[test]
    fn runtime_root_target_must_match_owned_repair_target() {
        let root = test_root("root-target-mismatch");
        let stores = stores();
        let runtime_root = managed_runtime_root(&stores, "java-runtime-epsilon");
        let java_executable = write_fake_java(&runtime_root);
        let decision = repair_decision(OwnershipClass::LauncherManaged);

        let outcome = execute_repair(&decision, &runtime_root, &java_executable, &stores);

        assert_eq!(outcome.status, GuardianRepairStatus::Blocked);
        assert!(!runtime_root.join(".axial-ready").exists());
        assert!(stores.failure_memory.list().is_empty());

        cleanup(&root);
    }

    #[test]
    fn arbitrary_runtime_root_cannot_enter_authorized_repair() {
        let root = test_root("root-binding");
        let runtime_cache = managed_runtime_cache();
        let runtime_root = root.join("user-runtime");
        let java_executable = runtime_root.join("bin").join("java");

        assert_eq!(
            ManagedRuntimeRoot::from_managed_root(&runtime_cache, &runtime_root, &java_executable)
                .expect_error_without_debug("outside runtime root"),
            ManagedRuntimeRootError::UnsupportedRoot
        );
        cleanup(&root);
    }

    #[test]
    fn malformed_or_non_repair_policy_is_blocked_before_execution() {
        let root = test_root("malformed-policy");
        let stores = stores();
        let runtime_root = managed_runtime_root(&stores, "java-runtime-delta");
        write_fake_java(&runtime_root);
        for decision in [
            {
                let decision = repair_decision(OwnershipClass::LauncherManaged);
                GuardianDecision::for_test(
                    decision.operation_id().cloned(),
                    decision.mode(),
                    GuardianActionKind::Block,
                    decision.diagnoses().to_vec(),
                    decision.action_plan().cloned(),
                )
            },
            {
                let decision = repair_decision(OwnershipClass::LauncherManaged);
                GuardianDecision::for_test(
                    decision.operation_id().cloned(),
                    GuardianMode::Disabled,
                    decision.kind(),
                    decision.diagnoses().to_vec(),
                    decision.action_plan().cloned(),
                )
            },
            {
                let decision = repair_decision(OwnershipClass::LauncherManaged);
                let mut action_plan = decision.action_plan().cloned().expect("plan");
                action_plan.prerequisite.confidence = crate::guardian::GuardianConfidence::High;
                GuardianDecision::for_test(
                    decision.operation_id().cloned(),
                    decision.mode(),
                    decision.kind(),
                    decision.diagnoses().to_vec(),
                    Some(action_plan),
                )
            },
            {
                let decision = repair_decision(OwnershipClass::LauncherManaged);
                let mut action_plan = decision.action_plan().cloned().expect("plan");
                action_plan.actions[0].reason = DiagnosisId::DownloadUnavailable;
                GuardianDecision::for_test(
                    decision.operation_id().cloned(),
                    decision.mode(),
                    decision.kind(),
                    decision.diagnoses().to_vec(),
                    Some(action_plan),
                )
            },
        ] {
            let _ = fs::remove_file(runtime_root.join(".axial-ready"));
            let error = authorize_managed_runtime_ready_marker_repair(&decision)
                .expect_error_without_debug("malformed policy rejects before execution");

            assert!(matches!(
                error,
                RepairAuthorizationRejection::NonRepairDecision
                    | RepairAuthorizationRejection::UnsupportedDiagnosis
            ));
            assert!(!runtime_root.join(".axial-ready").exists());
        }

        cleanup(&root);
    }

    #[test]
    fn post_repair_verification_failure_is_not_reported_as_repaired() {
        let root = test_root("postcondition-failure");
        let stores = stores();
        let runtime_root = managed_runtime_root(&stores, "java-runtime-delta");
        let java_executable = runtime_root.join("bin").join("java");
        let decision = repair_decision(OwnershipClass::LauncherManaged);

        let outcome = execute_repair(&decision, &runtime_root, &java_executable, &stores);

        assert_eq!(outcome.status, GuardianRepairStatus::Failed);
        assert!(!runtime_root.join(".axial-ready").exists());
        assert!(
            !outcome
                .facts
                .iter()
                .any(|fact| fact == "RuntimeRepairApplied")
        );
        assert!(outcome.facts.iter().any(|fact| fact == "RuntimeCorrupt"));
        let journal = stores
            .journals
            .get(&outcome.operation_id)
            .expect("journal entry");
        assert_eq!(journal.status, OperationStatus::Failed);
        assert_eq!(journal.outcome, Some(OperationOutcome::Failed));
        cleanup(&root);
    }

    #[test]
    fn public_repair_outcome_preserves_sanitized_journal_authority() {
        let root = test_root("safe-outcome-ids");
        let stores = stores();
        let runtime_root = managed_runtime_root(&stores, "java-runtime-delta");
        let java_executable = write_fake_java(&runtime_root);
        write_runtime_manifest_proof(&runtime_root, &java_executable);
        let decision = repair_decision(OwnershipClass::LauncherManaged);
        let hostile_operation_id = format!(
            "/home/alice/token/operation\n{}",
            "secret0123456789".repeat(12)
        );
        let decision = GuardianDecision::for_test(
            Some(OperationId::new(hostile_operation_id)),
            decision.mode(),
            decision.kind(),
            decision.diagnoses().to_vec(),
            decision.action_plan().cloned(),
        );

        let outcome = execute_repair(&decision, &runtime_root, &java_executable, &stores);
        let encoded = serde_json::to_string(&outcome).expect("outcome json");
        let lower = encoded.to_ascii_lowercase();

        assert_eq!(outcome.status, GuardianRepairStatus::Repaired);
        let journals = stores.journals.list();
        assert_eq!(journals.len(), 1);
        let journal = &journals[0];
        let terminal = journal
            .reconciliation_terminal()
            .expect("typed repair terminal");
        assert_eq!(outcome.operation_id, journal.operation_id);
        assert_eq!(&outcome.operation_id, terminal.operation_id());
        assert!(outcome.operation_id.as_str().chars().count() <= 96);
        assert!(outcome.operation_id.as_str().chars().all(|value| {
            value.is_ascii_alphanumeric() || matches!(value, '-' | '_' | '.' | '+' | ':')
        }));
        assert!(!lower.contains("/home"));
        assert!(!lower.contains("alice"));
        assert!(!lower.contains("token"));
        cleanup(&root);
    }

    #[test]
    fn execution_failure_records_failed_outcome_and_active_window() {
        let root = test_root("failure");
        let stores = stores();
        let runtime_root = managed_runtime_root(&stores, "java-runtime-delta");
        let java_executable = runtime_root.join("bin").join("java");
        fs::create_dir_all(runtime_root.parent().expect("runtime parent")).expect("test root");
        fs::write(&runtime_root, b"not a directory").expect("runtime root file");
        let decision = repair_decision(OwnershipClass::LauncherManaged);

        let outcome = execute_repair(&decision, &runtime_root, &java_executable, &stores);

        assert_eq!(outcome.status, GuardianRepairStatus::Failed);
        let journal = stores
            .journals
            .get(&outcome.operation_id)
            .expect("journal entry");
        assert_eq!(journal.status, OperationStatus::Failed);
        assert_eq!(journal.outcome, Some(OperationOutcome::Failed));
        let terminal = journal
            .reconciliation_terminal()
            .expect("typed terminal journal");
        let key = reconciliation_attempt_key(terminal.attempt());
        let memory = stores.failure_memory.get(&key).expect("memory entry");
        assert_eq!(
            memory.last_action_outcome,
            Some(FailureMemoryActionOutcome::Failed)
        );
        assert_eq!(memory.occurrence_count, 1);
        assert_eq!(memory.repair_attempt_count, 1);
        assert_eq!(
            memory.suppression_until.as_deref(),
            Some(terminal.suppression_until())
        );
        cleanup(&root);
    }

    fn execute_repair(
        decision: &GuardianDecision,
        runtime_root: &Path,
        java_executable: &Path,
        stores: &Stores,
    ) -> GuardianRepairOutcome {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime")
            .block_on(execute_repair_async(
                decision,
                runtime_root,
                java_executable,
                stores,
                None,
            ))
            .expect("persist managed-runtime repair journal")
    }

    async fn execute_repair_async(
        decision: &GuardianDecision,
        runtime_root: &Path,
        java_executable: &Path,
        stores: &Stores,
        terminal_failure: Option<&tokio::sync::Notify>,
    ) -> Result<GuardianRepairOutcome, crate::state::OperationJournalStoreError> {
        let authorization = authorize_managed_runtime_ready_marker_repair(decision)
            .expect("runtime repair authorization");
        let lifecycle = stores
            .state
            .acquire_instance_lifecycle(TEST_INSTANCE_ID)
            .await;
        let authority = stores
            .state
            .registered_reconciliation_authority(&lifecycle)
            .expect("registered reconciliation authority");
        execute_managed_runtime_ready_marker_repair(
            authorization,
            decision.operation_id().cloned(),
            authority,
            runtime_root_binding(
                stores.state.managed_runtime_cache(),
                runtime_root,
                java_executable,
            ),
            None,
            None,
            terminal_failure,
        )
        .await
    }

    fn assert_blocked_runtime_refusal(stores: &Stores, operation_id: &OperationId) {
        let journal = stores
            .journals
            .get(operation_id)
            .expect("current blocked runtime repair journal");
        assert_eq!(journal.status, OperationStatus::Blocked);
        assert_eq!(journal.outcome, Some(OperationOutcome::Blocked));
        assert!(journal.reconciliation_attempt().is_none());
        assert!(journal.reconciliation_terminal().is_none());
    }

    fn decision_target(decision: &GuardianDecision) -> TargetDescriptor {
        decision
            .action_plan()
            .expect("plan")
            .prerequisite
            .affected_targets[0]
            .clone()
    }

    fn repair_decision(ownership: OwnershipClass) -> GuardianDecision {
        repair_decision_with_operation_id(&format!("operation-{ownership:?}"), ownership)
    }

    fn repair_decision_with_operation_id(
        operation_id: &str,
        ownership: OwnershipClass,
    ) -> GuardianDecision {
        repair_decision_for_target_and_operation(
            TargetDescriptor::new(
                StabilizationSystem::Execution,
                TargetKind::Runtime,
                "java-runtime-delta",
                ownership,
            ),
            OperationId::new(operation_id),
        )
    }

    fn repair_decision_for_target(target: TargetDescriptor) -> GuardianDecision {
        let operation_id = OperationId::new(format!("operation-{:?}", target.ownership));
        repair_decision_for_target_and_operation(target, operation_id)
    }

    fn repair_decision_for_target_and_operation(
        target: TargetDescriptor,
        operation_id: OperationId,
    ) -> GuardianDecision {
        let ownership = target.ownership;
        let diagnosis_id = DiagnosisId::ManagedRuntimeCorrupt;
        let prerequisite = ActionPlanPrerequisite {
            diagnosis_id,
            ownership,
            confidence: crate::guardian::GuardianConfidence::Confirmed,
            affected_targets: vec![target.clone()],
            candidate_actions: vec![GuardianActionKind::Repair],
        };
        GuardianDecision::for_test(
            Some(operation_id),
            GuardianMode::Managed,
            GuardianActionKind::Repair,
            vec![diagnosis_id],
            Some(GuardianActionPlan::new(
                StabilizationSystem::Guardian,
                prerequisite,
                vec![GuardianAction {
                    kind: GuardianActionKind::Repair,
                    target: Some(target),
                    reason: diagnosis_id,
                }],
            )),
        )
    }

    const TEST_INSTANCE_ID: &str = "0000000000000001";

    struct Stores {
        state: AppState,
        journals: Arc<OperationJournalStore>,
        failure_memory: Arc<GuardianFailureMemoryStore>,
        fixture_root: PathBuf,
    }

    impl Drop for Stores {
        fn drop(&mut self) {
            cleanup(&self.fixture_root);
        }
    }

    fn stores() -> Stores {
        stores_with(
            OperationJournalStore::new(),
            GuardianFailureMemoryStore::new(),
        )
    }

    fn persistent_stores(paths: &AppPaths) -> Stores {
        let coordinator = PersistenceCoordinator::for_test(
            Arc::new(FailingWriteBackend {
                fail_next: AtomicBool::new(true),
            }),
            std::time::Duration::from_millis(20),
            std::time::Duration::from_millis(100),
        );
        stores_with(
            OperationJournalStore::try_load_from_paths_with_coordinator(paths, coordinator)
                .expect("claim operation journal persistence"),
            GuardianFailureMemoryStore::new(),
        )
    }

    fn stores_with(
        journals: OperationJournalStore,
        failure_memory: GuardianFailureMemoryStore,
    ) -> Stores {
        let journals = Arc::new(journals);
        let failure_memory = Arc::new(failure_memory);
        let root = test_root("registered-state");
        let paths = test_paths(&root);
        let config = Arc::new(
            axial_config::ConfigStore::load_from(paths.clone()).expect("load test config"),
        );
        let instances = Arc::new(
            axial_config::InstanceStore::from_snapshot(
                paths.clone(),
                InstanceRegistrySnapshot::new(
                    vec![new_instance(
                        TEST_INSTANCE_ID.to_string(),
                        "Guardian Runtime".to_string(),
                        "1.21.1".to_string(),
                        String::new(),
                        String::new(),
                    )],
                    TEST_INSTANCE_ID.to_string(),
                    Vec::new(),
                )
                .expect("valid registered instance"),
            )
            .expect("load test instances"),
        );
        fs::create_dir_all(paths.instances_dir.join(TEST_INSTANCE_ID))
            .expect("create registered instance root");
        fs::create_dir_all(&paths.library_dir).expect("create managed library root");
        let state = AppState::new(AppStateInit {
            app_name: "Axial".to_string(),
            version: "test".to_string(),
            config,
            instances,
            installs: Arc::new(InstallStore::new()),
            sessions: Arc::new(SessionStore::new()),
            performance: Arc::new(
                axial_performance::PerformanceManager::load_for_startup(&paths.config_dir)
                    .expect("load performance state"),
            ),
            startup_warnings: Vec::new(),
        })
        .with_reconciliation_stores(journals.clone(), failure_memory.clone());
        state.set_library_dir_for_test(paths.library_dir.to_string_lossy().into_owned());
        state.activate_known_good_inventory_for_test(
            TEST_INSTANCE_ID,
            KnownGoodInventory::from_test_entries(Vec::<TestKnownGoodEntry>::new())
                .expect("empty known-good inventory"),
        );
        fs::create_dir_all(state.managed_runtime_cache().root())
            .expect("create managed runtime root");
        Stores {
            state,
            journals,
            failure_memory,
            fixture_root: root,
        }
    }

    fn write_fake_java(runtime_root: &Path) -> PathBuf {
        let java_path = managed_runtime_java_path(runtime_root);
        fs::create_dir_all(java_path.parent().expect("java parent")).expect("runtime bin");
        fs::write(&java_path, b"java").expect("fake java");
        make_executable(&java_path);
        java_path
    }

    fn write_runtime_manifest_proof(runtime_root: &Path, java_path: &Path) {
        let bytes = fs::read(java_path).expect("read fake java");
        let relative_path = java_path
            .strip_prefix(runtime_root)
            .expect("java under runtime root")
            .to_string_lossy()
            .replace('\\', "/");
        let mut hasher = Sha1::new();
        hasher.update(&bytes);
        let sha1 = format!("{:x}", hasher.finalize());
        let manifest = serde_json::json!({
            "files": {
                relative_path: {
                    "type": "file",
                    "downloads": {
                        "raw": {
                            "url": "https://example.invalid/java",
                            "sha1": sha1,
                            "size": bytes.len()
                        }
                    }
                }
            }
        });
        fs::write(
            runtime_root.join(".axial-runtime-manifest.json"),
            serde_json::to_vec(&manifest).expect("manifest json"),
        )
        .expect("runtime manifest proof");
    }

    fn managed_runtime_java_path(runtime_root: &Path) -> PathBuf {
        if cfg!(target_os = "macos") {
            return runtime_root
                .join("jre.bundle")
                .join("Contents")
                .join("Home")
                .join("bin")
                .join("java");
        }

        runtime_root
            .join("bin")
            .join(if cfg!(target_os = "windows") {
                "javaw.exe"
            } else {
                "java"
            })
    }

    #[cfg(unix)]
    fn make_executable(path: &Path) {
        use std::os::unix::fs::PermissionsExt;

        let mut permissions = fs::metadata(path).expect("java metadata").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).expect("java executable");
    }

    #[cfg(not(unix))]
    fn make_executable(_path: &Path) {}

    fn test_paths(root: &Path) -> AppPaths {
        AppPaths {
            config_file: root.join("config").join("config.json"),
            instances_file: root.join("config").join("instances.json"),
            instances_dir: root.join("instances"),
            music_dir: root.join("music"),
            library_dir: root.join("library"),
            config_dir: root.join("config"),
        }
    }

    fn managed_runtime_root(stores: &Stores, runtime_id: &str) -> PathBuf {
        stores
            .state
            .managed_runtime_cache()
            .component_root(runtime_id)
            .expect("known managed runtime component")
    }

    fn managed_runtime_cache() -> ManagedRuntimeCache {
        ManagedRuntimeCache::isolated_for_test().expect("isolated managed runtime cache")
    }

    fn runtime_root_binding<'a>(
        runtime_cache: &'a ManagedRuntimeCache,
        runtime_root: &'a Path,
        java_executable: &'a Path,
    ) -> ManagedRuntimeRoot<'a> {
        ManagedRuntimeRoot::from_managed_root(runtime_cache, runtime_root, java_executable)
            .expect("managed runtime root binding")
    }

    fn test_root(prefix: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or_default();
        std::env::temp_dir().join(format!(
            "axial-guardian-repair-{prefix}-{}-{nanos:x}",
            std::process::id()
        ))
    }

    fn cleanup(root: &Path) {
        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_file(root);
    }
}
