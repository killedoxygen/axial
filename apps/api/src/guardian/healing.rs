use super::{
    DiagnosisId, GuardianActionKind, GuardianDomain, GuardianMode, ReadyMarker, RepairAuthorization,
};
use crate::execution::ExecutionFact;
use crate::execution::runtime::{
    ManagedRuntimeRepairPrimitive, ManagedRuntimeRepairRequest, ManagedRuntimeRoot,
    repair_managed_runtime,
};
use crate::observability::{RedactionAudience, sanitize_evidence_token};
use crate::state::contracts::{
    CommandKind, JournalId, OperationId, OperationJournalEntry, OperationJournalStep,
    OperationOutcome, OperationPhase, OperationStatus, OperationStepResult, OwnershipClass,
    RollbackState, StabilizationSystem, TargetDescriptor, TargetKind,
};
use crate::state::failure_memory::{
    FailureMemoryActionOutcome, GuardianFailureMemoryEntry, GuardianFailureMemoryStore,
};
use crate::state::{
    OperationJournalReconciliation, OperationJournalStore, OperationJournalStoreError,
    operation_journal_completed_step_is_visible, operation_journal_plan_is_visible,
    operation_journal_terminal_is_visible,
};
use chrono::{DateTime, Duration};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration as StdDuration;
use tracing::warn;

const READY_MARKER_REPAIR_STEP: &str = "recreate_managed_runtime_ready_marker";
const DEFAULT_REPAIR_SUPPRESSION_MINUTES: i64 = 15;
const RUNTIME_JOURNAL_RETRY_INITIAL_DELAY: StdDuration = StdDuration::from_millis(20);
const RUNTIME_JOURNAL_RETRY_MAX_DELAY: StdDuration = StdDuration::from_secs(1);

enum RuntimeJournalReconciliation {
    MutationCommitted,
    AcceptedFailure(OperationJournalStoreError),
    RetryMutation,
}

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
    Suppressed,
}

struct RuntimeTerminalContext<'a> {
    journals: &'a OperationJournalStore,
    failure_memory: &'a GuardianFailureMemoryStore,
    diagnosis_id: &'a DiagnosisId,
    target: &'a TargetDescriptor,
    mode: GuardianMode,
    observed_at: &'a str,
    terminal_failure: Option<&'a tokio::sync::Notify>,
}

enum RuntimeTerminal {
    Blocked {
        action: Option<GuardianActionKind>,
        summary: &'static str,
    },
    Suppressed {
        action: GuardianActionKind,
        suppression_until: String,
    },
    Repaired {
        action: GuardianActionKind,
        facts: Vec<String>,
    },
    Failed {
        action: GuardianActionKind,
        facts: Vec<String>,
        suppression_until: Option<String>,
    },
}

