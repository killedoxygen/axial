use super::{DiagnosisId, GuardianActionKind, GuardianDomain, GuardianMode};
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
use croopor_launcher::LaunchFailureClass;
use serde::{Deserialize, Serialize};

const DEFAULT_LAUNCH_RECOVERY_SUPPRESSION_MINUTES: i64 = 30;

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

pub struct GuardianLaunchRecoveryRequest<'a> {
    pub session_id: &'a str,
    pub mode: GuardianMode,
    pub kind: GuardianLaunchRecoveryKind,
    pub failure_class: LaunchFailureClass,
    pub observed_at: &'a str,
    pub user_intent_hash: Option<&'a str>,
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
    Failed,
    Suppressed,
}

pub fn record_launch_recovery_attempt(
    request: GuardianLaunchRecoveryRequest<'_>,
) -> GuardianLaunchRecoveryOutcome {
    let diagnosis_id = launch_recovery_diagnosis_id(request.kind, request.failure_class);
    let target = launch_recovery_target(request.session_id);
    let operation_id = new_launch_recovery_operation_id(request.session_id, request.kind);
    let action = request.kind.action_kind();
    let memory_key = FailureMemoryKey::for_observation(
        GuardianDomain::Launch,
        &diagnosis_id,
        &target,
        request.mode,
        request.user_intent_hash,
    );

    if let Some(entry) = request.failure_memory.get(&memory_key)
        && suppression_active(&entry, request.observed_at)
    {
        let suppression_until = entry.suppression_until.as_deref();
        record_launch_recovery_memory(
            request.failure_memory,
            &diagnosis_id,
            request.mode,
            &target,
            action,
            FailureMemoryActionOutcome::Suppressed,
            request.observed_at,
            request.user_intent_hash,
            suppression_until,
            false,
        );
        create_launch_recovery_journal(
            request.journals,
            &operation_id,
            &diagnosis_id,
            &target,
            request.kind,
            OperationStatus::Blocked,
            OperationOutcome::Suppressed,
            OperationStepResult::Skipped,
        );
        return launch_recovery_outcome(
            operation_id,
            diagnosis_id,
            action,
            GuardianLaunchRecoveryStatus::Suppressed,
            "guardian_launch_recovery_suppressed",
        );
    }

    record_launch_recovery_memory(
        request.failure_memory,
        &diagnosis_id,
        request.mode,
        &target,
        action,
        FailureMemoryActionOutcome::Retried,
        request.observed_at,
        request.user_intent_hash,
        None,
        true,
    );
    create_launch_recovery_journal(
        request.journals,
        &operation_id,
        &diagnosis_id,
        &target,
        request.kind,
        OperationStatus::Succeeded,
        OperationOutcome::Succeeded,
        OperationStepResult::Completed,
    );
    launch_recovery_outcome(
        operation_id,
        diagnosis_id,
        action,
        GuardianLaunchRecoveryStatus::Recorded,
        request.kind.summary(),
    )
}

pub fn record_launch_recovery_failure(
    request: GuardianLaunchRecoveryRequest<'_>,
) -> GuardianLaunchRecoveryOutcome {
    let diagnosis_id = launch_recovery_diagnosis_id(request.kind, request.failure_class);
    let target = launch_recovery_target(request.session_id);
    let operation_id = new_launch_recovery_operation_id(request.session_id, request.kind);
    let action = request.kind.action_kind();
    let suppression_until = default_suppression_until(request.observed_at);
    record_launch_recovery_memory(
        request.failure_memory,
        &diagnosis_id,
        request.mode,
        &target,
        action,
        FailureMemoryActionOutcome::Failed,
        request.observed_at,
        request.user_intent_hash,
        suppression_until.as_deref(),
        false,
    );
    create_launch_recovery_journal(
        request.journals,
        &operation_id,
        &diagnosis_id,
        &target,
        request.kind,
        OperationStatus::Failed,
        OperationOutcome::Failed,
        OperationStepResult::Failed,
    );
    launch_recovery_outcome(
        operation_id,
        diagnosis_id,
        action,
        GuardianLaunchRecoveryStatus::Failed,
        "guardian_launch_recovery_failed",
    )
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

fn launch_recovery_target(session_id: &str) -> TargetDescriptor {
    TargetDescriptor::new(
        StabilizationSystem::Guardian,
        TargetKind::Session,
        format!("launch_recovery_{}", safe_id(session_id, "session")),
        OwnershipClass::LauncherManaged,
    )
}

fn create_launch_recovery_journal(
    journals: &OperationJournalStore,
    operation_id: &OperationId,
    diagnosis_id: &DiagnosisId,
    target: &TargetDescriptor,
    kind: GuardianLaunchRecoveryKind,
    status: OperationStatus,
    outcome: OperationOutcome,
    step_result: OperationStepResult,
) {
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
        kind,
        OperationStepResult::Planned,
        target,
    ));
    entry
        .completed_steps
        .push(launch_recovery_step(kind, step_result, target));
    entry.outcome = Some(outcome);
    journals.create(entry);
}

