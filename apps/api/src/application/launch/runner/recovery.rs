use super::trace_launch_event;
use crate::guardian::{
    GuardianCopyRequest, GuardianDirective, GuardianLaunchRecoveryCurrentIntent,
    GuardianLaunchRecoveryJournalTransition, GuardianLaunchRecoveryOutcome,
    GuardianLaunchRecoveryPlan, GuardianLaunchRecoveryPlanRejection,
    GuardianLaunchRecoveryPlanRequest, GuardianLaunchRecoveryRecordRequest,
    GuardianManagedJavaReason, GuardianPresetDowngradeReason, GuardianStripJvmArgsReason,
    GuardianUserOutcome, author_guardian_copy, guardian_directive_description,
    guardian_failed_launch_recovery_log, guardian_summary_with_intervention,
    guardian_summary_with_suppressed_outcome, launch_recovery_journal_transition_conflicts,
    launch_recovery_journal_transition_matches, launch_recovery_user_intent_fingerprint,
    plan_launch_recovery_directive, record_launch_recovery_attempt, record_launch_recovery_failure,
    record_launch_recovery_success,
};
use crate::logging::timestamp_utc;
use crate::state::{AppState, OperationJournalReconciliation, OperationJournalStoreError};
use axial_launcher::{GuardianInterventionKind, GuardianSummary, LaunchFailureClass};
use std::time::Duration;

const JOURNAL_RETRY_INITIAL_DELAY: Duration = Duration::from_millis(25);
const JOURNAL_RETRY_MAX_DELAY: Duration = Duration::from_secs(1);

pub(super) enum RecoveryDirectiveOutcome {
    Apply(GuardianLaunchRecoveryPlan),
    Exhausted,
    Rejected,
    Suppressed(GuardianUserOutcome),
}

#[derive(Clone, Copy)]
pub(super) enum RecoveryDirectiveStage {
    Prepare,
    Startup,
}

impl RecoveryDirectiveStage {
    fn accepts(self, directive: &GuardianDirective) -> bool {
        match self {
            Self::Prepare => directive.is_prepare_recovery(),
            Self::Startup => directive.is_startup_recovery(),
        }
    }
}

pub(super) struct RecoveryDirectiveRequest<'a> {
    pub(super) state: &'a AppState,
    pub(super) session_id: &'a str,
    pub(super) intent: &'a axial_launcher::LaunchIntent,
    pub(super) directive: GuardianDirective,
    pub(super) stage: RecoveryDirectiveStage,
    pub(super) mode: crate::guardian::GuardianMode,
    pub(super) failure_class: LaunchFailureClass,
    pub(super) recovery_attempts: &'a mut u8,
    pub(super) max_recovery_attempts: u8,
    pub(super) guardian: &'a mut GuardianSummary,
}

pub(super) async fn handle_recovery_directive(
    request: RecoveryDirectiveRequest<'_>,
) -> Result<RecoveryDirectiveOutcome, OperationJournalStoreError> {
    if !request.stage.accepts(&request.directive) {
        return Ok(RecoveryDirectiveOutcome::Rejected);
    }
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
        let user_outcome = author_guardian_copy(GuardianCopyRequest::launch_recovery_suppressed(
            &plan.directive,
        ))
        .expect("launch recovery suppression copy request is closed");
        request
            .state
            .sessions()
            .emit_log(
                request.session_id,
                "system",
                user_outcome.summary().to_string(),
            )
            .await;
        *request.guardian =
            guardian_summary_with_suppressed_outcome(request.guardian, &user_outcome);
        return Ok(RecoveryDirectiveOutcome::Suppressed(user_outcome));
    }

    request
        .state
        .sessions()
        .emit_log(
            request.session_id,
            "system",
            guardian_directive_description(&plan.directive),
        )
        .await;
    Ok(RecoveryDirectiveOutcome::Apply(plan))
}

