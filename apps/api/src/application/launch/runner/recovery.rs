use super::trace_launch_event;
use crate::guardian::{
    GuardianLaunchRecoveryCurrentIntent, GuardianLaunchRecoveryDirective,
    GuardianLaunchRecoveryEffect, GuardianLaunchRecoveryJournalTransition,
    GuardianLaunchRecoveryKind, GuardianLaunchRecoveryOutcome, GuardianLaunchRecoveryPlan,
    GuardianLaunchRecoveryPlanRejection, GuardianLaunchRecoveryPlanRequest,
    GuardianLaunchRecoveryRecordRequest, GuardianUserOutcome,
    launch_recovery_journal_transition_conflicts, launch_recovery_journal_transition_matches,
    launch_recovery_public_action_label, launch_recovery_suppressed_user_outcome,
    launch_recovery_user_intent_fingerprint, plan_launch_recovery_directive,
    record_launch_recovery_attempt, record_launch_recovery_failure, record_launch_recovery_success,
};
use crate::logging::timestamp_utc;
use crate::state::{AppState, OperationJournalReconciliation, OperationJournalStoreError};
use axial_launcher::{
    GuardianDecision, GuardianInterventionKind, GuardianSummary, LaunchFailureClass,
};
use std::time::Duration;

const JOURNAL_RETRY_INITIAL_DELAY: Duration = Duration::from_millis(25);
const JOURNAL_RETRY_MAX_DELAY: Duration = Duration::from_secs(1);

pub(super) enum RecoveryDirectiveOutcome {
    Apply(GuardianLaunchRecoveryPlan),
    Exhausted,
    Rejected,
    Suppressed(GuardianUserOutcome),
}

pub(super) struct RecoveryDirectiveRequest<'a> {
    pub(super) state: &'a AppState,
    pub(super) session_id: &'a str,
    pub(super) intent: &'a axial_launcher::LaunchIntent,
    pub(super) directive: GuardianLaunchRecoveryDirective,
    pub(super) mode: crate::guardian::GuardianMode,
    pub(super) failure_class: LaunchFailureClass,
    pub(super) recovery_attempts: &'a mut u8,
    pub(super) max_recovery_attempts: u8,
    pub(super) guardian: &'a mut GuardianSummary,
}

pub(super) async fn handle_recovery_directive(
    request: RecoveryDirectiveRequest<'_>,
) -> Result<RecoveryDirectiveOutcome, OperationJournalStoreError> {
    if *request.recovery_attempts >= request.max_recovery_attempts {
        return Ok(RecoveryDirectiveOutcome::Exhausted);
    }
    *request.recovery_attempts += 1;

    let Ok(plan) = plan_guardian_launch_recovery_directive(
        request.intent,
        request.directive,
        request.mode,
        request.failure_class,
    ) else {
        return Ok(RecoveryDirectiveOutcome::Rejected);
    };
    let outcome =
        record_guardian_launch_recovery_attempt(request.state, request.session_id, &plan).await?;
    if outcome.status == crate::guardian::GuardianLaunchRecoveryStatus::Suppressed {
        let user_outcome = suppressed_launch_recovery_outcome(&plan);
        request
            .state
            .sessions()
            .emit_log(request.session_id, "system", user_outcome.summary.clone())
            .await;
        block_guardian_for_suppressed_launch_recovery(request.guardian, &user_outcome);
        return Ok(RecoveryDirectiveOutcome::Suppressed(user_outcome));
    }

    request
        .state
        .sessions()
        .emit_log(
            request.session_id,
            "system",
            plan.directive.description.clone(),
        )
        .await;
    Ok(RecoveryDirectiveOutcome::Apply(plan))
}

fn plan_guardian_launch_recovery_directive(
    intent: &axial_launcher::LaunchIntent,
    directive: GuardianLaunchRecoveryDirective,
    mode: crate::guardian::GuardianMode,
    failure_class: LaunchFailureClass,
) -> Result<GuardianLaunchRecoveryPlan, GuardianLaunchRecoveryPlanRejection> {
    let user_intent_hash = launch_recovery_user_intent_fingerprint(
        GuardianLaunchRecoveryCurrentIntent {
            target_version_id: &intent.target_version_id,
            requested_java: &intent.requested_java,
            explicit_jvm_args: &intent.extra_jvm_args,
            requested_preset: &intent.requested_preset,
        },
        directive.kind,
    )
    .ok_or(GuardianLaunchRecoveryPlanRejection::InvalidUserIntentFingerprint)?;
    plan_launch_recovery_directive(GuardianLaunchRecoveryPlanRequest {
        instance_id: &intent.instance_id,
        mode,
        directive,
        failure_class,
        user_intent_hash: user_intent_hash.as_str(),
    })
}

async fn record_guardian_launch_recovery_attempt(
    state: &AppState,
    session_id: &str,
    plan: &GuardianLaunchRecoveryPlan,
) -> Result<GuardianLaunchRecoveryOutcome, OperationJournalStoreError> {
    reject_mismatched_launch_recovery_transition(
        state,
        plan,
        GuardianLaunchRecoveryJournalTransition::Attempt,
    )?;
    let outcome = loop {
        let observed_at = timestamp_utc();
        match record_launch_recovery_attempt(GuardianLaunchRecoveryRecordRequest {
            plan,
            observed_at: observed_at.as_str(),
            journals: state.journals().as_ref(),
            failure_memory: state.failure_memory().as_ref(),
        })
        .await
        {
            Ok(outcome)
                if launch_recovery_transition_matches(
                    state,
                    plan,
                    GuardianLaunchRecoveryJournalTransition::Attempt,
                ) =>
            {
                break outcome;
            }
            Ok(_) => return Err(OperationJournalStoreError::AlreadyTerminal),
            Err(error) => {
                retry_launch_recovery_journal(
                    state,
                    session_id,
                    plan,
                    GuardianLaunchRecoveryJournalTransition::Attempt,
                    error,
                )
                .await?;
                continue;
            }
        }
    };
    trace_launch_event(
        session_id,
        &format!(
            "guardian launch recovery attempt: kind={:?} status={:?} operation={}",
            plan.directive.kind,
            outcome.status,
            outcome.operation_id.as_str()
        ),
    );
    Ok(outcome)
}

