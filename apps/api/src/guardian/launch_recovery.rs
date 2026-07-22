use super::{
    DiagnosisId, GuardianActionKind, GuardianDirective, GuardianDomain, GuardianMode,
    GuardianRecoveryIntentAxis, GuardianRecoveryMetadata,
};
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
use sha2::{Digest, Sha256};
use tracing::warn;

const DEFAULT_LAUNCH_RECOVERY_SUPPRESSION_MINUTES: i64 = 30;
const LAUNCH_RECOVERY_INTENT_DOMAIN: &[u8] = b"axial.guardian.launch-recovery-intent.v1";
const MAX_INTENT_TARGET_VERSION_CHARS: usize = 64;
const MAX_INTENT_JAVA_CHARS: usize = 4_096;
const MAX_INTENT_JVM_ARGS: usize = 64;
const MAX_INTENT_JVM_ARG_CHARS: usize = 4_096;
const MAX_INTENT_JVM_ARGS_BYTES: usize = 32_768;
const MAX_INTENT_PRESET_CHARS: usize = 96;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GuardianLaunchRecoveryCurrentIntent<'a> {
    pub target_version_id: &'a str,
    pub requested_java: &'a str,
    pub explicit_jvm_args: &'a [String],
    pub requested_preset: &'a str,
}

pub fn launch_recovery_user_intent_fingerprint(
    current: GuardianLaunchRecoveryCurrentIntent<'_>,
    directive: &GuardianDirective,
) -> Option<String> {
    launch_recovery_intent_fingerprint_for_metadata(current, directive.recovery_metadata()?)
}

fn launch_recovery_intent_fingerprint_for_metadata(
    current: GuardianLaunchRecoveryCurrentIntent<'_>,
    metadata: GuardianRecoveryMetadata,
) -> Option<String> {
    let target_version_id = current.target_version_id.trim();
    let target_version_id = sanitize_evidence_token(
        target_version_id,
        RedactionAudience::UserVisible,
        MAX_INTENT_TARGET_VERSION_CHARS,
    )
    .filter(|sanitized| sanitized == target_version_id)?;
    let mut hasher = Sha256::new();
    update_intent_frame(&mut hasher, b"domain", LAUNCH_RECOVERY_INTENT_DOMAIN);
    update_intent_frame(&mut hasher, b"target_version", target_version_id.as_bytes());
    update_intent_frame(&mut hasher, b"kind", metadata.fingerprint_tag);
    match metadata.intent_axis {
        GuardianRecoveryIntentAxis::RequestedJava => {
            let requested_java =
                bounded_intent_value(current.requested_java, MAX_INTENT_JAVA_CHARS)?;
            update_intent_frame(&mut hasher, b"requested_java", requested_java.as_bytes());
        }
        GuardianRecoveryIntentAxis::ExplicitJvmArgs => {
            if current.explicit_jvm_args.len() > MAX_INTENT_JVM_ARGS {
                return None;
            }
            let mut total_bytes = 0_usize;
            update_intent_frame(
                &mut hasher,
                b"jvm_arg_count",
                &(current.explicit_jvm_args.len() as u64).to_be_bytes(),
            );
            for argument in current.explicit_jvm_args {
                let argument = bounded_exact_intent_value(argument, MAX_INTENT_JVM_ARG_CHARS)?;
                total_bytes = total_bytes.checked_add(argument.len())?;
                if total_bytes > MAX_INTENT_JVM_ARGS_BYTES {
                    return None;
                }
                update_intent_frame(&mut hasher, b"jvm_arg", argument.as_bytes());
            }
        }
        GuardianRecoveryIntentAxis::RequestedPreset => {
            let requested_preset =
                bounded_intent_value(current.requested_preset, MAX_INTENT_PRESET_CHARS)?;
            update_intent_frame(
                &mut hasher,
                b"requested_preset",
                requested_preset.as_bytes(),
            );
        }
    }
    Some(format_launch_recovery_intent_fingerprint(&format!(
        "{:x}",
        hasher.finalize()
    )))
}

fn bounded_intent_value(value: &str, max_chars: usize) -> Option<&str> {
    bounded_exact_intent_value(value.trim(), max_chars)
}

fn bounded_exact_intent_value(value: &str, max_chars: usize) -> Option<&str> {
    (!value.chars().any(char::is_control) && value.chars().count() <= max_chars).then_some(value)
}

fn update_intent_frame(hasher: &mut Sha256, label: &[u8], value: &[u8]) {
    hasher.update((label.len() as u64).to_be_bytes());
    hasher.update(label);
    hasher.update((value.len() as u64).to_be_bytes());
    hasher.update(value);
}

fn format_launch_recovery_intent_fingerprint(hex_digest: &str) -> String {
    let mut fingerprint = String::with_capacity(78);
    fingerprint.push_str("sha256.");
    for (index, chunk) in hex_digest.as_bytes().chunks(8).enumerate() {
        if index > 0 {
            fingerprint.push('.');
        }
        fingerprint.push_str(std::str::from_utf8(chunk).expect("SHA-256 hex is ASCII"));
    }
    fingerprint
}

fn valid_launch_recovery_intent_fingerprint(value: &str) -> bool {
    let Some(digest) = value.strip_prefix("sha256.") else {
        return false;
    };
    let mut groups = digest.split('.');
    (0..8).all(|_| {
        groups.next().is_some_and(|group| {
            group.len() == 8
                && group
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        })
    }) && groups.next().is_none()
}

