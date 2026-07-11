use super::{DiagnosisId, GuardianActionKind, GuardianDomain, GuardianMode};
use crate::observability::{RedactionAudience, sanitize_evidence_token};
use crate::state::contracts::{
    CommandKind, JournalId, OperationId, OperationJournalEntry, OperationJournalStep,
    OperationOutcome, OperationPhase, OperationStatus, OperationStepResult, OwnershipClass,
    RollbackState, StabilizationSystem, TargetDescriptor, TargetKind,
};
use crate::state::failure_memory::{
    FailureMemoryActionOutcome, FailureMemoryKey, GuardianFailureMemoryEntry,
    GuardianFailureMemoryStore,
};
use crate::state::{OperationJournalStore, OperationJournalStoreError};
use axial_launcher::LaunchFailureClass;
use chrono::{DateTime, Duration, FixedOffset};
use serde::{Deserialize, Serialize};
use tracing::warn;

const DEFAULT_LAUNCH_RECOVERY_SUPPRESSION_MINUTES: i64 = 30;
const LAUNCH_RECOVERY_MAX_PLAN_DEPTH: u8 = 1;
const LAUNCH_RECOVERY_MAX_ATTEMPTS: u8 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum GuardianLaunchRecoveryKind {
    SwitchManagedRuntime,
    StripRawJvmArgs,
    DowngradePreset,
    DisableCustomGc,
}

impl GuardianLaunchRecoveryKind {
    pub fn action_kind(self) -> GuardianActionKind {
        match self {
            Self::SwitchManagedRuntime => GuardianActionKind::Fallback,
            Self::StripRawJvmArgs | Self::DisableCustomGc => GuardianActionKind::Strip,
            Self::DowngradePreset => GuardianActionKind::Downgrade,
        }
    }