pub(super) async fn record_successful_self_healing_if_any(
    state: &AppState,
    session_id: &str,
    recovery_plan: Option<&GuardianLaunchRecoveryPlan>,
) -> Result<(), OperationJournalStoreError> {
    let Some(plan) = recovery_plan else {
        return Ok(());
    };
    reject_mismatched_launch_recovery_transition(
        state,
        plan,
        GuardianLaunchRecoveryJournalTransition::Success,
    )?;
    let outcome = loop {
        let observed_at = timestamp_utc();
        match record_launch_recovery_success(GuardianLaunchRecoveryRecordRequest {
            plan,
            observed_at: observed_at.as_str(),
            journals: state.journals().as_ref(),
            failure_memory: state.failure_memory().as_ref(),
        })
        .await
        {
            Ok(outcome)
                if launch_recovery_transition_matches(
                    state,
                    plan,
                    GuardianLaunchRecoveryJournalTransition::Success,
                ) =>
            {
                break outcome;
            }
            Ok(_) => return Err(OperationJournalStoreError::AlreadyTerminal),
            Err(error) => {
                retry_launch_recovery_journal(
                    state,
                    session_id,
                    plan,
                    GuardianLaunchRecoveryJournalTransition::Success,
                    error,
                )
                .await?;
                continue;
            }
        }
    };
    trace_launch_event(
        session_id,
        &format!(
            "guardian launch recovery succeeded: kind={:?} operation={}",
            plan.directive.kind,
            outcome.operation_id.as_str()
        ),
    );
    Ok(())
}

pub(super) async fn record_failed_self_healing_if_any(
    state: &AppState,
    session_id: &str,
    recovery_plan: Option<&GuardianLaunchRecoveryPlan>,
    observed_failure_class: LaunchFailureClass,
) -> Result<(), OperationJournalStoreError> {
    let Some(plan) = recovery_plan else {
        return Ok(());
    };
    let trigger_failure_class = plan.trigger_failure_class;
    reject_mismatched_launch_recovery_transition(
        state,
        plan,
        GuardianLaunchRecoveryJournalTransition::Failure,
    )?;
    let outcome = loop {
        let observed_at = timestamp_utc();
        match record_launch_recovery_failure(GuardianLaunchRecoveryRecordRequest {
            plan,
            observed_at: observed_at.as_str(),
            journals: state.journals().as_ref(),
            failure_memory: state.failure_memory().as_ref(),
        })
        .await
        {
            Ok(outcome)
                if launch_recovery_transition_matches(
                    state,
                    plan,
                    GuardianLaunchRecoveryJournalTransition::Failure,
                ) =>
            {
                break outcome;
            }
            Ok(_) => return Err(OperationJournalStoreError::AlreadyTerminal),
            Err(error) => {
                retry_launch_recovery_journal(
                    state,
                    session_id,
                    plan,
                    GuardianLaunchRecoveryJournalTransition::Failure,
                    error,
                )
                .await?;
                continue;
            }
        }
    };
    trace_launch_event(
        session_id,
        &format!(
            "guardian launch recovery failed: kind={:?} trigger={trigger_failure_class:?} observed={observed_failure_class:?} operation={}",
            plan.directive.kind,
            outcome.operation_id.as_str()
        ),
    );
    state
        .sessions()
        .emit_log(
            session_id,
            "system",
            format!(
                "Guardian recorded failed launch self-healing for {}.",
                launch_recovery_public_action_label(plan.directive.kind)
            ),
        )
        .await;
    Ok(())
}

async fn retry_launch_recovery_journal(
    state: &AppState,
    session_id: &str,
    plan: &GuardianLaunchRecoveryPlan,
    transition: GuardianLaunchRecoveryJournalTransition,
    error: OperationJournalStoreError,
) -> Result<(), OperationJournalStoreError> {
    let error_class = error.class();
    let reconciliation = state
        .journals()
        .reconcile_transition(
            &plan.operation_id,
            error,
            JOURNAL_RETRY_INITIAL_DELAY,
            JOURNAL_RETRY_MAX_DELAY,
            |entry| launch_recovery_journal_transition_matches(entry, plan, transition),
        )
        .await;
    match reconciliation {
        Ok(
            OperationJournalReconciliation::CommittedAfterPersistenceFailure(_)
            | OperationJournalReconciliation::RequestedTransitionAlreadyCommitted
            | OperationJournalReconciliation::RetryRequestedTransition,
        ) => Ok(()),
        Err(error) => {
            trace_launch_event(
                session_id,
                &format!(
                    "guardian launch recovery journal rejected: kind={}",
                    error_class
                ),
            );
            Err(error)
        }
    }
}

fn launch_recovery_transition_matches(
    state: &AppState,
    plan: &GuardianLaunchRecoveryPlan,
    transition: GuardianLaunchRecoveryJournalTransition,
) -> bool {
    state
        .journals()
        .get(&plan.operation_id)
        .as_ref()
        .is_some_and(|entry| launch_recovery_journal_transition_matches(entry, plan, transition))
}

fn reject_mismatched_launch_recovery_transition(
    state: &AppState,
    plan: &GuardianLaunchRecoveryPlan,
    transition: GuardianLaunchRecoveryJournalTransition,
) -> Result<(), OperationJournalStoreError> {
    let Some(entry) = state.journals().get(&plan.operation_id) else {
        return Ok(());
    };
    if launch_recovery_journal_transition_conflicts(&entry, plan, transition) {
        return Err(OperationJournalStoreError::AlreadyTerminal);
    }
    Ok(())
}

fn suppressed_launch_recovery_outcome(plan: &GuardianLaunchRecoveryPlan) -> GuardianUserOutcome {
    launch_recovery_suppressed_user_outcome(plan)
}

fn block_guardian_for_suppressed_launch_recovery(
    guardian: &mut GuardianSummary,
    outcome: &GuardianUserOutcome,
) {
    let reason = outcome
        .details
        .first()
        .cloned()
        .unwrap_or_else(|| outcome.summary.clone());
    block_guardian_with_reason_and_guidance(guardian, Some(reason), outcome.guidance.clone());
}

fn record_guardian_intervention(
    guardian: &mut GuardianSummary,
    kind: GuardianInterventionKind,
    detail: impl Into<String>,
    silent: bool,
) {
    let existing_guidance = guardian.guidance.clone();
    guardian.record_intervention(kind, detail, silent);
    append_guardian_guidance_details(guardian, &existing_guidance);
}