pub(super) fn launch_recovery_intent_fingerprint_matches(
    diagnosis_id: &DiagnosisId,
    action: Option<GuardianActionKind>,
    stored_fingerprint: Option<&str>,
    current: GuardianLaunchRecoveryCurrentIntent<'_>,
) -> bool {
    let Some(stored_fingerprint) = stored_fingerprint else {
        return false;
    };
    valid_launch_recovery_intent_fingerprint(stored_fingerprint)
        && GuardianRecoveryMetadata::ALL
            .iter()
            .filter(|metadata| {
                metadata.diagnosis == *diagnosis_id && Some(metadata.action) == action
            })
            .any(|metadata| {
                launch_recovery_intent_fingerprint_for_metadata(current, *metadata).as_deref()
                    == Some(stored_fingerprint)
            })
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct GuardianLaunchRecoveryPlan {
    pub operation_id: OperationId,
    pub mode: GuardianMode,
    pub target: TargetDescriptor,
    pub directive: GuardianDirective,
    pub trigger_failure_class: LaunchFailureClass,
    pub diagnosis_id: DiagnosisId,
    pub user_intent_hash: String,
}

#[derive(Clone, Debug)]
pub struct GuardianLaunchRecoveryPlanRequest<'a> {
    pub instance_id: &'a str,
    pub mode: GuardianMode,
    pub directive: GuardianDirective,
    pub failure_class: LaunchFailureClass,
    pub user_intent_hash: &'a str,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum GuardianLaunchRecoveryPlanRejection {
    MismatchedDirectiveFailureClass,
    InvalidUserIntentFingerprint,
}

pub struct GuardianLaunchRecoveryRecordRequest<'a> {
    pub plan: &'a GuardianLaunchRecoveryPlan,
    pub observed_at: &'a str,
    pub journals: &'a OperationJournalStore,
    pub failure_memory: &'a GuardianFailureMemoryStore,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct GuardianLaunchRecoveryOutcome {
    pub operation_id: OperationId,
    pub status: GuardianLaunchRecoveryStatus,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GuardianLaunchRecoveryJournalTransition {
    Attempt,
    Success,
    Failure,
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
    let Some(diagnosis_id) = request.directive.recovery_diagnosis(request.failure_class) else {
        return Err(GuardianLaunchRecoveryPlanRejection::MismatchedDirectiveFailureClass);
    };
    if !valid_launch_recovery_intent_fingerprint(request.user_intent_hash) {
        return Err(GuardianLaunchRecoveryPlanRejection::InvalidUserIntentFingerprint);
    }

    Ok(GuardianLaunchRecoveryPlan {
        operation_id: new_launch_recovery_operation_id(&request.directive),
        mode: request.mode,
        target: launch_recovery_target(request.instance_id),
        directive: request.directive,
        trigger_failure_class: request.failure_class,
        diagnosis_id,
        user_intent_hash: request.user_intent_hash.to_ascii_lowercase(),
    })
}

pub async fn record_launch_recovery_attempt(
    request: GuardianLaunchRecoveryRecordRequest<'_>,
) -> Result<GuardianLaunchRecoveryOutcome, OperationJournalStoreError> {
    record_launch_recovery_attempt_with_existing_policy(request, false).await
}

pub(crate) async fn resume_launch_recovery_attempt(
    request: GuardianLaunchRecoveryRecordRequest<'_>,
) -> Result<GuardianLaunchRecoveryOutcome, OperationJournalStoreError> {
    record_launch_recovery_attempt_with_existing_policy(request, true).await
}

async fn record_launch_recovery_attempt_with_existing_policy(
    request: GuardianLaunchRecoveryRecordRequest<'_>,
    allow_existing: bool,
) -> Result<GuardianLaunchRecoveryOutcome, OperationJournalStoreError> {
    let plan = request.plan;
    let diagnosis_id = plan.diagnosis_id;
    let operation_id = plan.operation_id.clone();
    let memory_key = FailureMemoryKey::for_observation(
        GuardianDomain::Launch,
        &diagnosis_id,
        &plan.target,
        plan.mode,
        Some(plan.user_intent_hash.as_str()),
    );

    if let Some(entry) = request.failure_memory.get(&memory_key)
        && suppression_active(&entry, request.observed_at)
    {
        match request.journals.get(&operation_id) {
            Some(entry)
                if allow_existing && launch_recovery_suppressed_journal_matches(&entry, plan) => {}
            Some(_) => return Err(OperationJournalStoreError::AlreadyTerminal),
            None => {
                create_launch_recovery_terminal_journal(
                    request.journals,
                    plan,
                    OperationStatus::Blocked,
                    OperationOutcome::Suppressed,
                    OperationStepResult::Skipped,
                )
                .await?;
            }
        }
        return Ok(launch_recovery_outcome(
            operation_id,
            GuardianLaunchRecoveryStatus::Suppressed,
        ));
    }

    match request.journals.get(&operation_id) {
        Some(entry) if allow_existing && launch_recovery_planned_journal_matches(&entry, plan) => {}
        Some(_) => return Err(OperationJournalStoreError::AlreadyTerminal),
        None => {
            create_launch_recovery_planned_journal(request.journals, plan).await?;
        }
    }
    Ok(launch_recovery_outcome(
        operation_id,
        GuardianLaunchRecoveryStatus::Recorded,
    ))
}

pub async fn record_launch_recovery_success(
    request: GuardianLaunchRecoveryRecordRequest<'_>,
) -> Result<GuardianLaunchRecoveryOutcome, OperationJournalStoreError> {
    let plan = request.plan;
    let diagnosis_id = plan.diagnosis_id;
    let operation_id = plan.operation_id.clone();
    let action = plan.directive.action_kind();
    match request.journals.get(&operation_id) {
        Some(entry)
            if launch_recovery_journal_transition_matches(
                &entry,
                plan,
                GuardianLaunchRecoveryJournalTransition::Success,
            ) => {}
        Some(entry) if launch_recovery_planned_journal_matches(&entry, plan) => {
            request
                .journals
                .record_success(
                    &operation_id,
                    launch_recovery_step(plan, OperationStepResult::Completed),
                    OperationOutcome::Succeeded,
                )
                .await?;
        }
        Some(_) => return Err(OperationJournalStoreError::AlreadyTerminal),
        None => return Err(OperationJournalStoreError::MissingOperation),
    }
    record_launch_recovery_memory(
        request.failure_memory,
        &diagnosis_id,
        plan.mode,
        &plan.target,
        action,
        FailureMemoryActionOutcome::Retried,
        request.observed_at,
        Some(plan.user_intent_hash.as_str()),
        None,
        true,
    );
    Ok(launch_recovery_outcome(
        operation_id,
        GuardianLaunchRecoveryStatus::Succeeded,
    ))
}

