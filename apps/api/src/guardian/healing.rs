use super::{
    DiagnosisId, GuardianActionKind, GuardianDomain, GuardianMode, GuardianRepairExecutor,
    GuardianRepairMutation, GuardianRepairPlan, GuardianRepairTask, GuardianRepairTaskKind,
};
use crate::execution::ExecutionFact;
use crate::execution::runtime::{
    ManagedRuntimeRepairPrimitive, ManagedRuntimeRepairRequest, ManagedRuntimeRoot,
    repair_managed_runtime,
};
use crate::observability::{RedactionAudience, sanitize_evidence_token};
use crate::state::OperationJournalStore;
use crate::state::contracts::{
    CommandKind, JournalId, OperationId, OperationJournalEntry, OperationJournalStep,
    OperationOutcome, OperationPhase, OperationStatus, OperationStepResult, OwnershipClass,
    RollbackState, StabilizationSystem, TargetDescriptor, TargetKind,
};
use crate::state::failure_memory::{
    FailureMemoryActionOutcome, FailureMemoryKey, GuardianFailureMemoryEntry,
    GuardianFailureMemoryStore,
};
use chrono::{DateTime, Duration, FixedOffset};
use serde::{Deserialize, Serialize};

const READY_MARKER_REPAIR_STEP: &str = "recreate_managed_runtime_ready_marker";
const READY_MARKER_REPAIR_MAX_ATTEMPTS: u32 = 1;
const DEFAULT_REPAIR_SUPPRESSION_MINUTES: i64 = 15;

pub struct GuardianManagedRuntimeRepairRequest<'a> {
    pub operation_id: Option<OperationId>,
    pub mode: GuardianMode,
    pub plan: &'a GuardianRepairPlan,
    pub runtime_root: ManagedRuntimeRoot<'a>,
    pub journals: &'a OperationJournalStore,
    pub failure_memory: &'a GuardianFailureMemoryStore,
    pub observed_at: &'a str,
    pub suppression_until_on_failure: Option<&'a str>,
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
    NotNeeded,
    Repaired,
    Blocked,
    Failed,
    Suppressed,
}