pub(super) fn record_prelaunch_preset_adjustment_directive(
    guardian: &mut GuardianSummary,
    directive: &GuardianLaunchRecoveryDirective,
) {
    if matches!(
        (&directive.kind, &directive.effect),
        (
            GuardianLaunchRecoveryKind::DowngradePreset,
            GuardianLaunchRecoveryEffect::DowngradePreset { .. }
        )
    ) {
        record_guardian_intervention(
            guardian,
            GuardianInterventionKind::DowngradePreset,
            directive.description.clone(),
            false,
        );
    }
}

fn block_guardian_with_reason_and_guidance(
    guardian: &mut GuardianSummary,
    reason: Option<String>,
    guidance: Vec<String>,
) {
    let mut merged = guardian.guidance.clone();
    for detail in guidance {
        push_unique_detail(&mut merged, detail);
    }
    if let Some(reason) = reason {
        guardian.block_with_reason_and_guidance(reason, merged);
    } else {
        guardian.block_with_guidance(merged);
    }
}

pub(super) fn apply_prepare_recovery_directive(
    guardian: &mut GuardianSummary,
    attempt: &mut axial_launcher::service::AttemptOverrides,
    plan: &GuardianLaunchRecoveryPlan,
) {
    let directive = &plan.directive;
    let description = directive.description.clone();
    match &directive.effect {
        GuardianLaunchRecoveryEffect::ForceManagedRuntime => {
            record_guardian_intervention(
                guardian,
                GuardianInterventionKind::SwitchManagedRuntime,
                description.clone(),
                false,
            );
            attempt.record_runtime_intervention(description);
        }
        GuardianLaunchRecoveryEffect::StripRawJvmArgs => {
            record_guardian_intervention(
                guardian,
                GuardianInterventionKind::StripJvmArgs,
                description.clone(),
                false,
            );
            attempt.record_raw_jvm_args_intervention(description);
        }
        GuardianLaunchRecoveryEffect::DowngradePreset { .. }
        | GuardianLaunchRecoveryEffect::DisableCustomGc => {}
    }
}

pub(super) fn apply_startup_recovery_directive(
    guardian: &mut GuardianSummary,
    attempt: &mut axial_launcher::service::AttemptOverrides,
    plan: &GuardianLaunchRecoveryPlan,
) {
    let directive = &plan.directive;
    let description = directive.description.clone();
    attempt.record_startup_recovery(description.clone());
    match &directive.effect {
        GuardianLaunchRecoveryEffect::DowngradePreset { preset } => {
            record_guardian_intervention(
                guardian,
                GuardianInterventionKind::DowngradePreset,
                description,
                false,
            );
            attempt.preset_override = Some(preset.clone());
            attempt.disable_custom_gc = false;
        }
        GuardianLaunchRecoveryEffect::DisableCustomGc => {
            record_guardian_intervention(
                guardian,
                GuardianInterventionKind::DisableCustomGc,
                description,
                false,
            );
            attempt.preset_override = None;
            attempt.disable_custom_gc = true;
        }
        GuardianLaunchRecoveryEffect::ForceManagedRuntime => {
            record_guardian_intervention(
                guardian,
                GuardianInterventionKind::SwitchManagedRuntime,
                description,
                false,
            );
            attempt.force_managed_runtime = true;
            attempt.preset_override = None;
            attempt.disable_custom_gc = false;
        }
        GuardianLaunchRecoveryEffect::StripRawJvmArgs => {
            record_guardian_intervention(
                guardian,
                GuardianInterventionKind::StripJvmArgs,
                description,
                false,
            );
            attempt.ignore_extra_jvm_args = true;
        }
    }
}

pub(super) fn block_guardian_with_user_outcome(
    guardian: &mut GuardianSummary,
    outcome: &GuardianUserOutcome,
) {
    let existing_guidance = guardian.guidance.clone();
    let mut guidance = existing_guidance.clone();
    for detail in &outcome.guidance {
        push_unique_detail(&mut guidance, detail.clone());
    }

    let mut details = outcome.details.clone();
    for detail in &existing_guidance {
        push_unique_detail(&mut details, detail.clone());
    }
    for detail in &outcome.guidance {
        push_unique_detail(&mut details, detail.clone());
    }

    guardian.decision = GuardianDecision::Blocked;
    guardian.message = Some(outcome.summary.clone());
    guardian.details = details;
    guardian.guidance = guidance;
}

fn append_guardian_guidance_details(guardian: &mut GuardianSummary, guidance: &[String]) {
    for detail in guidance {
        push_unique_detail(&mut guardian.details, detail.clone());
    }
}

fn push_unique_detail(details: &mut Vec<String>, detail: String) {
    let detail = detail.trim();
    if detail.is_empty() || details.iter().any(|existing| existing == detail) {
        return;
    }
    details.push(detail.to_string());
}

#[cfg(test)]
mod tests {
    use super::super::launch_policy_guardian_mode;
    use super::super::status::serialize_guardian;
    use super::*;
    use crate::guardian::{
        GuardianStartupFailureObservation, GuardianStartupFailureRequest,
        guardian_startup_failure_outcome,
    };
    use crate::state::contracts::{OperationOutcome, OperationStatus, TargetKind};
    use crate::state::failure_memory::FailureMemoryActionOutcome;
    use crate::state::{AppStateInit, InstallStore, SessionStore};
    use axial_config::{AppPaths, ConfigStore, InstanceRegistrySnapshot, InstanceStore};
    use axial_launcher::{
        GuardianDecision, GuardianMode, LaunchSessionRecord, LaunchState, SessionId,
    };
    use axial_performance::PerformanceManager;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn launch_guardian_intervention_preserves_existing_warning_guidance() {
        let warning = "Launch memory budget is tight.".to_string();
        let mut guardian = GuardianSummary::new(GuardianMode::Managed);
        guardian.warn_with_guidance(vec![warning.clone()]);

        record_guardian_intervention(
            &mut guardian,
            GuardianInterventionKind::SwitchManagedRuntime,
            "Guardian switched to managed Java before launch.",
            false,
        );

        assert!(guardian.guidance.iter().any(|detail| detail == &warning));
        assert!(guardian.details.iter().any(|detail| detail == &warning));
    }

    #[test]
    fn launch_guardian_block_preserves_reason_before_warning_guidance() {
        let warning = "Launch memory budget is tight.".to_string();
        let guidance = "Remove the Java override or switch Guardian Mode back to Managed.";
        let reason = "explicit Java override targets Java 8 but this version requires Java 17";
        let mut guardian = GuardianSummary::new(GuardianMode::Managed);
        guardian.warn_with_guidance(vec![warning.clone()]);

        block_guardian_with_reason_and_guidance(
            &mut guardian,
            Some(format!(" {reason} ")),
            vec![guidance.to_string(), warning.clone()],
        );

        assert_eq!(
            guardian.details,
            vec![reason.to_string(), warning, guidance.to_string()]
        );
    }