pub async fn record_launch_recovery_failure(
    request: GuardianLaunchRecoveryRecordRequest<'_>,
) -> Result<GuardianLaunchRecoveryOutcome, OperationJournalStoreError> {
    let plan = request.plan;
    let diagnosis_id = plan.diagnosis_id;
    let operation_id = plan.operation_id.clone();
    let action = plan.directive.action_kind();
    let suppression_until = default_suppression_until(request.observed_at);
    match request.journals.get(&operation_id) {
        Some(entry)
            if launch_recovery_journal_transition_matches(
                &entry,
                plan,
                GuardianLaunchRecoveryJournalTransition::Failure,
            ) => {}
        Some(entry) if launch_recovery_planned_journal_matches(&entry, plan) => {
            request
                .journals
                .record_failure(
                    &operation_id,
                    launch_recovery_step(plan, OperationStepResult::Failed),
                    plan.directive
                        .recovery_step_id()
                        .expect("planned recovery directive"),
                    OperationOutcome::Failed,
                )
                .await?;
        }
        Some(_) => return Err(OperationJournalStoreError::AlreadyTerminal),
        None => return Err(OperationJournalStoreError::MissingOperation),
    }
    record_launch_recovery_memory(
        request.failure_memory,
        &diagnosis_id,
        plan.mode,
        &plan.target,
        action,
        FailureMemoryActionOutcome::Failed,
        request.observed_at,
        Some(plan.user_intent_hash.as_str()),
        suppression_until.as_deref(),
        true,
    );
    Ok(launch_recovery_outcome(
        operation_id,
        GuardianLaunchRecoveryStatus::Failed,
    ))
}

fn launch_recovery_target(instance_id: &str) -> TargetDescriptor {
    TargetDescriptor::new(
        StabilizationSystem::Guardian,
        TargetKind::Instance,
        instance_id,
        OwnershipClass::LauncherManaged,
    )
}

async fn create_launch_recovery_planned_journal(
    journals: &OperationJournalStore,
    plan: &GuardianLaunchRecoveryPlan,
) -> Result<(), OperationJournalStoreError> {
    let mut entry = OperationJournalEntry::new(
        JournalId::new(format!("journal-{}", plan.operation_id)),
        plan.operation_id.clone(),
        CommandKind::LaunchInstance,
        StabilizationSystem::Guardian,
        plan.target.ownership,
        RollbackState::NotApplicable,
    );
    entry.targets.push(plan.target.clone());
    entry.guardian_diagnosis_ids.push(plan.diagnosis_id);
    entry
        .planned_steps
        .push(launch_recovery_step(plan, OperationStepResult::Planned));
    create_fresh_launch_recovery_journal(journals, entry).await
}

async fn create_launch_recovery_terminal_journal(
    journals: &OperationJournalStore,
    plan: &GuardianLaunchRecoveryPlan,
    status: OperationStatus,
    outcome: OperationOutcome,
    step_result: OperationStepResult,
) -> Result<(), OperationJournalStoreError> {
    let mut entry = OperationJournalEntry::new(
        JournalId::new(format!("journal-{}", plan.operation_id)),
        plan.operation_id.clone(),
        CommandKind::LaunchInstance,
        StabilizationSystem::Guardian,
        plan.target.ownership,
        RollbackState::NotApplicable,
    );
    entry.status = status;
    entry.targets.push(plan.target.clone());
    entry.guardian_diagnosis_ids.push(plan.diagnosis_id);
    entry
        .planned_steps
        .push(launch_recovery_step(plan, OperationStepResult::Planned));
    entry
        .completed_steps
        .push(launch_recovery_step(plan, step_result));
    entry.outcome = Some(outcome);
    create_fresh_launch_recovery_journal(journals, entry).await
}

async fn create_fresh_launch_recovery_journal(
    journals: &OperationJournalStore,
    entry: OperationJournalEntry,
) -> Result<(), OperationJournalStoreError> {
    loop {
        match journals.create_fresh(entry.clone()).await {
            Err(OperationJournalStoreError::RetryRequired) => {
                if journals.retry().await.is_err() {
                    return Err(OperationJournalStoreError::AlreadyExists);
                }
            }
            result => return result,
        }
    }
}

pub fn launch_recovery_journal_transition_matches(
    entry: &OperationJournalEntry,
    plan: &GuardianLaunchRecoveryPlan,
    transition: GuardianLaunchRecoveryJournalTransition,
) -> bool {
    match transition {
        GuardianLaunchRecoveryJournalTransition::Attempt => {
            launch_recovery_planned_journal_matches(entry, plan)
                || launch_recovery_suppressed_journal_matches(entry, plan)
        }
        GuardianLaunchRecoveryJournalTransition::Success => {
            launch_recovery_journal_identity_matches(entry, plan)
                && entry.status == OperationStatus::Succeeded
                && entry.outcome == Some(OperationOutcome::Succeeded)
                && entry.failure_point.is_none()
                && entry.completed_steps
                    == [launch_recovery_step(plan, OperationStepResult::Completed)]
        }
        GuardianLaunchRecoveryJournalTransition::Failure => {
            launch_recovery_journal_identity_matches(entry, plan)
                && entry.status == OperationStatus::Failed
                && entry.outcome == Some(OperationOutcome::Failed)
                && entry.failure_point.as_deref()
                    == Some(
                        plan.directive
                            .recovery_step_id()
                            .expect("planned recovery directive"),
                    )
                && entry.completed_steps
                    == [launch_recovery_step(plan, OperationStepResult::Failed)]
        }
    }
}