enum RuntimeTerminalJournal {
    Create(OperationOutcome),
    Record(OperationStepResult, Option<&'static str>),
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn execute_managed_runtime_ready_marker_repair(
    authorization: RepairAuthorization<ReadyMarker>,
    operation_id: Option<OperationId>,
    runtime_root: ManagedRuntimeRoot<'_>,
    journals: &OperationJournalStore,
    failure_memory: &GuardianFailureMemoryStore,
    observed_at: &str,
    suppression_until_on_failure: Option<&str>,
    abandoned: Option<&AtomicBool>,
    ready_for_effect: Option<tokio::sync::oneshot::Sender<()>>,
    terminal_failure: Option<&tokio::sync::Notify>,
) -> Result<GuardianRepairOutcome, OperationJournalStoreError> {
    let operation_id = operation_id
        .as_ref()
        .map(safe_operation_id)
        .unwrap_or_else(new_repair_operation_id);
    let authorization = authorization.into_parts();
    let runtime_root_target = public_safe_target(runtime_root.target());
    let target = public_safe_target(&authorization.target);
    let terminal_context = RuntimeTerminalContext {
        journals,
        failure_memory,
        diagnosis_id: &authorization.diagnosis_id,
        target: &target,
        mode: authorization.mode,
        observed_at,
        terminal_failure,
    };
    if authorization.max_attempts == 0 {
        return finish_runtime_repair(
            &terminal_context,
            operation_id,
            RuntimeTerminal::Blocked {
                action: None,
                summary: "guardian_repair_blocked_by_policy",
            },
        )
        .await;
    }
    let action = authorization.action;
    if authorization.ownership != target.ownership
        || !ready_marker_repair_target_supported(&runtime_root_target)
        || target != runtime_root_target
    {
        return finish_runtime_repair(
            &terminal_context,
            operation_id,
            RuntimeTerminal::Blocked {
                action: Some(action),
                summary: "guardian_repair_blocked_unsupported_target",
            },
        )
        .await;
    }

    if let Some(suppression_until) =
        failure_memory
            .get(&authorization.suppression_key)
            .and_then(|entry| {
                super::repair_terminal::active_repair_suppression_until(&entry, observed_at)
            })
    {
        return finish_runtime_repair(
            &terminal_context,
            operation_id,
            RuntimeTerminal::Suppressed {
                action,
                suppression_until,
            },
        )
        .await;
    }

    if let Some(error) = create_planned_journal_reconciled(
        journals,
        &operation_id,
        &authorization.diagnosis_id,
        &target,
    )
    .await?
    {
        terminalize_recovered_runtime_journal(journals, &operation_id, &target).await?;
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

    match repair_managed_runtime(ManagedRuntimeRepairRequest {
        operation_id: Some(operation_id.clone()),
        target: target.clone(),
        runtime_root,
        primitive: ManagedRuntimeRepairPrimitive::RecreateReadyMarker,
    }) {
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
            let suppression_until = suppression_until_on_failure
                .map(str::to_owned)
                .or_else(|| default_suppression_until(observed_at));
            finish_runtime_repair(
                &terminal_context,
                operation_id,
                RuntimeTerminal::Failed {
                    action,
                    facts: fact_ids,
                    suppression_until,
                },
            )
            .await
        }
    }
}

async fn finish_runtime_repair(
    context: &RuntimeTerminalContext<'_>,
    operation_id: OperationId,
    terminal: RuntimeTerminal,
) -> Result<GuardianRepairOutcome, OperationJournalStoreError> {
    let (journal, action, status, facts, summary, suppression_until, repair_attempt) =
        match terminal {
            RuntimeTerminal::Blocked { action, summary } => (
                RuntimeTerminalJournal::Create(OperationOutcome::Blocked),
                action,
                GuardianRepairStatus::Blocked,
                Vec::new(),
                summary,
                None,
                false,
            ),
            RuntimeTerminal::Suppressed {
                action,
                suppression_until,
            } => (
                RuntimeTerminalJournal::Create(OperationOutcome::Suppressed),
                Some(action),
                GuardianRepairStatus::Suppressed,
                Vec::new(),
                "guardian_repair_suppressed",
                Some(suppression_until),
                false,
            ),
            RuntimeTerminal::Repaired { action, facts } => (
                RuntimeTerminalJournal::Record(OperationStepResult::Completed, None),
                Some(action),
                GuardianRepairStatus::Repaired,
                facts,
                "managed_runtime_ready_marker_repaired",
                default_suppression_until(context.observed_at),
                true,
            ),
            RuntimeTerminal::Failed {
                action,
                facts,
                suppression_until,
            } => (
                RuntimeTerminalJournal::Record(
                    OperationStepResult::Failed,
                    Some(READY_MARKER_REPAIR_STEP),
                ),
                Some(action),
                GuardianRepairStatus::Failed,
                facts,
                "managed_runtime_ready_marker_repair_failed",
                suppression_until,
                true,
            ),
        };
    let journal_operation_id = operation_id.clone();
    let journal_facts = facts.clone();
    let complete_journal = async move {
        match journal {
            RuntimeTerminalJournal::Create(outcome) => {
                create_terminal_journal_reconciled(
                    context.journals,
                    &journal_operation_id,
                    context.diagnosis_id,
                    context.target,
                    OperationStatus::Blocked,
                    outcome,
                    OperationStepResult::Skipped,
                    journal_facts,
                )
                .await
            }
            RuntimeTerminalJournal::Record(step_result, failure_point) => {
                record_runtime_terminal_reconciled(
                    context.journals,
                    &journal_operation_id,
                    repair_step(step_result, Some(context.target.clone()), journal_facts),
                    failure_point,
                    context.terminal_failure,
                )
                .await
            }
        }
    };
    super::repair_terminal::complete_repair_terminal(
        complete_journal,
        || {
            let memory_outcome = match status {
                GuardianRepairStatus::Repaired => FailureMemoryActionOutcome::Repaired,
                GuardianRepairStatus::Blocked => FailureMemoryActionOutcome::Blocked,
                GuardianRepairStatus::Failed => FailureMemoryActionOutcome::Failed,
                GuardianRepairStatus::Suppressed => FailureMemoryActionOutcome::Suppressed,
            };
            record_repair_memory(
                context.failure_memory,
                context.diagnosis_id,
                context.mode,
                context.target,
                action,
                memory_outcome,
                context.observed_at,
                suppression_until.as_deref(),
                repair_attempt,
            );
        },
        || {
            repair_outcome(
                operation_id,
                Some(*context.diagnosis_id),
                action,
                status,
                facts,
                summary,
            )
        },
    )
    .await
}

fn planned_runtime_journal(
    operation_id: &OperationId,
    diagnosis_id: &DiagnosisId,
    target: &TargetDescriptor,
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
    entry.planned_steps.push(repair_step(
        OperationStepResult::Planned,
        Some(target.clone()),
        Vec::new(),
    ));
    entry.guardian_diagnosis_ids.push(*diagnosis_id);
    entry
}

async fn reconcile_runtime_journal_error(
    journals: &OperationJournalStore,
    operation_id: &OperationId,
    error: OperationJournalStoreError,
    expected: impl Fn(&OperationJournalEntry) -> bool,
) -> Result<RuntimeJournalReconciliation, OperationJournalStoreError> {
    match journals
        .reconcile_transition(
            operation_id,
            error,
            RUNTIME_JOURNAL_RETRY_INITIAL_DELAY,
            RUNTIME_JOURNAL_RETRY_MAX_DELAY,
            expected,
        )
        .await?
    {
        OperationJournalReconciliation::CommittedAfterPersistenceFailure(error) => {
            Ok(RuntimeJournalReconciliation::AcceptedFailure(error))
        }
        OperationJournalReconciliation::RequestedTransitionAlreadyCommitted => {
            Ok(RuntimeJournalReconciliation::MutationCommitted)
        }
        OperationJournalReconciliation::RetryRequestedTransition => {
            Ok(RuntimeJournalReconciliation::RetryMutation)
        }
    }
}

async fn create_planned_journal_reconciled(
    journals: &OperationJournalStore,
    operation_id: &OperationId,
    diagnosis_id: &DiagnosisId,
    target: &TargetDescriptor,
) -> Result<Option<OperationJournalStoreError>, OperationJournalStoreError> {
    let expected = planned_runtime_journal(operation_id, diagnosis_id, target);
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
                match reconcile_runtime_journal_error(journals, operation_id, error, |entry| {
                    operation_journal_plan_is_visible(entry, &expected)
                })
                .await?
                {
                    RuntimeJournalReconciliation::MutationCommitted => return Ok(None),
                    RuntimeJournalReconciliation::AcceptedFailure(error) => return Ok(Some(error)),
                    RuntimeJournalReconciliation::RetryMutation => {}
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
                match reconcile_runtime_journal_error(journals, operation_id, error, |entry| {
                    operation_journal_terminal_is_visible(entry, &expected)
                })
                .await?
                {
                    RuntimeJournalReconciliation::MutationCommitted => return Ok(None),
                    RuntimeJournalReconciliation::AcceptedFailure(error) => return Ok(Some(error)),
                    RuntimeJournalReconciliation::RetryMutation => {}
                }
            }
        }
    }
}