pub fn execute_managed_runtime_ready_marker_repair(
    request: GuardianManagedRuntimeRepairRequest<'_>,
) -> GuardianRepairOutcome {
    let operation_id = request
        .operation_id
        .as_ref()
        .map(safe_operation_id)
        .unwrap_or_else(new_repair_operation_id);
    let runtime_root_target = public_safe_target(request.runtime_root.target());
    let plan = request.plan;
    let Some(action) = runtime_ready_marker_repair_task(plan) else {
        return repair_outcome(
            operation_id,
            Some(plan.diagnosis_id.clone()),
            None,
            GuardianRepairStatus::Blocked,
            Vec::new(),
            "guardian_repair_blocked_by_policy",
        );
    };

    let target = public_safe_target(&plan.target);

    if let Some(block_reason) = repair_plan_block_reason(request.mode, plan, &target) {
        record_repair_memory(
            request.failure_memory,
            &plan.diagnosis_id,
            request.mode,
            &target,
            action.action,
            FailureMemoryActionOutcome::Blocked,
            request.observed_at,
            None,
            false,
        );
        create_terminal_journal(
            request.journals,
            &operation_id,
            &plan.diagnosis_id,
            &target,
            OperationStatus::Blocked,
            OperationOutcome::Blocked,
            OperationStepResult::Skipped,
            Vec::new(),
        );
        return repair_outcome(
            operation_id,
            Some(plan.diagnosis_id.clone()),
            Some(action.action),
            GuardianRepairStatus::Blocked,
            Vec::new(),
            block_reason,
        );
    }

    if matches!(
        target.ownership,
        OwnershipClass::UserOwned | OwnershipClass::Unknown
    ) {
        record_repair_memory(
            request.failure_memory,
            &plan.diagnosis_id,
            request.mode,
            &target,
            action.action,
            FailureMemoryActionOutcome::Blocked,
            request.observed_at,
            None,
            false,
        );
        create_terminal_journal(
            request.journals,
            &operation_id,
            &plan.diagnosis_id,
            &target,
            OperationStatus::Blocked,
            OperationOutcome::Blocked,
            OperationStepResult::Skipped,
            Vec::new(),
        );
        return repair_outcome(
            operation_id,
            Some(plan.diagnosis_id.clone()),
            Some(action.action),
            GuardianRepairStatus::Blocked,
            Vec::new(),
            "guardian_repair_blocked_by_ownership",
        );
    }

    if !ready_marker_repair_target_supported(&target)
        || !ready_marker_repair_target_supported(&runtime_root_target)
        || target != runtime_root_target
    {
        record_repair_memory(
            request.failure_memory,
            &plan.diagnosis_id,
            request.mode,
            &target,
            action.action,
            FailureMemoryActionOutcome::Blocked,
            request.observed_at,
            None,
            false,
        );
        create_terminal_journal(
            request.journals,
            &operation_id,
            &plan.diagnosis_id,
            &target,
            OperationStatus::Blocked,
            OperationOutcome::Blocked,
            OperationStepResult::Skipped,
            Vec::new(),
        );
        return repair_outcome(
            operation_id,
            Some(plan.diagnosis_id.clone()),
            Some(action.action),
            GuardianRepairStatus::Blocked,
            Vec::new(),
            "guardian_repair_blocked_unsupported_target",
        );
    }

    let memory_key = FailureMemoryKey::for_observation(
        GuardianDomain::Runtime,
        &plan.diagnosis_id,
        &target,
        request.mode,
        None,
    );
    if let Some(suppression_until) = request
        .failure_memory
        .get(&memory_key)
        .and_then(|entry| runtime_repair_suppression_until(&entry, request.observed_at))
    {
        record_repair_memory(
            request.failure_memory,
            &plan.diagnosis_id,
            request.mode,
            &target,
            action.action,
            FailureMemoryActionOutcome::Suppressed,
            request.observed_at,
            Some(suppression_until.as_str()),
            false,
        );
        create_terminal_journal(
            request.journals,
            &operation_id,
            &plan.diagnosis_id,
            &target,
            OperationStatus::Blocked,
            OperationOutcome::Suppressed,
            OperationStepResult::Skipped,
            Vec::new(),
        );
        return repair_outcome(
            operation_id,
            Some(plan.diagnosis_id.clone()),
            Some(action.action),
            GuardianRepairStatus::Suppressed,
            Vec::new(),
            "guardian_repair_suppressed",
        );
    }

    create_planned_journal(request.journals, &operation_id, &plan.diagnosis_id, &target);

    match repair_managed_runtime(ManagedRuntimeRepairRequest {
        operation_id: Some(operation_id.clone()),
        target: target.clone(),
        runtime_root: request.runtime_root,
        primitive: ManagedRuntimeRepairPrimitive::RecreateReadyMarker,
    }) {
        Ok(report) => {
            let fact_ids = fact_ids(&report.facts);
            let default_suppression_until = default_suppression_until(request.observed_at);
            request.journals.record_success(
                &operation_id,
                repair_step(
                    OperationStepResult::Completed,
                    Some(target.clone()),
                    fact_ids.clone(),
                ),
                OperationOutcome::Succeeded,
            );
            record_repair_memory(
                request.failure_memory,
                &plan.diagnosis_id,
                request.mode,
                &target,
                action.action,
                FailureMemoryActionOutcome::Repaired,
                request.observed_at,
                default_suppression_until.as_deref(),
                true,
            );
            repair_outcome(
                operation_id,
                Some(plan.diagnosis_id.clone()),
                Some(action.action),
                GuardianRepairStatus::Repaired,
                fact_ids,
                "managed_runtime_ready_marker_repaired",
            )
        }
        Err(error) => {
            let fact_ids = fact_ids(&error.facts);
            let default_suppression_until = default_suppression_until(request.observed_at);
            let suppression_until = request
                .suppression_until_on_failure
                .or(default_suppression_until.as_deref());
            request.journals.record_failure(
                &operation_id,
                repair_step(
                    OperationStepResult::Failed,
                    Some(target.clone()),
                    fact_ids.clone(),
                ),
                READY_MARKER_REPAIR_STEP,
                OperationOutcome::Failed,
            );
            record_repair_memory(
                request.failure_memory,
                &plan.diagnosis_id,
                request.mode,
                &target,
                action.action,
                FailureMemoryActionOutcome::Failed,
                request.observed_at,
                suppression_until,
                true,
            );
            repair_outcome(
                operation_id,
                Some(plan.diagnosis_id.clone()),
                Some(action.action),
                GuardianRepairStatus::Failed,
                fact_ids,
                "managed_runtime_ready_marker_repair_failed",
            )
        }
    }
}