fn launch_recovery_planned_journal_matches(
    entry: &OperationJournalEntry,
    plan: &GuardianLaunchRecoveryPlan,
) -> bool {
    launch_recovery_journal_identity_matches(entry, plan)
        && entry.status == OperationStatus::Planned
        && entry.outcome.is_none()
        && entry.failure_point.is_none()
        && entry.completed_steps.is_empty()
}

fn launch_recovery_suppressed_journal_matches(
    entry: &OperationJournalEntry,
    plan: &GuardianLaunchRecoveryPlan,
) -> bool {
    launch_recovery_journal_identity_matches(entry, plan)
        && entry.status == OperationStatus::Blocked
        && entry.outcome == Some(OperationOutcome::Suppressed)
        && entry.failure_point.is_none()
        && entry.completed_steps == [launch_recovery_step(plan, OperationStepResult::Skipped)]
}

fn launch_recovery_journal_identity_matches(
    entry: &OperationJournalEntry,
    plan: &GuardianLaunchRecoveryPlan,
) -> bool {
    entry.journal_id.as_str() == format!("journal-{}", plan.operation_id)
        && entry.operation_id == plan.operation_id
        && entry.command == CommandKind::LaunchInstance
        && entry.owner == StabilizationSystem::Guardian
        && entry.ownership == plan.target.ownership
        && entry.targets == [plan.target.clone()]
        && entry.planned_steps == [launch_recovery_step(plan, OperationStepResult::Planned)]
        && entry.rollback == RollbackState::NotApplicable
        && entry.guardian_diagnosis_ids.len() == 1
        && entry.guardian_diagnosis_ids[0] == plan.diagnosis_id
}

pub fn launch_recovery_journal_transition_conflicts(
    entry: &OperationJournalEntry,
    plan: &GuardianLaunchRecoveryPlan,
    transition: GuardianLaunchRecoveryJournalTransition,
) -> bool {
    let transition_is_admissible = match transition {
        GuardianLaunchRecoveryJournalTransition::Attempt => {
            launch_recovery_planned_journal_matches(entry, plan)
                || launch_recovery_suppressed_journal_matches(entry, plan)
        }
        GuardianLaunchRecoveryJournalTransition::Success => {
            launch_recovery_planned_journal_matches(entry, plan)
                || launch_recovery_journal_transition_matches(entry, plan, transition)
        }
        GuardianLaunchRecoveryJournalTransition::Failure => {
            launch_recovery_planned_journal_matches(entry, plan)
                || launch_recovery_journal_transition_matches(entry, plan, transition)
        }
    };
    !transition_is_admissible
}

fn launch_recovery_step(
    plan: &GuardianLaunchRecoveryPlan,
    result: OperationStepResult,
) -> OperationJournalStep {
    let mut step = OperationJournalStep::new(
        plan.directive
            .recovery_step_id()
            .expect("planned recovery directive"),
        OperationPhase::Repairing,
    );
    step.result = result;
    step.changed_target = Some(plan.target.clone());
    step.generated_facts = vec![
        plan.directive
            .recovery_journal_summary()
            .expect("planned recovery directive")
            .to_string(),
    ];
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
        *diagnosis_id,
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
    status: GuardianLaunchRecoveryStatus,
) -> GuardianLaunchRecoveryOutcome {
    GuardianLaunchRecoveryOutcome {
        operation_id,
        status,
    }
}

fn new_launch_recovery_operation_id(_directive: &GuardianDirective) -> OperationId {
    OperationId::mint()
}

#[cfg(test)]
mod tests {
    use super::{
        GuardianLaunchRecoveryCurrentIntent, GuardianLaunchRecoveryPlan,
        GuardianLaunchRecoveryPlanRejection, GuardianLaunchRecoveryPlanRequest,
        GuardianLaunchRecoveryRecordRequest, GuardianLaunchRecoveryStatus, MAX_INTENT_JAVA_CHARS,
        MAX_INTENT_JVM_ARG_CHARS, create_launch_recovery_planned_journal,
        launch_recovery_journal_transition_matches, launch_recovery_step,
        launch_recovery_user_intent_fingerprint as author_intent_fingerprint,
        plan_launch_recovery_directive, record_launch_recovery_attempt,
        record_launch_recovery_failure, record_launch_recovery_success,
    };
    use crate::execution::persistence::{AtomicWriteBackend, PersistenceCoordinator};
    use crate::guardian::{
        GuardianActionKind, GuardianDirective, GuardianManagedJavaReason, GuardianMode,
        GuardianStripJvmArgsReason,
    };
    use crate::state::contracts::{
        CommandKind, JournalId, OperationJournalEntry, OperationStatus, OperationStepResult,
        OwnershipClass, RollbackState, StabilizationSystem, TargetKind,
    };
    use crate::state::failure_memory::{
        FailureMemoryActionOutcome, FailureMemorySnapshot, GuardianFailureMemoryStore,
        failure_memory_path,
    };
    use crate::state::{OperationJournalStore, OperationJournalStoreError};
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

    #[derive(Clone, Copy, Debug)]
    enum RecoveryCase {
        SwitchManagedRuntime,
        StripRawJvmArgs,
        DowngradePreset,
        DisableCustomGc,
    }