    #[test]
    fn startup_stalled_blocks_with_guardian_authored_status_payload() {
        let warning = "Launch memory budget is tight.".to_string();
        let mut guardian = GuardianSummary::new(GuardianMode::Managed);
        guardian.warn_with_guidance(vec![warning.clone()]);
        let outcome = guardian_startup_failure_outcome(GuardianStartupFailureRequest {
            mode: launch_policy_guardian_mode(GuardianMode::Managed),
            observation: GuardianStartupFailureObservation::Stalled,
            crash_evidence: None,
            target_version_id: "1.21.1",
            runtime_major: 21,
            requested_java_present: false,
            explicit_java_override_present: false,
            explicit_jvm_args_present: false,
            explicit_jvm_preset_present: false,
            startup_recovery_applied: false,
            disable_custom_gc: false,
            effective_preset: "performance",
        });
        block_guardian_with_user_outcome(&mut guardian, &outcome.user_outcome);
        let payload = serialize_guardian(Some(guardian.clone())).expect("guardian payload");

        assert_eq!(outcome.failure_class, LaunchFailureClass::StartupStalled);
        assert_eq!(guardian.decision, GuardianDecision::Blocked);
        assert_eq!(
            guardian.message.as_deref(),
            Some(outcome.user_outcome.summary.as_str())
        );
        assert_eq!(
            guardian.details.first(),
            outcome.user_outcome.details.first()
        );
        assert!(guardian.details.iter().any(|detail| detail == &warning));
        assert!(
            guardian
                .details
                .iter()
                .any(|detail| detail == "Review the latest game log before retrying.")
        );
        assert_eq!(payload["decision"], serde_json::json!("blocked"));
        assert_eq!(
            payload["message"],
            serde_json::json!(outcome.user_outcome.summary)
        );
        assert_eq!(
            payload["details"][0],
            serde_json::json!(outcome.user_outcome.details[0])
        );
    }

    #[test]
    fn startup_exited_blocks_with_observed_failure_guardian_summary() {
        let mut guardian = GuardianSummary::new(GuardianMode::Custom);
        let outcome = guardian_startup_failure_outcome(GuardianStartupFailureRequest {
            mode: launch_policy_guardian_mode(GuardianMode::Custom),
            observation: GuardianStartupFailureObservation::Exited {
                failure_class: LaunchFailureClass::JvmUnsupportedOption,
            },
            crash_evidence: None,
            target_version_id: "1.21.1",
            runtime_major: 21,
            requested_java_present: false,
            explicit_java_override_present: false,
            explicit_jvm_args_present: true,
            explicit_jvm_preset_present: false,
            startup_recovery_applied: false,
            disable_custom_gc: false,
            effective_preset: "performance",
        });
        block_guardian_with_user_outcome(&mut guardian, &outcome.user_outcome);
        let payload = serialize_guardian(Some(guardian.clone())).expect("guardian payload");

        assert_eq!(
            outcome.failure_class,
            LaunchFailureClass::JvmUnsupportedOption
        );
        assert!(outcome.directive.is_none());
        assert_eq!(guardian.decision, GuardianDecision::Blocked);
        assert_eq!(
            guardian.message.as_deref(),
            Some("Guardian blocked launch startup.")
        );
        assert_eq!(
            guardian.details,
            vec![
                "Minecraft exited before startup completed with a detected JVM option compatibility failure.",
                "Remove the explicit JVM args or switch Guardian Mode back to Managed.",
            ]
        );
        assert_eq!(payload["decision"], serde_json::json!("blocked"));
        assert_eq!(
            payload["details"][0],
            serde_json::json!(outcome.user_outcome.details[0])
        );
    }

    #[test]
    fn custom_preset_startup_failure_blocks_without_recovery_directive() {
        let mut guardian = GuardianSummary::new(GuardianMode::Custom);
        let outcome = guardian_startup_failure_outcome(GuardianStartupFailureRequest {
            mode: launch_policy_guardian_mode(GuardianMode::Custom),
            observation: GuardianStartupFailureObservation::Exited {
                failure_class: LaunchFailureClass::JvmUnsupportedOption,
            },
            crash_evidence: None,
            target_version_id: "1.21.1",
            runtime_major: 21,
            requested_java_present: false,
            explicit_java_override_present: false,
            explicit_jvm_args_present: false,
            explicit_jvm_preset_present: true,
            startup_recovery_applied: false,
            disable_custom_gc: false,
            effective_preset: "ultra_low_latency",
        });
        block_guardian_with_user_outcome(&mut guardian, &outcome.user_outcome);
        let payload = serialize_guardian(Some(guardian.clone())).expect("guardian payload");

        assert_eq!(
            outcome.guardian_decision.kind,
            crate::guardian::GuardianDecisionKind::Block
        );
        assert_eq!(
            outcome.user_outcome.decision,
            crate::guardian::GuardianDecisionKind::Block
        );
        assert!(outcome.directive.is_none());
        assert_eq!(guardian.decision, GuardianDecision::Blocked);
        assert_eq!(
            guardian.message.as_deref(),
            Some("Guardian blocked launch startup.")
        );
        assert!(guardian.details.iter().any(|detail| {
            detail
                == "Minecraft exited before startup completed with a detected JVM option compatibility failure."
        }));
        assert!(guardian.details.iter().any(|detail| {
            detail == "Choose a safer JVM preset or switch Guardian Mode back to Managed."
        }));
        assert_eq!(payload["decision"], serde_json::json!("blocked"));
        assert_eq!(
            payload["message"],
            serde_json::json!("Guardian blocked launch startup.")
        );
    }