fn create_planned_journal(
    journals: &OperationJournalStore,
    operation_id: &OperationId,
    diagnosis_id: &DiagnosisId,
    target: &TargetDescriptor,
) {
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
    entry
        .guardian_diagnosis_ids
        .push(safe_id(diagnosis_id.as_str(), "diagnosis"));
    journals.create(entry);
}

#[allow(clippy::too_many_arguments)]
fn create_terminal_journal(
    journals: &OperationJournalStore,
    operation_id: &OperationId,
    diagnosis_id: &DiagnosisId,
    target: &TargetDescriptor,
    status: OperationStatus,
    outcome: OperationOutcome,
    step_result: OperationStepResult,
    facts: Vec<String>,
) {
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
    entry
        .guardian_diagnosis_ids
        .push(safe_id(diagnosis_id.as_str(), "diagnosis"));
    entry.outcome = Some(outcome);
    journals.create(entry);
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
    action: GuardianActionKind,
    outcome: FailureMemoryActionOutcome,
    observed_at: &str,
    suppression_until: Option<&str>,
    repair_attempt: bool,
) {
    let mut entry = GuardianFailureMemoryEntry::observed(
        diagnosis_id.clone(),
        GuardianDomain::Runtime,
        target.clone(),
        mode,
        None,
        observed_at,
    )
    .with_action(action, outcome);
    if repair_attempt {
        entry = entry.with_repair_attempt();
    }
    if let Some(suppression_until) = suppression_until {
        entry = entry.with_suppression_until(suppression_until);
    }
    let _ = failure_memory.record(entry);
}

fn suppression_active(entry: &GuardianFailureMemoryEntry, now: &str) -> bool {
    let Some(suppression_until) = entry.suppression_until.as_deref() else {
        return false;
    };
    let Ok(suppression_until) = DateTime::parse_from_rfc3339(suppression_until) else {
        return false;
    };
    let Ok(now) = DateTime::<FixedOffset>::parse_from_rfc3339(now) else {
        return false;
    };
    suppression_until > now
}

fn runtime_repair_suppression_until(
    entry: &GuardianFailureMemoryEntry,
    now: &str,
) -> Option<String> {
    if suppression_active(entry, now) {
        return entry.suppression_until.clone();
    }
    if entry.repair_attempt_count >= READY_MARKER_REPAIR_MAX_ATTEMPTS
        && entry.suppression_until.is_none()
    {
        return default_suppression_until(now);
    }
    None
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
        diagnosis_id: diagnosis_id.as_ref().map(safe_diagnosis_id),
        action,
        status,
        facts,
        summary: safe_id(summary, "guardian_repair_outcome"),
    }
}

fn runtime_ready_marker_repair_task(plan: &GuardianRepairPlan) -> Option<&GuardianRepairTask> {
    plan.tasks.iter().find(|task| {
        task.kind == GuardianRepairTaskKind::RecreateManagedRuntimeReadyMarker
            && task.action == GuardianActionKind::Repair
            && task.executor == GuardianRepairExecutor::ExecutionRuntimeRepair
            && task.mutation == GuardianRepairMutation::RecreateManagedRuntimeReadyMarker
    })
}