fn launch_recovery_step(
    kind: GuardianLaunchRecoveryKind,
    result: OperationStepResult,
    target: &TargetDescriptor,
) -> OperationJournalStep {
    let mut step = OperationJournalStep::new(kind.step_id(), OperationPhase::Repairing);
    step.result = result;
    step.changed_target = Some(target.clone());
    step.generated_facts = vec![kind.summary().to_string()];
    step.rollback = RollbackState::NotApplicable;
    step
}

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
        operation_id: safe_operation_id(&operation_id),
        diagnosis_id: DiagnosisId::new(safe_id(diagnosis_id.as_str(), "diagnosis")),
        action,
        status,
        summary: safe_id(summary, "guardian_launch_recovery_outcome"),
    }
}

fn new_launch_recovery_operation_id(
    session_id: &str,
    kind: GuardianLaunchRecoveryKind,
) -> OperationId {
    OperationId::new(format!(
        "launch-recovery-{}-{}-{}",
        safe_id(session_id, "session"),
        kind.step_id(),
        uuid::Uuid::new_v4()
    ))
}

fn safe_operation_id(operation_id: &OperationId) -> OperationId {
    OperationId::new(safe_id(operation_id.as_str(), "operation"))
}

fn safe_id(value: &str, fallback: &str) -> String {
    sanitize_evidence_token(value, RedactionAudience::UserVisible, 96)
        .unwrap_or_else(|| fallback.to_string())
}

#[cfg(test)]
mod tests {
    use super::{
        GuardianLaunchRecoveryKind, GuardianLaunchRecoveryRequest, GuardianLaunchRecoveryStatus,
        record_launch_recovery_attempt, record_launch_recovery_failure,
    };
    use crate::guardian::{GuardianActionKind, GuardianMode};
    use crate::state::OperationJournalStore;
    use crate::state::contracts::{CommandKind, OperationOutcome, OperationStatus};
    use crate::state::failure_memory::{FailureMemoryActionOutcome, GuardianFailureMemoryStore};
    use croopor_launcher::LaunchFailureClass;

    #[test]
    fn launch_recovery_attempt_records_journal_and_memory() {
        let journals = OperationJournalStore::new();
        let failure_memory = GuardianFailureMemoryStore::new();

        let outcome = record_launch_recovery_attempt(request(
            "session-1",
            GuardianLaunchRecoveryKind::StripRawJvmArgs,
            LaunchFailureClass::JvmUnsupportedOption,
            "2026-06-15T10:00:00Z",
            None,
            &journals,
            &failure_memory,
        ));

        assert_eq!(outcome.status, GuardianLaunchRecoveryStatus::Recorded);
        assert_eq!(outcome.action, GuardianActionKind::Strip);
        let journal = journals
            .latest_for_command(CommandKind::LaunchInstance)
            .expect("launch recovery journal");
        assert_eq!(journal.status, OperationStatus::Succeeded);
        assert_eq!(journal.outcome, Some(OperationOutcome::Succeeded));
        assert_eq!(journal.planned_steps.len(), 1);
        assert_eq!(journal.completed_steps.len(), 1);

        let memory = failure_memory.list();
        assert_eq!(memory.len(), 1);
        assert_eq!(memory[0].last_action_kind, Some(GuardianActionKind::Strip));
        assert_eq!(
            memory[0].last_action_outcome,
            Some(FailureMemoryActionOutcome::Retried)
        );
        assert_eq!(memory[0].suppression_until, None);
    }

    #[test]
    fn launch_recovery_failure_sets_suppression_window() {
        let journals = OperationJournalStore::new();
        let failure_memory = GuardianFailureMemoryStore::new();

        let outcome = record_launch_recovery_failure(request(
            "session-2",
            GuardianLaunchRecoveryKind::SwitchManagedRuntime,
            LaunchFailureClass::JavaRuntimeMismatch,
            "2026-06-15T10:00:00Z",
            Some("java_override_present"),
            &journals,
            &failure_memory,
        ));

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
    }

    #[test]
    fn launch_recovery_attempt_is_suppressed_while_failure_window_is_active() {
        let journals = OperationJournalStore::new();
        let failure_memory = GuardianFailureMemoryStore::new();

        let _ = record_launch_recovery_failure(request(
            "session-3",
            GuardianLaunchRecoveryKind::DowngradePreset,
            LaunchFailureClass::JvmUnsupportedOption,
            "2026-06-15T10:00:00Z",
            Some("preset_override_present"),
            &journals,
            &failure_memory,
        ));

        let outcome = record_launch_recovery_attempt(request(
            "session-3",
            GuardianLaunchRecoveryKind::DowngradePreset,
            LaunchFailureClass::JvmUnsupportedOption,
            "2026-06-15T10:05:00Z",
            Some("preset_override_present"),
            &journals,
            &failure_memory,
        ));

        assert_eq!(outcome.status, GuardianLaunchRecoveryStatus::Suppressed);
        let memory = failure_memory.list();
        assert_eq!(
            memory[0].last_action_outcome,
            Some(FailureMemoryActionOutcome::Suppressed)
        );
    }

    fn request<'a>(
        session_id: &'a str,
        kind: GuardianLaunchRecoveryKind,
        failure_class: LaunchFailureClass,
        observed_at: &'a str,
        user_intent_hash: Option<&'a str>,
        journals: &'a OperationJournalStore,
        failure_memory: &'a GuardianFailureMemoryStore,
    ) -> GuardianLaunchRecoveryRequest<'a> {
        GuardianLaunchRecoveryRequest {
            session_id,
            mode: GuardianMode::Managed,
            kind,
            failure_class,
            observed_at,
            user_intent_hash,
            journals,
            failure_memory,
        }
    }
}