    #[test]
    fn prelaunch_preset_adjustment_records_backend_authored_guardian_intervention() {
        let directive = GuardianLaunchRecoveryDirective {
            kind: GuardianLaunchRecoveryKind::DowngradePreset,
            effect: GuardianLaunchRecoveryEffect::DowngradePreset {
                preset: "performance".to_string(),
            },
            description:
                "Guardian downgraded JVM preset from \"graalvm\" to \"performance\" before launch"
                    .to_string(),
        };
        let mut guardian = GuardianSummary::new(GuardianMode::Managed);
        guardian.warn_with_guidance(vec!["Keep existing launch guidance.".to_string()]);

        record_prelaunch_preset_adjustment_directive(&mut guardian, &directive);
        let payload = serialize_guardian(Some(guardian.clone())).expect("guardian payload");

        assert_eq!(guardian.decision, GuardianDecision::Intervened);
        assert_eq!(
            guardian.message.as_deref(),
            Some("Guardian adjusted launch settings for safety.")
        );
        assert!(guardian.details.iter().any(|detail| {
            detail == "JVM preset changed from GraalVM to Performance for compatibility."
        }));
        assert!(
            guardian
                .guidance
                .iter()
                .any(|detail| detail == "Keep existing launch guidance.")
        );
        assert_eq!(payload["decision"], serde_json::json!("intervened"));
        assert_eq!(
            payload["details"][0],
            serde_json::json!("JVM preset changed from GraalVM to Performance for compatibility.")
        );
    }