async fn record_runtime_terminal_reconciled(
    journals: &OperationJournalStore,
    operation_id: &OperationId,
    step: OperationJournalStep,
    failure_point: Option<&str>,
    terminal_failure: Option<&tokio::sync::Notify>,
) -> Result<Option<OperationJournalStoreError>, OperationJournalStoreError> {
    loop {
        let result = if let Some(failure_point) = failure_point {
            journals
                .record_failure(
                    operation_id,
                    step.clone(),
                    failure_point,
                    OperationOutcome::Failed,
                )
                .await
        } else {
            journals
                .record_success(operation_id, step.clone(), OperationOutcome::Succeeded)
                .await
        };
        match result {
            Ok(()) => return Ok(None),
            Err(OperationJournalStoreError::AlreadyTerminal)
                if journals.get(operation_id).is_some_and(|entry| {
                    runtime_terminal_transition_matches(&entry, operation_id, failure_point, &step)
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
                match reconcile_runtime_journal_error(journals, operation_id, error, |entry| {
                    runtime_terminal_transition_matches(entry, operation_id, failure_point, &step)
                })
                .await?
                {
                    RuntimeJournalReconciliation::MutationCommitted => return Ok(None),
                    RuntimeJournalReconciliation::AcceptedFailure(error) => {
                        return Ok(Some(error));
                    }
                    RuntimeJournalReconciliation::RetryMutation => {}
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
    record_runtime_terminal_reconciled(
        journals,
        operation_id,
        repair_step(
            OperationStepResult::Failed,
            Some(target.clone()),
            Vec::new(),
        ),
        Some("managed_runtime_repair_initialization_failed"),
        None,
    )
    .await
    .map(|_| ())
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
        OperationStepResult::Planned,
        Some(target.clone()),
        Vec::new(),
    ));
    entry
        .completed_steps
        .push(repair_step(step_result, Some(target.clone()), facts));
    entry.guardian_diagnosis_ids.push(*diagnosis_id);
    entry.outcome = Some(outcome);
    entry
}

fn runtime_terminal_transition_matches(
    entry: &OperationJournalEntry,
    operation_id: &OperationId,
    failure_point: Option<&str>,
    step: &OperationJournalStep,
) -> bool {
    let (status, outcome) = if failure_point.is_some() {
        (OperationStatus::Failed, OperationOutcome::Failed)
    } else {
        (OperationStatus::Succeeded, OperationOutcome::Succeeded)
    };
    entry.operation_id == *operation_id
        && entry.command == CommandKind::RepairInstance
        && entry.owner == StabilizationSystem::Guardian
        && step.changed_target.as_ref().is_some_and(|target| {
            entry.targets.contains(target) && entry.ownership == target.ownership
        })
        && entry.status == status
        && entry.outcome == Some(outcome)
        && entry.failure_point.as_deref() == failure_point
        && operation_journal_completed_step_is_visible(entry, step)
}

fn repair_step(
    result: OperationStepResult,
    target: Option<TargetDescriptor>,
    facts: Vec<String>,
) -> OperationJournalStep {
    let mut step = OperationJournalStep::new(READY_MARKER_REPAIR_STEP, OperationPhase::Repairing);
    step.result = result;
    step.changed_target = target;
    step.generated_facts = facts;
    step.rollback = RollbackState::NotApplicable;
    step
}

#[allow(clippy::too_many_arguments)]
fn record_repair_memory(
    failure_memory: &GuardianFailureMemoryStore,
    diagnosis_id: &DiagnosisId,
    mode: GuardianMode,
    target: &TargetDescriptor,
    action: Option<GuardianActionKind>,
    outcome: FailureMemoryActionOutcome,
    observed_at: &str,
    suppression_until: Option<&str>,
    repair_attempt: bool,
) {
    let observation = GuardianFailureMemoryEntry::observed(
        *diagnosis_id,
        GuardianDomain::Runtime,
        target.clone(),
        mode,
        None,
        observed_at,
    );
    let result = if let Some(action) = action {
        let mut entry = observation.with_action(action, outcome);
        if repair_attempt {
            entry = entry.with_repair_attempt();
        }
        if let Some(suppression_until) = suppression_until {
            entry = entry.with_suppression_until(suppression_until);
        }
        failure_memory.record(entry)
    } else {
        failure_memory.record_observation_preserving_loop_control(observation)
    };
    if let Err(error) = result {
        warn!(
            error_kind = error.class(),
            "failed to record Guardian managed-runtime repair failure memory"
        );
    }
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
        operation_id: safe_operation_id(&operation_id),
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

fn default_suppression_until(observed_at: &str) -> Option<String> {
    DateTime::parse_from_rfc3339(observed_at)
        .ok()
        .map(|observed_at| {
            (observed_at + Duration::minutes(DEFAULT_REPAIR_SUPPRESSION_MINUTES)).to_rfc3339()
        })
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
        GuardianRepairOutcome, GuardianRepairStatus, execute_managed_runtime_ready_marker_repair,
    };
    use crate::execution::persistence::{AtomicWriteBackend, PersistenceCoordinator};
    use crate::execution::runtime::{ManagedRuntimeRoot, ManagedRuntimeRootError};
    use crate::guardian::{
        ActionPlanPrerequisite, DiagnosisId, GuardianAction, GuardianActionKind,
        GuardianActionPlan, GuardianDecision, GuardianMode, RepairAuthorizationContext,
        RepairAuthorizationRejection, authorize_managed_runtime_ready_marker_repair,
    };
    use crate::state::OperationJournalStore;
    use crate::state::contracts::{
        OperationId, OperationOutcome, OperationStatus, OperationStepResult, OwnershipClass,
        StabilizationSystem, TargetDescriptor, TargetKind,
    };
    use crate::state::failure_memory::{
        FailureMemoryActionOutcome, FailureMemoryKey, GuardianFailureMemoryEntry,
        GuardianFailureMemoryStore,
    };
    use axial_config::AppPaths;
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
        let paths = test_paths(&root);
        let runtime_root = managed_runtime_root(&paths, "java_runtime_delta");
        let java_executable = write_fake_java(&runtime_root);
        write_runtime_manifest_proof(&runtime_root, &java_executable);
        let stores = stores();
        let decision = repair_decision(OwnershipClass::LauncherManaged);

        let outcome = execute_repair(
            &decision,
            &paths,
            &runtime_root,
            &java_executable,
            &stores,
            "2026-06-15T10:00:00Z",
            None,
        );

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
        assert_eq!(journal.planned_steps.len(), 1);
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

    #[tokio::test]
    async fn planned_commit_failure_prevents_managed_runtime_mutation() {
        let root = test_root("planned-commit-gate");
        let paths = test_paths(&root);
        let runtime_root = managed_runtime_root(&paths, "java_runtime_delta");
        let java_executable = write_fake_java(&runtime_root);
        write_runtime_manifest_proof(&runtime_root, &java_executable);
        let stores = persistent_stores(&paths);
        let decision = repair_decision(OwnershipClass::LauncherManaged);
        let result = execute_repair_async(
            &decision,
            &paths,
            &runtime_root,
            &java_executable,
            &stores,
            "2026-06-15T10:00:00Z",
            None,
            None,
        )
        .await;

        assert!(result.is_err());
        assert!(!runtime_root.join(".axial-ready").exists());
        assert!(stores.failure_memory.list().is_empty());
        let operation_id = decision.operation_id.as_ref().expect("repair operation id");
        let journal = stores
            .journals
            .get(operation_id)
            .expect("terminalized repair journal");
        assert_eq!(journal.status, OperationStatus::Failed);
        assert_eq!(journal.outcome, Some(OperationOutcome::Failed));
        assert!(!stores.journals.has_retry_candidate());
        cleanup(&root);
    }

    #[tokio::test]
    async fn terminal_commit_failure_signals_bounded_owner_and_retries_without_repair_replay() {
        let root = test_root("terminal-commit-retry");
        let paths = test_paths(&root);
        let runtime_root = managed_runtime_root(&paths, "java_runtime_delta");
        let java_executable = write_fake_java(&runtime_root);
        write_runtime_manifest_proof(&runtime_root, &java_executable);
        let backend = Arc::new(ControlledWriteBackend::default());
        backend.gate_attempt(2);
        let coordinator = PersistenceCoordinator::for_test(
            backend.clone(),
            std::time::Duration::from_millis(1),
            std::time::Duration::from_millis(5),
        );
        let stores = Stores {
            journals: OperationJournalStore::try_load_from_paths_with_coordinator(
                &paths,
                coordinator,
            )
            .expect("claim runtime journal persistence"),
            failure_memory: GuardianFailureMemoryStore::new(),
        };
        let decision = repair_decision(OwnershipClass::LauncherManaged);
        let terminal_failure = tokio::sync::Notify::new();
        let repair = execute_repair_async(
            &decision,
            &paths,
            &runtime_root,
            &java_executable,
            &stores,
            "2026-06-15T10:00:00Z",
            None,
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
        let operation_id = decision.operation_id.as_ref().expect("repair operation id");
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
    fn repeated_same_runtime_repair_is_suppressed() {
        let root = test_root("suppressed");
        let paths = test_paths(&root);
        let runtime_root = managed_runtime_root(&paths, "java_runtime_delta");
        let java_executable = runtime_root.join("bin").join("java");
        let stores = stores();
        let decision = repair_decision(OwnershipClass::LauncherManaged);
        let target = decision
            .action_plan
            .as_ref()
            .expect("plan")
            .prerequisite
            .affected_targets[0]
            .clone();
        stores
            .failure_memory
            .record(
                GuardianFailureMemoryEntry::observed(
                    DiagnosisId::ManagedRuntimeCorrupt,
                    crate::guardian::GuardianDomain::Runtime,
                    target.clone(),
                    GuardianMode::Managed,
                    None,
                    "2026-06-15T10:00:00Z",
                )
                .with_action(
                    GuardianActionKind::Repair,
                    FailureMemoryActionOutcome::Failed,
                )
                .with_repair_attempt()
                .with_suppression_until("2026-06-15T10:30:00Z"),
            )
            .expect("memory record");

        let outcome = execute_repair(
            &decision,
            &paths,
            &runtime_root,
            &java_executable,
            &stores,
            "2026-06-15T10:05:00Z",
            None,
        );

        assert_eq!(outcome.status, GuardianRepairStatus::Suppressed);
        assert!(!runtime_root.join(".axial-ready").exists());
        let journal = stores
            .journals
            .get(&outcome.operation_id)
            .expect("journal entry");
        assert_eq!(journal.status, OperationStatus::Blocked);
        assert_eq!(journal.outcome, Some(OperationOutcome::Suppressed));
        let memory_key = FailureMemoryKey::for_observation(
            crate::guardian::GuardianDomain::Runtime,
            &DiagnosisId::ManagedRuntimeCorrupt,
            &target,
            GuardianMode::Managed,
            None,
        );
        let memory = stores
            .failure_memory
            .get(&memory_key)
            .expect("memory entry");
        assert_eq!(
            memory.last_action_outcome,
            Some(FailureMemoryActionOutcome::Suppressed)
        );
        assert_eq!(
            memory.suppression_until.as_deref(),
            Some("2026-06-15T10:30:00Z")
        );
        cleanup(&root);
    }

    #[test]
    fn user_owned_and_unknown_runtime_repairs_are_rejected() {
        for ownership in [OwnershipClass::UserOwned, OwnershipClass::Unknown] {
            let root = test_root("ownership-rejected");
            let paths = test_paths(&root);
            let runtime_root = managed_runtime_root(&paths, "java_runtime_delta");
            let decision = repair_decision(ownership);

            let error = authorize_managed_runtime_ready_marker_repair(
                &decision,
                RepairAuthorizationContext::current_operation(),
            )
            .expect_error_without_debug("unsafe ownership rejects before execution");

            assert_eq!(error, RepairAuthorizationRejection::UnsafeOwnership);
            assert!(!runtime_root.join(".axial-ready").exists());
            cleanup(&root);
        }
    }

    #[test]
    fn unsupported_runtime_repair_target_is_blocked_before_execution() {
        let root = test_root("unsupported-target");
        let paths = test_paths(&root);
        let runtime_root = managed_runtime_root(&paths, "java_runtime_delta");
        let decision = repair_decision_for_target(TargetDescriptor::new(
            StabilizationSystem::Guardian,
            TargetKind::Runtime,
            "java_runtime_delta",
            OwnershipClass::LauncherManaged,
        ));

        let error = authorize_managed_runtime_ready_marker_repair(
            &decision,
            RepairAuthorizationContext::current_operation(),
        )
        .expect_error_without_debug("unsupported target rejects before execution");

        assert_eq!(error, RepairAuthorizationRejection::UnsupportedDiagnosis);
        assert!(!runtime_root.join(".axial-ready").exists());
        cleanup(&root);
    }

    #[test]
    fn runtime_root_target_must_match_owned_repair_target() {
        let root = test_root("root-target-mismatch");
        let paths = test_paths(&root);
        let runtime_root = managed_runtime_root(&paths, "other_runtime");
        let java_executable = write_fake_java(&runtime_root);
        let stores = stores();
        let decision = repair_decision(OwnershipClass::LauncherManaged);

        let outcome = execute_repair(
            &decision,
            &paths,
            &runtime_root,
            &java_executable,
            &stores,
            "2026-06-15T10:00:00Z",
            None,
        );

        assert_eq!(outcome.status, GuardianRepairStatus::Blocked);
        assert!(!runtime_root.join(".axial-ready").exists());

        cleanup(&root);
    }

    #[test]
    fn arbitrary_runtime_root_cannot_enter_authorized_repair() {
        let root = test_root("root-binding");
        let paths = test_paths(&root);
        let runtime_root = root.join("user-runtime");
        let java_executable = runtime_root.join("bin").join("java");

        assert_eq!(
            ManagedRuntimeRoot::from_app_paths(&paths, &runtime_root, &java_executable)
                .expect_error_without_debug("outside runtime root"),
            ManagedRuntimeRootError::UnsupportedRoot
        );
        cleanup(&root);
    }

    #[test]
    fn actionless_runtime_observation_preserves_existing_loop_control() {
        let stores = stores();
        let decision = repair_decision(OwnershipClass::LauncherManaged);
        let target = decision_target(&decision);
        let diagnosis_id = DiagnosisId::ManagedRuntimeCorrupt;
        stores
            .failure_memory
            .record(
                GuardianFailureMemoryEntry::observed(
                    diagnosis_id,
                    crate::guardian::GuardianDomain::Runtime,
                    target.clone(),
                    GuardianMode::Managed,
                    None,
                    "2026-06-15T09:00:00Z",
                )
                .with_action(
                    GuardianActionKind::Repair,
                    FailureMemoryActionOutcome::Failed,
                )
                .with_repair_attempt()
                .with_suppression_until("2026-06-15T10:30:00Z"),
            )
            .expect("existing loop control");

        super::record_repair_memory(
            &stores.failure_memory,
            &diagnosis_id,
            GuardianMode::Managed,
            &target,
            None,
            FailureMemoryActionOutcome::Blocked,
            "2026-06-15T10:00:00Z",
            None,
            false,
        );

        let memory = stores.failure_memory.list();
        assert_eq!(memory[0].occurrence_count, 2);
        assert_eq!(memory[0].repair_attempt_count, 1);
        assert_eq!(memory[0].last_action_kind, Some(GuardianActionKind::Repair));
        assert_eq!(
            memory[0].last_action_outcome,
            Some(FailureMemoryActionOutcome::Failed)
        );
        assert_eq!(
            memory[0].suppression_until.as_deref(),
            Some("2026-06-15T10:30:00Z")
        );
    }

    #[test]
    fn malformed_or_non_repair_policy_is_blocked_before_execution() {
        let root = test_root("malformed-policy");
        let paths = test_paths(&root);
        let runtime_root = managed_runtime_root(&paths, "java_runtime_delta");
        write_fake_java(&runtime_root);
        for decision in [
            {
                let mut decision = repair_decision(OwnershipClass::LauncherManaged);
                decision.kind = GuardianActionKind::Block;
                decision
            },
            {
                let mut decision = repair_decision(OwnershipClass::LauncherManaged);
                decision.mode = GuardianMode::Disabled;
                decision
            },
            {
                let mut decision = repair_decision(OwnershipClass::LauncherManaged);
                decision
                    .action_plan
                    .as_mut()
                    .expect("plan")
                    .prerequisite
                    .confidence = crate::guardian::GuardianConfidence::High;
                decision
            },
            {
                let mut decision = repair_decision(OwnershipClass::LauncherManaged);
                decision.action_plan.as_mut().expect("plan").actions[0].reason =
                    DiagnosisId::DownloadUnavailable;
                decision
            },
        ] {
            let _ = fs::remove_file(runtime_root.join(".axial-ready"));
            let error = authorize_managed_runtime_ready_marker_repair(
                &decision,
                RepairAuthorizationContext::current_operation(),
            )
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
    fn historical_runtime_attempt_without_cooldown_allows_safe_retry() {
        let root = test_root("historical-attempt-without-cooldown");
        let paths = test_paths(&root);
        let runtime_root = managed_runtime_root(&paths, "java_runtime_delta");
        let java_executable = write_fake_java(&runtime_root);
        write_runtime_manifest_proof(&runtime_root, &java_executable);
        let stores = stores();
        let decision = repair_decision(OwnershipClass::LauncherManaged);
        let target = decision_target(&decision);
        stores
            .failure_memory
            .record(
                GuardianFailureMemoryEntry::observed(
                    DiagnosisId::ManagedRuntimeCorrupt,
                    crate::guardian::GuardianDomain::Runtime,
                    target,
                    GuardianMode::Managed,
                    None,
                    "2026-06-15T09:00:00Z",
                )
                .with_action(
                    GuardianActionKind::Repair,
                    FailureMemoryActionOutcome::Failed,
                )
                .with_repair_attempt(),
            )
            .expect("memory record");

        let _ = fs::remove_file(runtime_root.join(".axial-ready"));
        let outcome = execute_repair(
            &decision,
            &paths,
            &runtime_root,
            &java_executable,
            &stores,
            "2026-06-15T10:00:00Z",
            None,
        );

        assert_eq!(outcome.status, GuardianRepairStatus::Repaired);
        assert!(runtime_root.join(".axial-ready").exists());
        let memory = stores.failure_memory.list();
        assert_eq!(memory[0].repair_attempt_count, 2);
        assert_eq!(
            memory[0].last_action_outcome,
            Some(FailureMemoryActionOutcome::Repaired)
        );
        assert!(memory[0].suppression_until.is_some());
        cleanup(&root);
    }

    #[test]
    fn expired_runtime_repair_cooldown_allows_new_safe_attempt() {
        let root = test_root("expired-attempt-limit");
        let paths = test_paths(&root);
        let runtime_root = managed_runtime_root(&paths, "java_runtime_delta");
        let java_executable = write_fake_java(&runtime_root);
        write_runtime_manifest_proof(&runtime_root, &java_executable);
        let stores = stores();
        let decision = repair_decision(OwnershipClass::LauncherManaged);
        let target = decision_target(&decision);
        stores
            .failure_memory
            .record(
                GuardianFailureMemoryEntry::observed(
                    DiagnosisId::ManagedRuntimeCorrupt,
                    crate::guardian::GuardianDomain::Runtime,
                    target,
                    GuardianMode::Managed,
                    None,
                    "2026-06-15T09:00:00Z",
                )
                .with_action(
                    GuardianActionKind::Repair,
                    FailureMemoryActionOutcome::Failed,
                )
                .with_repair_attempt()
                .with_suppression_until("2026-06-15T09:30:00Z"),
            )
            .expect("memory record");

        let _ = fs::remove_file(runtime_root.join(".axial-ready"));
        let outcome = execute_repair(
            &decision,
            &paths,
            &runtime_root,
            &java_executable,
            &stores,
            "2026-06-15T10:00:00Z",
            None,
        );

        assert_eq!(outcome.status, GuardianRepairStatus::Repaired);
        assert!(runtime_root.join(".axial-ready").exists());
        let memory = stores.failure_memory.list();
        assert_eq!(
            memory[0].last_action_outcome,
            Some(FailureMemoryActionOutcome::Repaired)
        );
        assert_eq!(memory[0].repair_attempt_count, 2);
        cleanup(&root);
    }

    #[test]
    fn post_repair_verification_failure_is_not_reported_as_repaired() {
        let root = test_root("postcondition-failure");
        let paths = test_paths(&root);
        let runtime_root = managed_runtime_root(&paths, "java_runtime_delta");
        let java_executable = runtime_root.join("bin").join("java");
        let stores = stores();
        let decision = repair_decision(OwnershipClass::LauncherManaged);

        let outcome = execute_repair(
            &decision,
            &paths,
            &runtime_root,
            &java_executable,
            &stores,
            "2026-06-15T10:00:00Z",
            None,
        );

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
    fn public_repair_outcome_ids_are_sanitized() {
        let root = test_root("safe-outcome-ids");
        let paths = test_paths(&root);
        let runtime_root = managed_runtime_root(&paths, "java_runtime_delta");
        let java_executable = write_fake_java(&runtime_root);
        write_runtime_manifest_proof(&runtime_root, &java_executable);
        let stores = stores();
        let mut decision = repair_decision(OwnershipClass::LauncherManaged);
        decision.operation_id = Some(OperationId::new("/home/alice/token/operation"));

        let outcome = execute_repair(
            &decision,
            &paths,
            &runtime_root,
            &java_executable,
            &stores,
            "2026-06-15T10:00:00Z",
            None,
        );
        let encoded = serde_json::to_string(&outcome).expect("outcome json");
        let lower = encoded.to_ascii_lowercase();

        assert_eq!(outcome.status, GuardianRepairStatus::Repaired);
        assert!(!lower.contains("/home"));
        assert!(!lower.contains("alice"));
        assert!(!lower.contains("token"));
        cleanup(&root);
    }

    #[test]
    fn execution_failure_records_failed_outcome_and_suppression() {
        let root = test_root("failure");
        let paths = test_paths(&root);
        let runtime_root = managed_runtime_root(&paths, "java_runtime_delta");
        let java_executable = runtime_root.join("bin").join("java");
        fs::create_dir_all(runtime_root.parent().expect("runtime parent")).expect("test root");
        fs::write(&runtime_root, b"not a directory").expect("runtime root file");
        let stores = stores();
        let decision = repair_decision(OwnershipClass::LauncherManaged);

        let outcome = execute_repair(
            &decision,
            &paths,
            &runtime_root,
            &java_executable,
            &stores,
            "2026-06-15T10:00:00Z",
            Some("2026-06-15T10:15:00Z"),
        );

        assert_eq!(outcome.status, GuardianRepairStatus::Failed);
        let journal = stores
            .journals
            .get(&outcome.operation_id)
            .expect("journal entry");
        assert_eq!(journal.status, OperationStatus::Failed);
        assert_eq!(journal.outcome, Some(OperationOutcome::Failed));
        let key = FailureMemoryKey::for_observation(
            crate::guardian::GuardianDomain::Runtime,
            &DiagnosisId::ManagedRuntimeCorrupt,
            &decision
                .action_plan
                .as_ref()
                .expect("plan")
                .prerequisite
                .affected_targets[0],
            GuardianMode::Managed,
            None,
        );
        let memory = stores.failure_memory.get(&key).expect("memory entry");
        assert_eq!(
            memory.last_action_outcome,
            Some(FailureMemoryActionOutcome::Failed)
        );
        assert_eq!(
            memory.suppression_until.as_deref(),
            Some("2026-06-15T10:15:00Z")
        );
        cleanup(&root);
    }

    fn execute_repair(
        decision: &GuardianDecision,
        paths: &AppPaths,
        runtime_root: &Path,
        java_executable: &Path,
        stores: &Stores,
        observed_at: &str,
        suppression_until_on_failure: Option<&str>,
    ) -> GuardianRepairOutcome {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime")
            .block_on(execute_repair_async(
                decision,
                paths,
                runtime_root,
                java_executable,
                stores,
                observed_at,
                suppression_until_on_failure,
                None,
            ))
            .expect("persist managed-runtime repair journal")
    }

    #[allow(clippy::too_many_arguments)]
    async fn execute_repair_async(
        decision: &GuardianDecision,
        paths: &AppPaths,
        runtime_root: &Path,
        java_executable: &Path,
        stores: &Stores,
        observed_at: &str,
        suppression_until_on_failure: Option<&str>,
        terminal_failure: Option<&tokio::sync::Notify>,
    ) -> Result<GuardianRepairOutcome, crate::state::OperationJournalStoreError> {
        let authorization = authorize_managed_runtime_ready_marker_repair(
            decision,
            RepairAuthorizationContext::current_operation(),
        )
        .expect("runtime repair authorization");
        execute_managed_runtime_ready_marker_repair(
            authorization,
            decision.operation_id.clone(),
            runtime_root_binding(paths, runtime_root, java_executable),
            &stores.journals,
            &stores.failure_memory,
            observed_at,
            suppression_until_on_failure,
            None,
            None,
            terminal_failure,
        )
        .await
    }

    fn decision_target(decision: &GuardianDecision) -> TargetDescriptor {
        decision
            .action_plan
            .as_ref()
            .expect("plan")
            .prerequisite
            .affected_targets[0]
            .clone()
    }

    fn repair_decision(ownership: OwnershipClass) -> GuardianDecision {
        repair_decision_for_target(TargetDescriptor::new(
            StabilizationSystem::Execution,
            TargetKind::Runtime,
            "java_runtime_delta",
            ownership,
        ))
    }

    fn repair_decision_for_target(target: TargetDescriptor) -> GuardianDecision {
        let ownership = target.ownership;
        let operation_id = OperationId::new(format!("operation-{ownership:?}"));
        let diagnosis_id = DiagnosisId::ManagedRuntimeCorrupt;
        let prerequisite = ActionPlanPrerequisite {
            diagnosis_id,
            ownership,
            confidence: crate::guardian::GuardianConfidence::Confirmed,
            affected_targets: vec![target.clone()],
            candidate_actions: vec![GuardianActionKind::Repair],
        };
        GuardianDecision {
            operation_id: Some(operation_id),
            mode: GuardianMode::Managed,
            kind: GuardianActionKind::Repair,
            diagnoses: vec![diagnosis_id],
            action_plan: Some(GuardianActionPlan::new(
                StabilizationSystem::Guardian,
                prerequisite,
                vec![GuardianAction {
                    kind: GuardianActionKind::Repair,
                    target: Some(target),
                    reason: diagnosis_id,
                }],
            )),
        }
    }

    struct Stores {
        journals: OperationJournalStore,
        failure_memory: GuardianFailureMemoryStore,
    }

    fn stores() -> Stores {
        Stores {
            journals: OperationJournalStore::new(),
            failure_memory: GuardianFailureMemoryStore::new(),
        }
    }

    fn persistent_stores(paths: &AppPaths) -> Stores {
        let coordinator = PersistenceCoordinator::for_test(
            Arc::new(FailingWriteBackend {
                fail_next: AtomicBool::new(true),
            }),
            std::time::Duration::from_millis(20),
            std::time::Duration::from_millis(100),
        );
        Stores {
            journals: OperationJournalStore::try_load_from_paths_with_coordinator(
                paths,
                coordinator,
            )
            .expect("claim operation journal persistence"),
            failure_memory: GuardianFailureMemoryStore::new(),
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

    fn managed_runtime_root(paths: &AppPaths, runtime_id: &str) -> PathBuf {
        paths.library_dir.join("runtime").join(runtime_id)
    }

    fn runtime_root_binding<'a>(
        paths: &AppPaths,
        runtime_root: &'a Path,
        java_executable: &'a Path,
    ) -> ManagedRuntimeRoot<'a> {
        ManagedRuntimeRoot::from_app_paths(paths, runtime_root, java_executable)
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
