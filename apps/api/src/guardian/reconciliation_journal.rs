use crate::state::contracts::{
    CommandKind, OperationId, OperationJournalEntry, OperationJournalStep, OperationOutcome,
    OperationPhase, OperationStatus, OperationStepResult, ReconciliationTerminal, RollbackState,
    StabilizationSystem, TargetDescriptor,
};
use crate::state::{
    OperationJournalReconciliation, OperationJournalStore, OperationJournalStoreError,
    operation_journal_completed_step_is_visible, record_reconciliation_journal_failure,
    record_reconciliation_journal_success,
};
use std::time::Duration;

const JOURNAL_RETRY_INITIAL_DELAY: Duration = Duration::from_millis(20);
const JOURNAL_RETRY_MAX_DELAY: Duration = Duration::from_secs(1);

pub(super) enum GuardianJournalReconciliation {
    MutationCommitted,
    AcceptedFailure(OperationJournalStoreError),
    RetryMutation,
}

pub(super) async fn reconcile_guardian_journal_error(
    journals: &OperationJournalStore,
    operation_id: &OperationId,
    error: OperationJournalStoreError,
    expected: impl Fn(&OperationJournalEntry) -> bool,
) -> Result<GuardianJournalReconciliation, OperationJournalStoreError> {
    match journals
        .reconcile_transition(
            operation_id,
            error,
            JOURNAL_RETRY_INITIAL_DELAY,
            JOURNAL_RETRY_MAX_DELAY,
            expected,
        )
        .await?
    {
        OperationJournalReconciliation::CommittedAfterPersistenceFailure(error) => {
            Ok(GuardianJournalReconciliation::AcceptedFailure(error))
        }
        OperationJournalReconciliation::RequestedTransitionAlreadyCommitted => {
            Ok(GuardianJournalReconciliation::MutationCommitted)
        }
        OperationJournalReconciliation::RetryRequestedTransition => {
            Ok(GuardianJournalReconciliation::RetryMutation)
        }
    }
}

pub(super) async fn record_reconciliation_terminal_reconciled(
    journals: &OperationJournalStore,
    operation_id: &OperationId,
    step: OperationJournalStep,
    failure_point: Option<&str>,
    terminal: &ReconciliationTerminal,
    terminal_failure: Option<&tokio::sync::Notify>,
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
                    reconciliation_terminal_transition_matches(
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
                if matches!(error, OperationJournalStoreError::Persistence(_))
                    && let Some(terminal_failure) = terminal_failure
                {
                    terminal_failure.notify_one();
                }
                match reconcile_guardian_journal_error(journals, operation_id, error, |entry| {
                    reconciliation_terminal_transition_matches(
                        entry,
                        operation_id,
                        failure_point,
                        &step,
                        terminal,
                    )
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

pub(super) fn repair_step(
    step_id: &str,
    result: OperationStepResult,
    target: Option<TargetDescriptor>,
    facts: Vec<String>,
) -> OperationJournalStep {
    repair_step_with_rollback(step_id, result, target, facts, RollbackState::NotApplicable)
}

pub(super) fn repair_step_with_rollback(
    step_id: &str,
    result: OperationStepResult,
    target: Option<TargetDescriptor>,
    facts: Vec<String>,
    rollback: RollbackState,
) -> OperationJournalStep {
    let mut step = OperationJournalStep::new(step_id, OperationPhase::Repairing);
    step.result = result;
    step.changed_target = target;
    step.generated_facts = facts;
    step.rollback = rollback;
    step
}

fn reconciliation_terminal_transition_matches(
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
    &entry.operation_id == operation_id
        && entry.command == CommandKind::RepairInstance
        && entry.owner == StabilizationSystem::Guardian
        && step.changed_target.as_ref().is_some_and(|target| {
            entry.targets.contains(target) && entry.ownership == target.ownership
        })
        && entry.status == status
        && entry.outcome == Some(outcome)
        && entry.failure_point.as_deref() == failure_point
        && entry.reconciliation_terminal() == Some(terminal)
        && operation_journal_completed_step_is_visible(entry, step)
}