    fn launch_recovery_user_intent_fingerprint(
        current: GuardianLaunchRecoveryCurrentIntent<'_>,
        kind: RecoveryCase,
    ) -> Option<String> {
        author_intent_fingerprint(current, &directive(kind))
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
            _target: &crate::state::contracts::TargetDescriptor,
            destination: &Path,
            contents: &[u8],
        ) -> io::Result<()> {
            let attempt = self.attempts.fetch_add(1, Ordering::SeqCst) + 1;
            if attempt == self.fail_on_attempt {
                return Err(io::Error::other(
                    "injected launch-recovery terminal failure",
                ));
            }
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(destination, contents)
        }
    }

    #[test]
    fn launch_recovery_intent_fingerprint_binds_exact_values_without_exposing_them() {
        let first_args = vec![
            "-Dtoken=secret-token".to_string(),
            "-XX:+UseZGC".to_string(),
        ];
        let reversed_args = first_args.iter().cloned().rev().collect::<Vec<_>>();
        let current = GuardianLaunchRecoveryCurrentIntent {
            target_version_id: "1.21.1",
            requested_java: "/home/alice/java/bin/java",
            explicit_jvm_args: &first_args,
            requested_preset: "graalvm",
        };

        for (kind, expected) in [
            (
                RecoveryCase::SwitchManagedRuntime,
                "sha256.c3aca930.0ba41e20.28f0654b.0612a6ae.0313d97f.58dcf863.f9ee0b49.fd52ff4f",
            ),
            (
                RecoveryCase::StripRawJvmArgs,
                "sha256.de867506.634cb239.8da234ca.7bd88189.17b63266.c7f01023.ce710618.d209bb9f",
            ),
            (
                RecoveryCase::DowngradePreset,
                "sha256.fb9f2119.766c0fb0.3b67a603.c74a388d.e486178e.a3e5e111.2dfe9b9f.dc2aabb4",
            ),
            (
                RecoveryCase::DisableCustomGc,
                "sha256.fb0729cc.bdfdd7f4.104c7a70.54a85013.d0dd5c5e.46683243.2acf143d.29134c7c",
            ),
        ] {
            let fingerprint = launch_recovery_user_intent_fingerprint(current, kind)
                .expect("valid exact intent fingerprint");
            assert_eq!(fingerprint, expected);
            assert_eq!(fingerprint.len(), 78);
            assert!(fingerprint.starts_with("sha256."));
            for secret in ["alice", "secret-token", "UseZGC", "graalvm"] {
                assert!(!fingerprint.contains(secret));
            }
        }

        let changed_java = GuardianLaunchRecoveryCurrentIntent {
            requested_java: "/opt/other-java/bin/java",
            ..current
        };
        let changed_args = GuardianLaunchRecoveryCurrentIntent {
            explicit_jvm_args: &reversed_args,
            ..current
        };
        let changed_preset = GuardianLaunchRecoveryCurrentIntent {
            requested_preset: "performance",
            ..current
        };
        let changed_version = GuardianLaunchRecoveryCurrentIntent {
            target_version_id: "1.20.1",
            ..current
        };
        assert_ne!(
            launch_recovery_user_intent_fingerprint(current, RecoveryCase::SwitchManagedRuntime),
            launch_recovery_user_intent_fingerprint(
                changed_java,
                RecoveryCase::SwitchManagedRuntime
            )
        );
        assert_ne!(
            launch_recovery_user_intent_fingerprint(current, RecoveryCase::StripRawJvmArgs),
            launch_recovery_user_intent_fingerprint(changed_args, RecoveryCase::StripRawJvmArgs)
        );
        for kind in [RecoveryCase::DowngradePreset, RecoveryCase::DisableCustomGc] {
            assert_ne!(
                launch_recovery_user_intent_fingerprint(current, kind),
                launch_recovery_user_intent_fingerprint(changed_preset, kind)
            );
        }
        assert_ne!(
            launch_recovery_user_intent_fingerprint(current, RecoveryCase::SwitchManagedRuntime),
            launch_recovery_user_intent_fingerprint(
                changed_version,
                RecoveryCase::SwitchManagedRuntime
            )
        );
    }

    #[test]
    fn launch_recovery_intent_fingerprint_rejects_invalid_or_oversized_material() {
        let oversized_args = vec!["x".repeat(MAX_INTENT_JVM_ARG_CHARS + 1)];
        let invalid_version = GuardianLaunchRecoveryCurrentIntent {
            target_version_id: "/home/alice/-Dtoken=secret-token",
            requested_java: "/opt/java/bin/java",
            explicit_jvm_args: &[],
            requested_preset: "graalvm",
        };
        let oversized_java = GuardianLaunchRecoveryCurrentIntent {
            target_version_id: "1.21.1",
            requested_java: &"x".repeat(MAX_INTENT_JAVA_CHARS + 1),
            explicit_jvm_args: &[],
            requested_preset: "graalvm",
        };
        let oversized_arg = GuardianLaunchRecoveryCurrentIntent {
            target_version_id: "1.21.1",
            requested_java: "/opt/java/bin/java",
            explicit_jvm_args: &oversized_args,
            requested_preset: "graalvm",
        };

        assert!(
            launch_recovery_user_intent_fingerprint(
                invalid_version,
                RecoveryCase::SwitchManagedRuntime
            )
            .is_none()
        );
        assert!(
            launch_recovery_user_intent_fingerprint(
                oversized_java,
                RecoveryCase::SwitchManagedRuntime
            )
            .is_none()
        );
        assert!(
            launch_recovery_user_intent_fingerprint(oversized_arg, RecoveryCase::StripRawJvmArgs)
                .is_none()
        );
    }

    #[tokio::test]
    async fn launch_recovery_attempt_records_journal_and_memory() {
        let journals = OperationJournalStore::new();
        let failure_memory = GuardianFailureMemoryStore::new();
        let plan = plan("session-1", RecoveryCase::StripRawJvmArgs);

        let outcome = record_launch_recovery_attempt(request(
            &plan,
            "2026-06-15T10:00:00Z",
            &journals,
            &failure_memory,
        ))
        .await
        .expect("persist launch recovery attempt");

        assert_eq!(outcome.status, GuardianLaunchRecoveryStatus::Recorded);
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
    async fn fresh_launch_recovery_refuses_matching_collision_but_resume_accepts() {
        let journals = OperationJournalStore::new();
        let failure_memory = GuardianFailureMemoryStore::new();
        let plan = plan("launch-recovery-collision", RecoveryCase::StripRawJvmArgs);
        create_launch_recovery_planned_journal(&journals, &plan)
            .await
            .expect("seed matching collision");

        assert!(matches!(
            record_launch_recovery_attempt(request(
                &plan,
                "2026-06-15T10:00:00Z",
                &journals,
                &failure_memory,
            ))
            .await,
            Err(OperationJournalStoreError::AlreadyTerminal)
        ));
        let resumed = resume_launch_recovery_attempt(request(
            &plan,
            "2026-06-15T10:00:00Z",
            &journals,
            &failure_memory,
        ))
        .await
        .expect("resume matching launch recovery");
        assert_eq!(resumed.operation_id, plan.operation_id);
        assert_eq!(resumed.status, GuardianLaunchRecoveryStatus::Recorded);
    }

    #[tokio::test]
    async fn launch_recovery_success_terminalizes_the_same_operation() {
        let journals = OperationJournalStore::new();
        let failure_memory = GuardianFailureMemoryStore::new();
        let plan = plan("session-success", RecoveryCase::StripRawJvmArgs);
        let attempt = record_launch_recovery_attempt(request(
            &plan,
            "2026-06-15T10:00:00Z",
            &journals,
            &failure_memory,
        ))
        .await
        .expect("persist launch recovery attempt");

        let outcome = record_launch_recovery_success(request(
            &plan,
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
        assert!(launch_recovery_journal_transition_matches(
            &journal,
            &plan,
            super::GuardianLaunchRecoveryJournalTransition::Success,
        ));
        let mut wrong_rollback = journal.clone();
        wrong_rollback.rollback = RollbackState::Available;
        assert!(!launch_recovery_journal_transition_matches(
            &wrong_rollback,
            &plan,
            super::GuardianLaunchRecoveryJournalTransition::Success,
        ));
        let mut extra_completed_step = journal.clone();
        extra_completed_step
            .completed_steps
            .push(launch_recovery_step(&plan, OperationStepResult::Completed));
        assert!(!launch_recovery_journal_transition_matches(
            &extra_completed_step,
            &plan,
            super::GuardianLaunchRecoveryJournalTransition::Success,
        ));
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
    async fn launch_recovery_terminal_rejects_foreign_same_id_journal() {
        for transition in ["success", "failure"] {
            let journals = OperationJournalStore::new();
            let failure_memory = GuardianFailureMemoryStore::new();
            let plan = plan("foreign-terminal-journal", RecoveryCase::StripRawJvmArgs);
            let mut foreign = OperationJournalEntry::new(
                JournalId::new("foreign-journal"),
                plan.operation_id.clone(),
                CommandKind::LaunchInstance,
                StabilizationSystem::Guardian,
                plan.target.ownership,
                RollbackState::NotApplicable,
            );
            foreign.targets.push(plan.target.clone());
            foreign.guardian_diagnosis_ids.push(plan.diagnosis_id);
            foreign
                .planned_steps
                .push(launch_recovery_step(&plan, OperationStepResult::Planned));
            journals
                .create(foreign)
                .await
                .expect("create foreign journal");

            let result = match transition {
                "success" => {
                    record_launch_recovery_success(request(
                        &plan,
                        "2026-06-15T10:01:00Z",
                        &journals,
                        &failure_memory,
                    ))
                    .await
                }
                "failure" => {
                    record_launch_recovery_failure(request(
                        &plan,
                        "2026-06-15T10:01:00Z",
                        &journals,
                        &failure_memory,
                    ))
                    .await
                }
                _ => unreachable!(),
            };

            assert!(matches!(
                result,
                Err(OperationJournalStoreError::AlreadyTerminal)
            ));
            let retained = journals
                .get(&plan.operation_id)
                .expect("foreign journal retained");
            assert_eq!(retained.journal_id.as_str(), "foreign-journal");
            assert_eq!(retained.status, OperationStatus::Planned);
            assert!(retained.completed_steps.is_empty());
            assert!(failure_memory.list().is_empty());
        }
    }

    #[tokio::test]
    async fn launch_recovery_suppression_rejects_existing_planned_journal() {
        let journals = OperationJournalStore::new();
        let failure_memory = GuardianFailureMemoryStore::new();
        let failed_plan = plan("planned-during-suppression", RecoveryCase::StripRawJvmArgs);
        record_launch_recovery_attempt(request(
            &failed_plan,
            "2026-06-15T10:00:00Z",
            &journals,
            &failure_memory,
        ))
        .await
        .expect("record failed-plan attempt");
        record_launch_recovery_failure(request(
            &failed_plan,
            "2026-06-15T10:01:00Z",
            &journals,
            &failure_memory,
        ))
        .await
        .expect("record suppression memory");

        let suppressed_plan = plan("planned-during-suppression", RecoveryCase::StripRawJvmArgs);
        create_launch_recovery_planned_journal(&journals, &suppressed_plan)
            .await
            .expect("seed exact planned journal");
        let result = record_launch_recovery_attempt(request(
            &suppressed_plan,
            "2026-06-15T10:02:00Z",
            &journals,
            &failure_memory,
        ))
        .await;

        assert!(matches!(
            result,
            Err(OperationJournalStoreError::AlreadyTerminal)
        ));
        let retained = journals
            .get(&suppressed_plan.operation_id)
            .expect("planned journal retained");
        assert_eq!(retained.status, OperationStatus::Planned);
        assert!(retained.completed_steps.is_empty());
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
        let plan = plan("session-terminal-retry", RecoveryCase::SwitchManagedRuntime);
        let attempt = record_launch_recovery_attempt(request(
            &plan,
            "2026-06-15T10:00:00Z",
            &journals,
            &failure_memory,
        ))
        .await
        .expect("persist launch recovery attempt");

        assert!(
            record_launch_recovery_failure(request(
                &plan,
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
        let plan = plan("session-journal-failure", RecoveryCase::StripRawJvmArgs);

        let result = record_launch_recovery_attempt(request(
            &plan,
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
        let plan = plan("session-2", RecoveryCase::SwitchManagedRuntime);

        record_launch_recovery_attempt(request(
            &plan,
            "2026-06-15T09:59:00Z",
            &journals,
            &failure_memory,
        ))
        .await
        .expect("persist launch recovery attempt");

        let outcome = record_launch_recovery_failure(request(
            &plan,
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
    async fn later_session_suppression_preserves_failed_memory() {
        let journals = OperationJournalStore::new();
        let failure_memory = GuardianFailureMemoryStore::new();
        let initial_plan = plan("instance-3", RecoveryCase::DowngradePreset);

        record_launch_recovery_attempt(request(
            &initial_plan,
            "2026-06-15T09:59:00Z",
            &journals,
            &failure_memory,
        ))
        .await
        .expect("persist launch recovery attempt");

        let _ = record_launch_recovery_failure(request(
            &initial_plan,
            "2026-06-15T10:00:00Z",
            &journals,
            &failure_memory,
        ))
        .await
        .expect("persist launch recovery failure");

        let suppressed_plan = plan("instance-3", RecoveryCase::DowngradePreset);

        let outcome = record_launch_recovery_attempt(request(
            &suppressed_plan,
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
        assert_eq!(memory[0].occurrence_count, 1);
        assert_eq!(memory[0].repair_attempt_count, 1);
        assert_eq!(
            memory[0].last_action_outcome,
            Some(FailureMemoryActionOutcome::Failed)
        );
        assert_eq!(
            memory[0].suppression_until.as_deref(),
            Some("2026-06-15T10:30:00+00:00")
        );
    }

    #[tokio::test]
    async fn launch_recovery_suppression_is_independent_between_instances() {
        let journals = OperationJournalStore::new();
        let failure_memory = GuardianFailureMemoryStore::new();
        let first_plan = plan("instance-a", RecoveryCase::DowngradePreset);

        record_launch_recovery_attempt(request(
            &first_plan,
            "2026-06-15T09:59:00Z",
            &journals,
            &failure_memory,
        ))
        .await
        .expect("persist first instance recovery attempt");
        record_launch_recovery_failure(request(
            &first_plan,
            "2026-06-15T10:00:00Z",
            &journals,
            &failure_memory,
        ))
        .await
        .expect("persist first instance recovery failure");

        let second_plan = plan("instance-b", RecoveryCase::DowngradePreset);
        let outcome = record_launch_recovery_attempt(request(
            &second_plan,
            "2026-06-15T10:05:00Z",
            &journals,
            &failure_memory,
        ))
        .await
        .expect("persist independent second instance recovery attempt");

        assert_eq!(outcome.status, GuardianLaunchRecoveryStatus::Recorded);
        record_launch_recovery_failure(request(
            &second_plan,
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
        let first_memory = GuardianFailureMemoryStore::try_load_from_paths(&paths)
            .expect("load Guardian failure-memory persistence");
        let initial_plan = plan("session-restart", RecoveryCase::DowngradePreset);

        record_launch_recovery_attempt(request(
            &initial_plan,
            "2026-06-15T09:59:00Z",
            &journals,
            &first_memory,
        ))
        .await
        .expect("persist launch recovery attempt");

        let _ = record_launch_recovery_failure(request(
            &initial_plan,
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
        let suppressed_plan = plan("session-restart", RecoveryCase::DowngradePreset);
        let outcome = record_launch_recovery_attempt(request(
            &suppressed_plan,
            "2026-06-15T10:05:00Z",
            &journals,
            &reloaded_memory,
        ))
        .await
        .expect("persist suppressed launch recovery attempt");

        assert_eq!(outcome.status, GuardianLaunchRecoveryStatus::Suppressed);
        let memory = reloaded_memory.list();
        assert_eq!(memory.len(), 1);
        assert_eq!(memory[0].occurrence_count, 1);
        assert_eq!(memory[0].repair_attempt_count, 1);
        assert_eq!(
            memory[0].last_action_outcome,
            Some(FailureMemoryActionOutcome::Failed)
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn expired_launch_recovery_suppression_allows_recovery_after_reload() {
        let root = test_root("expired-suppression");
        let paths = test_paths(&root);
        let journals = OperationJournalStore::new();
        let first_memory = GuardianFailureMemoryStore::try_load_from_paths(&paths)
            .expect("load Guardian failure-memory persistence");
        let initial_plan = plan("session-expired", RecoveryCase::SwitchManagedRuntime);

        record_launch_recovery_attempt(request(
            &initial_plan,
            "2026-06-15T09:59:00Z",
            &journals,
            &first_memory,
        ))
        .await
        .expect("persist launch recovery attempt");

        let _ = record_launch_recovery_failure(request(
            &initial_plan,
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
        let retry_plan = plan("session-expired", RecoveryCase::SwitchManagedRuntime);
        let attempt = record_launch_recovery_attempt(request(
            &retry_plan,
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
        assert!(memory[0].suppression_until.is_none());

        let next_plan = plan("session-expired", RecoveryCase::SwitchManagedRuntime);
        let next_attempt = record_launch_recovery_attempt(request(
            &next_plan,
            "2026-06-15T10:33:00Z",
            &journals,
            &reloaded_memory,
        ))
        .await
        .expect("successful retry removes next recovery suppression");
        assert_eq!(next_attempt.status, GuardianLaunchRecoveryStatus::Recorded);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn launch_recovery_directive_plan_derives_current_action() {
        let plan = plan("instance-4", RecoveryCase::SwitchManagedRuntime);

        assert_eq!(plan.directive.action_kind(), GuardianActionKind::Fallback);
        assert_eq!(plan.target.kind, TargetKind::Instance);
        assert_eq!(plan.target.id, "instance-4");
        assert_eq!(plan.target.ownership, OwnershipClass::LauncherManaged);
        assert!(super::valid_launch_recovery_intent_fingerprint(
            &plan.user_intent_hash
        ));
        assert!(
            !serde_json::to_string(&plan)
                .expect("plan json")
                .contains("/home")
        );
    }

    #[test]
    fn launch_recovery_plan_rejects_non_recovery_reasons() {
        for directive in [
            GuardianDirective::UseManagedJava {
                reason: GuardianManagedJavaReason::Preflight,
            },
            GuardianDirective::StripJvmArgs {
                reason: GuardianStripJvmArgsReason::Preflight,
            },
            GuardianDirective::compatibility_preset_downgrade("ultra_low_latency", "performance"),
        ] {
            let rejection = plan_launch_recovery_directive(GuardianLaunchRecoveryPlanRequest {
                instance_id: "instance-5",
                mode: GuardianMode::Managed,
                directive,
                failure_class: LaunchFailureClass::JavaRuntimeMismatch,
                user_intent_hash:
                    "sha256.aaaaaaaa.aaaaaaaa.aaaaaaaa.aaaaaaaa.aaaaaaaa.aaaaaaaa.aaaaaaaa.aaaaaaaa",
            })
            .expect_err("non-recovery directive rejected");
            assert_eq!(
                rejection,
                GuardianLaunchRecoveryPlanRejection::MismatchedDirectiveFailureClass
            );
        }
    }

    #[test]
    fn launch_recovery_plan_rejects_mismatched_failure_class() {
        let rejection = plan_launch_recovery_directive(GuardianLaunchRecoveryPlanRequest {
            instance_id: "instance-5",
            mode: GuardianMode::Managed,
            directive: directive(RecoveryCase::SwitchManagedRuntime),
            failure_class: LaunchFailureClass::JvmUnsupportedOption,
            user_intent_hash:
                "sha256.aaaaaaaa.aaaaaaaa.aaaaaaaa.aaaaaaaa.aaaaaaaa.aaaaaaaa.aaaaaaaa.aaaaaaaa",
        })
        .expect_err("mismatched failure class rejected");

        assert_eq!(
            rejection,
            GuardianLaunchRecoveryPlanRejection::MismatchedDirectiveFailureClass
        );
    }

    #[test]
    fn launch_recovery_plan_rejects_non_digest_intent_fingerprints() {
        for user_intent_hash in ["", "java_override_present:1.21.1"] {
            let rejection = plan_launch_recovery_directive(GuardianLaunchRecoveryPlanRequest {
                instance_id: "instance-5",
                mode: GuardianMode::Managed,
                directive: directive(RecoveryCase::SwitchManagedRuntime),
                failure_class: LaunchFailureClass::JavaRuntimeMismatch,
                user_intent_hash,
            })
            .expect_err("invalid intent fingerprint rejected");

            assert_eq!(
                rejection,
                GuardianLaunchRecoveryPlanRejection::InvalidUserIntentFingerprint
            );
        }
    }

    fn request<'a>(
        plan: &'a GuardianLaunchRecoveryPlan,
        observed_at: &'a str,
        journals: &'a OperationJournalStore,
        failure_memory: &'a GuardianFailureMemoryStore,
    ) -> GuardianLaunchRecoveryRecordRequest<'a> {
        GuardianLaunchRecoveryRecordRequest {
            plan,
            observed_at,
            journals,
            failure_memory,
        }
    }

    fn plan(instance_id: &str, kind: RecoveryCase) -> GuardianLaunchRecoveryPlan {
        let explicit_jvm_args = vec!["-Dexample=true".to_string()];
        let user_intent_hash = launch_recovery_user_intent_fingerprint(
            GuardianLaunchRecoveryCurrentIntent {
                target_version_id: "1.21.1",
                requested_java: "/opt/java/bin/java",
                explicit_jvm_args: &explicit_jvm_args,
                requested_preset: "graalvm",
            },
            kind,
        )
        .expect("valid test recovery intent");
        plan_launch_recovery_directive(GuardianLaunchRecoveryPlanRequest {
            instance_id,
            mode: GuardianMode::Managed,
            directive: directive(kind),
            failure_class: match kind {
                RecoveryCase::SwitchManagedRuntime => LaunchFailureClass::JavaRuntimeMismatch,
                RecoveryCase::StripRawJvmArgs
                | RecoveryCase::DowngradePreset
                | RecoveryCase::DisableCustomGc => LaunchFailureClass::JvmUnsupportedOption,
            },
            user_intent_hash: &user_intent_hash,
        })
        .expect("launch recovery plan")
    }

    fn directive(kind: RecoveryCase) -> GuardianDirective {
        match kind {
            RecoveryCase::SwitchManagedRuntime => GuardianDirective::UseManagedJava {
                reason: GuardianManagedJavaReason::StartupRecovery,
            },
            RecoveryCase::StripRawJvmArgs => GuardianDirective::StripJvmArgs {
                reason: GuardianStripJvmArgsReason::PrepareFailure,
            },
            RecoveryCase::DowngradePreset => {
                GuardianDirective::startup_preset_downgrade("performance")
            }
            RecoveryCase::DisableCustomGc => GuardianDirective::DisableCustomGc,
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
        AppPaths::from_root(root.to_path_buf()).expect("absolute test app root")
    }
}