    fn step_id(self) -> &'static str {
        match self {
            Self::SwitchManagedRuntime => "launch_recovery_switch_managed_runtime",
            Self::StripRawJvmArgs => "launch_recovery_strip_raw_jvm_args",
            Self::DowngradePreset => "launch_recovery_downgrade_preset",
            Self::DisableCustomGc => "launch_recovery_disable_custom_gc",
        }
    }

    fn summary(self) -> &'static str {
        match self {
            Self::SwitchManagedRuntime => "guardian_launch_recovery_switch_managed_runtime",
            Self::StripRawJvmArgs => "guardian_launch_recovery_strip_raw_jvm_args",
            Self::DowngradePreset => "guardian_launch_recovery_downgrade_preset",
            Self::DisableCustomGc => "guardian_launch_recovery_disable_custom_gc",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct GuardianLaunchRecoveryDirective {
    pub kind: GuardianLaunchRecoveryKind,
    pub effect: GuardianLaunchRecoveryEffect,
    pub description: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum GuardianLaunchRecoveryEffect {
    ForceManagedRuntime,
    StripRawJvmArgs,
    DowngradePreset { preset: String },
    DisableCustomGc,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum GuardianLaunchRecoveryExecutor {
    LaunchAttemptOverride,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum GuardianLaunchRecoveryMutation {
    OneAttemptOverride,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum GuardianLaunchRecoveryReversibility {
    NextAttemptOnly,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct GuardianLaunchRecoveryActionTemplate {
    pub kind: GuardianLaunchRecoveryKind,
    pub action_kind: GuardianActionKind,
    pub executor: GuardianLaunchRecoveryExecutor,
    pub mutation: GuardianLaunchRecoveryMutation,
    pub reversibility: GuardianLaunchRecoveryReversibility,
    pub max_attempts: u8,
    pub public_summary_template: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct GuardianLaunchRecoveryPlan {
    pub operation_id: OperationId,
    pub mode: GuardianMode,
    pub target: TargetDescriptor,
    pub directive: GuardianLaunchRecoveryDirective,
    pub trigger_failure_class: LaunchFailureClass,
    pub action_template: GuardianLaunchRecoveryActionTemplate,
    pub max_depth: u8,
    pub user_intent_hash: Option<String>,
}

#[derive(Clone, Debug)]
pub struct GuardianLaunchRecoveryPlanRequest<'a> {
    pub instance_id: &'a str,
    pub mode: GuardianMode,
    pub directive: GuardianLaunchRecoveryDirective,
    pub failure_class: LaunchFailureClass,
    pub user_intent_hash: Option<&'a str>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum GuardianLaunchRecoveryPlanRejection {
    EmptyDirectiveDescription,
    MismatchedDirectiveEffect,
}

pub struct GuardianLaunchRecoveryRecordRequest<'a> {
    pub plan: &'a GuardianLaunchRecoveryPlan,
    pub failure_class: LaunchFailureClass,
    pub observed_at: &'a str,
    pub journals: &'a OperationJournalStore,
    pub failure_memory: &'a GuardianFailureMemoryStore,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct GuardianLaunchRecoveryOutcome {
    pub operation_id: OperationId,
    pub diagnosis_id: DiagnosisId,
    pub action: GuardianActionKind,
    pub status: GuardianLaunchRecoveryStatus,
    pub summary: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum GuardianLaunchRecoveryStatus {
    Recorded,
    Succeeded,
    Failed,
    Suppressed,
}

pub fn plan_launch_recovery_directive(
    request: GuardianLaunchRecoveryPlanRequest<'_>,
) -> Result<GuardianLaunchRecoveryPlan, GuardianLaunchRecoveryPlanRejection> {
    if request.directive.description.trim().is_empty() {
        return Err(GuardianLaunchRecoveryPlanRejection::EmptyDirectiveDescription);
    }
    if !directive_kind_matches_effect(&request.directive) {
        return Err(GuardianLaunchRecoveryPlanRejection::MismatchedDirectiveEffect);
    }

    Ok(GuardianLaunchRecoveryPlan {
        operation_id: new_launch_recovery_operation_id(request.directive.kind),
        mode: request.mode,
        target: launch_recovery_target(request.instance_id),
        action_template: launch_recovery_action_template(&request.directive),
        directive: request.directive,
        trigger_failure_class: request.failure_class,
        max_depth: LAUNCH_RECOVERY_MAX_PLAN_DEPTH,
        user_intent_hash: request
            .user_intent_hash
            .and_then(|value| sanitize_evidence_token(value, RedactionAudience::UserVisible, 96)),
    })
}

pub async fn record_launch_recovery_attempt(
    request: GuardianLaunchRecoveryRecordRequest<'_>,
) -> Result<GuardianLaunchRecoveryOutcome, OperationJournalStoreError> {
    let plan = request.plan;
    let diagnosis_id = launch_recovery_diagnosis_id(plan.directive.kind, request.failure_class);
    let operation_id = plan.operation_id.clone();
    let action = plan.action_template.action_kind;
    let memory_key = FailureMemoryKey::for_observation(
        GuardianDomain::Launch,
        &diagnosis_id,
        &plan.target,
        plan.mode,
        plan.user_intent_hash.as_deref(),
    );

    if let Some(entry) = request.failure_memory.get(&memory_key)
        && suppression_active(&entry, request.observed_at)
    {
        let suppression_until = entry.suppression_until.as_deref();
        if !launch_recovery_journal_matches(
            request.journals,
            &operation_id,
            OperationStatus::Blocked,
            OperationOutcome::Suppressed,
        ) {
            create_launch_recovery_terminal_journal(
                request.journals,
                &operation_id,
                &diagnosis_id,
                &plan.target,
                plan,
                OperationStatus::Blocked,
                OperationOutcome::Suppressed,
                OperationStepResult::Skipped,
            )
            .await?;
        }
        record_launch_recovery_memory(
            request.failure_memory,
            &diagnosis_id,
            plan.mode,
            &plan.target,
            action,
            FailureMemoryActionOutcome::Suppressed,
            request.observed_at,
            plan.user_intent_hash.as_deref(),
            suppression_until,
            false,
        );
        return Ok(launch_recovery_outcome(
            operation_id,
            diagnosis_id,
            action,
            GuardianLaunchRecoveryStatus::Suppressed,
            "guardian_launch_recovery_suppressed",
        ));
    }

    match request.journals.get(&operation_id) {
        Some(entry) if launch_recovery_planned_journal_matches(&entry, plan, &diagnosis_id) => {}
        Some(_) => return Err(OperationJournalStoreError::AlreadyTerminal),
        None => {
            create_launch_recovery_planned_journal(
                request.journals,
                &operation_id,
                &diagnosis_id,
                &plan.target,
                plan,
            )
            .await?;
        }
    }
    Ok(launch_recovery_outcome(
        operation_id,
        diagnosis_id,
        action,
        GuardianLaunchRecoveryStatus::Recorded,
        plan.directive.kind.summary(),
    ))
}

pub async fn record_launch_recovery_success(
    request: GuardianLaunchRecoveryRecordRequest<'_>,
) -> Result<GuardianLaunchRecoveryOutcome, OperationJournalStoreError> {
    let plan = request.plan;
    let diagnosis_id = launch_recovery_diagnosis_id(plan.directive.kind, request.failure_class);
    let operation_id = plan.operation_id.clone();
    let action = plan.action_template.action_kind;
    if !launch_recovery_journal_matches(
        request.journals,
        &operation_id,
        OperationStatus::Succeeded,
        OperationOutcome::Succeeded,
    ) {
        request
            .journals
            .record_success(
                &operation_id,
                launch_recovery_step(plan, OperationStepResult::Completed, &plan.target),
                OperationOutcome::Succeeded,
            )
            .await?;
    }
    record_launch_recovery_memory(
        request.failure_memory,
        &diagnosis_id,
        plan.mode,
        &plan.target,
        action,
        FailureMemoryActionOutcome::Retried,
        request.observed_at,
        plan.user_intent_hash.as_deref(),
        None,
        true,
    );
    Ok(launch_recovery_outcome(
        operation_id,
        diagnosis_id,
        action,
        GuardianLaunchRecoveryStatus::Succeeded,
        plan.directive.kind.summary(),
    ))
}

pub async fn record_launch_recovery_failure(
    request: GuardianLaunchRecoveryRecordRequest<'_>,
) -> Result<GuardianLaunchRecoveryOutcome, OperationJournalStoreError> {
    let plan = request.plan;
    let diagnosis_id = launch_recovery_diagnosis_id(plan.directive.kind, request.failure_class);
    let operation_id = plan.operation_id.clone();
    let action = plan.action_template.action_kind;
    let suppression_until = default_suppression_until(request.observed_at);
    if !launch_recovery_journal_matches(
        request.journals,
        &operation_id,
        OperationStatus::Failed,
        OperationOutcome::Failed,
    ) {
        request
            .journals
            .record_failure(
                &operation_id,
                launch_recovery_step(plan, OperationStepResult::Failed, &plan.target),
                plan.directive.kind.step_id(),
                OperationOutcome::Failed,
            )
            .await?;
    }
    record_launch_recovery_memory(
        request.failure_memory,
        &diagnosis_id,
        plan.mode,
        &plan.target,
        action,
        FailureMemoryActionOutcome::Failed,
        request.observed_at,
        plan.user_intent_hash.as_deref(),
        suppression_until.as_deref(),
        true,
    );
    Ok(launch_recovery_outcome(
        operation_id,
        diagnosis_id,
        action,
        GuardianLaunchRecoveryStatus::Failed,
        "guardian_launch_recovery_failed",
    ))
}

fn launch_recovery_diagnosis_id(
    kind: GuardianLaunchRecoveryKind,
    failure_class: LaunchFailureClass,
) -> DiagnosisId {
    let id = match (kind, failure_class) {
        (GuardianLaunchRecoveryKind::SwitchManagedRuntime, _) => "java_runtime_recovery",
        (
            GuardianLaunchRecoveryKind::StripRawJvmArgs
            | GuardianLaunchRecoveryKind::DisableCustomGc,
            LaunchFailureClass::JvmUnsupportedOption
            | LaunchFailureClass::JvmExperimentalUnlock
            | LaunchFailureClass::JvmOptionOrdering,
        ) => "jvm_arg_unsupported",
        (GuardianLaunchRecoveryKind::DowngradePreset, _) => "jvm_preset_recovery",
        _ => "launch_startup_recovery",
    };
    DiagnosisId::new(id)
}

fn directive_kind_matches_effect(directive: &GuardianLaunchRecoveryDirective) -> bool {
    matches!(
        (&directive.kind, &directive.effect),
        (
            GuardianLaunchRecoveryKind::SwitchManagedRuntime,
            GuardianLaunchRecoveryEffect::ForceManagedRuntime
        ) | (
            GuardianLaunchRecoveryKind::StripRawJvmArgs,
            GuardianLaunchRecoveryEffect::StripRawJvmArgs
        ) | (
            GuardianLaunchRecoveryKind::DowngradePreset,
            GuardianLaunchRecoveryEffect::DowngradePreset { .. }
        ) | (
            GuardianLaunchRecoveryKind::DisableCustomGc,
            GuardianLaunchRecoveryEffect::DisableCustomGc
        )
    )
}

fn launch_recovery_action_template(
    directive: &GuardianLaunchRecoveryDirective,
) -> GuardianLaunchRecoveryActionTemplate {
    GuardianLaunchRecoveryActionTemplate {
        kind: directive.kind,
        action_kind: directive.kind.action_kind(),
        executor: GuardianLaunchRecoveryExecutor::LaunchAttemptOverride,
        mutation: GuardianLaunchRecoveryMutation::OneAttemptOverride,
        reversibility: GuardianLaunchRecoveryReversibility::NextAttemptOnly,
        max_attempts: LAUNCH_RECOVERY_MAX_ATTEMPTS,
        public_summary_template: directive.kind.summary().to_string(),
    }
}

fn launch_recovery_target(instance_id: &str) -> TargetDescriptor {
    TargetDescriptor::new(
        StabilizationSystem::Guardian,
        TargetKind::Instance,
        instance_id,
        OwnershipClass::LauncherManaged,
    )
}

#[allow(clippy::too_many_arguments)]
async fn create_launch_recovery_planned_journal(
    journals: &OperationJournalStore,
    operation_id: &OperationId,
    diagnosis_id: &DiagnosisId,
    target: &TargetDescriptor,
    plan: &GuardianLaunchRecoveryPlan,
) -> Result<(), OperationJournalStoreError> {
    let mut entry = OperationJournalEntry::new(
        JournalId::new(format!("journal-{}", operation_id.as_str())),
        operation_id.clone(),
        CommandKind::LaunchInstance,
        StabilizationSystem::Guardian,
        target.ownership,
        RollbackState::NotApplicable,
    );
    entry.targets.push(target.clone());
    entry
        .guardian_diagnosis_ids
        .push(safe_id(diagnosis_id.as_str(), "diagnosis"));
    entry.planned_steps.push(launch_recovery_step(
        plan,
        OperationStepResult::Planned,
        target,
    ));
    journals.create(entry).await
}

#[allow(clippy::too_many_arguments)]
async fn create_launch_recovery_terminal_journal(
    journals: &OperationJournalStore,
    operation_id: &OperationId,
    diagnosis_id: &DiagnosisId,
    target: &TargetDescriptor,
    plan: &GuardianLaunchRecoveryPlan,
    status: OperationStatus,
    outcome: OperationOutcome,
    step_result: OperationStepResult,
) -> Result<(), OperationJournalStoreError> {
    let mut entry = OperationJournalEntry::new(
        JournalId::new(format!("journal-{}", operation_id.as_str())),
        operation_id.clone(),
        CommandKind::LaunchInstance,
        StabilizationSystem::Guardian,
        target.ownership,
        RollbackState::NotApplicable,
    );
    entry.status = status;
    entry.targets.push(target.clone());
    entry
        .guardian_diagnosis_ids
        .push(safe_id(diagnosis_id.as_str(), "diagnosis"));
    entry.planned_steps.push(launch_recovery_step(
        plan,
        OperationStepResult::Planned,
        target,
    ));
    entry
        .completed_steps
        .push(launch_recovery_step(plan, step_result, target));
    entry.outcome = Some(outcome);
    journals.create(entry).await
}

fn launch_recovery_journal_matches(
    journals: &OperationJournalStore,
    operation_id: &OperationId,
    status: OperationStatus,
    outcome: OperationOutcome,
) -> bool {
    journals
        .get(operation_id)
        .is_some_and(|entry| entry.status == status && entry.outcome == Some(outcome))
}

fn launch_recovery_planned_journal_matches(
    entry: &OperationJournalEntry,
    plan: &GuardianLaunchRecoveryPlan,
    diagnosis_id: &DiagnosisId,
) -> bool {
    entry.status == OperationStatus::Planned
        && entry.outcome.is_none()
        && entry.targets.iter().any(|target| target == &plan.target)
        && entry
            .guardian_diagnosis_ids
            .iter()
            .any(|existing| existing == diagnosis_id.as_str())
        && entry
            .planned_steps
            .iter()
            .any(|step| step.step_id == plan.directive.kind.step_id())
}

fn launch_recovery_step(
    plan: &GuardianLaunchRecoveryPlan,
    result: OperationStepResult,
    target: &TargetDescriptor,
) -> OperationJournalStep {
    let mut step =
        OperationJournalStep::new(plan.directive.kind.step_id(), OperationPhase::Repairing);
    step.result = result;
    step.changed_target = Some(target.clone());
    step.generated_facts = vec![plan.action_template.public_summary_template.clone()];
    step.rollback = RollbackState::NotApplicable;
    step
}

#[allow(clippy::too_many_arguments)]
fn record_launch_recovery_memory(
    failure_memory: &GuardianFailureMemoryStore,
    diagnosis_id: &DiagnosisId,
    mode: GuardianMode,
    target: &TargetDescriptor,
    action: GuardianActionKind,
    outcome: FailureMemoryActionOutcome,
    observed_at: &str,
    user_intent_hash: Option<&str>,
    suppression_until: Option<&str>,
    repair_attempt: bool,
) {
    let mut entry = GuardianFailureMemoryEntry::observed(
        diagnosis_id.clone(),
        GuardianDomain::Launch,
        target.clone(),
        mode,
        user_intent_hash,
        observed_at,
    )
    .with_action(action, outcome);
    if repair_attempt {
        entry = entry.with_repair_attempt();
    }
    if let Some(suppression_until) = suppression_until {
        entry = entry.with_suppression_until(suppression_until);
    }
    if let Err(error) = failure_memory.record(entry) {
        warn!(
            error_kind = error.class(),
            "failed to record Guardian launch-recovery failure memory"
        );
    }
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

fn default_suppression_until(observed_at: &str) -> Option<String> {
    DateTime::parse_from_rfc3339(observed_at)
        .ok()
        .map(|observed_at| {
            (observed_at + Duration::minutes(DEFAULT_LAUNCH_RECOVERY_SUPPRESSION_MINUTES))
                .to_rfc3339()
        })
}

fn launch_recovery_outcome(
    operation_id: OperationId,
    diagnosis_id: DiagnosisId,
    action: GuardianActionKind,
    status: GuardianLaunchRecoveryStatus,
    summary: &str,
) -> GuardianLaunchRecoveryOutcome {
    GuardianLaunchRecoveryOutcome {
        operation_id,
        diagnosis_id: DiagnosisId::new(safe_id(diagnosis_id.as_str(), "diagnosis")),
        action,
        status,
        summary: safe_id(summary, "guardian_launch_recovery_outcome"),
    }
}

fn new_launch_recovery_operation_id(kind: GuardianLaunchRecoveryKind) -> OperationId {
    OperationId::new(format!(
        "launch-recovery-{}-{}",
        kind.step_id(),
        uuid::Uuid::new_v4()
    ))
}

fn safe_id(value: &str, fallback: &str) -> String {
    sanitize_evidence_token(value, RedactionAudience::UserVisible, 96)
        .unwrap_or_else(|| fallback.to_string())
}

#[cfg(test)]
mod tests {
    use super::{
        GuardianLaunchRecoveryDirective, GuardianLaunchRecoveryEffect, GuardianLaunchRecoveryKind,
        GuardianLaunchRecoveryPlan, GuardianLaunchRecoveryPlanRejection,
        GuardianLaunchRecoveryPlanRequest, GuardianLaunchRecoveryRecordRequest,
        GuardianLaunchRecoveryStatus, plan_launch_recovery_directive,
        record_launch_recovery_attempt, record_launch_recovery_failure,
        record_launch_recovery_success,
    };
    use crate::execution::file::{FileWriteRequest, write_file_atomically};
    use crate::execution::persistence::{AtomicWriteBackend, PersistenceCoordinator};
    use crate::guardian::{GuardianActionKind, GuardianMode};
    use crate::state::OperationJournalStore;
    use crate::state::contracts::{CommandKind, OperationStatus, OwnershipClass, TargetKind};
    use crate::state::failure_memory::{
        FailureMemoryActionOutcome, FailureMemorySnapshot, GuardianFailureMemoryStore,
        failure_memory_path,
    };
    use axial_config::AppPaths;
    use axial_launcher::LaunchFailureClass;
    use std::fs;
    use std::io;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct FailingWriteBackend;

    struct FailOnAttemptBackend {
        attempts: AtomicUsize,
        fail_on_attempt: usize,
    }

    impl AtomicWriteBackend for FailingWriteBackend {
        fn write(
            &self,
            _target: &crate::state::contracts::TargetDescriptor,
            _destination: &Path,
            _contents: &[u8],
        ) -> io::Result<()> {
            Err(io::Error::other("injected launch-recovery journal failure"))
        }
    }

    impl AtomicWriteBackend for FailOnAttemptBackend {
        fn write(
            &self,
            target: &crate::state::contracts::TargetDescriptor,
            destination: &Path,
            contents: &[u8],
        ) -> io::Result<()> {
            let attempt = self.attempts.fetch_add(1, Ordering::SeqCst) + 1;
            if attempt == self.fail_on_attempt {
                return Err(io::Error::other(
                    "injected launch-recovery terminal failure",
                ));
            }
            write_file_atomically(FileWriteRequest::new(target.clone(), destination, contents))
                .map(|_| ())
                .map_err(io::Error::from)
        }
    }

    #[tokio::test]
    async fn launch_recovery_attempt_records_journal_and_memory() {
        let journals = OperationJournalStore::new();
        let failure_memory = GuardianFailureMemoryStore::new();
        let plan = plan(
            "session-1",
            GuardianLaunchRecoveryKind::StripRawJvmArgs,
            None,
        );

        let outcome = record_launch_recovery_attempt(request(
            &plan,
            LaunchFailureClass::JvmUnsupportedOption,
            "2026-06-15T10:00:00Z",
            &journals,
            &failure_memory,
        ))
        .await
        .expect("persist launch recovery attempt");

        assert_eq!(outcome.status, GuardianLaunchRecoveryStatus::Recorded);
        assert_eq!(outcome.action, GuardianActionKind::Strip);
        let journal = journals
            .latest_for_command(CommandKind::LaunchInstance)
            .expect("launch recovery journal");
        assert_eq!(journal.status, OperationStatus::Planned);
        assert_eq!(journal.outcome, None);
        assert_eq!(journal.planned_steps.len(), 1);
        assert_eq!(journal.completed_steps.len(), 0);

        assert!(failure_memory.list().is_empty());
    }

    #[tokio::test]
    async fn launch_recovery_success_terminalizes_the_same_operation() {
        let journals = OperationJournalStore::new();
        let failure_memory = GuardianFailureMemoryStore::new();
        let plan = plan(
            "session-success",
            GuardianLaunchRecoveryKind::StripRawJvmArgs,
            None,
        );
        let attempt = record_launch_recovery_attempt(request(
            &plan,
            LaunchFailureClass::JvmUnsupportedOption,
            "2026-06-15T10:00:00Z",
            &journals,
            &failure_memory,
        ))
        .await
        .expect("persist launch recovery attempt");

        let outcome = record_launch_recovery_success(request(
            &plan,
            LaunchFailureClass::JvmUnsupportedOption,
            "2026-06-15T10:01:00Z",
            &journals,
            &failure_memory,
        ))
        .await
        .expect("persist launch recovery success");

        assert_eq!(outcome.operation_id, attempt.operation_id);
        assert_eq!(outcome.status, GuardianLaunchRecoveryStatus::Succeeded);
        let journal = journals
            .get(&outcome.operation_id)
            .expect("recovery journal");
        assert_eq!(journal.status, OperationStatus::Succeeded);
        assert_eq!(journal.completed_steps.len(), 1);
        let memory = failure_memory.list();
        assert_eq!(memory.len(), 1);
        assert_eq!(memory[0].last_action_kind, Some(GuardianActionKind::Strip));
        assert_eq!(
            memory[0].last_action_outcome,
            Some(FailureMemoryActionOutcome::Retried)
        );
        assert_eq!(memory[0].repair_attempt_count, 1);
    }

    #[tokio::test]
    async fn terminal_commit_retry_reconciles_the_same_failure_operation() {
        let root = test_root("terminal-commit-retry");
        let paths = test_paths(&root);
        let coordinator = PersistenceCoordinator::for_test(
            Arc::new(FailOnAttemptBackend {
                attempts: AtomicUsize::new(0),
                fail_on_attempt: 2,
            }),
            std::time::Duration::from_millis(20),
            std::time::Duration::from_millis(100),
        );
        let journals =
            OperationJournalStore::try_load_from_paths_with_coordinator(&paths, coordinator)
                .expect("claim operation journal persistence");
        let failure_memory = GuardianFailureMemoryStore::new();
        let plan = plan(
            "session-terminal-retry",
            GuardianLaunchRecoveryKind::SwitchManagedRuntime,
            None,
        );
        let attempt = record_launch_recovery_attempt(request(
            &plan,
            LaunchFailureClass::JavaRuntimeMismatch,
            "2026-06-15T10:00:00Z",
            &journals,
            &failure_memory,
        ))
        .await
        .expect("persist launch recovery attempt");

        assert!(
            record_launch_recovery_failure(request(
                &plan,
                LaunchFailureClass::JavaRuntimeMismatch,
                "2026-06-15T10:01:00Z",
                &journals,
                &failure_memory,
            ))
            .await
            .is_err()
        );
        assert_eq!(
            journals
                .get(&attempt.operation_id)
                .expect("planned journal")
                .status,
            OperationStatus::Planned
        );
        assert!(failure_memory.list().is_empty());

        journals.retry().await.expect("retry terminal candidate");
        let outcome = record_launch_recovery_failure(request(
            &plan,
            LaunchFailureClass::JavaRuntimeMismatch,
            "2026-06-15T10:01:00Z",
            &journals,
            &failure_memory,
        ))
        .await
        .expect("reconcile launch recovery failure");

        assert_eq!(outcome.operation_id, attempt.operation_id);
        assert_eq!(journals.list().len(), 1);
        assert_eq!(
            journals
                .get(&outcome.operation_id)
                .expect("failed journal")
                .status,
            OperationStatus::Failed
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn launch_recovery_commit_failure_returns_no_outcome_or_memory() {
        let root = test_root("attempt-commit-gate");
        let paths = test_paths(&root);
        let coordinator = PersistenceCoordinator::for_test(
            Arc::new(FailingWriteBackend),
            std::time::Duration::from_millis(20),
            std::time::Duration::from_millis(100),
        );
        let journals =
            OperationJournalStore::try_load_from_paths_with_coordinator(&paths, coordinator)
                .expect("claim operation journal persistence");
        let failure_memory = GuardianFailureMemoryStore::new();
        let plan = plan(
            "session-journal-failure",
            GuardianLaunchRecoveryKind::StripRawJvmArgs,
            None,
        );

        let result = record_launch_recovery_attempt(request(
            &plan,
            LaunchFailureClass::JvmUnsupportedOption,
            "2026-06-15T10:00:00Z",
            &journals,
            &failure_memory,
        ))
        .await;

        assert!(result.is_err());
        assert!(failure_memory.list().is_empty());
        assert!(
            journals
                .latest_for_command(CommandKind::LaunchInstance)
                .is_none()
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn launch_recovery_failure_sets_suppression_window() {
        let journals = OperationJournalStore::new();
        let failure_memory = GuardianFailureMemoryStore::new();
        let plan = plan(
            "session-2",
            GuardianLaunchRecoveryKind::SwitchManagedRuntime,
            Some("java_override_present"),
        );

        record_launch_recovery_attempt(request(
            &plan,
            LaunchFailureClass::JavaRuntimeMismatch,
            "2026-06-15T09:59:00Z",
            &journals,
            &failure_memory,
        ))
        .await
        .expect("persist launch recovery attempt");

        let outcome = record_launch_recovery_failure(request(
            &plan,
            LaunchFailureClass::JavaRuntimeMismatch,
            "2026-06-15T10:00:00Z",
            &journals,
            &failure_memory,
        ))
        .await
        .expect("persist launch recovery failure");

        assert_eq!(outcome.status, GuardianLaunchRecoveryStatus::Failed);
        let memory = failure_memory.list();
        assert_eq!(memory.len(), 1);
        assert_eq!(
            memory[0].last_action_outcome,
            Some(FailureMemoryActionOutcome::Failed)
        );
        assert_eq!(
            memory[0].suppression_until.as_deref(),
            Some("2026-06-15T10:30:00+00:00")
        );
        assert_eq!(memory[0].repair_attempt_count, 1);
    }

    #[tokio::test]
    async fn later_session_for_same_instance_is_suppressed_and_merges_memory() {
        let journals = OperationJournalStore::new();
        let failure_memory = GuardianFailureMemoryStore::new();
        let initial_plan = plan(
            "instance-3",
            GuardianLaunchRecoveryKind::DowngradePreset,
            Some("preset_override_present"),
        );

        record_launch_recovery_attempt(request(
            &initial_plan,
            LaunchFailureClass::JvmUnsupportedOption,
            "2026-06-15T09:59:00Z",
            &journals,
            &failure_memory,
        ))
        .await
        .expect("persist launch recovery attempt");

        let _ = record_launch_recovery_failure(request(
            &initial_plan,
            LaunchFailureClass::JvmUnsupportedOption,
            "2026-06-15T10:00:00Z",
            &journals,
            &failure_memory,
        ))
        .await
        .expect("persist launch recovery failure");

        let suppressed_plan = plan(
            "instance-3",
            GuardianLaunchRecoveryKind::DowngradePreset,
            Some("preset_override_present"),
        );

        let outcome = record_launch_recovery_attempt(request(
            &suppressed_plan,
            LaunchFailureClass::JvmUnsupportedOption,
            "2026-06-15T10:05:00Z",
            &journals,
            &failure_memory,
        ))
        .await
        .expect("persist suppressed launch recovery attempt");

        assert_eq!(outcome.status, GuardianLaunchRecoveryStatus::Suppressed);
        assert_ne!(initial_plan.operation_id, suppressed_plan.operation_id);
        let memory = failure_memory.list();
        assert_eq!(memory.len(), 1);
        assert_eq!(memory[0].target.kind, TargetKind::Instance);
        assert_eq!(memory[0].target.id, "instance-3");
        assert_eq!(memory[0].occurrence_count, 2);
        assert_eq!(
            memory[0].last_action_outcome,
            Some(FailureMemoryActionOutcome::Suppressed)
        );
    }

    #[tokio::test]
    async fn launch_recovery_suppression_is_independent_between_instances() {
        let journals = OperationJournalStore::new();
        let failure_memory = GuardianFailureMemoryStore::new();
        let first_plan = plan(
            "instance-a",
            GuardianLaunchRecoveryKind::DowngradePreset,
            Some("preset_override_present"),
        );

        record_launch_recovery_attempt(request(
            &first_plan,
            LaunchFailureClass::JvmUnsupportedOption,
            "2026-06-15T09:59:00Z",
            &journals,
            &failure_memory,
        ))
        .await
        .expect("persist first instance recovery attempt");
        record_launch_recovery_failure(request(
            &first_plan,
            LaunchFailureClass::JvmUnsupportedOption,
            "2026-06-15T10:00:00Z",
            &journals,
            &failure_memory,
        ))
        .await
        .expect("persist first instance recovery failure");

        let second_plan = plan(
            "instance-b",
            GuardianLaunchRecoveryKind::DowngradePreset,
            Some("preset_override_present"),
        );
        let outcome = record_launch_recovery_attempt(request(
            &second_plan,
            LaunchFailureClass::JvmUnsupportedOption,
            "2026-06-15T10:05:00Z",
            &journals,
            &failure_memory,
        ))
        .await
        .expect("persist independent second instance recovery attempt");

        assert_eq!(outcome.status, GuardianLaunchRecoveryStatus::Recorded);
        record_launch_recovery_failure(request(
            &second_plan,
            LaunchFailureClass::JvmUnsupportedOption,
            "2026-06-15T10:06:00Z",
            &journals,
            &failure_memory,
        ))
        .await
        .expect("persist independent second instance recovery failure");
        let memory = failure_memory.list();
        assert_eq!(memory.len(), 2);
        assert!(memory.iter().any(|entry| entry.target.id == "instance-a"));
        assert!(memory.iter().any(|entry| entry.target.id == "instance-b"));
    }

    #[tokio::test]
    async fn launch_recovery_suppression_survives_failure_memory_store_reload() {
        let root = test_root("restart-suppression");
        let paths = test_paths(&root);
        let journals = OperationJournalStore::new();
        let first_memory = GuardianFailureMemoryStore::load_from_paths(&paths);
        let initial_plan = plan(
            "session-restart",
            GuardianLaunchRecoveryKind::DowngradePreset,
            Some("preset_override_present"),
        );

        record_launch_recovery_attempt(request(
            &initial_plan,
            LaunchFailureClass::JvmUnsupportedOption,
            "2026-06-15T09:59:00Z",
            &journals,
            &first_memory,
        ))
        .await
        .expect("persist launch recovery attempt");

        let _ = record_launch_recovery_failure(request(
            &initial_plan,
            LaunchFailureClass::JvmUnsupportedOption,
            "2026-06-15T10:00:00Z",
            &journals,
            &first_memory,
        ))
        .await
        .expect("persist launch recovery failure");

        first_memory.flush().await.expect("flush failure memory");
        first_memory.close().await.expect("close failure memory");
        drop(first_memory);
        let reloaded_memory = reload_failure_memory(&paths);
        let suppressed_plan = plan(
            "session-restart",
            GuardianLaunchRecoveryKind::DowngradePreset,
            Some("preset_override_present"),
        );
        let outcome = record_launch_recovery_attempt(request(
            &suppressed_plan,
            LaunchFailureClass::JvmUnsupportedOption,
            "2026-06-15T10:05:00Z",
            &journals,
            &reloaded_memory,
        ))
        .await
        .expect("persist suppressed launch recovery attempt");

        assert_eq!(outcome.status, GuardianLaunchRecoveryStatus::Suppressed);
        let memory = reloaded_memory.list();
        assert_eq!(memory.len(), 1);
        assert_eq!(memory[0].occurrence_count, 2);
        assert_eq!(
            memory[0].last_action_outcome,
            Some(FailureMemoryActionOutcome::Suppressed)
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn expired_launch_recovery_suppression_allows_recovery_after_reload() {
        let root = test_root("expired-suppression");
        let paths = test_paths(&root);
        let journals = OperationJournalStore::new();
        let first_memory = GuardianFailureMemoryStore::load_from_paths(&paths);
        let initial_plan = plan(
            "session-expired",
            GuardianLaunchRecoveryKind::SwitchManagedRuntime,
            Some("java_override_present"),
        );

        record_launch_recovery_attempt(request(
            &initial_plan,
            LaunchFailureClass::JavaRuntimeMismatch,
            "2026-06-15T09:59:00Z",
            &journals,
            &first_memory,
        ))
        .await
        .expect("persist launch recovery attempt");

        let _ = record_launch_recovery_failure(request(
            &initial_plan,
            LaunchFailureClass::JavaRuntimeMismatch,
            "2026-06-15T10:00:00Z",
            &journals,
            &first_memory,
        ))
        .await
        .expect("persist launch recovery failure");

        first_memory.flush().await.expect("flush failure memory");
        first_memory.close().await.expect("close failure memory");
        drop(first_memory);
        let reloaded_memory = reload_failure_memory(&paths);
        let retry_plan = plan(
            "session-expired",
            GuardianLaunchRecoveryKind::SwitchManagedRuntime,
            Some("java_override_present"),
        );
        let attempt = record_launch_recovery_attempt(request(
            &retry_plan,
            LaunchFailureClass::JavaRuntimeMismatch,
            "2026-06-15T10:31:00Z",
            &journals,
            &reloaded_memory,
        ))
        .await
        .expect("persist launch recovery attempt");

        assert_eq!(attempt.status, GuardianLaunchRecoveryStatus::Recorded);
        assert_eq!(
            reloaded_memory.list()[0].last_action_outcome,
            Some(FailureMemoryActionOutcome::Failed)
        );
        record_launch_recovery_success(request(
            &retry_plan,
            LaunchFailureClass::JavaRuntimeMismatch,
            "2026-06-15T10:32:00Z",
            &journals,
            &reloaded_memory,
        ))
        .await
        .expect("persist launch recovery success");
        let memory = reloaded_memory.list();
        assert_eq!(memory.len(), 1);
        assert_eq!(
            memory[0].last_action_outcome,
            Some(FailureMemoryActionOutcome::Retried)
        );
        assert_eq!(memory[0].repair_attempt_count, 2);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn launch_recovery_directive_plan_declares_action_template() {
        let plan = plan(
            "instance-4",
            GuardianLaunchRecoveryKind::SwitchManagedRuntime,
            Some("java_override_present:1.21.1"),
        );

        assert_eq!(
            plan.action_template.kind,
            GuardianLaunchRecoveryKind::SwitchManagedRuntime
        );
        assert_eq!(
            plan.action_template.action_kind,
            GuardianActionKind::Fallback
        );
        assert_eq!(plan.action_template.max_attempts, 1);
        assert_eq!(plan.max_depth, 1);
        assert_eq!(plan.target.kind, TargetKind::Instance);
        assert_eq!(plan.target.id, "instance-4");
        assert_eq!(plan.target.ownership, OwnershipClass::LauncherManaged);
        assert!(
            plan.user_intent_hash
                .as_deref()
                .is_some_and(|value| value.contains("java_override_present"))
        );
        assert!(
            !serde_json::to_string(&plan)
                .expect("plan json")
                .contains("/home")
        );
    }

    #[test]
    fn launch_recovery_plan_rejects_mismatched_directive_effect() {
        let rejection = plan_launch_recovery_directive(GuardianLaunchRecoveryPlanRequest {
            instance_id: "instance-5",
            mode: GuardianMode::Managed,
            directive: GuardianLaunchRecoveryDirective {
                kind: GuardianLaunchRecoveryKind::StripRawJvmArgs,
                effect: GuardianLaunchRecoveryEffect::ForceManagedRuntime,
                description: "invalid directive".to_string(),
            },
            failure_class: LaunchFailureClass::JvmUnsupportedOption,
            user_intent_hash: None,
        })
        .expect_err("mismatched directive rejected");

        assert_eq!(
            rejection,
            GuardianLaunchRecoveryPlanRejection::MismatchedDirectiveEffect
        );
    }

    fn request<'a>(
        plan: &'a GuardianLaunchRecoveryPlan,
        failure_class: LaunchFailureClass,
        observed_at: &'a str,
        journals: &'a OperationJournalStore,
        failure_memory: &'a GuardianFailureMemoryStore,
    ) -> GuardianLaunchRecoveryRecordRequest<'a> {
        GuardianLaunchRecoveryRecordRequest {
            plan,
            failure_class,
            observed_at,
            journals,
            failure_memory,
        }
    }

    fn plan(
        instance_id: &str,
        kind: GuardianLaunchRecoveryKind,
        user_intent_hash: Option<&str>,
    ) -> GuardianLaunchRecoveryPlan {
        plan_launch_recovery_directive(GuardianLaunchRecoveryPlanRequest {
            instance_id,
            mode: GuardianMode::Managed,
            directive: directive(kind),
            failure_class: LaunchFailureClass::JvmUnsupportedOption,
            user_intent_hash,
        })
        .expect("launch recovery plan")
    }

    fn directive(kind: GuardianLaunchRecoveryKind) -> GuardianLaunchRecoveryDirective {
        match kind {
	            GuardianLaunchRecoveryKind::SwitchManagedRuntime => GuardianLaunchRecoveryDirective {
	                kind,
	                effect: GuardianLaunchRecoveryEffect::ForceManagedRuntime,
	                description: "Guardian switched to managed Java before launch".to_string(),
	            },
	            GuardianLaunchRecoveryKind::StripRawJvmArgs => GuardianLaunchRecoveryDirective {
	                kind,
	                effect: GuardianLaunchRecoveryEffect::StripRawJvmArgs,
	                description: "Guardian removed incompatible explicit JVM args before launch"
	                    .to_string(),
	            },
	            GuardianLaunchRecoveryKind::DowngradePreset => GuardianLaunchRecoveryDirective {
	                kind,
	                effect: GuardianLaunchRecoveryEffect::DowngradePreset {
	                    preset: "performance".to_string(),
	                },
	                description: "Automatic retry: downgraded JVM preset to \"performance\" after startup failure"
	                    .to_string(),
	            },
	            GuardianLaunchRecoveryKind::DisableCustomGc => GuardianLaunchRecoveryDirective {
	                kind,
	                effect: GuardianLaunchRecoveryEffect::DisableCustomGc,
	                description: "Automatic retry: disabled custom GC flags after startup failure"
	                    .to_string(),
	            },
        }
    }

    fn reload_failure_memory(paths: &AppPaths) -> GuardianFailureMemoryStore {
        let encoded =
            fs::read_to_string(failure_memory_path(paths)).expect("read persisted failure memory");
        let snapshot =
            FailureMemorySnapshot::from_json(&encoded).expect("decode persisted failure memory");
        let store = GuardianFailureMemoryStore::new();
        store
            .load_snapshot(snapshot)
            .expect("reload persisted failure memory");
        store
    }

    fn test_root(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "axial-launch-recovery-{name}-{}",
            uuid::Uuid::new_v4()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("create test root");
        root
    }

    fn test_paths(root: &Path) -> AppPaths {
        let config_dir = root.join("config");
        AppPaths {
            config_file: config_dir.join("config.json"),
            instances_file: config_dir.join("instances.json"),
            instances_dir: root.join("instances"),
            music_dir: root.join("music"),
            library_dir: root.join("library"),
            config_dir,
        }
    }
}