    #[tokio::test]
    async fn recovery_coordinator_rejects_unfingerprintable_intent_without_side_effects() {
        let root = unique_test_dir("runner-recovery-coordinator-rejected");
        let state = test_app_state(&root);
        let session_id = "runner-recovery-coordinator-rejected";
        state
            .sessions()
            .insert(test_record(session_id))
            .await
            .expect("insert session");
        let mut events = state
            .sessions()
            .subscribe(session_id)
            .await
            .expect("subscribe to session events");
        let mut intent = test_launch_intent(&root, session_id);
        intent.target_version_id = "/invalid/version".to_string();
        let mut recovery_attempts = 0;
        let mut guardian = GuardianSummary::new(GuardianMode::Managed);
        guardian.warn_with_guidance(vec!["Keep existing launch guidance.".to_string()]);
        let original_guardian = guardian.clone();

        let outcome = handle_recovery_directive(RecoveryDirectiveRequest {
            state: &state,
            session_id,
            intent: &intent,
            directive: test_recovery_directive(GuardianLaunchRecoveryKind::StripRawJvmArgs),
            mode: crate::guardian::GuardianMode::Managed,
            failure_class: LaunchFailureClass::JvmUnsupportedOption,
            recovery_attempts: &mut recovery_attempts,
            max_recovery_attempts: 3,
            guardian: &mut guardian,
        })
        .await
        .expect("reject invalid recovery intent without a journal error");

        assert!(matches!(outcome, RecoveryDirectiveOutcome::Rejected));
        assert_eq!(recovery_attempts, 1);
        assert_eq!(guardian, original_guardian);
        assert!(state.journals().list().is_empty());
        assert!(state.failure_memory().list().is_empty());
        assert!(matches!(
            events.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn recovery_coordinator_returns_and_emits_suppression_outcome() {
        let root = unique_test_dir("runner-recovery-coordinator-suppressed");
        let state = test_app_state(&root);
        let session_id = "runner-recovery-coordinator-suppressed";
        state
            .sessions()
            .insert(test_record(session_id))
            .await
            .expect("insert session");
        let intent = test_launch_intent(&root, session_id);
        let failed_plan = test_recovery_plan(&intent, GuardianLaunchRecoveryKind::StripRawJvmArgs);
        record_guardian_launch_recovery_attempt(&state, session_id, &failed_plan)
            .await
            .expect("persist initial recovery attempt");
        record_failed_self_healing_if_any(
            &state,
            session_id,
            Some(&failed_plan),
            LaunchFailureClass::JvmUnsupportedOption,
        )
        .await
        .expect("persist failed recovery attempt");
        assert_eq!(state.journals().list().len(), 1);

        let mut events = state
            .sessions()
            .subscribe(session_id)
            .await
            .expect("subscribe to session events");
        let expected = suppressed_launch_recovery_outcome(&failed_plan);
        let mut recovery_attempts = 0;
        let mut guardian = GuardianSummary::new(GuardianMode::Managed);
        guardian.warn_with_guidance(vec!["Keep existing launch guidance.".to_string()]);

        let outcome = handle_recovery_directive(RecoveryDirectiveRequest {
            state: &state,
            session_id,
            intent: &intent,
            directive: test_recovery_directive(GuardianLaunchRecoveryKind::StripRawJvmArgs),
            mode: crate::guardian::GuardianMode::Managed,
            failure_class: LaunchFailureClass::JvmUnsupportedOption,
            recovery_attempts: &mut recovery_attempts,
            max_recovery_attempts: 3,
            guardian: &mut guardian,
        })
        .await
        .expect("coordinate suppressed recovery");

        let RecoveryDirectiveOutcome::Suppressed(returned) = outcome else {
            panic!("expected suppressed recovery outcome");
        };
        assert_eq!(returned, expected);
        assert_eq!(recovery_attempts, 1);
        assert_eq!(guardian.decision, GuardianDecision::Blocked);
        assert_eq!(
            guardian.message.as_deref(),
            Some("Guardian blocked an unsafe launch setup.")
        );
        assert!(
            guardian
                .details
                .iter()
                .any(|detail| detail == &expected.details[0])
        );
        assert!(guardian.guidance.iter().any(|detail| {
            detail == "Review the latest game log or change the affected launch setting before retrying."
        }));
        let log = match events.try_recv().expect("suppression recovery log") {
            axial_launcher::LaunchEvent::Log(log) => log,
            event => panic!("expected suppression recovery log, got {event:?}"),
        };
        assert_eq!(log.source, "system");
        assert_eq!(log.text, expected.summary);
        assert_eq!(state.journals().list().len(), 2);
        assert!(state.journals().list().iter().any(|journal| {
            journal.status == OperationStatus::Blocked
                && journal.outcome == Some(OperationOutcome::Suppressed)
        }));
        let memory = state.failure_memory().list();
        assert_eq!(memory.len(), 1);
        assert_eq!(
            memory[0].last_action_outcome,
            Some(FailureMemoryActionOutcome::Suppressed)
        );

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn launch_recovery_memory_records_redacted_attempt_failure_and_suppression() {
        let root = unique_test_dir("runner-launch-recovery-memory");
        let state = test_app_state(&root);
        let session_id = "runner-launch-recovery-memory";
        state
            .sessions()
            .insert(test_record(session_id))
            .await
            .expect("insert session");
        let intent = test_launch_intent(&root, session_id);
        let plan = test_recovery_plan(&intent, GuardianLaunchRecoveryKind::StripRawJvmArgs);
        assert_eq!(plan.target.kind, TargetKind::Instance);
        assert_eq!(plan.target.id, intent.instance_id);

        let attempt = record_guardian_launch_recovery_attempt(&state, session_id, &plan)
            .await
            .expect("persist launch recovery attempt");
        assert_eq!(
            attempt.status,
            crate::guardian::GuardianLaunchRecoveryStatus::Recorded
        );

        record_failed_self_healing_if_any(
            &state,
            session_id,
            Some(&plan),
            LaunchFailureClass::JvmUnsupportedOption,
        )
        .await
        .expect("persist failed launch recovery");

        let suppressed_plan =
            test_recovery_plan(&intent, GuardianLaunchRecoveryKind::StripRawJvmArgs);
        let later_session_id = "runner-launch-recovery-memory-later-session";

        let suppressed =
            record_guardian_launch_recovery_attempt(&state, later_session_id, &suppressed_plan)
                .await
                .expect("persist suppressed launch recovery attempt");
        assert_eq!(
            suppressed.status,
            crate::guardian::GuardianLaunchRecoveryStatus::Suppressed
        );
        let user_outcome = suppressed_launch_recovery_outcome(&suppressed_plan);
        let mut guardian = GuardianSummary::new(GuardianMode::Managed);
        guardian.warn_with_guidance(vec!["Keep existing launch guidance.".to_string()]);
        block_guardian_for_suppressed_launch_recovery(&mut guardian, &user_outcome);
        let payload = serialize_guardian(Some(guardian.clone())).expect("guardian payload");

        assert_eq!(guardian.decision, GuardianDecision::Blocked);
        assert!(guardian.interventions.is_empty());
        assert_eq!(
            guardian.message.as_deref(),
            Some("Guardian blocked an unsafe launch setup.")
        );
        assert!(guardian.details.iter().any(|detail| {
            detail == "Guardian suppressed a repeated launch self-healing retry for explicit JVM argument recovery because the same recovery failed recently."
        }));
        assert!(guardian.guidance.iter().any(|detail| {
            detail == "Review the latest game log or change the affected launch setting before retrying."
        }));
        assert_eq!(payload["decision"], serde_json::json!("blocked"));
        assert_eq!(
            payload["message"],
            serde_json::json!("Guardian blocked an unsafe launch setup.")
        );

        let memory = state.failure_memory().list();
        assert_eq!(memory.len(), 1);
        assert_eq!(
            memory[0].last_action_outcome,
            Some(FailureMemoryActionOutcome::Suppressed)
        );
        assert_eq!(memory[0].repair_attempt_count, 1);
        assert!(memory[0].suppression_until.is_some());

        let memory_json = serde_json::to_string(&memory).expect("memory json");
        assert!(
            memory[0]
                .user_intent_hash
                .as_deref()
                .is_some_and(|value| { value.len() == 78 && value.starts_with("sha256.") })
        );
        for fragment in ["-Dtoken", "raw-secret-token", "-XX:+UseZGC", "/home/alice"] {
            assert!(
                !memory_json.contains(fragment),
                "launch recovery memory leaked {fragment:?}: {memory_json}"
            );
        }
        let payload_json = payload.to_string();
        for fragment in ["-Dtoken", "raw-secret-token", "-XX:+UseZGC", "/home/alice"] {
            assert!(
                !payload_json.contains(fragment),
                "suppressed recovery payload leaked {fragment:?}: {payload_json}"
            );
        }

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn later_failure_class_preserves_trigger_journal_and_memory_identity() {
        for (name, kind) in [
            (
                "runner-strip-recovery-trigger-identity",
                GuardianLaunchRecoveryKind::StripRawJvmArgs,
            ),
            (
                "runner-gc-recovery-trigger-identity",
                GuardianLaunchRecoveryKind::DisableCustomGc,
            ),
        ] {
            let root = unique_test_dir(name);
            let state = test_app_state(&root);
            let session_id = name;
            state
                .sessions()
                .insert(test_record(session_id))
                .await
                .expect("insert session");
            let intent = test_launch_intent(&root, session_id);
            let plan = test_recovery_plan(&intent, kind);
            assert_eq!(
                plan.trigger_failure_class,
                LaunchFailureClass::JvmUnsupportedOption
            );

            record_guardian_launch_recovery_attempt(&state, session_id, &plan)
                .await
                .expect("persist trigger-keyed launch recovery attempt");
            record_failed_self_healing_if_any(
                &state,
                session_id,
                Some(&plan),
                LaunchFailureClass::Unknown,
            )
            .await
            .expect("later failure terminalizes trigger-keyed recovery");

            let journal = state
                .journals()
                .get(&plan.operation_id)
                .expect("terminal recovery journal");
            assert_eq!(journal.status, OperationStatus::Failed);
            assert_eq!(journal.outcome, Some(OperationOutcome::Failed));
            assert!(launch_recovery_journal_transition_matches(
                &journal,
                &plan,
                GuardianLaunchRecoveryJournalTransition::Failure,
            ));
            assert_eq!(
                journal.guardian_diagnosis_ids,
                vec!["jvm_arg_unsupported".to_string()]
            );
            assert_eq!(state.journals().list().len(), 1);
            let memory = state.failure_memory().list();
            assert_eq!(memory.len(), 1);
            assert_eq!(memory[0].diagnosis_id.as_str(), "jvm_arg_unsupported");
            assert!(memory[0].key.as_str().contains("jvm_arg_unsupported"));
            assert_eq!(
                memory[0].last_action_outcome,
                Some(FailureMemoryActionOutcome::Failed)
            );

            let suppressed_plan = test_recovery_plan(&intent, kind);
            let suppressed =
                record_guardian_launch_recovery_attempt(&state, session_id, &suppressed_plan)
                    .await
                    .expect("trigger-keyed failure memory suppresses the next attempt");
            assert_eq!(
                suppressed.status,
                crate::guardian::GuardianLaunchRecoveryStatus::Suppressed
            );
            let _ = fs::remove_dir_all(root);
        }
    }

    #[tokio::test]
    async fn repeated_preset_downgrade_recovery_is_suppressed_for_same_launch_intent() {
        let root = unique_test_dir("runner-preset-recovery-suppression");
        let state = test_app_state(&root);
        let session_id = "runner-preset-recovery-suppression";
        state
            .sessions()
            .insert(test_record(session_id))
            .await
            .expect("insert session");
        let intent = test_launch_intent(&root, session_id);
        let plan = test_recovery_plan(&intent, GuardianLaunchRecoveryKind::DowngradePreset);

        let attempt = record_guardian_launch_recovery_attempt(&state, session_id, &plan)
            .await
            .expect("persist launch recovery attempt");
        assert_eq!(
            attempt.status,
            crate::guardian::GuardianLaunchRecoveryStatus::Recorded
        );

        record_failed_self_healing_if_any(
            &state,
            session_id,
            Some(&plan),
            LaunchFailureClass::JvmUnsupportedOption,
        )
        .await
        .expect("persist failed launch recovery");

        let suppressed_plan =
            test_recovery_plan(&intent, GuardianLaunchRecoveryKind::DowngradePreset);

        let suppressed =
            record_guardian_launch_recovery_attempt(&state, session_id, &suppressed_plan)
                .await
                .expect("persist suppressed launch recovery attempt");
        assert_eq!(
            suppressed.status,
            crate::guardian::GuardianLaunchRecoveryStatus::Suppressed
        );

        let memory = state.failure_memory().list();
        assert_eq!(memory.len(), 1);
        assert_eq!(
            memory[0].last_action_outcome,
            Some(FailureMemoryActionOutcome::Suppressed)
        );
        assert_eq!(memory[0].repair_attempt_count, 1);
        assert!(memory[0].suppression_until.is_some());

        let memory_json = serde_json::to_string(&memory).expect("memory json");
        assert!(
            memory[0]
                .user_intent_hash
                .as_deref()
                .is_some_and(|value| { value.len() == 78 && value.starts_with("sha256.") })
        );
        for fragment in [
            "-Dtoken",
            "raw-secret-token",
            "-XX:+UseZGC",
            "/home/alice",
            "graalvm",
        ] {
            assert!(
                !memory_json.contains(fragment),
                "preset recovery memory leaked {fragment:?}: {memory_json}"
            );
        }

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn runner_retains_ownership_until_terminal_journal_retry_succeeds() {
        let root = unique_test_dir("runner-terminal-journal-retry");
        let state = test_app_state(&root);
        let session_id = "runner-terminal-journal-retry";
        state
            .sessions()
            .insert(test_record(session_id))
            .await
            .expect("insert session");
        let intent = test_launch_intent(&root, session_id);
        let plan = test_recovery_plan(&intent, GuardianLaunchRecoveryKind::StripRawJvmArgs);
        record_guardian_launch_recovery_attempt(&state, session_id, &plan)
            .await
            .expect("persist launch recovery attempt");
        let journal_path = root
            .join("config")
            .join("state")
            .join("operation-journals.json");
        fs::remove_file(&journal_path).expect("remove journal snapshot");
        fs::create_dir_all(&journal_path).expect("block journal snapshot destination");

        let state_task = state.clone();
        let plan_task = plan.clone();
        let task = tokio::spawn(async move {
            record_failed_self_healing_if_any(
                &state_task,
                session_id,
                Some(&plan_task),
                LaunchFailureClass::JvmUnsupportedOption,
            )
            .await
        });
        tokio::time::sleep(Duration::from_millis(100)).await;

        assert!(!task.is_finished());
        assert_eq!(
            state
                .journals()
                .get(&plan.operation_id)
                .expect("planned recovery journal")
                .status,
            OperationStatus::Planned
        );
        fs::remove_dir_all(&journal_path).expect("restore journal destination");
        tokio::time::timeout(Duration::from_secs(3), task)
            .await
            .expect("journal retry completes")
            .expect("retry task")
            .expect("terminal recovery commit");

        assert_eq!(
            state
                .journals()
                .get(&plan.operation_id)
                .expect("terminal recovery journal")
                .status,
            OperationStatus::Failed
        );
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn nonretryable_missing_operation_propagates_without_spinning() {
        let root = unique_test_dir("runner-missing-recovery-operation");
        let state = test_app_state(&root);
        let session_id = "runner-missing-recovery-operation";
        state
            .sessions()
            .insert(test_record(session_id))
            .await
            .expect("insert session");
        let intent = test_launch_intent(&root, session_id);
        let plan = test_recovery_plan(&intent, GuardianLaunchRecoveryKind::StripRawJvmArgs);

        let error = tokio::time::timeout(
            Duration::from_millis(250),
            record_failed_self_healing_if_any(
                &state,
                session_id,
                Some(&plan),
                LaunchFailureClass::JvmUnsupportedOption,
            ),
        )
        .await
        .expect("nonretryable journal error returns")
        .expect_err("missing planned operation must fail");

        assert!(matches!(
            error,
            OperationJournalStoreError::MissingOperation
        ));
        assert!(!state.journals().has_retry_candidate());
        assert!(state.failure_memory().list().is_empty());
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn cancelled_runner_retry_preserves_candidate_for_reconciliation() {
        let root = unique_test_dir("runner-terminal-journal-cancel");
        let state = test_app_state(&root);
        let session_id = "runner-terminal-journal-cancel";
        state
            .sessions()
            .insert(test_record(session_id))
            .await
            .expect("insert session");
        let intent = test_launch_intent(&root, session_id);
        let plan = test_recovery_plan(&intent, GuardianLaunchRecoveryKind::StripRawJvmArgs);
        record_guardian_launch_recovery_attempt(&state, session_id, &plan)
            .await
            .expect("persist launch recovery attempt");
        assert!(state.failure_memory().list().is_empty());
        let journal_path = root
            .join("config")
            .join("state")
            .join("operation-journals.json");
        fs::remove_file(&journal_path).expect("remove journal snapshot");
        fs::create_dir_all(&journal_path).expect("block journal snapshot destination");

        let state_task = state.clone();
        let plan_task = plan.clone();
        let task = tokio::spawn(async move {
            record_failed_self_healing_if_any(
                &state_task,
                session_id,
                Some(&plan_task),
                LaunchFailureClass::JvmUnsupportedOption,
            )
            .await
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        task.abort();
        let _ = task.await;
        assert!(state.failure_memory().list().is_empty());

        fs::remove_dir_all(&journal_path).expect("restore journal destination");
        state
            .journals()
            .retry()
            .await
            .expect("commit preserved terminal candidate");
        record_failed_self_healing_if_any(
            &state,
            session_id,
            Some(&plan),
            LaunchFailureClass::JvmUnsupportedOption,
        )
        .await
        .expect("reconcile terminal recovery");

        assert_eq!(state.journals().list().len(), 1);
        assert_eq!(
            state
                .journals()
                .get(&plan.operation_id)
                .expect("reconciled recovery journal")
                .status,
            OperationStatus::Failed
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn suppressed_launch_recovery_block_uses_existing_guardian_block_copy() {
        let intent = test_launch_intent(Path::new("/tmp/axial-test"), "session");
        let plan = test_recovery_plan(&intent, GuardianLaunchRecoveryKind::StripRawJvmArgs);
        let outcome = suppressed_launch_recovery_outcome(&plan);
        let reason = outcome.summary.clone();
        let mut guardian = GuardianSummary::new(GuardianMode::Managed);
        guardian.warn_with_guidance(vec!["Keep existing launch guidance.".to_string()]);

        block_guardian_for_suppressed_launch_recovery(&mut guardian, &outcome);

        assert_eq!(guardian.decision, GuardianDecision::Blocked);
        assert_eq!(
            guardian.message.as_deref(),
            Some("Guardian blocked an unsafe launch setup.")
        );
        assert!(guardian.details.iter().any(|detail| detail == &reason));
        assert!(
            guardian
                .guidance
                .iter()
                .any(|detail| detail == "Keep existing launch guidance.")
        );
        assert!(guardian.guidance.iter().any(|detail| {
            detail == "Review the latest game log or change the affected launch setting before retrying."
        }));
    }

    #[test]
    fn application_recovery_planning_rejects_unfingerprintable_intent() {
        for mut intent in [
            test_launch_intent(Path::new("/tmp/axial-test"), "invalid-version"),
            test_launch_intent(Path::new("/tmp/axial-test"), "oversized-args"),
        ] {
            if intent.session_id == "invalid-version" {
                intent.target_version_id = "/invalid/version".to_string();
            } else {
                intent.extra_jvm_args = vec!["x".repeat(4_097)];
            }
            let rejection = plan_guardian_launch_recovery_directive(
                &intent,
                GuardianLaunchRecoveryDirective {
                    kind: GuardianLaunchRecoveryKind::StripRawJvmArgs,
                    effect: GuardianLaunchRecoveryEffect::StripRawJvmArgs,
                    description: "Guardian removed incompatible explicit JVM args before launch"
                        .to_string(),
                },
                crate::guardian::GuardianMode::Managed,
                LaunchFailureClass::JvmUnsupportedOption,
            )
            .expect_err("unfingerprintable launch intent rejected");
            assert_eq!(
                rejection,
                GuardianLaunchRecoveryPlanRejection::InvalidUserIntentFingerprint
            );
        }
    }

    fn test_recovery_plan(
        intent: &axial_launcher::LaunchIntent,
        kind: GuardianLaunchRecoveryKind,
    ) -> GuardianLaunchRecoveryPlan {
        let directive = test_recovery_directive(kind);
        plan_guardian_launch_recovery_directive(
            intent,
            directive,
            crate::guardian::GuardianMode::Managed,
            match kind {
                GuardianLaunchRecoveryKind::SwitchManagedRuntime => {
                    LaunchFailureClass::JavaRuntimeMismatch
                }
                GuardianLaunchRecoveryKind::StripRawJvmArgs
                | GuardianLaunchRecoveryKind::DowngradePreset
                | GuardianLaunchRecoveryKind::DisableCustomGc => {
                    LaunchFailureClass::JvmUnsupportedOption
                }
            },
        )
        .expect("recovery plan")
    }

    fn test_recovery_directive(
        kind: GuardianLaunchRecoveryKind,
    ) -> GuardianLaunchRecoveryDirective {
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

    fn test_app_state(root: &Path) -> AppState {
        let paths = test_paths(root);
        let config = Arc::new(ConfigStore::load_from(paths.clone()).expect("load config"));
        let instances = Arc::new(
            InstanceStore::from_snapshot(paths.clone(), InstanceRegistrySnapshot::default())
                .expect("load instances"),
        );
        AppState::new(AppStateInit {
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
        })
    }

    fn test_paths(root: &Path) -> AppPaths {
        let config_dir = root.join("config");
        AppPaths {
            config_file: config_dir.join("config.json"),
            instances_file: config_dir.join("instances.json"),
            instances_dir: config_dir.join("instances"),
            music_dir: config_dir.join("music"),
            library_dir: config_dir.join("library"),
            config_dir,
        }
    }

    fn test_record(session_id: &str) -> LaunchSessionRecord {
        LaunchSessionRecord {
            session_id: SessionId(session_id.to_string()),
            instance_id: "instance".to_string(),
            version_id: "1.21.1".to_string(),
            launched_at: Some("2026-01-01T00:00:00.000Z".to_string()),
            benchmark: None,
            state: LaunchState::Queued,
            pid: None,
            process_started_at_ms: None,
            boot_completed_at_ms: None,
            boot_duration_ms: None,
            priority: None,
            exit_code: None,
            command: Vec::new(),
            java_path: None,
            natives_dir: None,
            failure: None,
            crash_evidence: None,
            healing: None,
            guardian: None,
            outcome: None,
            stages: Vec::new(),
        }
    }

    fn test_launch_intent(root: &Path, session_id: &str) -> axial_launcher::LaunchIntent {
        axial_launcher::LaunchIntent {
            session_id: session_id.to_string(),
            library_dir: root.join("library"),
            instance_id: "instance".to_string(),
            version_id: "1.21.1".to_string(),
            target_version_id: "1.21.1".to_string(),
            loader: "vanilla".to_string(),
            is_modded: false,
            username: "Player".to_string(),
            auth: axial_launcher::LaunchAuthContext::offline("Player"),
            requested_java: "/home/alice/.jdks/bad-java/bin/java".to_string(),
            requested_preset: "graalvm".to_string(),
            extra_jvm_args: vec![
                "-Dtoken=raw-secret-token".to_string(),
                "-XX:+UseZGC".to_string(),
            ],
            max_memory_mb: 4096,
            min_memory_mb: 1024,
            resolution: None,
            launcher_name: "axial".to_string(),
            launcher_version: "test".to_string(),
            game_dir: None,
            guardian: axial_launcher::LaunchGuardianContext {
                mode: GuardianMode::Managed,
                java_override_origin: Some(axial_launcher::OverrideOrigin::Instance),
                preset_override_origin: Some(axial_launcher::OverrideOrigin::Instance),
                raw_jvm_args_origin: Some(axial_launcher::OverrideOrigin::Instance),
            },
            performance_mode: "managed".to_string(),
        }
    }

    fn unique_test_dir(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_nanos();
        std::env::temp_dir().join(format!("{prefix}-{}-{nanos}", std::process::id()))
    }
}