fn plan_guardian_launch_recovery_directive(
    intent: &axial_launcher::LaunchIntent,
    directive: GuardianDirective,
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
        &directive,
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
            "guardian launch recovery attempt: directive={:?} status={:?} operation={}",
            plan.directive,
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
            "guardian launch recovery succeeded: directive={:?} operation={}",
            plan.directive,
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
            "guardian launch recovery failed: directive={:?} trigger={trigger_failure_class:?} observed={observed_failure_class:?} operation={}",
            plan.directive,
            outcome.operation_id.as_str()
        ),
    );
    state
        .sessions()
        .emit_log(
            session_id,
            "system",
            guardian_failed_launch_recovery_log(&plan.directive),
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

pub(super) fn record_prelaunch_preset_adjustment_directive(
    guardian: &mut GuardianSummary,
    directive: &GuardianDirective,
) {
    match directive {
        GuardianDirective::DowngradeJvmPreset {
            reason: GuardianPresetDowngradeReason::Compatibility { .. },
            ..
        } => {
            *guardian = guardian_summary_with_intervention(
                guardian,
                GuardianInterventionKind::DowngradePreset,
                guardian_directive_description(directive),
                false,
            )
        }
        _ => unreachable!("prelaunch preset adjustment emitted non-compatibility directive"),
    }
}

pub(super) fn apply_prepare_recovery_directive(
    guardian: &mut GuardianSummary,
    attempt: &mut axial_launcher::service::AttemptOverrides,
    plan: &GuardianLaunchRecoveryPlan,
) -> bool {
    let directive = &plan.directive;
    if !directive.is_prepare_recovery() {
        return false;
    }
    let description = guardian_directive_description(directive);
    match directive {
        GuardianDirective::UseManagedJava {
            reason: GuardianManagedJavaReason::PrepareFailure,
        } => {
            *guardian = guardian_summary_with_intervention(
                guardian,
                GuardianInterventionKind::SwitchManagedRuntime,
                description.clone(),
                false,
            );
            attempt.record_runtime_intervention(description);
        }
        GuardianDirective::StripJvmArgs {
            reason: GuardianStripJvmArgsReason::PrepareFailure,
        } => {
            *guardian = guardian_summary_with_intervention(
                guardian,
                GuardianInterventionKind::StripJvmArgs,
                description.clone(),
                false,
            );
            attempt.record_raw_jvm_args_intervention(description);
        }
        _ => return false,
    }
    true
}

pub(super) fn apply_startup_recovery_directive(
    guardian: &mut GuardianSummary,
    attempt: &mut axial_launcher::service::AttemptOverrides,
    plan: &GuardianLaunchRecoveryPlan,
) -> bool {
    let directive = &plan.directive;
    if !directive.is_startup_recovery() {
        return false;
    }
    let description = guardian_directive_description(directive);
    attempt.record_startup_recovery(description.clone());
    match directive {
        GuardianDirective::DowngradeJvmPreset {
            preset,
            reason: GuardianPresetDowngradeReason::StartupRecovery,
        } => {
            *guardian = guardian_summary_with_intervention(
                guardian,
                GuardianInterventionKind::DowngradePreset,
                description,
                false,
            );
            attempt.preset_override = Some(preset.as_str().to_string());
            attempt.disable_custom_gc = false;
        }
        GuardianDirective::DisableCustomGc => {
            *guardian = guardian_summary_with_intervention(
                guardian,
                GuardianInterventionKind::DisableCustomGc,
                description,
                false,
            );
            attempt.preset_override = None;
            attempt.disable_custom_gc = true;
        }
        GuardianDirective::UseManagedJava {
            reason: GuardianManagedJavaReason::StartupRecovery,
        } => {
            *guardian = guardian_summary_with_intervention(
                guardian,
                GuardianInterventionKind::SwitchManagedRuntime,
                description,
                false,
            );
            attempt.force_managed_runtime = true;
            attempt.preset_override = None;
            attempt.disable_custom_gc = false;
        }
        _ => return false,
    }
    true
}

#[cfg(test)]
mod tests {
    use super::super::status::serialize_guardian;
    use super::*;
    use crate::application::guardian_conversion::api_guardian_mode;
    use crate::guardian::{
        GuardianStartupFailureObservation, GuardianStartupFailureRequest,
        guardian_startup_failure_outcome, guardian_summary_with_blocked_outcome,
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

    #[derive(Clone, Copy, Debug)]
    enum RecoveryCase {
        SwitchManagedRuntime,
        StripRawJvmArgs,
        DowngradePreset,
        DisableCustomGc,
    }

    #[test]
    fn launch_guardian_intervention_preserves_existing_warning_guidance() {
        let warning = "Launch memory budget is tight.".to_string();
        let mut guardian = GuardianSummary::new(GuardianMode::Managed);
        guardian.warn_with_guidance(vec![warning.clone()]);

        guardian = guardian_summary_with_intervention(
            &guardian,
            GuardianInterventionKind::SwitchManagedRuntime,
            "Guardian switched to managed Java before launch.".to_string(),
            false,
        );

        assert!(guardian.guidance.iter().any(|detail| detail == &warning));
        assert!(guardian.details.iter().any(|detail| detail == &warning));
    }

    #[test]
    fn recovery_executors_apply_every_closed_directive_effect() {
        let intent = test_launch_intent(Path::new("/tmp/axial-test"), "executor-effects");

        let mut guardian = GuardianSummary::new(GuardianMode::Managed);
        let mut attempt = axial_launcher::service::AttemptOverrides::default();
        let managed_prepare = test_recovery_plan_for_directive(
            &intent,
            GuardianDirective::UseManagedJava {
                reason: GuardianManagedJavaReason::PrepareFailure,
            },
            LaunchFailureClass::JavaRuntimeMismatch,
        );
        assert!(apply_prepare_recovery_directive(
            &mut guardian,
            &mut attempt,
            &managed_prepare
        ));
        assert!(attempt.force_managed_runtime);
        assert!(attempt.runtime_intervention_applied);

        let mut guardian = GuardianSummary::new(GuardianMode::Managed);
        let mut attempt = axial_launcher::service::AttemptOverrides::default();
        let strip_prepare = test_recovery_plan(&intent, RecoveryCase::StripRawJvmArgs);
        assert!(apply_prepare_recovery_directive(
            &mut guardian,
            &mut attempt,
            &strip_prepare
        ));
        assert!(attempt.ignore_extra_jvm_args);
        assert!(attempt.raw_jvm_args_intervention_applied);

        let mut guardian = GuardianSummary::new(GuardianMode::Managed);
        let mut attempt = axial_launcher::service::AttemptOverrides::default();
        let managed_startup = test_recovery_plan(&intent, RecoveryCase::SwitchManagedRuntime);
        assert!(apply_startup_recovery_directive(
            &mut guardian,
            &mut attempt,
            &managed_startup
        ));
        assert!(attempt.force_managed_runtime);
        assert!(attempt.startup_recovery_applied);
        assert_eq!(attempt.retry_count, 1);

        let mut guardian = GuardianSummary::new(GuardianMode::Managed);
        let mut attempt = axial_launcher::service::AttemptOverrides::default();
        let preset_startup = test_recovery_plan(&intent, RecoveryCase::DowngradePreset);
        assert!(apply_startup_recovery_directive(
            &mut guardian,
            &mut attempt,
            &preset_startup
        ));
        assert_eq!(attempt.preset_override.as_deref(), Some("performance"));
        assert!(!attempt.disable_custom_gc);

        let mut guardian = GuardianSummary::new(GuardianMode::Managed);
        let mut attempt = axial_launcher::service::AttemptOverrides::default();
        let gc_startup = test_recovery_plan(&intent, RecoveryCase::DisableCustomGc);
        assert!(apply_startup_recovery_directive(
            &mut guardian,
            &mut attempt,
            &gc_startup
        ));
        assert_eq!(attempt.preset_override, None);
        assert!(attempt.disable_custom_gc);

        let mut guardian = GuardianSummary::new(GuardianMode::Managed);
        let mut attempt = axial_launcher::service::AttemptOverrides::default();
        assert!(!apply_startup_recovery_directive(
            &mut guardian,
            &mut attempt,
            &managed_prepare
        ));
        assert!(!attempt.startup_recovery_applied);
        assert!(!attempt.force_managed_runtime);

        assert!(!apply_prepare_recovery_directive(
            &mut guardian,
            &mut attempt,
            &managed_startup
        ));
        assert!(!attempt.runtime_intervention_applied);
    }

    #[test]
    fn startup_stalled_blocks_with_guardian_authored_status_payload() {
        let warning = "Launch memory budget is tight.".to_string();
        let mut guardian = GuardianSummary::new(GuardianMode::Managed);
        guardian.warn_with_guidance(vec![warning.clone()]);
        let outcome = guardian_startup_failure_outcome(GuardianStartupFailureRequest {
            mode: api_guardian_mode(GuardianMode::Managed),
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
        guardian = guardian_summary_with_blocked_outcome(&guardian, &outcome.user_outcome);
        let payload = serialize_guardian(Some(guardian.clone())).expect("guardian payload");

        assert_eq!(outcome.failure_class, LaunchFailureClass::StartupStalled);
        assert_eq!(guardian.decision, GuardianDecision::Blocked);
        assert_eq!(
            guardian.message.as_deref(),
            Some(outcome.user_outcome.summary())
        );
        assert_eq!(
            guardian.details.first(),
            outcome.user_outcome.details().first()
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
            serde_json::json!(outcome.user_outcome.summary())
        );
        assert_eq!(
            payload["details"][0],
            serde_json::json!(outcome.user_outcome.details()[0])
        );
    }

    #[test]
    fn startup_exited_blocks_with_observed_failure_guardian_summary() {
        let mut guardian = GuardianSummary::new(GuardianMode::Custom);
        let outcome = guardian_startup_failure_outcome(GuardianStartupFailureRequest {
            mode: api_guardian_mode(GuardianMode::Custom),
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
        guardian = guardian_summary_with_blocked_outcome(&guardian, &outcome.user_outcome);
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
            serde_json::json!(outcome.user_outcome.details()[0])
        );
    }

    #[test]
    fn custom_preset_startup_failure_blocks_without_recovery_directive() {
        let mut guardian = GuardianSummary::new(GuardianMode::Custom);
        let outcome = guardian_startup_failure_outcome(GuardianStartupFailureRequest {
            mode: api_guardian_mode(GuardianMode::Custom),
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
        guardian = guardian_summary_with_blocked_outcome(&guardian, &outcome.user_outcome);
        let payload = serialize_guardian(Some(guardian.clone())).expect("guardian payload");

        assert_eq!(
            outcome.guardian_decision.kind,
            crate::guardian::GuardianActionKind::Block
        );
        assert_eq!(
            outcome.user_outcome.decision(),
            crate::guardian::GuardianActionKind::Block
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
        let directive = GuardianDirective::compatibility_preset_downgrade("graalvm", "performance");
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
            directive: test_recovery_directive(RecoveryCase::StripRawJvmArgs),
            stage: RecoveryDirectiveStage::Prepare,
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
        let failed_plan = test_recovery_plan(&intent, RecoveryCase::StripRawJvmArgs);
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
        let expected = author_guardian_copy(GuardianCopyRequest::launch_recovery_suppressed(
            &failed_plan.directive,
        ))
        .expect("launch recovery suppression copy");
        let mut recovery_attempts = 0;
        let mut guardian = GuardianSummary::new(GuardianMode::Managed);
        guardian.warn_with_guidance(vec!["Keep existing launch guidance.".to_string()]);

        let outcome = handle_recovery_directive(RecoveryDirectiveRequest {
            state: &state,
            session_id,
            intent: &intent,
            directive: test_recovery_directive(RecoveryCase::StripRawJvmArgs),
            stage: RecoveryDirectiveStage::Prepare,
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
                .any(|detail| detail == &expected.details()[0])
        );
        assert!(guardian.guidance.iter().any(|detail| {
            detail == "Review the latest game log or change the affected launch setting before retrying."
        }));
        let log = match events.try_recv().expect("suppression recovery log") {
            axial_launcher::LaunchEvent::Log(log) => log,
            event => panic!("expected suppression recovery log, got {event:?}"),
        };
        assert_eq!(log.source, "system");
        assert_eq!(log.text, expected.summary());
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
        let plan = test_recovery_plan(&intent, RecoveryCase::StripRawJvmArgs);
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

        let suppressed_plan = test_recovery_plan(&intent, RecoveryCase::StripRawJvmArgs);
        let later_session_id = "runner-launch-recovery-memory-later-session";

        let suppressed =
            record_guardian_launch_recovery_attempt(&state, later_session_id, &suppressed_plan)
                .await
                .expect("persist suppressed launch recovery attempt");
        assert_eq!(
            suppressed.status,
            crate::guardian::GuardianLaunchRecoveryStatus::Suppressed
        );
        let user_outcome = author_guardian_copy(GuardianCopyRequest::launch_recovery_suppressed(
            &suppressed_plan.directive,
        ))
        .expect("launch recovery suppression copy");
        let mut guardian = GuardianSummary::new(GuardianMode::Managed);
        guardian.warn_with_guidance(vec!["Keep existing launch guidance.".to_string()]);
        guardian = guardian_summary_with_suppressed_outcome(&guardian, &user_outcome);
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
                RecoveryCase::StripRawJvmArgs,
            ),
            (
                "runner-gc-recovery-trigger-identity",
                RecoveryCase::DisableCustomGc,
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
                vec![crate::guardian::DiagnosisId::JvmArgUnsupported]
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
        let plan = test_recovery_plan(&intent, RecoveryCase::DowngradePreset);

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

        let suppressed_plan = test_recovery_plan(&intent, RecoveryCase::DowngradePreset);

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
        let plan = test_recovery_plan(&intent, RecoveryCase::StripRawJvmArgs);
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
        let plan = test_recovery_plan(&intent, RecoveryCase::StripRawJvmArgs);

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
        let plan = test_recovery_plan(&intent, RecoveryCase::StripRawJvmArgs);
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
        let plan = test_recovery_plan(&intent, RecoveryCase::StripRawJvmArgs);
        let outcome = author_guardian_copy(GuardianCopyRequest::launch_recovery_suppressed(
            &plan.directive,
        ))
        .expect("launch recovery suppression copy");
        let reason = outcome.summary().to_string();
        let mut guardian = GuardianSummary::new(GuardianMode::Managed);
        guardian.warn_with_guidance(vec!["Keep existing launch guidance.".to_string()]);

        guardian = guardian_summary_with_suppressed_outcome(&guardian, &outcome);

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
                GuardianDirective::StripJvmArgs {
                    reason: GuardianStripJvmArgsReason::PrepareFailure,
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
        kind: RecoveryCase,
    ) -> GuardianLaunchRecoveryPlan {
        let directive = test_recovery_directive(kind);
        test_recovery_plan_for_directive(
            intent,
            directive,
            match kind {
                RecoveryCase::SwitchManagedRuntime => LaunchFailureClass::JavaRuntimeMismatch,
                RecoveryCase::StripRawJvmArgs
                | RecoveryCase::DowngradePreset
                | RecoveryCase::DisableCustomGc => LaunchFailureClass::JvmUnsupportedOption,
            },
        )
    }

    fn test_recovery_plan_for_directive(
        intent: &axial_launcher::LaunchIntent,
        directive: GuardianDirective,
        failure_class: LaunchFailureClass,
    ) -> GuardianLaunchRecoveryPlan {
        plan_guardian_launch_recovery_directive(
            intent,
            directive,
            crate::guardian::GuardianMode::Managed,
            failure_class,
        )
        .expect("recovery plan")
    }

    fn test_recovery_directive(kind: RecoveryCase) -> GuardianDirective {
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