fn repair_plan_block_reason(
    mode: GuardianMode,
    plan: &GuardianRepairPlan,
    target: &TargetDescriptor,
) -> Option<&'static str> {
    if mode == GuardianMode::Disabled {
        return Some("guardian_repair_blocked_by_policy");
    }
    if plan.diagnosis_id.as_str() != "managed_runtime_corrupt" {
        return Some("guardian_repair_blocked_by_policy");
    }
    if plan.ownership != target.ownership {
        return Some("guardian_repair_blocked_by_policy");
    }
    if !plan
        .tasks
        .iter()
        .any(|task| task.target == *target && task.ownership == target.ownership)
    {
        return Some("guardian_repair_blocked_by_policy");
    }
    if !plan.tasks.iter().any(|task| {
        task.kind == GuardianRepairTaskKind::JournalRepairStart
            && task.executor == GuardianRepairExecutor::StateJournal
    }) {
        return Some("guardian_repair_blocked_by_policy");
    }
    if !plan.tasks.iter().any(|task| {
        task.kind == GuardianRepairTaskKind::RecordRepairOutcome
            && task.executor == GuardianRepairExecutor::GuardianOutcomeRecorder
    }) {
        return Some("guardian_repair_blocked_by_policy");
    }
    None
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

fn safe_diagnosis_id(diagnosis_id: &DiagnosisId) -> DiagnosisId {
    DiagnosisId::new(safe_id(diagnosis_id.as_str(), "diagnosis"))
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
        GuardianManagedRuntimeRepairRequest, GuardianRepairOutcome, GuardianRepairStatus,
        execute_managed_runtime_ready_marker_repair,
    };
    use crate::execution::runtime::{ManagedRuntimeRoot, ManagedRuntimeRootError};
    use crate::guardian::{
        ActionPlanPrerequisite, DiagnosisId, GuardianAction, GuardianActionKind,
        GuardianActionPlan, GuardianDecision, GuardianDecisionKind, GuardianMode,
        GuardianRepairPlan, GuardianRepairPlanRejection, GuardianRepairPlanningContext,
        plan_managed_runtime_ready_marker_repair,
    };
    use crate::state::OperationJournalStore;
    use crate::state::contracts::{
        OperationId, OperationOutcome, OperationStatus, OwnershipClass, StabilizationSystem,
        TargetDescriptor, TargetKind,
    };
    use crate::state::failure_memory::{
        FailureMemoryActionOutcome, FailureMemoryKey, GuardianFailureMemoryEntry,
        GuardianFailureMemoryStore,
    };
    use croopor_config::AppPaths;
    use sha1::{Digest, Sha1};
    use std::fs;
    use std::path::{Path, PathBuf};

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
        assert!(runtime_root.join(".croopor-ready").is_file());
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
                    DiagnosisId::new("managed_runtime_corrupt"),
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
        assert!(!runtime_root.join(".croopor-ready").exists());
        let journal = stores
            .journals
            .get(&outcome.operation_id)
            .expect("journal entry");
        assert_eq!(journal.status, OperationStatus::Blocked);
        assert_eq!(journal.outcome, Some(OperationOutcome::Suppressed));
        let memory_key = FailureMemoryKey::for_observation(
            crate::guardian::GuardianDomain::Runtime,
            &DiagnosisId::new("managed_runtime_corrupt"),
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

            let error = plan_managed_runtime_ready_marker_repair(
                &decision,
                GuardianRepairPlanningContext::current_operation(),
            )
            .expect_err("unsafe ownership rejects before execution");

            assert_eq!(error, GuardianRepairPlanRejection::UnsafeOwnership);
            assert!(!runtime_root.join(".croopor-ready").exists());
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

        let error = plan_managed_runtime_ready_marker_repair(
            &decision,
            GuardianRepairPlanningContext::current_operation(),
        )
        .expect_err("unsupported target rejects before execution");

        assert_eq!(error, GuardianRepairPlanRejection::UnsupportedDiagnosis);
        assert!(!runtime_root.join(".croopor-ready").exists());
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
        assert!(!runtime_root.join(".croopor-ready").exists());

        cleanup(&root);
    }

    #[test]
    fn arbitrary_runtime_root_cannot_build_guardian_repair_request() {
        let root = test_root("root-binding");
        let paths = test_paths(&root);
        let runtime_root = root.join("user-runtime");
        let java_executable = runtime_root.join("bin").join("java");

        assert_eq!(
            ManagedRuntimeRoot::from_app_paths(&paths, &runtime_root, &java_executable)
                .expect_err("outside runtime root"),
            ManagedRuntimeRootError::UnsupportedRoot
        );
        cleanup(&root);
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
                decision.kind = GuardianDecisionKind::Block;
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
                    DiagnosisId::new("other_diagnosis");
                decision
            },
        ] {
            let _ = fs::remove_file(runtime_root.join(".croopor-ready"));
            let error = plan_managed_runtime_ready_marker_repair(
                &decision,
                GuardianRepairPlanningContext::current_operation(),
            )
            .expect_err("malformed policy rejects before execution");

            assert!(matches!(
                error,
                GuardianRepairPlanRejection::NonRepairDecision
                    | GuardianRepairPlanRejection::UnsupportedDiagnosis
            ));
            assert!(!runtime_root.join(".croopor-ready").exists());
        }

        cleanup(&root);
    }

    #[test]
    fn repair_attempt_limit_suppresses_without_active_cooldown() {
        let root = test_root("attempt-limit");
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
                    DiagnosisId::new("managed_runtime_corrupt"),
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

        let _ = fs::remove_file(runtime_root.join(".croopor-ready"));
        let outcome = execute_repair(
            &decision,
            &paths,
            &runtime_root,
            &java_executable,
            &stores,
            "2026-06-15T10:00:00Z",
            None,
        );

        assert_eq!(outcome.status, GuardianRepairStatus::Suppressed);
        assert!(!runtime_root.join(".croopor-ready").exists());
        let memory = stores.failure_memory.list();
        assert_eq!(memory[0].repair_attempt_count, 1);
        assert_eq!(
            memory[0].last_action_outcome,
            Some(FailureMemoryActionOutcome::Suppressed)
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
                    DiagnosisId::new("managed_runtime_corrupt"),
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

        let _ = fs::remove_file(runtime_root.join(".croopor-ready"));
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
        assert!(runtime_root.join(".croopor-ready").exists());
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
        assert!(!runtime_root.join(".croopor-ready").exists());
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
            &DiagnosisId::new("managed_runtime_corrupt"),
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
        let plan = repair_plan(decision);
        execute_managed_runtime_ready_marker_repair(request(
            &plan,
            decision.operation_id.clone(),
            decision.mode,
            paths,
            runtime_root,
            java_executable,
            stores,
            observed_at,
            suppression_until_on_failure,
        ))
    }

    #[allow(clippy::too_many_arguments)]
    fn request<'a>(
        plan: &'a GuardianRepairPlan,
        operation_id: Option<OperationId>,
        mode: GuardianMode,
        paths: &AppPaths,
        runtime_root: &'a Path,
        java_executable: &'a Path,
        stores: &'a Stores,
        observed_at: &'a str,
        suppression_until_on_failure: Option<&'a str>,
    ) -> GuardianManagedRuntimeRepairRequest<'a> {
        GuardianManagedRuntimeRepairRequest {
            operation_id,
            mode,
            plan,
            runtime_root: runtime_root_binding(paths, runtime_root, java_executable),
            journals: &stores.journals,
            failure_memory: &stores.failure_memory,
            observed_at,
            suppression_until_on_failure,
        }
    }

    fn repair_plan(decision: &GuardianDecision) -> GuardianRepairPlan {
        plan_managed_runtime_ready_marker_repair(
            decision,
            GuardianRepairPlanningContext::current_operation(),
        )
        .expect("runtime repair plan")
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
        let diagnosis_id = DiagnosisId::new("managed_runtime_corrupt");
        let prerequisite = ActionPlanPrerequisite {
            diagnosis_id: diagnosis_id.clone(),
            ownership,
            confidence: crate::guardian::GuardianConfidence::Confirmed,
            affected_targets: vec![target.clone()],
            candidate_actions: vec![GuardianActionKind::Repair],
        };
        GuardianDecision {
            operation_id: Some(operation_id),
            mode: GuardianMode::Managed,
            kind: GuardianDecisionKind::Repair,
            diagnoses: vec![diagnosis_id.clone()],
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
            runtime_root.join(".croopor-runtime-manifest.json"),
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
            "croopor-guardian-repair-{prefix}-{}-{nanos:x}",
            std::process::id()
        ))
    }

    fn cleanup(root: &Path) {
        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_file(root);
    }
}
