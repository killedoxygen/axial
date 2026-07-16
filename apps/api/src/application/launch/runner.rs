mod failure;
mod metadata;
mod prewarm;
mod proof;
mod recovery;
mod spawn;
mod status;

use crate::application::guardian_conversion::api_guardian_mode;
use crate::application::launch_application_stage_evidence;
use crate::application::registered_artifact_recovery::{
    REGISTERED_ARTIFACT_REPAIR_SUPPRESSION_MINUTES, RegisteredArtifactComponentRebuildSource,
    RegisteredArtifactRecoveryEntry, execute_registered_artifact_recovery_sequence,
    new_registered_artifact_repair_operation_id,
};
use crate::execution::integrity::sense_integrity_tier1;
use crate::execution::launch::{
    LaunchCommandPreparationRequest, launch_command_stage_evidence, prepare_launch_command,
};
use crate::guardian::{
    DiagnosisId, GuardianActionKind, GuardianArtifactRepairStatus, GuardianCopyRequest,
    GuardianFact, GuardianLaunchRecoveryPlan, GuardianObservedLaunchFailurePhase,
    GuardianPrepareFailureRequest, GuardianPresetAdjustmentRequest,
    GuardianStartupFailureObservation, GuardianStartupFailureRequest, GuardianSummary,
    author_guardian_copy, guardian_fact_from_execution,
    guardian_prelaunch_preset_adjustment_directive, guardian_prepare_failure_outcome,
    guardian_startup_failure_outcome, guardian_summary_with_artifact_repair_outcome,
    guardian_summary_with_blocked_outcome, guardian_summary_with_observed_outcome,
    is_guardian_launch_crash_class, record_launch_failure_observation, user_mod_set_drift_fact,
};
use crate::logging::{append_trace, timestamp_utc};
use crate::observability::telemetry::{
    TelemetryErrorArea, TelemetryErrorKind, TelemetryErrorLevel, TelemetryEvent,
    TelemetryLaunchOutcome,
};
use crate::state::launch_reports::LaunchProofContext;
use crate::state::{
    AppState, LaunchEvent, LaunchFailureTermination, LaunchFailureTerminationErrorClass,
    LaunchStatusEvent, OperationJournalStoreError, RegisteredArtifactFindings, StartupOutcome,
};
use axial_launcher::{
    LaunchFailureClass, LaunchSessionExitReason, LaunchSessionOutcome, LaunchSessionOutcomeKind,
    LaunchState, PreparedLaunchAttempt, build_healing_summary, prepare_launch_attempt_with_events,
};
use axial_minecraft::download::repair_virtual_assets_from_index;
use axial_minecraft::paths::assets_dir;
use failure::{LaunchFailure, fail_launch, fail_launch_for_journal};
use metadata::persist_launch_metadata;
use prewarm::{format_prewarm_run_summary, prewarm_launch_plan};
use proof::persist_launch_proof_with_context_owned as persist_launch_proof_with_context;
use recovery::{
    RecoveryDirectiveOutcome, RecoveryDirectiveRequest, RecoveryDirectiveStage,
    apply_prepare_recovery_directive, apply_startup_recovery_directive, handle_recovery_directive,
    record_failed_self_healing_if_any, record_prelaunch_preset_adjustment_directive,
    record_successful_self_healing_if_any,
};
use spawn::{
    launch_command_target, launch_spawn_failed_stage_evidence, launch_spawn_stage_evidence,
};
use status::{emit_status, launch_state_for_preparation_event, serialize_guardian};
use tokio::process::Command;

pub use failure::sanitize_live_launch_failure_message;
pub(in crate::application::launch) use proof::persist_launch_proof_owned;

pub(super) async fn persist_launch_proof_for_reservation_failure(
    state: &AppState,
    producer: &crate::state::ProducerLease,
    session_id: &str,
    launched_at: Option<&str>,
    proof_context: &LaunchProofContext,
) {
    persist_launch_proof_with_context(
        state,
        producer,
        session_id,
        launched_at,
        "failed",
        Some(proof_context),
    )
    .await;
}

const STARTUP_OBSERVATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const SESSION_TERMINAL_REATTACH_DELAY: std::time::Duration = std::time::Duration::from_millis(25);
const MAX_RECOVERY_ATTEMPTS: u8 = 3;

pub struct LaunchSuccess {
    pub session_id: String,
    pub instance_id: String,
    pub pid: u32,
    pub launched_at: String,
    pub max_memory_mb: i32,
    pub min_memory_mb: i32,
    pub healing: Option<axial_launcher::LaunchHealingSummary>,
    pub guardian: Option<GuardianSummary>,
}

#[derive(Clone)]
pub struct LaunchRequestError {
    pub message: String,
    pub healing: Option<axial_launcher::LaunchHealingSummary>,
    pub guardian: Option<GuardianSummary>,
}

enum LaunchTerminalizationDisposition {
    Complete(Result<LaunchSuccess, LaunchRequestError>),
    Retained(Result<LaunchSuccess, LaunchRequestError>),
    Settled(Result<LaunchSuccess, LaunchRequestError>),
}

enum TerminalObservationHandoff {
    Observe { guardian: GuardianSummary },
    Preserve,
}

struct LaunchSessionRunTask {
    application: crate::application::LaunchInstanceStaging,
    preflight_stage_evidence: Vec<axial_launcher::LaunchStageEvidence>,
    instance: axial_config::Instance,
    intent: axial_launcher::LaunchIntent,
    guardian: GuardianSummary,
    launched_at: String,
    benchmark: Option<crate::state::launch_reports::LaunchBenchmarkMetadata>,
    resource_budget: Option<crate::state::launch_reports::LaunchProofResourceBudget>,
    java_probe_receipt: Option<axial_minecraft::JavaRuntimeProbeReceipt>,
}

impl LaunchSessionRunTask {
    fn from_prepared(
        task: super::session::LaunchSessionTask,
    ) -> (crate::state::IntegrityForegroundLease, Self) {
        let super::session::LaunchSessionTask {
            integrity_foreground,
            application,
            preflight_stage_evidence,
            instance,
            intent,
            guardian,
            launched_at,
            benchmark,
            resource_budget,
            java_probe_receipt,
        } = task;
        (
            integrity_foreground,
            Self {
                application,
                preflight_stage_evidence,
                instance,
                intent,
                guardian,
                launched_at,
                benchmark,
                resource_budget,
                java_probe_receipt,
            },
        )
    }
}

pub(crate) async fn launch_session(
    state: AppState,
    task: super::session::LaunchSessionTask,
    producer: crate::state::ProducerLease,
) -> Result<LaunchSuccess, LaunchRequestError> {
    launch_session_with_control(state, task, producer, LaunchLoopControl::default()).await
}

#[cfg(test)]
pub(crate) async fn launch_session_with_persisted_runtime_manifest_for_test(
    state: AppState,
    task: super::session::LaunchSessionTask,
    producer: crate::state::ProducerLease,
) -> Result<LaunchSuccess, LaunchRequestError> {
    launch_session_with_control(
        state,
        task,
        producer,
        LaunchLoopControl {
            runtime_prepare_source: Some(LaunchRuntimePrepareSource::PersistedManifest),
            ..LaunchLoopControl::default()
        },
    )
    .await
}

async fn launch_session_with_control(
    state: AppState,
    task: super::session::LaunchSessionTask,
    producer: crate::state::ProducerLease,
    control: LaunchLoopControl,
) -> Result<LaunchSuccess, LaunchRequestError> {
    let session_id = task.intent.session_id.clone();
    let instance_id = task.intent.instance_id.clone();
    let guardian_mode = api_guardian_mode(task.intent.guardian.mode);
    let initial_guardian = task.guardian.clone();
    let launched_at = task.launched_at.clone();
    let proof_context = LaunchProofContext::from_intent(&task.intent)
        .with_benchmark(task.benchmark.clone())
        .with_resource_budget(task.resource_budget.clone());
    let sessions = state.sessions().clone();
    let observer_handoff = if let Some(events) = sessions.subscribe(&session_id).await {
        let (handoff_tx, handoff_rx) = tokio::sync::oneshot::channel();
        let observer_state = state.clone();
        let observer_session_id = session_id.clone();
        producer.spawn_child(async move {
            own_terminal_observation(
                observer_state,
                observer_session_id,
                instance_id,
                guardian_mode,
                launched_at,
                proof_context,
                events,
                handoff_rx,
            )
            .await;
        });
        Some(handoff_tx)
    } else {
        None
    };
    let (integrity_foreground, task) = LaunchSessionRunTask::from_prepared(task);
    let mut integrity_foreground = Some(integrity_foreground);
    let result = launch_session_inner_with_control(
        state.clone(),
        task,
        &producer,
        &mut integrity_foreground,
        &control,
    )
    .await;
    let disposition = terminalize_unhandled_launch_error(
        &state,
        &producer,
        &session_id,
        result,
        &mut integrity_foreground,
    )
    .await;
    drop(integrity_foreground);
    let (handoff, transfer_hold) = match &disposition {
        LaunchTerminalizationDisposition::Complete(Ok(success)) => (
            TerminalObservationHandoff::Observe {
                guardian: success.guardian.clone().unwrap_or(initial_guardian),
            },
            true,
        ),
        LaunchTerminalizationDisposition::Complete(Err(_)) => {
            (TerminalObservationHandoff::Preserve, false)
        }
        LaunchTerminalizationDisposition::Retained(_)
        | LaunchTerminalizationDisposition::Settled(_) => {
            (TerminalObservationHandoff::Preserve, false)
        }
    };
    let handoff_succeeded =
        observer_handoff.is_some_and(|handoff_tx| handoff_tx.send(handoff).is_ok());
    let observer_owns_hold = transfer_hold && handoff_succeeded;

    match disposition {
        LaunchTerminalizationDisposition::Complete(result) => {
            if !observer_owns_hold {
                sessions.release_terminal_retention_hold(&session_id).await;
            }
            result
        }
        LaunchTerminalizationDisposition::Retained(result)
        | LaunchTerminalizationDisposition::Settled(result) => result,
    }
}

#[allow(clippy::too_many_arguments)]
async fn own_terminal_observation(
    state: AppState,
    session_id: String,
    instance_id: String,
    guardian_mode: crate::guardian::GuardianMode,
    launched_at: String,
    proof_context: LaunchProofContext,
    mut events: tokio::sync::broadcast::Receiver<LaunchEvent>,
    handoff: tokio::sync::oneshot::Receiver<TerminalObservationHandoff>,
) {
    let guardian = match handoff.await {
        Ok(TerminalObservationHandoff::Observe { guardian }) => guardian,
        Ok(TerminalObservationHandoff::Preserve) => return,
        Err(_) => {
            state
                .sessions()
                .release_terminal_retention_hold(&session_id)
                .await;
            return;
        }
    };

    let terminal = loop {
        match events.recv().await {
            Ok(LaunchEvent::Status(status))
                if matches!(status.state.as_str(), "failed" | "exited") =>
            {
                let record = state.sessions().get(&session_id).await;
                if record.as_ref().is_some_and(|record| {
                    matches!(record.state, LaunchState::Failed | LaunchState::Exited)
                }) {
                    break record;
                }
            }
            Ok(_) => {}
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                let record = state.sessions().get(&session_id).await;
                if record.as_ref().is_some_and(|record| {
                    matches!(record.state, LaunchState::Failed | LaunchState::Exited)
                }) {
                    break record;
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                break state.sessions().get(&session_id).await;
            }
        }
    };

    if let Some(record) = terminal {
        let accepted_failure_class = record
            .failure
            .as_ref()
            .map(|failure| failure.class)
            .filter(|failure_class| is_guardian_launch_crash_class(*failure_class));
        let observed_phase = match (
            record.boot_completed_at_ms,
            record.outcome.as_ref().map(|outcome| outcome.reason),
        ) {
            (None, Some(LaunchSessionExitReason::StartupFailed)) => {
                Some(GuardianObservedLaunchFailurePhase::BeforeBoot)
            }
            (Some(_), Some(LaunchSessionExitReason::CrashedAfterBoot)) => {
                Some(GuardianObservedLaunchFailurePhase::AfterBoot)
            }
            _ => None,
        };
        if let (Some(failure_class), Some(observed_phase)) =
            (accepted_failure_class, observed_phase)
        {
            settle_observed_launch_failure(
                &state,
                &session_id,
                &instance_id,
                guardian_mode,
                failure_class,
                observed_phase,
                &launched_at,
                proof_context,
                record,
                guardian,
            )
            .await;
        } else {
            persist_terminal_proof(&state, &session_id, &launched_at, proof_context, record).await;
        }
    }

    state
        .sessions()
        .release_terminal_retention_hold(&session_id)
        .await;
}

async fn persist_terminal_proof(
    state: &AppState,
    session_id: &str,
    launched_at: &str,
    proof_context: LaunchProofContext,
    record: crate::state::LaunchSessionRecord,
) {
    let outcome = match record.outcome.as_ref().map(|outcome| outcome.kind) {
        Some(LaunchSessionOutcomeKind::Clean) => "completed",
        Some(LaunchSessionOutcomeKind::Stopped) => return,
        Some(LaunchSessionOutcomeKind::Failed | LaunchSessionOutcomeKind::Unknown) => "failed",
        None if record.state == LaunchState::Failed => "failed",
        None => "exited",
    };
    if state
        .launch_reports()
        .persist(
            record,
            Some(launched_at.to_string()),
            outcome.to_string(),
            Some(proof_context),
        )
        .await
        .is_err()
    {
        tracing::warn!(session_id, "failed to persist terminal launch proof");
    }
}

#[allow(clippy::too_many_arguments)]
async fn settle_observed_launch_failure(
    state: &AppState,
    session_id: &str,
    instance_id: &str,
    guardian_mode: crate::guardian::GuardianMode,
    failure_class: LaunchFailureClass,
    observed_phase: GuardianObservedLaunchFailurePhase,
    launched_at: &str,
    proof_context: LaunchProofContext,
    record: crate::state::LaunchSessionRecord,
    mut guardian: GuardianSummary,
) {
    let Some(user_outcome) = author_guardian_copy(GuardianCopyRequest::observed_launch_failure(
        failure_class,
        record.crash_evidence.as_ref(),
        observed_phase,
    )) else {
        persist_terminal_proof(state, session_id, launched_at, proof_context, record).await;
        return;
    };
    let terminal_state = match observed_phase {
        GuardianObservedLaunchFailurePhase::BeforeBoot => {
            guardian = guardian_summary_with_observed_outcome(&guardian, &user_outcome);
            "failed"
        }
        GuardianObservedLaunchFailurePhase::AfterBoot => {
            guardian = guardian_summary_with_observed_outcome(&guardian, &user_outcome);
            "exited"
        }
    };

    state
        .sessions()
        .emit_status(
            session_id,
            LaunchStatusEvent {
                state: terminal_state.to_string(),
                benchmark: None,
                pid: None,
                exit_code: record.exit_code,
                failure_class: Some(failure_class.as_str().to_string()),
                failure_detail: Some(user_outcome.summary().to_string()),
                crash_evidence: record.crash_evidence.clone(),
                healing: record.healing.clone(),
                guardian: serialize_guardian(Some(guardian)),
                outcome: record.outcome.clone(),
                notice: None,
                evidence: Vec::new(),
                stages: Vec::new(),
            },
        )
        .await;

    let observed_at = timestamp_utc();
    if let Err(error) = record_launch_failure_observation(
        state.failure_memory(),
        instance_id,
        guardian_mode,
        failure_class,
        &observed_at,
    ) {
        tracing::warn!(
            error_kind = error.class(),
            failure_class = failure_class.as_str(),
            "failed to record observed launch failure"
        );
    }

    let Some(updated) = state.sessions().get(session_id).await else {
        return;
    };
    if state
        .launch_reports()
        .persist(
            updated,
            Some(launched_at.to_string()),
            "failed".to_string(),
            Some(proof_context),
        )
        .await
        .is_err()
    {
        tracing::warn!(
            failure_class = failure_class.as_str(),
            "failed to persist observed launch failure proof"
        );
    }
}

async fn terminalize_unhandled_launch_error(
    state: &AppState,
    producer: &crate::state::ProducerLease,
    session_id: &str,
    result: Result<LaunchSuccess, LaunchRequestError>,
    integrity_foreground: &mut Option<crate::state::IntegrityForegroundLease>,
) -> LaunchTerminalizationDisposition {
    let error = match result {
        Ok(success) => {
            return LaunchTerminalizationDisposition::Complete(Ok(success));
        }
        Err(error) => error,
    };
    let error = LaunchRequestError {
        message: sanitize_live_launch_failure_message(&error.message),
        healing: error.healing,
        guardian: error.guardian,
    };
    let Some(record) = state.sessions().get(session_id).await else {
        return LaunchTerminalizationDisposition::Complete(Err(error));
    };
    if matches!(record.state, LaunchState::Failed | LaunchState::Exited) {
        return LaunchTerminalizationDisposition::Complete(Err(error));
    }

    match state
        .sessions()
        .terminate_for_launch_failure(session_id)
        .await
    {
        LaunchFailureTermination::Ready(lease) => {
            let terminal_error =
                finalize_unhandled_launch_error(state, producer, session_id, error).await;
            lease.release().await;
            LaunchTerminalizationDisposition::Settled(Err(terminal_error))
        }
        LaunchFailureTermination::Pending(pending) => {
            trace_unconfirmed_launch_failure_termination(pending.error_class());
            let deferred_state = state.clone();
            let deferred_session_id = session_id.to_string();
            let deferred_error = error.clone();
            let deferred_producer = producer.claim_child();
            let retained_foreground = integrity_foreground
                .take()
                .expect("pending preboot terminalization must retain foreground authority");
            producer.spawn_child(async move {
                match pending.wait_for_settlement().await {
                    Ok(lease) => {
                        let _ = finalize_unhandled_launch_error(
                            &deferred_state,
                            &deferred_producer,
                            &deferred_session_id,
                            deferred_error,
                        )
                        .await;
                        lease.release().await;
                        drop(retained_foreground);
                    }
                    Err(error_class) => {
                        trace_unconfirmed_launch_failure_termination(error_class);
                        retain_integrity_foreground_until_session_terminal(
                            deferred_state,
                            deferred_session_id,
                            retained_foreground,
                        )
                        .await;
                    }
                }
            });
            LaunchTerminalizationDisposition::Retained(Err(error))
        }
        LaunchFailureTermination::Unconfirmed(error_class) => {
            trace_unconfirmed_launch_failure_termination(error_class);
            let retained_state = state.clone();
            let retained_session_id = session_id.to_string();
            let retained_foreground = integrity_foreground
                .take()
                .expect("unconfirmed preboot terminalization must retain foreground authority");
            producer.spawn_child(async move {
                retain_integrity_foreground_until_session_terminal(
                    retained_state,
                    retained_session_id,
                    retained_foreground,
                )
                .await;
            });
            LaunchTerminalizationDisposition::Retained(Err(error))
        }
    }
}

async fn retain_integrity_foreground_until_session_terminal(
    state: AppState,
    session_id: String,
    integrity_foreground: crate::state::IntegrityForegroundLease,
) {
    let _integrity_foreground = integrity_foreground;
    let mut changes = state.sessions().subscribe_changes();
    loop {
        let terminal = state
            .sessions()
            .get(&session_id)
            .await
            .is_none_or(|record| matches!(record.state, LaunchState::Failed | LaunchState::Exited));
        if terminal {
            return;
        }

        match changes.recv().await {
            Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                tokio::time::sleep(SESSION_TERMINAL_REATTACH_DELAY).await;
                changes = state.sessions().subscribe_changes();
            }
        }
    }
}

async fn finalize_unhandled_launch_error(
    state: &AppState,
    producer: &crate::state::ProducerLease,
    session_id: &str,
    error: LaunchRequestError,
) -> LaunchRequestError {
    state.telemetry().emit(TelemetryEvent::launch_completed(
        TelemetryLaunchOutcome::Failure,
    ));
    fail_launch_for_journal(
        state,
        producer,
        session_id,
        &error.message,
        error.healing,
        error.guardian,
    )
    .await
}

fn trace_unconfirmed_launch_failure_termination(error_class: LaunchFailureTerminationErrorClass) {
    tracing::warn!(
        termination_error_class = error_class.as_str(),
        "launch failure termination remains unconfirmed"
    );
}

#[cfg(test)]
async fn launch_session_inner(
    state: AppState,
    task: LaunchSessionRunTask,
    producer: &crate::state::ProducerLease,
    integrity_foreground: &mut Option<crate::state::IntegrityForegroundLease>,
) -> Result<LaunchSuccess, LaunchRequestError> {
    launch_session_inner_with_control(
        state,
        task,
        producer,
        integrity_foreground,
        &LaunchLoopControl::default(),
    )
    .await
}

async fn launch_session_inner_with_control(
    state: AppState,
    task: LaunchSessionRunTask,
    producer: &crate::state::ProducerLease,
    integrity_foreground: &mut Option<crate::state::IntegrityForegroundLease>,
    control: &LaunchLoopControl,
) -> Result<LaunchSuccess, LaunchRequestError> {
    let LaunchSessionRunTask {
        application,
        preflight_stage_evidence,
        instance,
        intent,
        mut guardian,
        launched_at,
        benchmark,
        resource_budget,
        java_probe_receipt,
    } = task;
    let session_id = intent.session_id.clone();
    trace_launch_event(
        &session_id,
        &format!("application command staged: {:?}", application.command.kind),
    );
    let mut initial_evidence = launch_application_stage_evidence(&application);
    initial_evidence.extend(preflight_stage_evidence);
    state
        .sessions()
        .record_stage_evidence(&session_id, initial_evidence)
        .await;
    if let Some(benchmark_payload) = benchmark
        .as_ref()
        .map(super::launch_benchmark_status_payload)
    {
        state
            .sessions()
            .attach_benchmark(&session_id, benchmark_payload)
            .await;
    }
    let proof_context = LaunchProofContext::from_intent(&intent)
        .with_benchmark(benchmark)
        .with_resource_budget(resource_budget);
    let mut attempt = axial_launcher::service::AttemptOverrides::default();
    let mut last_recovery_plan: Option<GuardianLaunchRecoveryPlan> = None;
    let mut recovery_attempts = 0_u8;
    let mut registered_recovery_process_retry_used = false;
    let mut launch_completion_pending = false;
    emit_launch_started(
        &state,
        &mut launch_completion_pending,
        Some(intent.loader.clone()),
    );

    loop {
        trace_launch_event(&session_id, "launch_session entered");
        state
            .sessions()
            .emit_log(
                &session_id,
                "system",
                format!("Preparing launch for {}.", instance.name),
            )
            .await;

        let (preparation_event_tx, mut preparation_event_rx) =
            tokio::sync::mpsc::unbounded_channel();
        let preparation_event_sender = preparation_event_tx.clone();
        let preparation_status_state = state.clone();
        let preparation_status_session_id = session_id.clone();
        let preparation_status_guardian = guardian.clone();
        let (preparation_status_done_tx, preparation_status_done_rx) =
            tokio::sync::oneshot::channel();
        producer.spawn_child(async move {
            while let Some(event) = preparation_event_rx.recv().await {
                emit_status(
                    &preparation_status_state,
                    &preparation_status_session_id,
                    launch_state_for_preparation_event(event),
                    None,
                    None,
                    None,
                    Some(preparation_status_guardian.clone()),
                )
                .await;
            }
            let _ = preparation_status_done_tx.send(());
        });
        let prepared_result = if let Some(error) = control.forced_prepare_failure() {
            drop(preparation_event_sender);
            Err(error)
        } else {
            #[cfg(test)]
            if control.runtime_prepare_source() == LaunchRuntimePrepareSource::PersistedManifest {
                axial_launcher::prepare_launch_attempt_with_persisted_runtime_manifest_for_test(
                    state.managed_runtime_cache(),
                    &intent,
                    &attempt,
                    java_probe_receipt.as_ref(),
                    move |event| {
                        let _ = preparation_event_sender.send(event);
                    },
                )
                .await
            } else {
                prepare_launch_attempt_with_events(
                    state.managed_runtime_cache(),
                    &intent,
                    &attempt,
                    java_probe_receipt.as_ref(),
                    move |event| {
                        let _ = preparation_event_sender.send(event);
                    },
                )
                .await
            }
            #[cfg(not(test))]
            prepare_launch_attempt_with_events(
                state.managed_runtime_cache(),
                &intent,
                &attempt,
                java_probe_receipt.as_ref(),
                move |event| {
                    let _ = preparation_event_sender.send(event);
                },
            )
            .await
        };
        drop(preparation_event_tx);
        let _ = preparation_status_done_rx.await;

        let prepared = match prepared_result {
            Ok(prepared) => prepared,
            Err(error) => {
                let failure_class = error.failure_class.unwrap_or(LaunchFailureClass::Unknown);
                if let Some(recovery_plan) = last_recovery_plan.take() {
                    record_failed_self_healing_if_any(
                        &state,
                        &session_id,
                        Some(&recovery_plan),
                        failure_class,
                    )
                    .await
                    .map_err(guardian_journal_error)?;
                }
                trace_launch_event(&session_id, &format!("prepare failed: {}", error.message));
                let prepare_outcome =
                    guardian_prepare_failure_outcome(GuardianPrepareFailureRequest {
                        mode: api_guardian_mode(intent.guardian.mode),
                        failure_class,
                        public_error: &error.message,
                        requested_java_present: !intent.requested_java.trim().is_empty(),
                        explicit_java_override_present: intent.guardian.has_java_override(),
                        explicit_jvm_args_present: intent.guardian.has_raw_jvm_args()
                            && !intent.extra_jvm_args.is_empty(),
                        runtime_intervention_applied: attempt.runtime_intervention_applied,
                        raw_jvm_args_intervention_applied: attempt
                            .raw_jvm_args_intervention_applied,
                    });
                if let Some(directive) = prepare_outcome.directive.clone() {
                    let failure_message =
                        match handle_recovery_directive(RecoveryDirectiveRequest {
                            state: &state,
                            session_id: &session_id,
                            intent: &intent,
                            directive,
                            stage: RecoveryDirectiveStage::Prepare,
                            mode: api_guardian_mode(intent.guardian.mode),
                            failure_class,
                            recovery_attempts: &mut recovery_attempts,
                            max_recovery_attempts: MAX_RECOVERY_ATTEMPTS,
                            guardian: &mut guardian,
                        })
                        .await
                        .map_err(guardian_journal_error)?
                        {
                            RecoveryDirectiveOutcome::Apply(recovery_plan) => {
                                if control.apply_prepare_recovery_directive(
                                    &mut guardian,
                                    &mut attempt,
                                    &recovery_plan,
                                ) {
                                    last_recovery_plan = Some(recovery_plan);
                                }
                                continue;
                            }
                            RecoveryDirectiveOutcome::Exhausted => {
                                control.record_capped_prepare_failure(&error);
                                guardian = guardian_summary_with_blocked_outcome(
                                    &guardian,
                                    &prepare_outcome.user_outcome,
                                );
                                prepare_outcome.user_outcome.summary().to_string()
                            }
                            RecoveryDirectiveOutcome::Rejected => {
                                guardian = guardian_summary_with_blocked_outcome(
                                    &guardian,
                                    &prepare_outcome.user_outcome,
                                );
                                prepare_outcome.user_outcome.summary().to_string()
                            }
                            RecoveryDirectiveOutcome::Suppressed(recovery_user_outcome) => {
                                recovery_user_outcome.summary().to_string()
                            }
                        };
                    return Err(finish_launch_failure(
                        &state,
                        producer,
                        &session_id,
                        &mut launch_completion_pending,
                        LaunchFailure {
                            proof_context: Some(&proof_context),
                            class: failure_class,
                            message: &failure_message,
                            healing: error.healing,
                            guardian: Some(guardian.clone()),
                            outcome: None,
                        },
                    )
                    .await);
                }
                guardian =
                    guardian_summary_with_blocked_outcome(&guardian, &prepare_outcome.user_outcome);
                return Err(finish_launch_failure(
                    &state,
                    producer,
                    &session_id,
                    &mut launch_completion_pending,
                    LaunchFailure {
                        proof_context: Some(&proof_context),
                        class: failure_class,
                        message: &error.message,
                        healing: error.healing,
                        guardian: Some(guardian.clone()),
                        outcome: None,
                    },
                )
                .await);
            }
        };
        if let Some(directive) =
            guardian_prelaunch_preset_adjustment_directive(GuardianPresetAdjustmentRequest {
                mode: api_guardian_mode(intent.guardian.mode),
                requested_preset: &intent.requested_preset,
                effective_preset: &prepared.effective_preset,
                explicit_jvm_preset_present: intent.guardian.has_named_preset(),
            })
        {
            record_prelaunch_preset_adjustment_directive(&mut guardian, &directive);
        }

        trace_launch_event(
            &session_id,
            &format!(
                "prepare finished total={}ms version={}ms runtime={}ms planning={}ms java_probe_count={} java_probe_source={}",
                prepared.metrics.total_ms,
                prepared.metrics.version_ms,
                prepared.metrics.runtime_ms,
                prepared.metrics.planning_ms,
                prepared.metrics.java_probe_count,
                prepared.metrics.java_probe_source,
            ),
        );

        state
            .sessions()
            .emit_log(
                &session_id,
                "system",
                format!(
                    "Launch prep finished in {} ms (version {} ms, runtime {} ms, plan {} ms).",
                    prepared.metrics.total_ms,
                    prepared.metrics.version_ms,
                    prepared.metrics.runtime_ms,
                    prepared.metrics.planning_ms,
                ),
            )
            .await;

        emit_status(
            &state,
            &session_id,
            LaunchState::Preparing,
            None,
            None,
            prepared.healing.clone(),
            Some(guardian.clone()),
        )
        .await;
        state
            .sessions()
            .emit_log(
                &session_id,
                "system",
                format!(
                    "Using Java {} via {}.",
                    prepared.runtime.effective_info.major, prepared.runtime.effective_source
                ),
            )
            .await;

        let launch_command = match prepare_launch_command(LaunchCommandPreparationRequest::new(
            launch_command_target(&session_id),
            &prepared.plan.command,
            &prepared.plan.game_dir,
        )) {
            Ok(command) => {
                state
                    .sessions()
                    .record_stage_evidence(
                        &session_id,
                        launch_command_stage_evidence(&command.facts),
                    )
                    .await;
                command
            }
            Err(error) => {
                record_failed_self_healing_if_any(
                    &state,
                    &session_id,
                    last_recovery_plan.as_ref(),
                    LaunchFailureClass::Unknown,
                )
                .await
                .map_err(guardian_journal_error)?;
                state
                    .sessions()
                    .record_stage_evidence(&session_id, launch_command_stage_evidence(&error.facts))
                    .await;
                trace_launch_event(
                    &session_id,
                    &format!("launch command preparation failed: {}", error),
                );
                return Err(finish_launch_failure(
                    &state,
                    producer,
                    &session_id,
                    &mut launch_completion_pending,
                    LaunchFailure {
                        proof_context: Some(&proof_context),
                        class: LaunchFailureClass::Unknown,
                        message: &error.to_string(),
                        healing: prepared.healing.clone(),
                        guardian: Some(guardian.clone()),
                        outcome: None,
                    },
                )
                .await);
            }
        };

        let asset_repair =
            repair_legacy_virtual_assets_before_launch(&intent.library_dir, &prepared.plan).await;
        match &asset_repair {
            Ok(outcome) => trace_launch_event(
                &session_id,
                &format!(
                    "runner asset-index repair_stage_full_object_parse_attempts={} result={}",
                    outcome.full_object_parse_attempts(),
                    outcome.label()
                ),
            ),
            Err(_) => trace_launch_event(
                &session_id,
                "runner asset-index repair_stage_full_object_parse_attempts=1 result=failed",
            ),
        }
        if let Err(error) = asset_repair {
            record_failed_self_healing_if_any(
                &state,
                &session_id,
                last_recovery_plan.as_ref(),
                LaunchFailureClass::Unknown,
            )
            .await
            .map_err(guardian_journal_error)?;
            trace_launch_event(
                &session_id,
                &format!("legacy virtual asset repair failed: {error}"),
            );
            return Err(finish_launch_failure(
                &state,
                producer,
                &session_id,
                &mut launch_completion_pending,
                LaunchFailure {
                    proof_context: Some(&proof_context),
                    class: LaunchFailureClass::Unknown,
                    message: &error.to_string(),
                    healing: prepared.healing.clone(),
                    guardian: Some(guardian.clone()),
                    outcome: None,
                },
            )
            .await);
        }

        emit_status(
            &state,
            &session_id,
            LaunchState::Prewarming,
            None,
            None,
            prepared.healing.clone(),
            Some(guardian.clone()),
        )
        .await;
        let prewarm =
            prewarm_launch_plan(&prepared.plan, proof_context.resource_budget.as_ref()).await;
        let prewarm_summary = format_prewarm_run_summary(&prewarm);
        trace_launch_event(&session_id, &prewarm_summary);
        state
            .sessions()
            .emit_log(&session_id, "system", prewarm_summary)
            .await;

        trace_launch_event(
            &session_id,
            &format!(
                "launch command prepared facts={}",
                launch_command.facts.len()
            ),
        );
        let mut command = Command::new(launch_command.program);
        command.args(launch_command.args);
        command.current_dir(launch_command.game_dir);

        let record = crate::state::LaunchSessionRecord {
            session_id: axial_launcher::SessionId(session_id.clone()),
            instance_id: intent.instance_id.clone(),
            version_id: intent.version_id.clone(),
            launched_at: Some(launched_at.clone()),
            benchmark: None,
            state: LaunchState::Starting,
            pid: None,
            process_started_at_ms: None,
            boot_completed_at_ms: None,
            boot_duration_ms: None,
            priority: None,
            exit_code: None,
            command: prepared.plan.command.clone(),
            java_path: Some(prepared.runtime.effective_path.clone()),
            natives_dir: prepared
                .plan
                .natives_dir
                .as_ref()
                .map(|path| path.to_string_lossy().to_string()),
            failure: None,
            crash_evidence: None,
            healing: prepared
                .healing
                .as_ref()
                .and_then(|value| serde_json::to_value(value).ok()),
            guardian: serialize_guardian(Some(guardian.clone())),
            outcome: None,
            stages: Vec::new(),
        };

        let launched = match state.sessions().start_process(record, command).await {
            Ok(record) => {
                state
                    .sessions()
                    .record_stage_evidence(
                        &session_id,
                        launch_spawn_stage_evidence(&session_id, &record),
                    )
                    .await;
                record
            }
            Err(error) => {
                record_failed_self_healing_if_any(
                    &state,
                    &session_id,
                    last_recovery_plan.as_ref(),
                    LaunchFailureClass::Unknown,
                )
                .await
                .map_err(guardian_journal_error)?;
                state
                    .sessions()
                    .record_stage_evidence(&session_id, launch_spawn_failed_stage_evidence())
                    .await;
                emit_pending_launch_failure(&state, &mut launch_completion_pending);
                state.telemetry().emit(TelemetryEvent::error_captured(
                    TelemetryErrorKind::LaunchSpawnFailed,
                    TelemetryErrorArea::Launch,
                    TelemetryErrorLevel::Error,
                    LaunchSessionExitReason::SpawnFailed.summary(),
                ));
                trace_launch_event(&session_id, &format!("spawn failed: {error}"));
                return Err(finish_launch_failure(
                    &state,
                    producer,
                    &session_id,
                    &mut launch_completion_pending,
                    LaunchFailure {
                        proof_context: Some(&proof_context),
                        class: LaunchFailureClass::Unknown,
                        message: &format!("failed to start launch process: {error}"),
                        healing: prepared.healing.clone(),
                        guardian: Some(guardian.clone()),
                        outcome: Some(LaunchSessionOutcome::from_reason(
                            LaunchSessionExitReason::SpawnFailed,
                        )),
                    },
                )
                .await);
            }
        };
        trace_launch_event(&session_id, &format!("spawned pid={:?}", launched.pid));

        emit_status(
            &state,
            &session_id,
            LaunchState::Monitoring,
            launched.pid,
            None,
            prepared.healing.clone(),
            Some(guardian.clone()),
        )
        .await;

        let outcome = state
            .sessions()
            .wait_for_startup(&session_id, STARTUP_OBSERVATION_TIMEOUT)
            .await;

        match outcome {
            StartupOutcome::Stable | StartupOutcome::TimedOut => {
                record_successful_self_healing_if_any(
                    &state,
                    &session_id,
                    last_recovery_plan.as_ref(),
                )
                .await
                .map_err(guardian_journal_error)?;
                emit_launch_completed(
                    &state,
                    &mut launch_completion_pending,
                    TelemetryLaunchOutcome::Success,
                );
                emit_status(
                    &state,
                    &session_id,
                    LaunchState::Running,
                    launched.pid,
                    None,
                    prepared.healing.clone(),
                    Some(guardian.clone()),
                )
                .await;
                persist_launch_proof_with_context(
                    &state,
                    producer,
                    &session_id,
                    Some(launched_at.as_str()),
                    "running",
                    Some(&proof_context),
                )
                .await;
                if let Err(stage) = persist_launch_metadata(
                    &state,
                    integrity_foreground
                        .as_ref()
                        .expect("successful launch must retain foreground through metadata"),
                    &instance.id,
                    &intent.username,
                    intent.max_memory_mb,
                    intent.min_memory_mb,
                    &launched_at,
                )
                .await
                {
                    tracing::warn!(?stage, "launch metadata persistence failed");
                }
                drop(spawn_successful_user_mod_witness_publication(
                    &state,
                    producer,
                    integrity_foreground.as_ref().expect(
                        "successful launch must retain foreground through witness publication",
                    ),
                    &intent.instance_id,
                ));
                return Ok(LaunchSuccess {
                    session_id: session_id.clone(),
                    instance_id: intent.instance_id.clone(),
                    pid: launched.pid.unwrap_or_default(),
                    launched_at: launched_at.clone(),
                    max_memory_mb: intent.max_memory_mb,
                    min_memory_mb: intent.min_memory_mb,
                    healing: prepared.healing.clone(),
                    guardian: Some(guardian.clone()),
                });
            }
            StartupOutcome::Exited | StartupOutcome::Stalled => {
                let stalled = matches!(outcome, StartupOutcome::Stalled);
                if stalled {
                    let _ = state.sessions().kill(&session_id).await;
                }

                let terminal_record = if stalled {
                    None
                } else {
                    state.sessions().get(&session_id).await
                };
                let failure_class = if stalled {
                    LaunchFailureClass::StartupStalled
                } else {
                    terminal_record
                        .as_ref()
                        .and_then(|record| record.failure.as_ref().map(|failure| failure.class))
                        .unwrap_or(LaunchFailureClass::Unknown)
                };
                let observation = if stalled {
                    GuardianStartupFailureObservation::Stalled
                } else {
                    GuardianStartupFailureObservation::Exited { failure_class }
                };
                let user_mod_witness_fact = sense_user_mod_witness_drift_after_failure(
                    &state,
                    integrity_foreground
                        .as_ref()
                        .expect("preboot launch must retain foreground authority"),
                    &intent.instance_id,
                    failure_class,
                )
                .await;
                let mut integrity = if registered_recovery_process_retry_used {
                    StartupFailureIntegrity::default()
                } else {
                    sense_startup_failure_integrity(
                        &state,
                        integrity_foreground
                            .as_ref()
                            .expect("preboot launch must retain foreground authority"),
                        &intent.instance_id,
                        &intent.library_dir,
                        failure_class,
                    )
                    .await
                };
                if let Some(fact) = user_mod_witness_fact {
                    integrity.facts.push(fact);
                }
                control.record_startup_integrity(&integrity);
                let guardian_mode = api_guardian_mode(intent.guardian.mode);
                let startup_outcome = {
                    let repair_candidate = (!registered_recovery_process_retry_used)
                        .then(|| integrity.repair_candidate())
                        .flatten();
                    guardian_startup_failure_outcome(GuardianStartupFailureRequest {
                        mode: guardian_mode,
                        observation,
                        crash_evidence: terminal_record
                            .as_ref()
                            .and_then(|record| record.crash_evidence.as_ref()),
                        integrity_facts: &integrity.facts,
                        registered_artifact_repair_candidate: repair_candidate,
                        target_version_id: &intent.target_version_id,
                        runtime_major: prepared.runtime.effective_info.major,
                        requested_java_present: !intent.requested_java.trim().is_empty(),
                        explicit_java_override_present: intent.guardian.has_java_override(),
                        explicit_jvm_args_present: intent.guardian.has_raw_jvm_args()
                            && !intent.extra_jvm_args.is_empty(),
                        explicit_jvm_preset_present: intent.guardian.has_named_preset(),
                        startup_recovery_applied: attempt.startup_recovery_applied,
                        disable_custom_gc: attempt.disable_custom_gc,
                        effective_preset: &prepared.effective_preset,
                    })
                };
                let failure_class = startup_outcome.failure_class;
                if is_guardian_launch_crash_class(failure_class) {
                    let observed_at = timestamp_utc();
                    if let Err(error) = record_launch_failure_observation(
                        state.failure_memory(),
                        &intent.instance_id,
                        guardian_mode,
                        failure_class,
                        &observed_at,
                    ) {
                        tracing::warn!(
                            error_kind = error.class(),
                            failure_class = failure_class.as_str(),
                            "failed to record startup launch failure observation"
                        );
                    }
                }
                if let Some(recovery_plan) = last_recovery_plan.take() {
                    record_failed_self_healing_if_any(
                        &state,
                        &session_id,
                        Some(&recovery_plan),
                        failure_class,
                    )
                    .await
                    .map_err(guardian_journal_error)?;
                }
                state.telemetry().emit(TelemetryEvent::error_captured(
                    TelemetryErrorKind::LaunchStartupFailed,
                    TelemetryErrorArea::Launch,
                    TelemetryErrorLevel::Error,
                    failure_class.as_str(),
                ));

                match registered_artifact_startup_disposition(
                    guardian_mode,
                    startup_outcome.guardian_decision.kind(),
                    registered_recovery_process_retry_used,
                ) {
                    RegisteredArtifactStartupDisposition::TerminalizeRetryFailure => {
                        let healing =
                            startup_failure_healing(&intent, &prepared, &attempt, failure_class);
                        guardian = guardian_summary_with_blocked_outcome(
                            &guardian,
                            &startup_outcome.user_outcome,
                        );
                        return Err(finish_launch_failure(
                            &state,
                            producer,
                            &session_id,
                            &mut launch_completion_pending,
                            LaunchFailure {
                                proof_context: Some(&proof_context),
                                class: failure_class,
                                message: startup_outcome.user_outcome.summary(),
                                healing,
                                guardian: Some(guardian.clone()),
                                outcome: None,
                            },
                        )
                        .await);
                    }
                    RegisteredArtifactStartupDisposition::ExecuteRepair => {
                        let healing =
                            startup_failure_healing(&intent, &prepared, &attempt, failure_class);
                        let Some(findings) = integrity.into_findings() else {
                            trace_launch_event(
                                &session_id,
                                "registered artifact repair evidence was unavailable",
                            );
                            return Err(finish_registered_artifact_repair_failure(
                                &state,
                                producer,
                                &session_id,
                                &mut launch_completion_pending,
                                &proof_context,
                                failure_class,
                                healing,
                                guardian,
                                DiagnosisId::LauncherManagedArtifactCorrupt,
                                GuardianArtifactRepairStatus::Blocked,
                            )
                            .await);
                        };
                        let authorization =
                            match findings.authorize_repair(&startup_outcome.guardian_decision) {
                                Ok(authorization) => authorization,
                                Err(_) => {
                                    trace_launch_event(
                                        &session_id,
                                        "registered artifact repair authorization was rejected",
                                    );
                                    return Err(finish_registered_artifact_repair_failure(
                                        &state,
                                        producer,
                                        &session_id,
                                        &mut launch_completion_pending,
                                        &proof_context,
                                        failure_class,
                                        healing,
                                        guardian,
                                        DiagnosisId::LauncherManagedArtifactCorrupt,
                                        GuardianArtifactRepairStatus::Blocked,
                                    )
                                    .await);
                                }
                            };
                        let operation_id = startup_outcome
                            .guardian_decision
                            .operation_id()
                            .cloned()
                            .unwrap_or_else(new_registered_artifact_repair_operation_id);
                        let admission = match state
                            .admit_registered_artifact_repair(
                                authorization,
                                operation_id.clone(),
                                chrono::Duration::minutes(
                                    REGISTERED_ARTIFACT_REPAIR_SUPPRESSION_MINUTES,
                                ),
                            )
                            .await
                        {
                            Ok(admission) => admission,
                            Err(_) => {
                                trace_launch_event(
                                    &session_id,
                                    "registered artifact repair admission was rejected",
                                );
                                return Err(finish_registered_artifact_repair_failure(
                                    &state,
                                    producer,
                                    &session_id,
                                    &mut launch_completion_pending,
                                    &proof_context,
                                    failure_class,
                                    healing,
                                    guardian,
                                    DiagnosisId::LauncherManagedArtifactCorrupt,
                                    GuardianArtifactRepairStatus::Blocked,
                                )
                                .await);
                            }
                        };
                        let retained_foreground = integrity_foreground.take().expect(
                            "registered artifact recovery must retain foreground authority",
                        );
                        let recovery_state = state.clone();
                        let component_rebuild_source =
                            control.registered_artifact_component_rebuild_source();
                        let repair_task = producer.claim_child().spawn_joinable(async move {
                            let client = reqwest::Client::new();
                            let outcome = execute_registered_artifact_recovery_sequence(
                                &recovery_state,
                                RegisteredArtifactRecoveryEntry::Fresh(Box::new(admission)),
                                &client,
                                component_rebuild_source,
                            )
                            .await;
                            (outcome, retained_foreground)
                        });
                        let recovery_outcome = match repair_task.await {
                            Ok((Ok(outcome), retained_foreground)) => {
                                *integrity_foreground = Some(retained_foreground);
                                outcome
                            }
                            Ok((Err(_), retained_foreground)) => {
                                *integrity_foreground = Some(retained_foreground);
                                trace_launch_event(
                                    &session_id,
                                    "registered artifact recovery execution failed",
                                );
                                return Err(finish_registered_artifact_repair_failure(
                                    &state,
                                    producer,
                                    &session_id,
                                    &mut launch_completion_pending,
                                    &proof_context,
                                    failure_class,
                                    healing,
                                    guardian,
                                    DiagnosisId::LauncherManagedArtifactCorrupt,
                                    GuardianArtifactRepairStatus::Failed,
                                )
                                .await);
                            }
                            Err(_) => {
                                trace_launch_event(
                                    &session_id,
                                    "registered artifact recovery owner stopped",
                                );
                                return Err(finish_registered_artifact_repair_failure(
                                    &state,
                                    producer,
                                    &session_id,
                                    &mut launch_completion_pending,
                                    &proof_context,
                                    failure_class,
                                    healing,
                                    guardian,
                                    DiagnosisId::LauncherManagedArtifactCorrupt,
                                    GuardianArtifactRepairStatus::Failed,
                                )
                                .await);
                            }
                        };
                        let repair_user_outcome =
                            author_guardian_copy(GuardianCopyRequest::artifact_repair(
                                recovery_outcome.diagnosis_id,
                                recovery_outcome.effective_status,
                            ))
                            .expect("registered artifact repair copy request is closed");
                        guardian = guardian_summary_with_artifact_repair_outcome(
                            &guardian,
                            &repair_user_outcome,
                        );
                        state
                            .sessions()
                            .emit_log(
                                &session_id,
                                "system",
                                repair_user_outcome.summary().to_string(),
                            )
                            .await;
                        match recovery_outcome.effective_status {
                            GuardianArtifactRepairStatus::Repaired => {
                                registered_recovery_process_retry_used = true;
                                continue;
                            }
                            GuardianArtifactRepairStatus::Blocked
                            | GuardianArtifactRepairStatus::Failed => {
                                return Err(finish_launch_failure(
                                    &state,
                                    producer,
                                    &session_id,
                                    &mut launch_completion_pending,
                                    LaunchFailure {
                                        proof_context: Some(&proof_context),
                                        class: failure_class,
                                        message: repair_user_outcome.summary(),
                                        healing,
                                        guardian: Some(guardian.clone()),
                                        outcome: None,
                                    },
                                )
                                .await);
                            }
                        }
                    }
                    RegisteredArtifactStartupDisposition::ContinueStartupRecovery => {}
                }

                if let Some(directive) = startup_outcome.directive.clone() {
                    let failure_message =
                        match handle_recovery_directive(RecoveryDirectiveRequest {
                            state: &state,
                            session_id: &session_id,
                            intent: &intent,
                            directive,
                            stage: RecoveryDirectiveStage::Startup,
                            mode: api_guardian_mode(intent.guardian.mode),
                            failure_class,
                            recovery_attempts: &mut recovery_attempts,
                            max_recovery_attempts: MAX_RECOVERY_ATTEMPTS,
                            guardian: &mut guardian,
                        })
                        .await
                        .map_err(guardian_journal_error)?
                        {
                            RecoveryDirectiveOutcome::Apply(recovery_plan) => {
                                if apply_startup_recovery_directive(
                                    &mut guardian,
                                    &mut attempt,
                                    &recovery_plan,
                                ) {
                                    last_recovery_plan = Some(recovery_plan);
                                    continue;
                                } else {
                                    guardian = guardian_summary_with_blocked_outcome(
                                        &guardian,
                                        &startup_outcome.user_outcome,
                                    );
                                    startup_outcome.user_outcome.summary().to_string()
                                }
                            }
                            RecoveryDirectiveOutcome::Exhausted => {
                                guardian = guardian_summary_with_blocked_outcome(
                                    &guardian,
                                    &startup_outcome.user_outcome,
                                );
                                startup_outcome.user_outcome.summary().to_string()
                            }
                            RecoveryDirectiveOutcome::Rejected => {
                                guardian = guardian_summary_with_blocked_outcome(
                                    &guardian,
                                    &startup_outcome.user_outcome,
                                );
                                startup_outcome.user_outcome.summary().to_string()
                            }
                            RecoveryDirectiveOutcome::Suppressed(recovery_user_outcome) => {
                                recovery_user_outcome.summary().to_string()
                            }
                        };
                    let healing =
                        startup_failure_healing(&intent, &prepared, &attempt, failure_class);
                    return Err(finish_launch_failure(
                        &state,
                        producer,
                        &session_id,
                        &mut launch_completion_pending,
                        LaunchFailure {
                            proof_context: Some(&proof_context),
                            class: failure_class,
                            message: &failure_message,
                            healing,
                            guardian: Some(guardian.clone()),
                            outcome: None,
                        },
                    )
                    .await);
                }

                let healing = startup_failure_healing(&intent, &prepared, &attempt, failure_class);
                guardian =
                    guardian_summary_with_blocked_outcome(&guardian, &startup_outcome.user_outcome);
                return Err(finish_launch_failure(
                    &state,
                    producer,
                    &session_id,
                    &mut launch_completion_pending,
                    LaunchFailure {
                        proof_context: Some(&proof_context),
                        class: failure_class,
                        message: startup_outcome.user_outcome.summary(),
                        healing,
                        guardian: Some(guardian.clone()),
                        outcome: None,
                    },
                )
                .await);
            }
        }
    }
}

#[derive(Default)]
struct LaunchLoopControl {
    #[cfg(test)]
    forced_prepare_failure: Option<std::sync::Arc<ForcedPrepareFailure>>,
    #[cfg(test)]
    registered_artifact_component_rebuild_source: Option<RegisteredArtifactComponentRebuildSource>,
    #[cfg(test)]
    observed_startup_integrity:
        Option<std::sync::Arc<std::sync::Mutex<Vec<crate::guardian::GuardianFactId>>>>,
    #[cfg(test)]
    runtime_prepare_source: Option<LaunchRuntimePrepareSource>,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LaunchRuntimePrepareSource {
    Production,
    PersistedManifest,
}

impl LaunchLoopControl {
    #[cfg(test)]
    fn runtime_prepare_source(&self) -> LaunchRuntimePrepareSource {
        self.runtime_prepare_source
            .unwrap_or(LaunchRuntimePrepareSource::Production)
    }

    fn registered_artifact_component_rebuild_source(
        &self,
    ) -> RegisteredArtifactComponentRebuildSource {
        #[cfg(test)]
        if let Some(source) = self.registered_artifact_component_rebuild_source {
            return source;
        }
        RegisteredArtifactComponentRebuildSource::Production
    }

    fn record_startup_integrity(&self, _integrity: &StartupFailureIntegrity) {
        #[cfg(test)]
        if let Some(observed) = self.observed_startup_integrity.as_ref() {
            observed
                .lock()
                .expect("startup integrity observation lock")
                .extend(_integrity.facts.iter().map(|fact| fact.id));
        }
    }

    fn forced_prepare_failure(&self) -> Option<axial_launcher::LaunchPreparationError> {
        #[cfg(test)]
        if let Some(failure) = self.forced_prepare_failure.as_ref() {
            return Some(failure.next());
        }
        None
    }

    fn apply_prepare_recovery_directive(
        &self,
        guardian: &mut GuardianSummary,
        attempt: &mut axial_launcher::service::AttemptOverrides,
        recovery_plan: &GuardianLaunchRecoveryPlan,
    ) -> bool {
        #[cfg(test)]
        if self.forced_prepare_failure.is_some() {
            return false;
        }
        apply_prepare_recovery_directive(guardian, attempt, recovery_plan)
    }

    fn record_capped_prepare_failure(&self, _error: &axial_launcher::LaunchPreparationError) {
        #[cfg(test)]
        if let Some(failure) = self.forced_prepare_failure.as_ref() {
            failure.record_capped(&_error.message);
        }
    }
}

#[cfg(test)]
#[derive(Default)]
struct ForcedPrepareFailure {
    observed: std::sync::atomic::AtomicU8,
    capped_message: std::sync::Mutex<Option<String>>,
}

#[cfg(test)]
impl ForcedPrepareFailure {
    fn next(&self) -> axial_launcher::LaunchPreparationError {
        let ordinal = self
            .observed
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        axial_launcher::LaunchPreparationError {
            message: format!("forced prepare failure {ordinal}"),
            failure_class: Some(LaunchFailureClass::JavaRuntimeMismatch),
            healing: None,
        }
    }

    fn record_capped(&self, message: &str) {
        *self
            .capped_message
            .lock()
            .expect("forced prepare failure lock poisoned") = Some(message.to_string());
    }
}

fn guardian_journal_error(_error: OperationJournalStoreError) -> LaunchRequestError {
    LaunchRequestError {
        message:
            "Could not record the launch recovery safely. Check app data permissions and try again."
                .to_string(),
        healing: None,
        guardian: None,
    }
}

#[allow(clippy::too_many_arguments)]
async fn finish_registered_artifact_repair_failure(
    state: &AppState,
    producer: &crate::state::ProducerLease,
    session_id: &str,
    launch_completion_pending: &mut bool,
    proof_context: &LaunchProofContext,
    failure_class: LaunchFailureClass,
    healing: Option<axial_launcher::LaunchHealingSummary>,
    guardian: GuardianSummary,
    diagnosis_id: DiagnosisId,
    status: GuardianArtifactRepairStatus,
) -> LaunchRequestError {
    let user_outcome =
        author_guardian_copy(GuardianCopyRequest::artifact_repair(diagnosis_id, status))
            .expect("registered artifact repair copy request is closed");
    let guardian = guardian_summary_with_artifact_repair_outcome(&guardian, &user_outcome);
    finish_launch_failure(
        state,
        producer,
        session_id,
        launch_completion_pending,
        LaunchFailure {
            proof_context: Some(proof_context),
            class: failure_class,
            message: user_outcome.summary(),
            healing,
            guardian: Some(guardian),
            outcome: None,
        },
    )
    .await
}

async fn finish_launch_failure(
    state: &AppState,
    producer: &crate::state::ProducerLease,
    session_id: &str,
    launch_completion_pending: &mut bool,
    failure: LaunchFailure<'_>,
) -> LaunchRequestError {
    emit_pending_launch_failure(state, launch_completion_pending);
    fail_launch(state, producer, session_id, failure).await
}

fn emit_launch_started(
    state: &AppState,
    launch_completion_pending: &mut bool,
    loader_key: Option<String>,
) {
    if *launch_completion_pending {
        return;
    }

    state
        .telemetry()
        .emit(TelemetryEvent::launch_started(loader_key));
    *launch_completion_pending = true;
}

fn emit_launch_completed(
    state: &AppState,
    launch_completion_pending: &mut bool,
    outcome: TelemetryLaunchOutcome,
) {
    state
        .telemetry()
        .emit(TelemetryEvent::launch_completed(outcome));
    *launch_completion_pending = false;
}

fn emit_pending_launch_failure(state: &AppState, launch_completion_pending: &mut bool) {
    if !*launch_completion_pending {
        return;
    }

    emit_launch_completed(
        state,
        launch_completion_pending,
        TelemetryLaunchOutcome::Failure,
    );
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LegacyVirtualAssetRepairOutcome {
    SkippedModern,
    RepairedLegacy,
}

impl LegacyVirtualAssetRepairOutcome {
    fn full_object_parse_attempts(self) -> usize {
        match self {
            Self::SkippedModern => 0,
            Self::RepairedLegacy => 1,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::SkippedModern => "skipped_modern",
            Self::RepairedLegacy => "repaired_legacy",
        }
    }
}

async fn repair_legacy_virtual_assets_before_launch(
    library_dir: &std::path::Path,
    plan: &axial_launcher::VanillaLaunchPlan,
) -> Result<LegacyVirtualAssetRepairOutcome, axial_minecraft::download::DownloadError> {
    if !plan.requires_virtual_asset_repair {
        return Ok(LegacyVirtualAssetRepairOutcome::SkippedModern);
    }
    let asset_index_id = plan.version.asset_index.id.trim();
    let asset_index_path = assets_dir(library_dir)
        .join("indexes")
        .join(format!("{asset_index_id}.json"));
    let repaired = repair_virtual_assets_from_index(library_dir, &asset_index_path).await?;
    if !repaired {
        return Err(axial_minecraft::download::DownloadError::Integrity(
            "asset index legacy flags changed during launch preparation".to_string(),
        ));
    }
    Ok(LegacyVirtualAssetRepairOutcome::RepairedLegacy)
}

fn startup_failure_healing(
    intent: &axial_launcher::LaunchIntent,
    prepared: &PreparedLaunchAttempt,
    attempt: &axial_launcher::service::AttemptOverrides,
    failure_class: LaunchFailureClass,
) -> Option<axial_launcher::LaunchHealingSummary> {
    build_healing_summary(axial_launcher::service::HealingSummaryInput {
        auth_mode: if intent.auth.is_offline() {
            "offline"
        } else {
            "online"
        },
        requested_java_path: &intent.requested_java,
        requested_preset: &intent.requested_preset,
        effective_java_path: Some(prepared.runtime.effective_path.as_str()),
        effective_preset: Some(prepared.effective_preset.as_str()),
        fallback_applied: attempt.fallback_applied.as_deref(),
        retry_count: attempt.retry_count,
        failure_class: Some(failure_class),
    })
}

fn failure_class_needs_tier1_integrity(failure_class: LaunchFailureClass) -> bool {
    matches!(
        failure_class,
        LaunchFailureClass::LauncherManagedArtifactSignature
            | LaunchFailureClass::ClasspathModuleConflict
            | LaunchFailureClass::MissingDependency
    )
}

fn failure_class_needs_user_mod_witness(failure_class: LaunchFailureClass) -> bool {
    matches!(
        failure_class,
        LaunchFailureClass::MissingDependency
            | LaunchFailureClass::ModTransformationFailure
            | LaunchFailureClass::ModAttributedCrash
            | LaunchFailureClass::ClasspathModuleConflict
            | LaunchFailureClass::LoaderBootstrapFailure
    )
}

fn spawn_successful_user_mod_witness_publication(
    state: &AppState,
    producer: &crate::state::ProducerLease,
    foreground: &crate::state::IntegrityForegroundLease,
    instance_id: &str,
) -> tokio::sync::oneshot::Receiver<()> {
    let witness_state = state.clone();
    let witness_foreground = foreground.retained();
    let witness_instance_id = instance_id.to_string();
    let (completed_tx, completed_rx) = tokio::sync::oneshot::channel();
    producer.spawn_child(async move {
        if witness_state
            .publish_successful_user_mod_witness(&witness_foreground, &witness_instance_id)
            .await
            .is_err()
        {
            tracing::warn!(
                instance_id = witness_instance_id,
                "successful launch user mod witness publication was unavailable"
            );
        }
        let _ = completed_tx.send(());
    });
    completed_rx
}

async fn sense_user_mod_witness_drift_after_failure(
    state: &AppState,
    foreground: &crate::state::IntegrityForegroundLease,
    instance_id: &str,
    failure_class: LaunchFailureClass,
) -> Option<GuardianFact> {
    if !failure_class_needs_user_mod_witness(failure_class) {
        return None;
    }
    state
        .user_mod_witness_drifted_after_failure(foreground, instance_id)
        .await
        .then(user_mod_set_drift_fact)
}

#[derive(Default)]
struct StartupFailureIntegrity {
    facts: Vec<GuardianFact>,
    findings: Option<RegisteredArtifactFindings>,
}

impl StartupFailureIntegrity {
    fn repair_candidate(&self) -> Option<crate::state::RegisteredArtifactRepairCandidate<'_>> {
        self.findings.as_ref()?.repair_candidate()
    }

    fn into_findings(self) -> Option<RegisteredArtifactFindings> {
        self.findings
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RegisteredArtifactStartupDisposition {
    ContinueStartupRecovery,
    ExecuteRepair,
    TerminalizeRetryFailure,
}

fn registered_artifact_startup_disposition(
    mode: crate::guardian::GuardianMode,
    decision: GuardianActionKind,
    process_retry_used: bool,
) -> RegisteredArtifactStartupDisposition {
    if process_retry_used {
        return RegisteredArtifactStartupDisposition::TerminalizeRetryFailure;
    }
    if mode == crate::guardian::GuardianMode::Managed && decision == GuardianActionKind::Repair {
        return RegisteredArtifactStartupDisposition::ExecuteRepair;
    }
    RegisteredArtifactStartupDisposition::ContinueStartupRecovery
}

async fn sense_startup_failure_integrity(
    state: &AppState,
    integrity_foreground: &crate::state::IntegrityForegroundLease,
    instance_id: &str,
    library_dir: &std::path::Path,
    failure_class: LaunchFailureClass,
) -> StartupFailureIntegrity {
    if !failure_class_needs_tier1_integrity(failure_class) {
        return StartupFailureIntegrity::default();
    }

    let Ok(lifecycle) = state
        .acquire_integrity_instance_lifecycle(integrity_foreground, instance_id)
        .await
    else {
        return StartupFailureIntegrity::default();
    };
    sense_integrity_tier1(state, integrity_foreground, &lifecycle, library_dir)
        .await
        .map(|admitted| {
            let (report, findings) = admitted.into_parts();
            let facts = report
                .facts
                .iter()
                .map(|fact| {
                    guardian_fact_from_execution(
                        fact,
                        crate::state::contracts::OperationPhase::Launching,
                    )
                })
                .collect();
            StartupFailureIntegrity {
                facts,
                findings: Some(findings),
            }
        })
        .unwrap_or_default()
}

pub fn trace_launch_event(session_id: &str, message: &str) {
    append_trace("launch", session_id, message);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guardian::{
        GuardianActionKind, GuardianArtifactRepairSettlement, GuardianComponentRebuildStatus,
        GuardianDomain, GuardianMode, GuardianSummaryDecision,
        GuardianWholeInstanceRematerializationDisposition,
        GuardianWholeInstanceRematerializationStatus, assess_whole_instance_rematerialization,
        execute_managed_version_bundle_component_rebuild,
        execute_registered_guardian_artifact_repair, execute_whole_instance_rematerialization_with,
        guardian_summary_for_test, guardian_user_outcome_for_test,
    };
    use crate::observability::telemetry::{DEFAULT_POSTHOG_HOST, TelemetryHub};
    use crate::state::contracts::{
        OperationId, OperationPhase, OperationStatus, OwnershipClass, ReconciliationComponent,
        ReconciliationRung, ReconciliationTerminalOutcome, StabilizationSystem, TargetDescriptor,
        TargetKind,
    };
    use crate::state::failure_memory::{FailureMemorySnapshot, failure_memory_path};
    use crate::state::{
        AppStateInit, GuardianFailureMemoryStore, InstallStore, LaunchEvent, OperationJournalStore,
        ReconciliationEvidenceRejection, SessionStore, reconciliation_attempt_key,
    };
    use axial_config::{
        AppConfig, AppPaths, ConfigStore, Instance, InstanceRegistrySnapshot, InstanceStore,
    };
    use axial_launcher::{
        CrashEvidence, LaunchAuthContext, LaunchGuardianContext, LaunchIntent, LaunchSessionRecord,
        OverrideOrigin, SessionId,
    };
    use axial_minecraft::known_good::{
        KnownGoodArtifactKind, KnownGoodIntegrity, KnownGoodInventory, TestKnownGoodEntry,
        TestKnownGoodIntegrity, TestKnownGoodRoot, known_good_entry_path,
    };
    use axial_minecraft::runtime::RuntimeId;
    use axial_performance::PerformanceManager;
    use sha1::{Digest as _, Sha1};
    use std::fs;
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use std::time::{SystemTime, UNIX_EPOCH};

    const TEST_TELEMETRY_INSTALL_ID: &str = "123e4567-e89b-12d3-a456-426614174000";
    const TEST_TELEMETRY_KEY: &str = "phc_test";
    const MANAGED_LIBRARY_FIXTURE_PATH: &str =
        "libraries/org/axial/fixture/1.0.0/fixture-1.0.0.jar";
    const MANAGED_LIBRARY_FIXTURE_BYTES: &[u8] = b"axial managed Libraries fixture";
    const CRASH_E2E_INSTANCE_ID: &str = "0123456789abcdef";
    const CRASH_E2E_FABRIC_VERSION_ID: &str =
        "loader-v2-YXhpYWwtaW5zdGFsbGVkLWxvYWRlcgABAAYxLjIxLjEABzAuMTYuMTA";
    const CRASH_E2E_FABRIC_LIBRARIES: [(&str, &str); 2] = [
        (
            "net.fabricmc:fabric-loader:0.16.10",
            "net/fabricmc/fabric-loader/0.16.10/fabric-loader-0.16.10.jar",
        ),
        (
            "net.fabricmc:intermediary:1.21.1",
            "net/fabricmc/intermediary/1.21.1/intermediary-1.21.1.jar",
        ),
    ];

    #[test]
    fn tier1_integrity_has_an_exhaustive_closed_failure_class_trigger() {
        for &failure_class in LaunchFailureClass::ALL {
            assert_eq!(
                failure_class_needs_tier1_integrity(failure_class),
                matches!(
                    failure_class,
                    LaunchFailureClass::LauncherManagedArtifactSignature
                        | LaunchFailureClass::ClasspathModuleConflict
                        | LaunchFailureClass::MissingDependency
                ),
                "unexpected Tier1 admission for {}",
                failure_class.as_str()
            );
        }
    }

    #[test]
    fn user_mod_witness_has_an_exhaustive_closed_failure_class_trigger() {
        for &failure_class in LaunchFailureClass::ALL {
            assert_eq!(
                failure_class_needs_user_mod_witness(failure_class),
                matches!(
                    failure_class,
                    LaunchFailureClass::MissingDependency
                        | LaunchFailureClass::ModTransformationFailure
                        | LaunchFailureClass::ModAttributedCrash
                        | LaunchFailureClass::ClasspathModuleConflict
                        | LaunchFailureClass::LoaderBootstrapFailure
                ),
                "unexpected user mod witness admission for {}",
                failure_class.as_str()
            );
        }
    }

    #[tokio::test]
    async fn production_user_mod_witness_path_is_mode_orthogonal_and_condition_only() {
        fn outcome(
            mode: GuardianMode,
            integrity_facts: &[GuardianFact],
        ) -> crate::guardian::GuardianLaunchFailureOutcome {
            guardian_startup_failure_outcome(GuardianStartupFailureRequest {
                mode,
                observation: GuardianStartupFailureObservation::Exited {
                    failure_class: LaunchFailureClass::MissingDependency,
                },
                crash_evidence: None,
                integrity_facts,
                registered_artifact_repair_candidate: None,
                target_version_id: "1.21.1",
                runtime_major: 21,
                requested_java_present: false,
                explicit_java_override_present: false,
                explicit_jvm_args_present: false,
                explicit_jvm_preset_present: false,
                startup_recovery_applied: false,
                disable_custom_gc: false,
                effective_preset: "performance",
            })
        }

        let root = unique_test_dir("production-user-mod-witness");
        let state = test_app_state(&root);
        let instance = state
            .instances()
            .insert_for_test("User mod witness", "1.21.1")
            .expect("insert witness instance");
        let instance_mode = instance.performance_mode.clone();
        let mods_dir = state.instances().game_dir(&instance.id).join("mods");
        fs::create_dir_all(&mods_dir).expect("create witness mods root");
        fs::write(mods_dir.join("user.jar"), b"baseline").expect("write baseline user mod");
        let producer = state.try_claim_producer().expect("claim witness producer");
        let publication_foreground = state
            .register_integrity_foreground()
            .expect("register publication foreground")
            .wait_for_settlement()
            .await;
        let publication = spawn_successful_user_mod_witness_publication(
            &state,
            &producer,
            &publication_foreground,
            &instance.id,
        );
        drop(publication_foreground);
        tokio::time::timeout(Duration::from_secs(2), publication)
            .await
            .expect("producer-owned publication deadline")
            .expect("producer-owned publication completed");

        fs::write(mods_dir.join("added.jar"), b"added").expect("add drifted user mod");
        let comparison_foreground = state
            .register_integrity_foreground()
            .expect("register comparison foreground")
            .wait_for_settlement()
            .await;
        assert!(
            sense_user_mod_witness_drift_after_failure(
                &state,
                &comparison_foreground,
                &instance.id,
                LaunchFailureClass::OutOfMemory,
            )
            .await
            .is_none(),
            "ineligible preboot failure must omit the witness"
        );
        let drift_fact = sense_user_mod_witness_drift_after_failure(
            &state,
            &comparison_foreground,
            &instance.id,
            LaunchFailureClass::MissingDependency,
        )
        .await
        .expect("eligible preboot failure compares the successful baseline");

        for &mode in GuardianMode::ALL {
            let baseline = outcome(mode, &[]);
            let with_drift = outcome(mode, std::slice::from_ref(&drift_fact));
            assert_eq!(baseline.guardian_decision, with_drift.guardian_decision);
            assert_eq!(baseline.directive, with_drift.directive);
            assert!(with_drift.user_outcome.details().contains(
                &"The active mods changed since the last successful launch.".to_string()
            ));
            assert!(
                with_drift
                    .user_outcome
                    .details()
                    .iter()
                    .all(|line| !line.contains("added.jar"))
            );
        }
        assert_eq!(
            state
                .instances()
                .get(&instance.id)
                .map(|current| current.performance_mode),
            Some(instance_mode),
            "the witness must not consult or rewrite instance performance mode"
        );

        drop(comparison_foreground);
        drop(producer);
        tokio::time::timeout(Duration::from_secs(5), state.shutdown())
            .await
            .expect("witness shutdown deadline")
            .expect("witness shutdown settles");
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn library_component_escalation_retains_exact_transaction_proofs() {
        let root = unique_test_dir("libraries-component-rebuild");
        let instance_id = "0000000000000001";
        let (state, active_inventory) =
            test_libraries_recovery_app_state(&root, instance_id, "http://127.0.0.1:0/unreachable");
        let library_root = PathBuf::from(
            state
                .library_dir()
                .expect("State-authored Libraries recovery root"),
        );
        let foreground = state
            .register_integrity_foreground()
            .expect("register Libraries recovery foreground")
            .wait_for_settlement()
            .await;
        let lifecycle = state.acquire_instance_lifecycle(instance_id).await;
        let operation_id = OperationId::new("registered-library-leaf-failed");
        let report = sense_integrity_tier1(&state, &foreground, &lifecycle, &library_root)
            .await
            .expect("sense missing registered Libraries fixture");
        assert_eq!(report.facts.len(), 1);
        let (_, findings) = report.into_parts();
        let target = findings
            .repair_candidate()
            .map(|candidate| candidate.target())
            .expect("source-backed registered Libraries target")
            .clone();
        let authorization = findings
            .authorize_repair(&registered_artifact_repair_decision(
                "registered-library-leaf-failed",
                target,
            ))
            .expect("authorize exact registered Libraries repair");
        let admission = state
            .admit_registered_artifact_repair(
                authorization,
                operation_id.clone(),
                chrono::Duration::minutes(30),
            )
            .await
            .expect("admit registered Libraries repair");
        drop(lifecycle);

        let recovery = execute_registered_artifact_recovery_sequence(
            &state,
            RegisteredArtifactRecoveryEntry::Fresh(Box::new(admission)),
            &reqwest::Client::new(),
            RegisteredArtifactComponentRebuildSource::Fixture,
        )
        .await
        .expect("settle registered Libraries component recovery");

        assert_eq!(
            recovery.diagnosis_id,
            DiagnosisId::LauncherManagedArtifactCorrupt
        );
        assert_eq!(
            recovery.effective_status,
            GuardianArtifactRepairStatus::Repaired
        );
        let leaf_entry = state
            .journals()
            .get(&operation_id)
            .expect("registered Libraries leaf journal");
        let leaf_attempt = leaf_entry
            .reconciliation_attempt()
            .expect("registered Libraries leaf attempt");
        let leaf_terminal = leaf_entry
            .reconciliation_terminal()
            .expect("registered Libraries leaf terminal");
        assert_eq!(leaf_attempt.rung(), ReconciliationRung::RepairArtifact);
        assert_eq!(leaf_attempt.component(), ReconciliationComponent::Libraries);
        assert_eq!(
            leaf_terminal.outcome(),
            ReconciliationTerminalOutcome::Failed
        );
        assert_eq!(
            state
                .failure_memory()
                .get(&reconciliation_attempt_key(leaf_attempt))
                .and_then(|entry| entry.reconciliation_terminal().cloned()),
            Some(leaf_terminal.clone())
        );
        let component_entry = state
            .journals()
            .list()
            .into_iter()
            .find(|entry| {
                entry
                    .reconciliation_attempt()
                    .is_some_and(|attempt| attempt.rung() == ReconciliationRung::RebuildComponent)
            })
            .expect("Libraries component rebuild journal");
        assert_eq!(component_entry.status, OperationStatus::Succeeded);
        assert!(
            component_entry
                .planned_steps
                .iter()
                .any(|step| { step.step_id == "rebuild_managed_libraries_component" })
        );
        assert!(
            component_entry
                .planned_steps
                .iter()
                .all(|step| !step.step_id.contains("quarantine"))
        );
        let component_attempt = component_entry
            .reconciliation_attempt()
            .expect("Libraries component attempt");
        let component_terminal = component_entry
            .reconciliation_terminal()
            .expect("Libraries component terminal");
        assert_eq!(
            component_terminal.outcome(),
            ReconciliationTerminalOutcome::Succeeded
        );
        assert_eq!(
            state
                .failure_memory()
                .get(&reconciliation_attempt_key(component_attempt))
                .and_then(|entry| entry.reconciliation_terminal().cloned()),
            Some(component_terminal.clone())
        );
        assert_eq!(
            fs::read(library_root.join(MANAGED_LIBRARY_FIXTURE_PATH))
                .expect("rebuilt Libraries fixture"),
            MANAGED_LIBRARY_FIXTURE_BYTES
        );
        let lifecycle = state.acquire_instance_lifecycle(instance_id).await;
        let verification = state
            .mint_known_good_verification_lease(&foreground, &lifecycle, &library_root)
            .expect("current Libraries verification lease");
        assert!(std::ptr::eq(
            verification.execution_parts().5,
            Arc::as_ref(&active_inventory),
        ));
        drop(verification);
        let postcheck = sense_integrity_tier1(&state, &foreground, &lifecycle, &library_root)
            .await
            .expect("fresh Libraries Tier1 postcheck");
        assert!(postcheck.facts.is_empty());
        drop((postcheck, lifecycle));

        drop(foreground);
        state
            .close_known_good_inventories()
            .await
            .expect("close Libraries recovery known-good store");
        state
            .close_instance_registry()
            .await
            .expect("close Libraries recovery instance registry");
        state
            .journals()
            .close()
            .await
            .expect("close Libraries recovery journal");
        state
            .failure_memory()
            .close()
            .await
            .expect("close Libraries recovery memory");
        drop(state);
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn deleted_library_uses_leaf_repair_and_second_process_reaches_boot() {
        let root = unique_test_dir("deleted-library-launch-continuation");
        let instance_id = "0000000000000001";
        let session_id = "deleted-library-launch-continuation";
        let (repair_source, source_server) =
            serve_registered_library_once(MANAGED_LIBRARY_FIXTURE_BYTES).await;
        let (state, _) = test_libraries_recovery_app_state(&root, instance_id, &repair_source);
        let library_root = PathBuf::from(
            state
                .library_dir()
                .expect("State-authored deleted-Libraries root"),
        );
        let library_path = library_root.join(MANAGED_LIBRARY_FIXTURE_PATH);
        let process_count_path = root.join("deleted-library-process-count");
        let java_path =
            write_deleted_library_launch_fixture(&root, &library_path, &process_count_path);
        let user_owned = write_user_owned_launch_sentinels(&state, instance_id);
        let mut session = test_record(session_id);
        session.instance_id = instance_id.to_string();
        state
            .sessions()
            .insert(session)
            .await
            .expect("insert deleted-Libraries launch session");
        let producer = state
            .try_claim_producer()
            .expect("claim deleted-Libraries launch producer");
        let mut task = test_recovery_launch_task(&state, session_id, &root).await;
        retarget_test_launch_task(&mut task, instance_id);
        task.instance.java_path = java_path.clone();
        task.intent.requested_java = java_path;
        task.intent.game_dir = Some(state.instances().game_dir(instance_id));
        let (integrity_foreground, task) = LaunchSessionRunTask::from_prepared(task);
        let mut integrity_foreground = Some(integrity_foreground);

        let launch_result = tokio::time::timeout(
            Duration::from_secs(10),
            launch_session_inner(state.clone(), task, &producer, &mut integrity_foreground),
        )
        .await
        .expect("deleted-Libraries launch deadline");
        let launched = match launch_result {
            Ok(launched) => launched,
            Err(error) => {
                let terminal = state.sessions().get(session_id).await;
                panic!(
                    "deleted-Libraries recovery must reach process 2: {}; terminal={terminal:?}",
                    error.message
                );
            }
        };

        assert_eq!(launched.session_id, session_id);
        assert_eq!(
            fs::read_to_string(&process_count_path).expect("launch process count"),
            "2"
        );
        assert_eq!(
            fs::read(&library_path).expect("leaf-repaired Libraries fixture"),
            MANAGED_LIBRARY_FIXTURE_BYTES
        );
        assert_eq!(user_owned.len(), 5);
        assert_user_owned_launch_sentinels(&user_owned);
        let reconciliation = state
            .journals()
            .list()
            .into_iter()
            .filter_map(|journal| {
                let attempt = journal.reconciliation_attempt()?.clone();
                Some((journal, attempt))
            })
            .collect::<Vec<_>>();
        assert_eq!(reconciliation.len(), 1);
        let leaf = reconciliation
            .first()
            .expect("deleted-Libraries R1 terminal");
        assert_eq!(leaf.1.rung(), ReconciliationRung::RepairArtifact);
        assert_eq!(leaf.0.status, OperationStatus::Succeeded);
        assert_eq!(leaf.1.component(), ReconciliationComponent::Libraries);
        let leaf_terminal = leaf
            .0
            .reconciliation_terminal()
            .expect("deleted-Libraries R1 result");
        assert_eq!(
            leaf_terminal.outcome(),
            ReconciliationTerminalOutcome::Succeeded
        );
        assert!(leaf_terminal.quarantine_checkpoint().is_empty());
        assert_eq!(
            state
                .failure_memory()
                .get(&reconciliation_attempt_key(&leaf.1))
                .and_then(|entry| entry.reconciliation_terminal().cloned()),
            Some(leaf_terminal.clone())
        );

        let foreground = integrity_foreground
            .as_ref()
            .expect("successful launch retains integrity foreground");
        let lifecycle = state.acquire_instance_lifecycle(instance_id).await;
        let postcheck = sense_integrity_tier1(&state, foreground, &lifecycle, &library_root)
            .await
            .expect("deleted-Libraries Tier1 postcheck");
        assert!(postcheck.facts.is_empty());
        drop((postcheck, lifecycle));
        source_server
            .await
            .expect("deleted-Libraries source server task");

        let _ = state.sessions().kill(session_id).await;
        drop(integrity_foreground);
        state
            .close_known_good_inventories()
            .await
            .expect("close deleted-Libraries known-good store");
        state
            .close_instance_registry()
            .await
            .expect("close deleted-Libraries instance registry");
        state
            .journals()
            .close()
            .await
            .expect("close deleted-Libraries journal");
        state
            .failure_memory()
            .close()
            .await
            .expect("close deleted-Libraries memory");
        drop((producer, state));
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn wrong_content_client_rebuilds_version_bundle_and_second_process_reaches_boot() {
        let root = unique_test_dir("wrong-content-client-launch-continuation");
        let instance_id = "0000000000000001";
        let session_id = "wrong-content-client-launch-continuation";
        let (state, client_path, expected_client) =
            test_version_bundle_recovery_app_state(&root, instance_id);
        let wrong_client = fs::read(&client_path).expect("wrong-content VersionBundle client");
        assert_eq!(wrong_client.len(), expected_client.len());
        assert_ne!(wrong_client, expected_client);
        let process_count_path = root.join("wrong-content-client-process-count");
        let java_path = write_version_bundle_launch_fixture(
            &root,
            &client_path,
            &expected_client,
            &process_count_path,
        );
        let user_owned = write_user_owned_launch_sentinels(&state, instance_id);
        let mut session = test_record(session_id);
        session.instance_id = instance_id.to_string();
        state
            .sessions()
            .insert(session)
            .await
            .expect("insert wrong-content client launch session");
        let producer = state
            .try_claim_producer()
            .expect("claim wrong-content client launch producer");
        let mut task = test_recovery_launch_task(&state, session_id, &root).await;
        retarget_test_launch_task(&mut task, instance_id);
        task.instance.java_path = java_path.clone();
        task.intent.requested_java = java_path;
        task.intent.game_dir = Some(state.instances().game_dir(instance_id));
        let (integrity_foreground, task) = LaunchSessionRunTask::from_prepared(task);
        let mut integrity_foreground = Some(integrity_foreground);
        let observed_startup_integrity = Arc::new(std::sync::Mutex::new(Vec::new()));
        let control = LaunchLoopControl {
            registered_artifact_component_rebuild_source: Some(
                RegisteredArtifactComponentRebuildSource::Fixture,
            ),
            observed_startup_integrity: Some(observed_startup_integrity.clone()),
            ..LaunchLoopControl::default()
        };

        let launched = tokio::time::timeout(
            Duration::from_secs(10),
            launch_session_inner_with_control(
                state.clone(),
                task,
                &producer,
                &mut integrity_foreground,
                &control,
            ),
        )
        .await
        .expect("wrong-content client launch deadline")
        .unwrap_or_else(|error| {
            panic!(
                "VersionBundle recovery must reach process 2: {}",
                error.message
            )
        });

        assert_eq!(launched.session_id, session_id);
        assert_eq!(
            fs::read_to_string(&process_count_path).expect("VersionBundle process count"),
            "2"
        );
        assert_eq!(
            fs::read(&client_path).expect("rebuilt VersionBundle client"),
            expected_client
        );
        let running = state
            .sessions()
            .get(session_id)
            .await
            .expect("running VersionBundle launch");
        assert_eq!(running.state, LaunchState::Running);
        assert!(running.boot_completed_at_ms.is_some());
        assert!(
            observed_startup_integrity
                .lock()
                .expect("observed startup integrity")
                .contains(&crate::guardian::GuardianFactId::ArtifactHashMismatch),
            "process-triggered Tier1 must observe the same-size client hash mismatch"
        );

        let reconciliation = state
            .journals()
            .list()
            .into_iter()
            .filter_map(|journal| {
                let attempt = journal.reconciliation_attempt()?.clone();
                Some((journal, attempt))
            })
            .collect::<Vec<_>>();
        assert_eq!(reconciliation.len(), 2);
        let leaf = reconciliation
            .iter()
            .find(|(_, attempt)| attempt.rung() == ReconciliationRung::RepairArtifact)
            .expect("wrong-content VersionBundle R1 terminal");
        let component = reconciliation
            .iter()
            .find(|(_, attempt)| attempt.rung() == ReconciliationRung::RebuildComponent)
            .expect("wrong-content VersionBundle R2 terminal");
        assert_eq!(leaf.1.component(), ReconciliationComponent::VersionBundle);
        assert_eq!(
            component.1.component(),
            ReconciliationComponent::VersionBundle
        );
        assert_eq!(leaf.0.status, OperationStatus::Failed);
        assert_eq!(component.0.status, OperationStatus::Succeeded);
        assert_eq!(
            leaf.0.failure_point.as_deref(),
            Some(crate::state::REGISTERED_ARTIFACT_COMPONENT_REBUILD_FAILURE_POINT)
        );
        assert!(leaf.0.planned_steps.iter().all(|step| {
            step.step_id != "download_artifact_to_temp"
                && step.step_id != "quarantine_launcher_managed_target"
        }));
        assert!(
            component.0.planned_steps.iter().any(|step| {
                step.step_id == crate::state::VERSION_BUNDLE_COMPONENT_REBUILD_STEP
            })
        );
        let leaf_terminal = leaf
            .0
            .reconciliation_terminal()
            .expect("wrong-content VersionBundle R1 result");
        let component_terminal = component
            .0
            .reconciliation_terminal()
            .expect("wrong-content VersionBundle R2 result");
        assert_eq!(
            leaf_terminal.outcome(),
            ReconciliationTerminalOutcome::Failed
        );
        assert_eq!(
            component_terminal.outcome(),
            ReconciliationTerminalOutcome::Succeeded
        );
        assert!(leaf_terminal.quarantine_checkpoint().is_empty());
        assert!(component_terminal.quarantine_checkpoint().is_empty());
        for (entry, attempt) in [leaf, component] {
            assert_eq!(
                state
                    .failure_memory()
                    .get(&reconciliation_attempt_key(attempt))
                    .and_then(|memory| memory.reconciliation_terminal().cloned()),
                entry.reconciliation_terminal().cloned()
            );
        }
        let foreground = integrity_foreground
            .as_ref()
            .expect("successful VersionBundle launch retains foreground");
        let lifecycle = state.acquire_instance_lifecycle(instance_id).await;
        let postcheck =
            sense_integrity_tier1(&state, foreground, &lifecycle, &root.join("library"))
                .await
                .expect("wrong-content VersionBundle Tier1 postcheck");
        assert!(postcheck.facts.is_empty());
        assert_user_owned_launch_sentinels(&user_owned);
        drop((postcheck, lifecycle));

        let _ = state.sessions().kill(session_id).await;
        drop(integrity_foreground);
        state
            .close_known_good_inventories()
            .await
            .expect("close VersionBundle known-good store");
        state
            .close_instance_registry()
            .await
            .expect("close VersionBundle instance registry");
        state
            .journals()
            .close()
            .await
            .expect("close VersionBundle journal");
        state
            .failure_memory()
            .close()
            .await
            .expect("close VersionBundle memory");
        drop((producer, state));
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn mass_tamper_escalates_to_explicit_whole_instance_recovery_and_fresh_launch() {
        let root = unique_test_dir("mass-tamper-whole-instance-recovery");
        let paths = test_paths(&root);
        let instance_id = "0000000000000001";
        let state = test_registered_recovery_app_state(
            &root,
            instance_id,
            "Mass tamper whole-instance recovery",
        );
        let core_fixture = prepare_whole_instance_launch_fixture(&state).await;
        let active_inventory = state.activate_known_good_inventory_for_test_with_identity(
            instance_id,
            core_fixture.known_good_inventory(),
        );
        let mut user_owned = write_user_owned_launch_sentinels(&state, instance_id);
        let managed_root_unknown = PathBuf::from(
            state
                .library_dir()
                .expect("mass-tamper managed root for unknown-owned sentinel"),
        )
        .join("mods")
        .join("user-owned.txt");
        fs::create_dir_all(
            managed_root_unknown
                .parent()
                .expect("managed-root unknown-owned sentinel parent"),
        )
        .expect("create managed-root unknown-owned sentinel parent");
        fs::write(&managed_root_unknown, b"unknown-owned").expect("write unknown-owned sentinel");
        user_owned.push((managed_root_unknown, b"unknown-owned".to_vec()));
        let corruption = tamper_every_managed_inventory_entry(&state, &active_inventory);
        assert!(
            !corruption.deleted.exists(),
            "one managed entry must remain deleted when sensing begins"
        );
        assert!(
            corruption.replaced.is_file(),
            "one managed entry must be removed and recreated"
        );
        assert!(
            corruption.tampered.is_file(),
            "one separate managed entry must be mutated in place"
        );

        let foreground = state
            .register_integrity_foreground()
            .expect("register mass-tamper integrity foreground")
            .wait_for_settlement()
            .await;
        let lifecycle = state.acquire_instance_lifecycle(instance_id).await;
        let report = sense_integrity_tier1(
            &state,
            &foreground,
            &lifecycle,
            &PathBuf::from(state.library_dir().expect("mass-tamper managed root")),
        )
        .await
        .expect("sense mass tampering");
        assert!(
            !report.facts.is_empty(),
            "mass tampering must produce Tier1 integrity evidence"
        );
        let (_, findings) = report.into_parts();
        let target = findings
            .repair_candidate()
            .map(|candidate| candidate.target())
            .expect("mass tampering retains a repairable VersionBundle target")
            .clone();
        let authorization = findings
            .authorize_repair(&registered_artifact_repair_decision(
                "mass-tamper-r1",
                target,
            ))
            .expect("authorize exact mass-tamper R1");
        drop(lifecycle);
        let r1_operation_id = OperationId::new("mass-tamper-r1");
        let r1_admission = state
            .admit_registered_artifact_repair(
                authorization,
                r1_operation_id.clone(),
                chrono::Duration::minutes(30),
            )
            .await
            .expect("admit exact mass-tamper R1");
        assert_eq!(
            r1_admission.attempt().component(),
            ReconciliationComponent::VersionBundle
        );
        let continuation = match execute_registered_guardian_artifact_repair(
            r1_admission,
            &reqwest::Client::new(),
        )
        .await
        .expect("settle real mass-tamper R1")
        {
            GuardianArtifactRepairSettlement::Failed(failure) => (*failure).into_continuation(),
            GuardianArtifactRepairSettlement::Completed(_) => {
                panic!("mass-tampered VersionBundle must require R2")
            }
        };

        let r2_operation_id = OperationId::new("mass-tamper-r2");
        let r2_admission = state
            .admit_registered_artifact_component_rebuild(
                continuation,
                r2_operation_id.clone(),
                chrono::Duration::minutes(30),
            )
            .await
            .expect("admit exact mass-tamper R2");
        assert_eq!(
            r2_admission.attempt().component(),
            ReconciliationComponent::VersionBundle
        );
        let r2_outcome =
            execute_managed_version_bundle_component_rebuild(r2_admission, |effect| async move {
                effect.failed_before_effect(["mass_tamper_component_fixture_failed".to_string()])
            })
            .await
            .expect("settle failed mass-tamper R2 fixture");
        assert_eq!(r2_outcome.status, GuardianComponentRebuildStatus::Failed);

        let eligibility = state
            .whole_instance_rematerialization_eligibility(instance_id)
            .await
            .expect("failed mass-tamper R2 is eligible for R3 assessment");
        let GuardianWholeInstanceRematerializationDisposition::Offered(offer) =
            assess_whole_instance_rematerialization(eligibility, GuardianMode::Managed)
                .expect("failed mass-tamper R2 assessment")
        else {
            panic!("Managed mass tampering must produce an explicit R3 offer");
        };
        let r3_operation_id = OperationId::new("mass-tamper-r3");
        let request = state
            .try_admit_request()
            .expect("admit explicit mass-tamper R3 request");
        let core_calls = Arc::new(AtomicUsize::new(0));
        let observed_core_calls = core_calls.clone();
        let result = crate::application::spawn_explicit_whole_instance_rematerialization_with(
            state.clone(),
            request.producer_handoff(),
            offer,
            r3_operation_id.clone(),
            chrono::Duration::minutes(30),
            move |admission| {
                execute_whole_instance_rematerialization_with(
                    admission,
                    move |_, runtime_cache, _| async move {
                        observed_core_calls.fetch_add(1, Ordering::SeqCst);
                        axial_minecraft::publish_managed_whole_instance_fixture_for_test(
                            core_fixture,
                            &runtime_cache,
                        )
                        .await
                    },
                )
            },
        )
        .expect("spawn Application-owned mass-tamper R3");
        drop(request);
        let r3_outcome = result
            .await
            .expect("mass-tamper R3 owner result")
            .expect("mass-tamper R3 settlement");
        assert_eq!(core_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            r3_outcome.status(),
            GuardianWholeInstanceRematerializationStatus::Rematerialized
        );

        let reconciliation = state
            .journals()
            .list()
            .into_iter()
            .filter_map(|journal| {
                let attempt = journal.reconciliation_attempt()?.clone();
                Some((journal, attempt))
            })
            .collect::<Vec<_>>();
        assert_eq!(reconciliation.len(), 3);
        for (operation_id, rung, component) in [
            (
                &r1_operation_id,
                ReconciliationRung::RepairArtifact,
                ReconciliationComponent::VersionBundle,
            ),
            (
                &r2_operation_id,
                ReconciliationRung::RebuildComponent,
                ReconciliationComponent::VersionBundle,
            ),
            (
                &r3_operation_id,
                ReconciliationRung::RematerializeInstance,
                ReconciliationComponent::WholeInstance,
            ),
        ] {
            let (journal, attempt) = reconciliation
                .iter()
                .find(|(journal, _)| journal.operation_id == *operation_id)
                .expect("exact mass-tamper reconciliation journal");
            assert_eq!(attempt.rung(), rung);
            assert_eq!(attempt.component(), component);
            let terminal = journal
                .reconciliation_terminal()
                .expect("mass-tamper reconciliation terminal");
            assert_eq!(
                terminal.outcome(),
                if rung == ReconciliationRung::RematerializeInstance {
                    ReconciliationTerminalOutcome::Succeeded
                } else {
                    ReconciliationTerminalOutcome::Failed
                }
            );
            assert!(terminal.quarantine_checkpoint().is_empty());
            assert_eq!(
                state
                    .failure_memory()
                    .get(&reconciliation_attempt_key(attempt))
                    .and_then(|memory| memory.reconciliation_terminal().cloned()),
                Some(terminal.clone())
            );
        }

        let postcheck_lifecycle = state.acquire_instance_lifecycle(instance_id).await;
        let verification = state
            .mint_known_good_verification_lease(
                &foreground,
                &postcheck_lifecycle,
                &PathBuf::from(state.library_dir().expect("rematerialized managed root")),
            )
            .expect("mint rematerialized inventory verification");
        assert!(std::ptr::eq(
            verification.execution_parts().5,
            Arc::as_ref(&active_inventory),
        ));
        drop(verification);
        let postcheck = sense_integrity_tier1(
            &state,
            &foreground,
            &postcheck_lifecycle,
            &PathBuf::from(state.library_dir().expect("rematerialized managed root")),
        )
        .await
        .expect("verify rematerialized inventory");
        assert!(postcheck.facts.is_empty());
        drop((postcheck, postcheck_lifecycle));
        assert_user_owned_launch_sentinels(&user_owned);

        #[cfg(unix)]
        {
            let producer = state
                .try_claim_producer()
                .expect("claim fresh mass-tamper launch producer");
            let prepared = super::super::session::prepare_launch_session_owned(
                &state,
                super::super::LaunchRequest {
                    instance_id: instance_id.to_string(),
                    username: None,
                    max_memory_mb: None,
                    min_memory_mb: None,
                    client_started_at_ms: None,
                },
                &producer,
            )
            .await
            .unwrap_or_else(|(_, payload)| panic!("prepare rematerialized launch: {payload:?}"));
            let session_id = prepared.task.intent.session_id.clone();
            let launched = tokio::time::timeout(
                Duration::from_secs(10),
                launch_session_with_persisted_runtime_manifest_for_test(
                    state.clone(),
                    prepared.task,
                    producer,
                ),
            )
            .await
            .expect("rematerialized launch deadline")
            .unwrap_or_else(|error| panic!("rematerialized launch: {}", error.message));
            assert_eq!(launched.instance_id, instance_id);
            assert_eq!(
                fs::read_to_string(
                    state
                        .instances()
                        .game_dir(instance_id)
                        .join("guardian-whole-instance-process-count")
                )
                .expect("rematerialized launch process count"),
                "1"
            );
            let running = state
                .sessions()
                .get(&session_id)
                .await
                .expect("rematerialized running session");
            assert_eq!(running.state, LaunchState::Running);
            assert!(running.boot_completed_at_ms.is_some());
            state
                .sessions()
                .kill(&session_id)
                .await
                .expect("stop rematerialized launch");
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let terminal = state
                        .sessions()
                        .get(&session_id)
                        .await
                        .is_none_or(|record| {
                            matches!(record.state, LaunchState::Failed | LaunchState::Exited)
                        });
                    if terminal {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("rematerialized launch terminal deadline");
        }
        assert_user_owned_launch_sentinels(&user_owned);

        drop(foreground);
        state
            .journals()
            .close()
            .await
            .expect("close mass-tamper journals before reload");
        state
            .failure_memory()
            .close()
            .await
            .expect("close mass-tamper memory before reload");
        let restarted_journals = Arc::new(
            OperationJournalStore::try_load_from_paths(&paths)
                .expect("reload durable mass-tamper journals"),
        );
        let restarted_memory = Arc::new(
            GuardianFailureMemoryStore::try_load_from_paths(&paths)
                .expect("reload durable mass-tamper memory"),
        );
        let restarted_state = state
            .clone()
            .with_reconciliation_stores(restarted_journals.clone(), restarted_memory.clone());
        let reloaded_r3 = restarted_journals
            .get(&r3_operation_id)
            .expect("reloaded mass-tamper R3 journal");
        let reloaded_terminal = reloaded_r3
            .reconciliation_terminal()
            .cloned()
            .expect("reloaded mass-tamper R3 terminal");
        assert_eq!(
            reloaded_terminal.outcome(),
            ReconciliationTerminalOutcome::Succeeded
        );
        assert_eq!(
            restarted_memory
                .get(&reconciliation_attempt_key(reloaded_terminal.attempt()))
                .and_then(|memory| memory.reconciliation_terminal().cloned()),
            Some(reloaded_terminal)
        );
        assert_eq!(
            restarted_state
                .whole_instance_rematerialization_eligibility(instance_id)
                .await
                .err(),
            Some(ReconciliationEvidenceRejection::SuppressedPriorAttempt)
        );
        assert_eq!(
            restarted_journals
                .list()
                .into_iter()
                .filter(|journal| journal.reconciliation_attempt().is_some())
                .count(),
            3,
            "reloaded suppression must not mint or admit another rung-three attempt"
        );

        restarted_journals
            .close()
            .await
            .expect("close reloaded mass-tamper journals");
        restarted_memory
            .close()
            .await
            .expect("close reloaded mass-tamper memory");
        restarted_state
            .close_known_good_inventories()
            .await
            .expect("close mass-tamper known-good store");
        restarted_state
            .close_instance_registry()
            .await
            .expect("close mass-tamper instance registry");
        drop((restarted_state, state));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn non_managed_modes_never_execute_registered_artifact_repair() {
        for mode in [GuardianMode::Custom, GuardianMode::Disabled] {
            assert_eq!(
                registered_artifact_startup_disposition(mode, GuardianActionKind::Repair, false,),
                RegisteredArtifactStartupDisposition::ContinueStartupRecovery
            );
        }
    }

    #[tokio::test]
    async fn dropping_outer_launch_owner_keeps_owned_repair_settlement_alive() {
        let root = unique_test_dir("owned-launch-repair-settlement");
        let state = test_app_state(&root);
        let producer = state
            .try_claim_producer()
            .expect("claim outer launch producer");
        let foreground = state
            .register_integrity_foreground()
            .expect("register repair integrity foreground")
            .wait_for_settlement()
            .await;
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (finish_tx, finish_rx) = tokio::sync::oneshot::channel();
        let settled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let child_settled = settled.clone();
        let outer = tokio::spawn(async move {
            let _repair = producer.claim_child().spawn_joinable(async move {
                let _foreground = foreground;
                let _ = started_tx.send(());
                let _ = finish_rx.await;
                child_settled.store(true, std::sync::atomic::Ordering::Release);
            });
            std::future::pending::<()>().await;
        });

        tokio::time::timeout(Duration::from_secs(2), started_rx)
            .await
            .expect("owned repair child start deadline")
            .expect("owned repair child started");
        outer.abort();
        let _ = outer.await;
        assert!(
            !state.subscribe_integrity_idle().borrow().is_stably_idle(),
            "repair child must retain lifecycle ownership after launch cancellation"
        );
        finish_tx.send(()).expect("release repair settlement");
        tokio::time::timeout(Duration::from_secs(2), async {
            while !settled.load(std::sync::atomic::Ordering::Acquire)
                || !state.subscribe_integrity_idle().borrow().is_stably_idle()
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("owned repair settlement deadline");
        let _ = fs::remove_dir_all(root);
    }

    fn empty_guardian_summary(mode: axial_launcher::GuardianMode) -> GuardianSummary {
        guardian_summary_for_test(
            mode,
            GuardianSummaryDecision::Allowed,
            None,
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )
    }

    #[tokio::test]
    async fn launch_loop_caps_a_prepare_directive_that_never_marks_itself_applied() {
        let root = unique_test_dir("launch-recovery-cap");
        let state = test_app_state(&root);
        let session_id = "launch-recovery-cap";
        state
            .sessions()
            .insert(test_record(session_id))
            .await
            .expect("insert session");
        let forced_failure = Arc::new(ForcedPrepareFailure::default());
        let control = LaunchLoopControl {
            forced_prepare_failure: Some(forced_failure.clone()),
            ..LaunchLoopControl::default()
        };
        let producer = state.try_claim_producer().expect("claim launch producer");
        let task = test_recovery_launch_task(&state, session_id, &root).await;
        let (integrity_foreground, task) = LaunchSessionRunTask::from_prepared(task);
        let mut integrity_foreground = Some(integrity_foreground);

        let result = tokio::time::timeout(
            Duration::from_secs(10),
            launch_session_inner_with_control(
                state.clone(),
                task,
                &producer,
                &mut integrity_foreground,
                &control,
            ),
        )
        .await
        .unwrap_or_else(|_| {
            panic!(
                "launch recovery loop stalled after {} forced failures",
                forced_failure
                    .observed
                    .load(std::sync::atomic::Ordering::SeqCst)
            )
        });
        let error = match result {
            Ok(_) => panic!("non-applying recovery must fail at the cap"),
            Err(error) => error,
        };

        assert!(
            integrity_foreground.is_some(),
            "preboot launch failure must return foreground ownership to terminalization"
        );
        drop(integrity_foreground);
        assert!(state.subscribe_integrity_idle().borrow().is_stably_idle());

        assert_eq!(
            forced_failure
                .observed
                .load(std::sync::atomic::Ordering::SeqCst),
            MAX_RECOVERY_ATTEMPTS + 1
        );
        assert_eq!(
            forced_failure
                .capped_message
                .lock()
                .expect("forced prepare failure lock")
                .as_deref(),
            Some("forced prepare failure 4")
        );
        let final_outcome = guardian_prepare_failure_outcome(GuardianPrepareFailureRequest {
            mode: crate::guardian::GuardianMode::Managed,
            failure_class: LaunchFailureClass::JavaRuntimeMismatch,
            public_error: "forced prepare failure 4",
            requested_java_present: true,
            explicit_java_override_present: true,
            explicit_jvm_args_present: false,
            runtime_intervention_applied: false,
            raw_jvm_args_intervention_applied: false,
        });
        assert_eq!(error.message, final_outcome.user_outcome.summary());
        let record = state
            .sessions()
            .get(session_id)
            .await
            .expect("failed launch session");
        assert_eq!(
            record
                .stages
                .iter()
                .flat_map(|stage| &stage.evidence)
                .filter(|evidence| {
                    evidence.id == "guardian_launch_safety_decision"
                        && evidence.system == "guardian"
                })
                .count(),
            1
        );

        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn launch_foreground_releases_only_after_success_metadata_settles() {
        let root = unique_test_dir("launch-foreground-success-metadata");
        let state = test_app_state(&root);
        let session_id = "launch-foreground-success-metadata";
        let registered = state
            .instances()
            .insert_for_test("Launch metadata gate".to_string(), "1.21.1".to_string())
            .expect("register launch instance");
        let mut record = test_record(session_id);
        record.instance_id = registered.id.clone();
        state
            .sessions()
            .insert(record)
            .await
            .expect("insert metadata-gated session");
        let mut events = state
            .sessions()
            .subscribe(session_id)
            .await
            .expect("subscribe metadata-gated session");
        let java_path = write_delayed_boot_launch_fixture(&root);
        let producer = state
            .try_claim_producer()
            .expect("claim metadata-gated producer");
        let mut task = test_recovery_launch_task(&state, session_id, &root).await;
        retarget_test_launch_task(&mut task, &registered.id);
        task.instance.java_path = java_path.clone();
        task.intent.requested_java = java_path;
        task.intent.game_dir = Some(root.join("instance"));
        task.launched_at = "2026-01-01T00:00:00.000Z".to_string();
        let registry = state
            .instances()
            .acquire_mutation()
            .await
            .expect("hold launch metadata registry gate");
        assert!(!state.subscribe_integrity_idle().borrow().is_stably_idle());

        let launch = tokio::spawn(launch_session(state.clone(), task, producer));
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let LaunchEvent::Status(status) =
                    events.recv().await.expect("metadata-gated launch event")
                    && status.state == "running"
                {
                    break;
                }
            }
        })
        .await
        .expect("launch reaches running status");
        tokio::time::timeout(Duration::from_secs(5), async {
            while !state.instance_lifecycle_is_held(&registered.id).await {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("launch metadata holds instance lifecycle");
        assert!(
            !launch.is_finished(),
            "launch must wait for metadata commit"
        );
        assert!(
            !state.subscribe_integrity_idle().borrow().is_stably_idle(),
            "foreground lease must remain live while metadata is blocked"
        );

        drop(registry);
        let launched = tokio::time::timeout(Duration::from_secs(5), launch)
            .await
            .expect("launch metadata settles")
            .expect("launch owner")
            .unwrap_or_else(|error| panic!("metadata-gated launch failed: {}", error.message));
        assert_eq!(launched.session_id, session_id);
        let stored = state
            .instances()
            .get(&registered.id)
            .expect("launch instance remains");
        assert_eq!(stored.last_played_at, "2026-01-01T00:00:00.000Z");
        assert_eq!(
            state.instances().last_instance_id().as_deref(),
            Some(registered.id.as_str())
        );
        assert!(state.subscribe_integrity_idle().borrow().is_stably_idle());
        let _ = state.sessions().kill(session_id).await;
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn out_of_memory_startup_exit_persists_bounded_proof_and_failure_memory() {
        let root = unique_test_dir("launch-out-of-memory-e2e");
        let paths = test_paths(&root);
        let session_id = "launch-out-of-memory-e2e";
        let java_path = write_out_of_memory_launch_fixture(&root);
        assert_scanner_recognizes_fabric_crash_install(&root);
        let instance = test_fabric_crash_instance(&java_path, 1024);
        let state = test_fabric_crash_app_state(&root, &instance);
        let producer = state.try_claim_producer().expect("claim OOM producer");
        let mut task = test_recovery_launch_task(&state, session_id, &root).await;
        retarget_test_launch_task(&mut task, CRASH_E2E_INSTANCE_ID);
        align_fabric_crash_task(&mut task, &java_path);
        task.instance.max_memory_mb = 1024;
        task.intent.max_memory_mb = 1024;
        task.intent.game_dir = Some(state.instances().game_dir(&task.instance.id));
        task.launched_at = "2026-01-01T00:00:00.000Z".to_string();
        let (integrity_foreground, task) = LaunchSessionRunTask::from_prepared(task);
        let mut integrity_foreground = Some(integrity_foreground);
        let mut session = test_record(session_id);
        session.instance_id = CRASH_E2E_INSTANCE_ID.to_string();
        session.version_id = CRASH_E2E_FABRIC_VERSION_ID.to_string();
        state
            .sessions()
            .insert(session)
            .await
            .expect("insert OOM session");
        let mut events = state
            .sessions()
            .subscribe(session_id)
            .await
            .expect("subscribe to OOM session");
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            launch_session_inner(state.clone(), task, &producer, &mut integrity_foreground),
        )
        .await
        .expect("OOM launch deadline");
        let error = match result {
            Ok(_) => panic!("OOM launch must fail"),
            Err(error) => error,
        };
        drop(integrity_foreground);

        assert_eq!(error.message, "Guardian blocked launch startup.");
        assert!(error.message.chars().count() <= 180);
        let guardian = error.guardian.as_ref().expect("OOM Guardian summary");
        assert_eq!(
            guardian.decision(),
            crate::guardian::GuardianSummaryDecision::Blocked
        );
        assert_eq!(guardian.message(), Some(error.message.as_str()));
        assert!(guardian.details().iter().any(|detail| {
            detail == "Minecraft exited before startup completed after running out of memory."
        }));
        assert!(guardian.guidance().iter().any(|detail| {
            detail
                == "Review the instance memory allocation and close memory-heavy apps before retrying."
        }));

        let record = state
            .sessions()
            .get(session_id)
            .await
            .expect("terminal OOM session");
        assert_eq!(record.state, LaunchState::Exited);
        assert_eq!(
            record.failure.as_ref().map(|failure| failure.class),
            Some(LaunchFailureClass::OutOfMemory)
        );
        assert_eq!(
            record
                .failure
                .as_ref()
                .and_then(|failure| failure.detail.as_deref()),
            Some("Guardian blocked launch startup.")
        );
        assert_eq!(
            record.outcome.as_ref().expect("OOM session outcome").reason,
            LaunchSessionExitReason::StartupFailed
        );
        let crash_evidence = record.crash_evidence.as_ref().expect("OOM crash evidence");
        assert_eq!(
            crash_evidence.source,
            axial_launcher::CrashArtifactKind::MinecraftCrashReport
        );
        assert!(crash_evidence.names_out_of_memory);

        let status_payload = super::super::reports::launch_status_payload(&state, session_id)
            .await
            .expect("OOM status payload");
        assert_eq!(status_payload["failure_class"], "out_of_memory");
        assert_eq!(
            status_payload["failure_detail"],
            "Guardian blocked launch startup."
        );
        assert_eq!(status_payload["guardian"]["decision"], "blocked");
        assert_eq!(
            status_payload["crash_evidence"]["exception_class"],
            "java.lang.OutOfMemoryError"
        );

        let proof = state
            .launch_reports()
            .load(session_id)
            .expect("OOM proof persisted");
        assert_eq!(proof.outcome, "failed");
        assert_eq!(proof.failure_class.as_deref(), Some("out_of_memory"));
        assert_eq!(
            proof
                .session_outcome
                .as_ref()
                .expect("OOM proof outcome")
                .reason,
            LaunchSessionExitReason::StartupFailed
        );
        assert_eq!(
            proof.failure_detail.as_deref(),
            Some("Guardian blocked launch startup.")
        );
        assert!(
            proof
                .crash_evidence
                .as_ref()
                .is_some_and(|evidence| evidence.names_out_of_memory)
        );
        let report_payload = super::super::reports::launch_report_payload(&state, session_id)
            .expect("OOM report payload");
        assert_eq!(report_payload["failure_class"], "out_of_memory");

        let memory = state.failure_memory().list();
        assert_eq!(memory.len(), 1);
        assert_out_of_memory_observation(&memory[0], CRASH_E2E_INSTANCE_ID);
        state
            .failure_memory()
            .flush()
            .await
            .expect("flush OOM failure memory");
        let memory_json = fs::read_to_string(failure_memory_path(&paths))
            .expect("read persisted OOM failure memory");
        let persisted = FailureMemorySnapshot::from_json(&memory_json)
            .expect("strict persisted OOM failure memory");
        assert_eq!(persisted.entries.len(), 1);
        assert_out_of_memory_observation(&persisted.entries[0], CRASH_E2E_INSTANCE_ID);
        let preflight = super::super::session::prepare_launch_preflight(
            &state,
            CRASH_E2E_INSTANCE_ID.to_string(),
        )
        .await
        .expect("prepare next preflight after OOM crash");
        assert_eq!(preflight.status, "ready");
        assert!(
            preflight.readiness.launchable,
            "Fabric OOM preflight readiness: {:?}",
            preflight.readiness.reasons
        );
        assert_eq!(
            preflight.guardian.decision(),
            crate::guardian::GuardianSummaryDecision::Warned
        );
        let recent_failure = preflight
            .guardian_facts
            .iter()
            .find(|fact| fact.id.as_str() == "recent_startup_failure")
            .expect("OOM crash memory reaches actual next preflight");
        assert!(
            recent_failure
                .fields
                .iter()
                .any(|field| { field.key == "failure_class" && field.value == "out_of_memory" })
        );
        assert!(
            preflight.guardian.details().iter().any(|detail| {
                detail.contains("out-of-memory crash") && detail.contains("today")
            })
        );
        assert_oom_preflight_guidance(&preflight, recent_failure);
        let low_end_preflight =
            super::super::session::prepare_launch_preflight_with_memory_profile_for_test(
                &state,
                CRASH_E2E_INSTANCE_ID.to_string(),
                super::super::session::LaunchPreflightMemoryProfile {
                    host_total_memory_mb: Some(4096),
                    host_available_memory_mb: Some(3072),
                    host_used_memory_mb: Some(1024),
                    launcher_process_memory_mb: Some(128),
                },
            )
            .await
            .expect("prepare low-end preflight after Fabric OOM crash");
        assert!(low_end_preflight.readiness.launchable);
        assert_eq!(
            low_end_preflight.guardian.decision(),
            crate::guardian::GuardianSummaryDecision::Warned
        );
        assert_eq!(low_end_preflight.memory.max_memory_mb, 1024);
        assert_eq!(
            low_end_preflight
                .resource_budget
                .estimated_remaining_memory_mb,
            Some(3072)
        );
        assert!(!low_end_preflight.resource_budget.memory_pressure);
        let low_end_failure = low_end_preflight
            .guardian_facts
            .iter()
            .find(|fact| fact.id.as_str() == "recent_startup_failure")
            .expect("low-end OOM memory fact");
        assert_eq!(
            guardian_fact_field(low_end_failure, "current_memory_mb"),
            Some("1024")
        );
        assert_eq!(
            guardian_fact_field(low_end_failure, "suggested_memory_mb"),
            Some("2048")
        );
        assert!(
            low_end_preflight.guardian.details().iter().any(|detail| {
                detail.contains("out-of-memory crash") && detail.contains("today")
            })
        );
        assert!(low_end_preflight.guardian.guidance().iter().any(|guidance| {
            guidance
                == "Increase this instance's maximum memory from 1024 MB to 2048 MB before relaunching."
        }));
        assert_eq!(state.sessions().active_session_count().await, 0);
        assert!(
            !state
                .sessions()
                .has_active_instance(CRASH_E2E_INSTANCE_ID)
                .await
        );
        let original_session = state
            .sessions()
            .get(session_id)
            .await
            .expect("original OOM session remains");
        assert_eq!(original_session.state, LaunchState::Exited);
        assert_eq!(original_session.version_id, CRASH_E2E_FABRIC_VERSION_ID);
        let preflight_json = serde_json::to_string(&preflight).expect("OOM preflight json");

        let mut event_payloads = String::new();
        while let Ok(event) = events.try_recv() {
            match event {
                LaunchEvent::Status(status) => event_payloads.push_str(
                    &super::super::reports::public_launch_status_json(&status).to_string(),
                ),
                LaunchEvent::Log(log) => event_payloads.push_str(
                    &serde_json::to_string(&log).expect("serialize public OOM log event"),
                ),
            }
        }
        let status_json = status_payload.to_string();
        let report_json = report_payload.to_string();
        for payload in [
            error.message.as_str(),
            status_json.as_str(),
            report_json.as_str(),
            memory_json.as_str(),
            event_payloads.as_str(),
            preflight_json.as_str(),
        ] {
            assert_no_out_of_memory_sensitive_decoys(payload);
            assert!(!payload.contains(&java_path));
        }

        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn post_boot_out_of_memory_updates_guardian_proof_and_failure_memory() {
        let root = unique_test_dir("launch-post-boot-out-of-memory-e2e");
        let paths = test_paths(&root);
        let state = test_app_state(&root);
        let session_id = "launch-post-boot-out-of-memory-e2e";
        state
            .sessions()
            .insert(test_record(session_id))
            .await
            .expect("insert post-boot OOM session");
        let mut events = state
            .sessions()
            .subscribe(session_id)
            .await
            .expect("subscribe to post-boot OOM session");
        let java_path = write_post_boot_out_of_memory_launch_fixture(&root);
        let producer = state
            .try_claim_producer()
            .expect("claim post-boot OOM producer");
        let mut task = test_recovery_launch_task(&state, session_id, &root).await;
        task.instance.java_path = java_path.clone();
        task.intent.requested_java = java_path;
        task.intent.game_dir = Some(root.join("instance"));
        task.launched_at = "2026-01-01T00:00:00.000Z".to_string();
        let launched = tokio::time::timeout(
            Duration::from_secs(10),
            launch_session(state.clone(), task, producer),
        )
        .await
        .expect("post-boot OOM launch deadline")
        .unwrap_or_else(|error| panic!("launch must reach running before OOM: {}", error.message));
        assert_eq!(launched.session_id, session_id);

        let terminal_status = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match events.recv().await.expect("post-boot OOM event") {
                    LaunchEvent::Status(status)
                        if status.failure_detail.as_deref()
                            == Some("Minecraft stopped after running out of memory.") =>
                    {
                        break status;
                    }
                    _ => {}
                }
            }
        })
        .await
        .expect("post-boot OOM observer deadline");
        assert_eq!(terminal_status.state, "exited");
        assert_eq!(
            terminal_status.failure_class.as_deref(),
            Some("out_of_memory")
        );
        assert_eq!(
            terminal_status
                .outcome
                .as_ref()
                .expect("post-boot OOM outcome")
                .reason,
            LaunchSessionExitReason::CrashedAfterBoot
        );
        assert_eq!(
            terminal_status
                .guardian
                .as_ref()
                .and_then(|guardian| guardian.get("decision")),
            Some(&serde_json::json!("warned"))
        );
        assert!(
            terminal_status
                .crash_evidence
                .as_ref()
                .is_some_and(|evidence| evidence.names_out_of_memory)
        );

        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if state.sessions().retention_hold_count(session_id).await == Some(0) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("post-boot OOM observer settlement");

        let record = state
            .sessions()
            .get(session_id)
            .await
            .expect("post-boot OOM record");
        assert!(record.boot_completed_at_ms.is_some());
        assert_eq!(
            record.failure.as_ref().map(|failure| failure.class),
            Some(LaunchFailureClass::OutOfMemory)
        );
        assert_eq!(
            record
                .outcome
                .as_ref()
                .expect("post-boot record outcome")
                .reason,
            LaunchSessionExitReason::CrashedAfterBoot
        );
        assert_eq!(
            record
                .crash_evidence
                .as_ref()
                .and_then(|evidence| evidence.exception_class.as_ref())
                .map(|exception| exception.as_str()),
            Some("java.lang.OutOfMemoryError")
        );
        assert_eq!(
            state.sessions().retention_hold_count(session_id).await,
            Some(0)
        );

        let proof = state
            .launch_reports()
            .load(session_id)
            .expect("post-boot OOM proof");
        assert_eq!(proof.outcome, "failed");
        assert_eq!(proof.failure_class.as_deref(), Some("out_of_memory"));
        assert_eq!(
            proof
                .session_outcome
                .as_ref()
                .expect("post-boot proof outcome")
                .reason,
            LaunchSessionExitReason::CrashedAfterBoot
        );
        assert_eq!(
            proof.failure_detail.as_deref(),
            Some("Minecraft stopped after running out of memory.")
        );
        assert!(
            proof
                .crash_evidence
                .as_ref()
                .is_some_and(|evidence| evidence.names_out_of_memory)
        );

        let memory = state.failure_memory().list();
        assert_eq!(memory.len(), 1);
        assert_out_of_memory_observation(&memory[0], "instance");
        state
            .failure_memory()
            .flush()
            .await
            .expect("flush post-boot OOM memory");
        let memory_json =
            fs::read_to_string(failure_memory_path(&paths)).expect("read post-boot OOM memory");
        let report_json = super::super::reports::launch_report_payload(&state, session_id)
            .expect("post-boot OOM report payload")
            .to_string();
        let status_json = serde_json::to_string(&terminal_status)
            .expect("serialize post-boot OOM terminal status");
        for payload in [
            memory_json.as_str(),
            report_json.as_str(),
            status_json.as_str(),
        ] {
            assert_no_out_of_memory_sensitive_decoys(payload);
        }

        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn post_boot_mod_crash_settles_guardian_copy_proof_and_failure_memory() {
        let root = unique_test_dir("launch-post-boot-mod-crash-e2e");
        let paths = test_paths(&root);
        let session_id = "launch-post-boot-mod-crash-e2e";
        let java_path = write_post_boot_mod_crash_launch_fixture(&root);
        assert_scanner_recognizes_fabric_crash_install(&root);
        let instance = test_fabric_crash_instance(&java_path, 4096);
        let state = test_fabric_crash_app_state(&root, &instance);
        let producer = state
            .try_claim_producer()
            .expect("claim mod crash producer");
        let mut task = test_recovery_launch_task(&state, session_id, &root).await;
        retarget_test_launch_task(&mut task, CRASH_E2E_INSTANCE_ID);
        align_fabric_crash_task(&mut task, &java_path);
        task.intent.game_dir = Some(state.instances().game_dir(&task.instance.id));
        task.launched_at = "2026-01-01T00:00:00.000Z".to_string();
        let mut session = test_record(session_id);
        session.instance_id = CRASH_E2E_INSTANCE_ID.to_string();
        session.version_id = CRASH_E2E_FABRIC_VERSION_ID.to_string();
        state
            .sessions()
            .insert(session)
            .await
            .expect("insert mod crash session");
        let mut events = state
            .sessions()
            .subscribe(session_id)
            .await
            .expect("subscribe mod crash session");
        let launched = tokio::time::timeout(
            Duration::from_secs(10),
            launch_session(state.clone(), task, producer),
        )
        .await
        .expect("mod crash launch deadline")
        .unwrap_or_else(|error| panic!("launch must reach running: {}", error.message));
        assert_eq!(launched.session_id, session_id);

        let (terminal, event_payloads) = tokio::time::timeout(Duration::from_secs(5), async {
            let mut event_payloads = String::new();
            loop {
                match events.recv().await.expect("mod crash event") {
                    LaunchEvent::Status(status) => {
                        event_payloads.push_str(
                            &super::super::reports::public_launch_status_json(&status).to_string(),
                        );
                        if status.state == "exited" {
                            break (status, event_payloads);
                        }
                    }
                    LaunchEvent::Log(log) => event_payloads.push_str(
                        &serde_json::to_string(&log).expect("serialize public mod crash log event"),
                    ),
                }
            }
        })
        .await
        .expect("mod crash terminal deadline");
        assert_eq!(
            terminal.outcome.as_ref().expect("mod crash outcome").reason,
            LaunchSessionExitReason::CrashedAfterBoot
        );
        let evidence = terminal
            .crash_evidence
            .as_ref()
            .expect("mod crash evidence");
        assert_eq!(evidence.suspected_mods.len(), 1);
        assert_eq!(evidence.suspected_mods[0].name.as_str(), "Example Machines");
        assert!(terminal.guardian.is_some());
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if state.sessions().retention_hold_count(session_id).await == Some(0)
                    && state
                        .launch_reports()
                        .load(session_id)
                        .is_some_and(|proof| proof.outcome == "failed")
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("mod crash proof settlement");

        let proof = state
            .launch_reports()
            .load(session_id)
            .expect("mod crash proof");
        let proof_evidence = proof
            .crash_evidence
            .as_ref()
            .expect("persisted mod crash evidence");
        assert_eq!(proof_evidence.suspected_mods.len(), 1);
        assert_eq!(
            proof_evidence.suspected_mods[0]
                .version
                .as_ref()
                .map(|version| version.as_str()),
            Some("3.2.1")
        );
        assert_eq!(proof.failure_class.as_deref(), Some("mod_attributed_crash"));
        assert!(
            proof
                .failure_detail
                .as_deref()
                .is_some_and(|detail| detail.contains("Example Machines"))
        );
        let status_payload = super::super::reports::launch_status_payload(&state, session_id)
            .await
            .expect("settled mod crash status payload");
        assert_eq!(
            status_payload["crash_evidence"]["suspected_mods"][0]["name"],
            "Example Machines"
        );
        assert_eq!(status_payload["failure_class"], "mod_attributed_crash");
        assert!(
            status_payload["failure_detail"]
                .as_str()
                .is_some_and(|detail| detail.contains("Example Machines"))
        );
        let proof_json = serde_json::to_string(&proof).expect("serialize mod crash proof");
        let memory = state.failure_memory().list();
        assert_eq!(memory.len(), 1);
        assert_eq!(memory[0].diagnosis_id.as_str(), "mod_attributed_crash");
        assert_eq!(memory[0].target.id, CRASH_E2E_INSTANCE_ID);
        assert_eq!(memory[0].occurrence_count, 1);
        state
            .failure_memory()
            .flush()
            .await
            .expect("flush mod crash failure memory");
        let memory_json = fs::read_to_string(failure_memory_path(&paths))
            .expect("read persisted mod crash failure memory");
        let persisted = FailureMemorySnapshot::from_json(&memory_json)
            .expect("strict persisted mod crash failure memory");
        assert_eq!(persisted.entries, memory);
        let preflight = super::super::session::prepare_launch_preflight(
            &state,
            CRASH_E2E_INSTANCE_ID.to_string(),
        )
        .await
        .expect("prepare next preflight after mod crash");
        assert_eq!(preflight.status, "ready");
        assert!(preflight.readiness.launchable);
        assert_eq!(
            preflight.guardian.decision(),
            crate::guardian::GuardianSummaryDecision::Warned
        );
        let recent_failure = preflight
            .guardian_facts
            .iter()
            .find(|fact| fact.id.as_str() == "recent_startup_failure")
            .expect("mod crash memory reaches actual next preflight");
        assert!(recent_failure.fields.iter().any(|field| {
            field.key == "failure_class" && field.value == "mod_attributed_crash"
        }));
        assert!(
            preflight.guardian.details().iter().any(|detail| {
                detail.contains("mod-attributed crash") && detail.contains("today")
            })
        );
        assert!(preflight.guardian.guidance().iter().any(|guidance| {
            guidance
                == "Review recently changed mods and disable the suspected mod before relaunching."
        }));
        let normal_preflight =
            super::super::session::prepare_launch_preflight_with_memory_profile_for_test(
                &state,
                CRASH_E2E_INSTANCE_ID.to_string(),
                super::super::session::LaunchPreflightMemoryProfile {
                    host_total_memory_mb: Some(16_384),
                    host_available_memory_mb: Some(12_288),
                    host_used_memory_mb: Some(4096),
                    launcher_process_memory_mb: Some(256),
                },
            )
            .await
            .expect("prepare normal-host preflight after Fabric mod crash");
        assert!(normal_preflight.readiness.launchable);
        assert_eq!(
            normal_preflight.guardian.decision(),
            crate::guardian::GuardianSummaryDecision::Warned
        );
        assert_eq!(normal_preflight.memory.max_memory_mb, 4096);
        assert_eq!(
            normal_preflight
                .resource_budget
                .estimated_remaining_memory_mb,
            Some(12_288)
        );
        assert!(!normal_preflight.resource_budget.memory_pressure);
        let normal_failure = normal_preflight
            .guardian_facts
            .iter()
            .find(|fact| fact.id.as_str() == "recent_startup_failure")
            .expect("normal-host mod crash memory fact");
        assert_eq!(
            guardian_fact_field(normal_failure, "failure_class"),
            Some("mod_attributed_crash")
        );
        assert!(
            normal_preflight.guardian.details().iter().any(|detail| {
                detail.contains("mod-attributed crash") && detail.contains("today")
            })
        );
        assert!(normal_preflight.guardian.guidance().iter().any(|guidance| {
            guidance
                == "Review recently changed mods and disable the suspected mod before relaunching."
        }));
        assert_eq!(state.sessions().active_session_count().await, 0);
        assert!(
            !state
                .sessions()
                .has_active_instance(CRASH_E2E_INSTANCE_ID)
                .await
        );
        let original_session = state
            .sessions()
            .get(session_id)
            .await
            .expect("original mod crash session remains");
        assert_eq!(original_session.state, LaunchState::Exited);
        assert_eq!(original_session.version_id, CRASH_E2E_FABRIC_VERSION_ID);
        let preflight_json = serde_json::to_string(&preflight).expect("mod preflight json");
        let status_json = status_payload.to_string();
        for payload in [
            status_json.as_str(),
            proof_json.as_str(),
            memory_json.as_str(),
            event_payloads.as_str(),
            preflight_json.as_str(),
        ] {
            for private in ["/home/alice", "raw-secret-token", "-Duser.home"] {
                assert!(!payload.contains(private));
            }
            assert!(!payload.contains(&java_path));
        }

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn cancelled_launch_handoff_releases_terminal_observer_hold() {
        let root = unique_test_dir("cancelled-terminal-observer-handoff");
        let state = test_app_state(&root);
        let session_id = "cancelled-terminal-observer-handoff";
        state
            .sessions()
            .insert(test_record(session_id))
            .await
            .expect("insert observer handoff session");
        let events = state
            .sessions()
            .subscribe(session_id)
            .await
            .expect("subscribe observer handoff");
        let (handoff_tx, handoff_rx) = tokio::sync::oneshot::channel();
        drop(handoff_tx);
        let producer = state
            .try_claim_producer()
            .expect("claim observer task producer");
        let task = test_recovery_launch_task(&state, session_id, &root).await;
        let proof_context = LaunchProofContext::from_intent(&task.intent);
        drop(task);
        drop(producer);

        own_terminal_observation(
            state.clone(),
            session_id.to_string(),
            "instance".to_string(),
            GuardianMode::Managed,
            "2026-01-01T00:00:00.000Z".to_string(),
            proof_context,
            events,
            handoff_rx,
        )
        .await;

        assert_eq!(
            state.sessions().retention_hold_count(session_id).await,
            Some(0)
        );
        assert!(state.failure_memory().list().is_empty());
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn terminal_observer_ignores_queued_terminal_event_after_retry_started() {
        let root = unique_test_dir("stale-terminal-observer-event");
        let state = test_app_state(&root);
        let session_id = "stale-terminal-observer-event";
        state
            .sessions()
            .insert(test_record(session_id))
            .await
            .expect("insert stale-event session");
        let events = state
            .sessions()
            .subscribe(session_id)
            .await
            .expect("subscribe stale-event observer");
        let (handoff_tx, handoff_rx) = tokio::sync::oneshot::channel();
        let producer = state
            .try_claim_producer()
            .expect("claim stale-event producer");
        let task = test_recovery_launch_task(&state, session_id, &root).await;
        let proof_context = LaunchProofContext::from_intent(&task.intent);
        drop(task);
        drop(producer);
        let observer_state = state.clone();
        let mut observer = tokio::spawn(own_terminal_observation(
            observer_state,
            session_id.to_string(),
            "instance".to_string(),
            GuardianMode::Managed,
            "2026-01-01T00:00:00.000Z".to_string(),
            proof_context,
            events,
            handoff_rx,
        ));

        emit_status(
            &state,
            session_id,
            LaunchState::Exited,
            None,
            None,
            None,
            None,
        )
        .await;
        emit_status(
            &state,
            session_id,
            LaunchState::Preparing,
            None,
            None,
            None,
            None,
        )
        .await;
        assert!(
            handoff_tx
                .send(TerminalObservationHandoff::Observe {
                    guardian: empty_guardian_summary(axial_launcher::GuardianMode::Managed),
                })
                .is_ok(),
            "handoff retry observer"
        );

        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut observer)
                .await
                .is_err(),
            "queued attempt-1 terminal event must not settle the retry session"
        );

        emit_status(
            &state,
            session_id,
            LaunchState::Exited,
            None,
            None,
            None,
            None,
        )
        .await;
        tokio::time::timeout(Duration::from_secs(2), observer)
            .await
            .expect("fresh terminal observation deadline")
            .expect("fresh terminal observer task");

        assert_eq!(
            state
                .sessions()
                .get(session_id)
                .await
                .expect("fresh terminal session")
                .state,
            LaunchState::Exited
        );
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn accepted_startup_failure_settles_copy_memory_proof_and_hold() {
        let root = unique_test_dir("accepted-startup-failure-terminal-observer");
        let state = test_app_state(&root);
        let session_id = "accepted-startup-failure-terminal-observer";
        state
            .sessions()
            .insert(test_record(session_id))
            .await
            .expect("insert observed startup failure session");
        let events = state
            .sessions()
            .subscribe(session_id)
            .await
            .expect("subscribe observed startup failure");
        let (handoff_tx, handoff_rx) = tokio::sync::oneshot::channel();
        let producer = state
            .try_claim_producer()
            .expect("claim observed failure task producer");
        let task = test_recovery_launch_task(&state, session_id, &root).await;
        let proof_context = LaunchProofContext::from_intent(&task.intent);
        drop(task);
        drop(producer);
        let observer_state = state.clone();
        let observer = tokio::spawn(own_terminal_observation(
            observer_state,
            session_id.to_string(),
            "instance".to_string(),
            GuardianMode::Managed,
            "2026-01-01T00:00:00.000Z".to_string(),
            proof_context,
            events,
            handoff_rx,
        ));
        assert!(
            handoff_tx
                .send(TerminalObservationHandoff::Observe {
                    guardian: empty_guardian_summary(axial_launcher::GuardianMode::Managed),
                })
                .is_ok(),
            "handoff observed startup failure"
        );
        let crash_evidence: CrashEvidence = serde_json::from_value(serde_json::json!({
            "source": "minecraft_crash_report",
            "truncated": false,
            "exception_class": "net.minecraftforge.fml.common.MissingModsException",
            "suspected_mods": [],
            "names_out_of_memory": false
        }))
        .expect("typed missing dependency evidence");
        state
            .sessions()
            .emit_status(
                session_id,
                LaunchStatusEvent {
                    state: "exited".to_string(),
                    benchmark: None,
                    pid: None,
                    exit_code: Some(1),
                    failure_class: Some(LaunchFailureClass::MissingDependency.as_str().to_string()),
                    failure_detail: Some("Minecraft failed during startup.".to_string()),
                    crash_evidence: Some(crash_evidence),
                    healing: None,
                    guardian: None,
                    outcome: Some(LaunchSessionOutcome::from_reason(
                        LaunchSessionExitReason::StartupFailed,
                    )),
                    notice: None,
                    evidence: Vec::new(),
                    stages: Vec::new(),
                },
            )
            .await;

        tokio::time::timeout(Duration::from_secs(2), observer)
            .await
            .expect("observed startup failure settlement deadline")
            .expect("observed startup failure task");

        let record = state
            .sessions()
            .get(session_id)
            .await
            .expect("settled startup failure session");
        assert_eq!(record.state, LaunchState::Failed);
        assert!(record.boot_completed_at_ms.is_none());
        assert_eq!(
            record.failure.as_ref().map(|failure| failure.class),
            Some(LaunchFailureClass::MissingDependency)
        );
        let guardian = record.guardian.as_ref().expect("startup failure guardian");
        assert_eq!(guardian["decision"], "blocked");
        assert_eq!(guardian["message"], "Guardian blocked launch startup.");
        let guardian_details = guardian["details"].as_array().expect("Guardian details");
        let guardian_guidance = guardian["guidance"].as_array().expect("Guardian guidance");
        assert!(!guardian_guidance.is_empty());
        for guidance in guardian_guidance {
            assert!(guardian_details.contains(guidance));
        }
        assert_eq!(
            record
                .failure
                .as_ref()
                .and_then(|failure| failure.detail.as_deref()),
            Some("Guardian blocked launch startup.")
        );
        let memory = state.failure_memory().list();
        assert_eq!(memory.len(), 1);
        assert_eq!(memory[0].diagnosis_id.as_str(), "missing_dependency");
        assert_eq!(memory[0].target.id, "instance");
        assert_eq!(memory[0].occurrence_count, 1);
        let proof = state
            .launch_reports()
            .load(session_id)
            .expect("observed startup failure proof");
        assert_eq!(proof.outcome, "failed");
        assert_eq!(proof.failure_class.as_deref(), Some("missing_dependency"));
        assert_eq!(
            proof.failure_detail.as_deref(),
            Some("Guardian blocked launch startup.")
        );
        assert_eq!(
            state.sessions().retention_hold_count(session_id).await,
            Some(0)
        );

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn modern_plan_skips_repair_stage_full_asset_index_parse() {
        let root = unique_test_dir("modern-asset-index-guard");
        write_runner_asset_index(&root, "modern", r#"{"objects":"not-an-object-map"}"#);
        let plan = test_asset_launch_plan("modern", false);

        let outcome = repair_legacy_virtual_assets_before_launch(&root, &plan)
            .await
            .expect("modern asset repair guard");

        assert_eq!(outcome, LegacyVirtualAssetRepairOutcome::SkippedModern);
        assert_eq!(outcome.full_object_parse_attempts(), 0);
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn legacy_plan_invokes_repair_stage_full_asset_index_parser() {
        let root = unique_test_dir("legacy-asset-index-guard");
        write_runner_asset_index(
            &root,
            "legacy",
            r#"{"objects":"not-an-object-map","map_to_resources":true}"#,
        );
        let plan = test_asset_launch_plan("legacy", true);

        assert!(matches!(
            repair_legacy_virtual_assets_before_launch(&root, &plan).await,
            Err(axial_minecraft::download::DownloadError::ParseVersion(_))
        ));
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn rejected_launch_recovery_plan_finishes_terminal_and_persists_proof() {
        let root = unique_test_dir("rejected-launch-recovery-plan");
        let state = test_app_state(&root);
        let session_id = "rejected-launch-recovery-plan";
        state
            .sessions()
            .insert(test_record(session_id))
            .await
            .expect("insert session");
        let mut guardian = empty_guardian_summary(axial_launcher::GuardianMode::Managed);
        let user_outcome = guardian_user_outcome_for_test(
            GuardianActionKind::Block,
            OperationPhase::Preparing,
            "Guardian blocked launch recovery planning.",
            &["The recovery directive could not be planned safely."],
            &[],
        );

        guardian = guardian_summary_with_blocked_outcome(&guardian, &user_outcome);
        let mut launch_completion_pending = true;
        let error = finish_launch_failure(
            &state,
            &state.try_claim_producer().expect("claim failure producer"),
            session_id,
            &mut launch_completion_pending,
            LaunchFailure {
                proof_context: None,
                class: LaunchFailureClass::Unknown,
                message: user_outcome.summary(),
                healing: None,
                guardian: Some(guardian.clone()),
                outcome: None,
            },
        )
        .await;

        assert_eq!(error.message, user_outcome.summary());
        assert_eq!(
            guardian.decision(),
            crate::guardian::GuardianSummaryDecision::Blocked
        );
        let record = state
            .sessions()
            .get(session_id)
            .await
            .expect("terminal session");
        assert_eq!(record.state, LaunchState::Exited);
        let proof = state
            .launch_reports()
            .load(session_id)
            .expect("proof persisted");
        assert_eq!(proof.session_id, session_id);

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn nonretryable_journal_error_cannot_leave_a_queued_session_orphaned() {
        let root = unique_test_dir("nonretryable-launch-journal-error");
        let state = test_app_state(&root);
        let session_id = "nonretryable-launch-journal-error";
        state
            .sessions()
            .insert(test_record(session_id))
            .await
            .expect("insert session");
        let error = guardian_journal_error(OperationJournalStoreError::MissingOperation);

        let producer = state.try_claim_producer().expect("claim terminal producer");
        let mut integrity_foreground = Some(
            state
                .register_integrity_foreground()
                .expect("register terminal foreground")
                .wait_for_settlement()
                .await,
        );
        let result = match terminalize_unhandled_launch_error(
            &state,
            &producer,
            session_id,
            Err(error),
            &mut integrity_foreground,
        )
        .await
        {
            LaunchTerminalizationDisposition::Complete(result)
            | LaunchTerminalizationDisposition::Retained(result)
            | LaunchTerminalizationDisposition::Settled(result) => result,
        };

        assert!(result.is_err());
        assert!(integrity_foreground.is_some());
        let record = state
            .sessions()
            .get(session_id)
            .await
            .expect("terminal session");
        assert_eq!(record.state, LaunchState::Failed);
        assert_eq!(
            state.sessions().retention_hold_count(session_id).await,
            Some(0)
        );
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn spawned_process_journal_error_settles_as_guardian_failure() {
        let root = unique_test_dir("spawned-launch-journal-error");
        let state = test_app_state(&root);
        let session_id = "spawned-launch-journal-error";
        state
            .sessions()
            .insert(test_record(session_id))
            .await
            .expect("insert session");
        let mut command = Command::new("sh");
        command.arg("-c").arg("sleep 30");
        state
            .sessions()
            .start_process(test_record(session_id), command)
            .await
            .expect("spawn test process");
        let error = guardian_journal_error(OperationJournalStoreError::MissingOperation);
        let expected_message = error.message.clone();

        let producer = state.try_claim_producer().expect("claim terminal producer");
        let mut integrity_foreground = Some(
            state
                .register_integrity_foreground()
                .expect("register terminal foreground")
                .wait_for_settlement()
                .await,
        );
        let result = match terminalize_unhandled_launch_error(
            &state,
            &producer,
            session_id,
            Err(error),
            &mut integrity_foreground,
        )
        .await
        {
            LaunchTerminalizationDisposition::Complete(result)
            | LaunchTerminalizationDisposition::Retained(result)
            | LaunchTerminalizationDisposition::Settled(result) => result,
        };

        assert!(result.is_err());
        assert!(integrity_foreground.is_some());
        let record = state
            .sessions()
            .get(session_id)
            .await
            .expect("terminal session");
        assert_eq!(record.state, LaunchState::Failed);
        assert_eq!(
            record
                .failure
                .as_ref()
                .and_then(|failure| failure.detail.as_deref()),
            Some(expected_message.as_str())
        );
        assert_ne!(
            record.outcome.expect("failure outcome").reason,
            LaunchSessionExitReason::LauncherStopped
        );
        assert_eq!(
            state.sessions().retention_hold_count(session_id).await,
            Some(0)
        );
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rejected_process_termination_retains_active_launch_until_confirmed_settlement() {
        let root = unique_test_dir("rejected-launch-failure-termination");
        let state = test_app_state_with_library(&root);
        let instance = state
            .instances()
            .insert_for_test("Termination rejection".to_string(), "1.21.1".to_string())
            .expect("add test instance");
        let session_id = "rejected-launch-failure-termination";
        let process_release = root.join("release-process");
        let mut record = test_record(session_id);
        record.instance_id = instance.id.clone();
        state
            .sessions()
            .insert(record.clone())
            .await
            .expect("insert session");
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg("while [ ! -f \"$1\" ]; do sleep 0.01; done")
            .arg("rejected-launch-failure-termination")
            .arg(&process_release);
        state
            .sessions()
            .start_process(record, command)
            .await
            .expect("spawn test process");
        assert!(
            state
                .sessions()
                .reject_next_process_start_kill(session_id)
                .await
        );
        let error = guardian_journal_error(OperationJournalStoreError::MissingOperation);
        let expected_message = error.message.clone();

        let producer = state.try_claim_producer().expect("claim terminal producer");
        let mut integrity_foreground = Some(
            state
                .register_integrity_foreground()
                .expect("register terminal foreground")
                .wait_for_settlement()
                .await,
        );
        let result = match terminalize_unhandled_launch_error(
            &state,
            &producer,
            session_id,
            Err(error),
            &mut integrity_foreground,
        )
        .await
        {
            LaunchTerminalizationDisposition::Complete(result)
            | LaunchTerminalizationDisposition::Retained(result)
            | LaunchTerminalizationDisposition::Settled(result) => result,
        };

        let returned_error = match result {
            Ok(_) => panic!("journal error must remain public"),
            Err(error) => error,
        };
        assert_eq!(returned_error.message, expected_message);
        assert!(integrity_foreground.is_none());
        assert!(
            !state.subscribe_integrity_idle().borrow().is_stably_idle(),
            "pending terminalization must retain foreground ownership"
        );
        let active = state
            .sessions()
            .get(session_id)
            .await
            .expect("active session");
        assert!(!matches!(
            active.state,
            LaunchState::Failed | LaunchState::Exited
        ));
        assert!(active.pid.is_some());
        assert!(state.sessions().has_active_instance(&instance.id).await);
        assert_eq!(
            state.sessions().retention_hold_count(session_id).await,
            Some(1)
        );

        let conflict = match super::super::prepare_launch_session(
            &state,
            super::super::LaunchRequest {
                instance_id: instance.id.clone(),
                username: None,
                max_memory_mb: None,
                min_memory_mb: None,
                client_started_at_ms: None,
            },
        )
        .await
        {
            Ok(_) => panic!("a live retained child must block a second launch"),
            Err(error) => error,
        };
        assert_eq!(conflict.0, axum::http::StatusCode::CONFLICT);
        assert_eq!(
            conflict.1.0["error"],
            "instance already has an active session"
        );

        fs::write(&process_release, b"release").expect("release process naturally");
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let settled = state
                    .sessions()
                    .get(session_id)
                    .await
                    .expect("settled session");
                if settled.state == LaunchState::Failed
                    && state.sessions().retention_hold_count(session_id).await == Some(0)
                    && state.subscribe_integrity_idle().borrow().is_stably_idle()
                {
                    assert_eq!(
                        settled
                            .failure
                            .as_ref()
                            .and_then(|failure| failure.detail.as_deref()),
                        Some(expected_message.as_str())
                    );
                    assert_ne!(
                        settled.outcome.expect("failure outcome").reason,
                        LaunchSessionExitReason::LauncherStopped
                    );
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("deferred launch failure settlement");
        assert!(!state.sessions().has_active_instance(&instance.id).await);
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn unconfirmed_launch_failure_retains_foreground_until_exact_session_terminal() {
        let root = unique_test_dir("unconfirmed-launch-failure-foreground");
        let state = test_app_state(&root);
        let session_id = "unconfirmed-launch-failure-foreground";
        let mut record = test_record(session_id);
        record.pid = Some(42);
        state
            .sessions()
            .insert(record)
            .await
            .expect("insert unconfirmed session");
        let error = guardian_journal_error(OperationJournalStoreError::MissingOperation);
        let producer = state.try_claim_producer().expect("claim terminal producer");
        let mut integrity_foreground = Some(
            state
                .register_integrity_foreground()
                .expect("register terminal foreground")
                .wait_for_settlement()
                .await,
        );

        let disposition = terminalize_unhandled_launch_error(
            &state,
            &producer,
            session_id,
            Err(error),
            &mut integrity_foreground,
        )
        .await;

        assert!(matches!(
            disposition,
            LaunchTerminalizationDisposition::Retained(Err(_))
        ));
        assert!(integrity_foreground.is_none());
        assert!(
            !state.subscribe_integrity_idle().borrow().is_stably_idle(),
            "unconfirmed terminalization must retain foreground ownership"
        );

        emit_status(
            &state,
            session_id,
            LaunchState::Failed,
            None,
            None,
            None,
            None,
        )
        .await;
        tokio::time::timeout(Duration::from_secs(1), async {
            while !state.subscribe_integrity_idle().borrow().is_stably_idle() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("unconfirmed foreground release after exact terminal state");
        assert_eq!(
            state
                .sessions()
                .get(session_id)
                .await
                .expect("terminal session")
                .state,
            LaunchState::Failed
        );
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn unconfirmed_foreground_owner_drains_after_shutdown_session_settlement() {
        let root = unique_test_dir("unconfirmed-launch-failure-shutdown");
        let state = test_app_state(&root);
        let session_id = "unconfirmed-launch-failure-shutdown";
        let mut record = test_record(session_id);
        record.pid = Some(42);
        state
            .sessions()
            .insert(record)
            .await
            .expect("insert unconfirmed session");
        let producer = state.try_claim_producer().expect("claim terminal producer");
        let mut integrity_foreground = Some(
            state
                .register_integrity_foreground()
                .expect("register terminal foreground")
                .wait_for_settlement()
                .await,
        );

        let disposition = terminalize_unhandled_launch_error(
            &state,
            &producer,
            session_id,
            Err(guardian_journal_error(
                OperationJournalStoreError::MissingOperation,
            )),
            &mut integrity_foreground,
        )
        .await;

        assert!(matches!(
            disposition,
            LaunchTerminalizationDisposition::Retained(Err(_))
        ));
        assert!(integrity_foreground.is_none());
        drop(producer);
        tokio::time::timeout(Duration::from_secs(2), state.shutdown())
            .await
            .expect("shutdown must not deadlock on retained foreground")
            .expect("shutdown after exact session settlement");
        assert!(state.sessions().get(session_id).await.is_none());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn launch_started_emits_once_while_completion_is_pending() {
        let root = unique_test_dir("launch-started-once");
        let state = test_app_state_with_telemetry(&root);
        let mut pending = false;

        emit_launch_started(&state, &mut pending, Some("fabric".to_string()));
        assert!(pending);

        let queued = state.telemetry().queued_batch_for_test();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0]["event"], "launch_started");
        assert_eq!(queued[0]["properties"]["loader_key"], "fabric");

        emit_launch_started(&state, &mut pending, Some("neoforge".to_string()));
        let queued = state.telemetry().queued_batch_for_test();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0]["properties"]["loader_key"], "fabric");

        emit_launch_completed(&state, &mut pending, TelemetryLaunchOutcome::Success);
        assert!(!pending);
        assert_eq!(state.telemetry().queue_len_for_test(), 2);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn pending_launch_failure_emits_once_only_when_completion_is_pending() {
        let root = unique_test_dir("pending-launch-failure");
        let state = test_app_state_with_telemetry(&root);
        let mut pending = false;

        emit_pending_launch_failure(&state, &mut pending);
        assert_eq!(state.telemetry().queue_len_for_test(), 0);

        pending = true;
        emit_pending_launch_failure(&state, &mut pending);
        assert!(!pending);

        let queued = state.telemetry().queued_batch_for_test();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0]["event"], "launch_completed");
        assert_eq!(queued[0]["properties"]["outcome"], "failure");

        emit_pending_launch_failure(&state, &mut pending);
        assert_eq!(state.telemetry().queue_len_for_test(), 1);

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn runner_records_redacted_command_stage_evidence_into_status() {
        let root = unique_test_dir("runner-stage-evidence");
        let state = test_app_state(&root);
        let session_id = "runner-stage-evidence";
        state
            .sessions()
            .insert(test_record(session_id))
            .await
            .expect("insert session");
        let mut events = state
            .sessions()
            .subscribe(session_id)
            .await
            .expect("subscribe");

        emit_status(
            &state,
            session_id,
            LaunchState::Preparing,
            None,
            None,
            None,
            None,
        )
        .await;
        let _ = tokio::time::timeout(Duration::from_secs(1), events.recv())
            .await
            .expect("preparing status")
            .expect("preparing status event");

        let command = vec![
            r"C:\Users\Alice\.jdks\java.exe".to_string(),
            "-cp".to_string(),
            "libraries".to_string(),
        ];
        let prepared = prepare_launch_command(LaunchCommandPreparationRequest::new(
            launch_command_target(session_id),
            &command,
            &root,
        ))
        .expect("prepared command");
        state
            .sessions()
            .record_stage_evidence(session_id, launch_command_stage_evidence(&prepared.facts))
            .await;

        emit_status(
            &state,
            session_id,
            LaunchState::Prewarming,
            None,
            None,
            None,
            None,
        )
        .await;
        let status_event = tokio::time::timeout(Duration::from_secs(1), events.recv())
            .await
            .expect("prewarming status")
            .expect("prewarming status event");
        let LaunchEvent::Status(status) = status_event else {
            panic!("expected status event");
        };
        let preparing_stage = status
            .stages
            .iter()
            .find(|stage| stage.stage == "preparing")
            .expect("preparing stage");
        assert!(
            preparing_stage
                .evidence
                .iter()
                .any(|evidence| evidence.id == "execution_launch_command_prepared")
        );
        let status_json = serde_json::to_string(&status).expect("status json");
        assert_no_sensitive_stage_evidence(&status_json);

        let _ = fs::remove_dir_all(root);
    }

    fn test_recovery_launch_instance() -> Instance {
        Instance {
            id: "instance".to_string(),
            name: "Recovery cap".to_string(),
            version_id: "1.21.1".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            last_played_at: String::new(),
            art_seed: 0,
            max_memory_mb: 4096,
            min_memory_mb: 1024,
            java_path: String::new(),
            window_width: 0,
            window_height: 0,
            jvm_preset: String::new(),
            performance_mode: "managed".to_string(),
            extra_jvm_args: String::new(),
            auto_optimize: false,
            icon: String::new(),
            accent: String::new(),
        }
    }

    async fn test_recovery_launch_task(
        state: &AppState,
        session_id: &str,
        root: &Path,
    ) -> super::super::session::LaunchSessionTask {
        let integrity_foreground = state
            .register_integrity_foreground()
            .expect("register test launch foreground")
            .wait_for_settlement()
            .await;
        super::super::session::LaunchSessionTask {
            integrity_foreground,
            application: crate::application::stage_launch_instance_command(
                crate::application::LaunchInstanceCommand {
                    instance_id: "instance".to_string(),
                    username: None,
                    max_memory_mb: None,
                    min_memory_mb: None,
                    client_started_at_ms: None,
                },
                Some(session_id.to_string()),
            ),
            preflight_stage_evidence: crate::application::launch_preflight_stage_evidence(
                &crate::guardian::guardian_preflight_outcome(
                    crate::guardian::GuardianPreflightOutcomeRequest::new(
                        crate::guardian::GuardianMode::Managed,
                        &[],
                    ),
                ),
                "managed",
            ),
            instance: test_recovery_launch_instance(),
            intent: LaunchIntent {
                session_id: session_id.to_string(),
                library_dir: root.join("library"),
                instance_id: "instance".to_string(),
                version_id: "1.21.1".to_string(),
                target_version_id: "1.21.1".to_string(),
                loader: "vanilla".to_string(),
                is_modded: false,
                username: "Player".to_string(),
                auth: LaunchAuthContext::offline("Player"),
                requested_java: "configured-java".to_string(),
                requested_preset: String::new(),
                extra_jvm_args: Vec::new(),
                max_memory_mb: 4096,
                min_memory_mb: 1024,
                resolution: None,
                launcher_name: "axial".to_string(),
                launcher_version: "test".to_string(),
                game_dir: None,
                guardian: LaunchGuardianContext {
                    mode: axial_launcher::GuardianMode::Managed,
                    java_override_origin: Some(OverrideOrigin::Instance),
                    preset_override_origin: None,
                    raw_jvm_args_origin: None,
                },
                performance_mode: "managed".to_string(),
            },
            guardian: empty_guardian_summary(axial_launcher::GuardianMode::Managed),
            launched_at: "2026-01-01T00:00:00Z".to_string(),
            benchmark: None,
            resource_budget: None,
            java_probe_receipt: None,
        }
    }

    fn retarget_test_launch_task(
        task: &mut super::super::session::LaunchSessionTask,
        instance_id: &str,
    ) {
        task.instance.id = instance_id.to_string();
        task.intent.instance_id = instance_id.to_string();
        task.application = crate::application::stage_launch_instance_command(
            crate::application::LaunchInstanceCommand {
                instance_id: instance_id.to_string(),
                username: None,
                max_memory_mb: None,
                min_memory_mb: None,
                client_started_at_ms: None,
            },
            Some(task.intent.session_id.clone()),
        );
    }

    #[cfg(unix)]
    fn test_fabric_crash_instance(java_path: &str, max_memory_mb: i32) -> Instance {
        let mut instance = test_recovery_launch_instance();
        instance.id = CRASH_E2E_INSTANCE_ID.to_string();
        instance.version_id = CRASH_E2E_FABRIC_VERSION_ID.to_string();
        instance.java_path = java_path.to_string();
        instance.max_memory_mb = max_memory_mb;
        instance
    }

    #[cfg(unix)]
    fn align_fabric_crash_task(
        task: &mut super::super::session::LaunchSessionTask,
        java_path: &str,
    ) {
        task.instance.version_id = CRASH_E2E_FABRIC_VERSION_ID.to_string();
        task.instance.java_path = java_path.to_string();
        task.intent.version_id = CRASH_E2E_FABRIC_VERSION_ID.to_string();
        task.intent.target_version_id = "1.21.1".to_string();
        task.intent.loader = "fabric".to_string();
        task.intent.is_modded = true;
        task.intent.requested_java = java_path.to_string();
    }

    #[cfg(unix)]
    fn assert_scanner_recognizes_fabric_crash_install(root: &Path) {
        let version_report = axial_minecraft::scan_versions_report(&root.join("library"))
            .expect("scan Fabric crash fixture");
        assert_eq!(
            version_report.state,
            axial_minecraft::VersionScanState::Ready
        );
        let fabric_version = version_report
            .versions
            .iter()
            .find(|version| version.id == CRASH_E2E_FABRIC_VERSION_ID)
            .expect("scanner-recognized Fabric crash fixture");
        assert!(fabric_version.installed);
        assert!(fabric_version.launchable);
        assert_eq!(fabric_version.inherits_from, "1.21.1");
        let fabric_loader = fabric_version
            .loader
            .as_ref()
            .expect("authoritative Fabric loader metadata");
        assert_eq!(
            fabric_loader.component_id,
            axial_minecraft::LoaderComponentId::Fabric
        );
        assert_eq!(
            fabric_loader.build_id,
            axial_minecraft::build_id_for(
                axial_minecraft::LoaderComponentId::Fabric,
                "1.21.1",
                "0.16.10"
            )
        );
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

    fn test_libraries_recovery_app_state(
        root: &Path,
        instance_id: &str,
        repair_source: &str,
    ) -> (AppState, Arc<KnownGoodInventory>) {
        let state = test_registered_recovery_app_state(root, instance_id, "Libraries recovery");
        let library_dir = PathBuf::from(
            state
                .library_dir()
                .expect("Libraries recovery managed root"),
        );
        fs::create_dir_all(
            library_dir
                .join(MANAGED_LIBRARY_FIXTURE_PATH)
                .parent()
                .expect("Libraries recovery managed library parent"),
        )
        .expect("Libraries recovery managed library directory");
        let active_inventory = state.activate_known_good_inventory_for_test_with_identity(
            instance_id,
            KnownGoodInventory::from_test_entries([TestKnownGoodEntry {
                root: TestKnownGoodRoot::Libraries,
                path: MANAGED_LIBRARY_FIXTURE_PATH
                    .strip_prefix("libraries/")
                    .expect("Libraries fixture inventory-relative path")
                    .to_string(),
                kind: KnownGoodArtifactKind::Library,
                integrity: TestKnownGoodIntegrity::Sha1 {
                    digest: format!("{:x}", Sha1::digest(MANAGED_LIBRARY_FIXTURE_BYTES)),
                    size: MANAGED_LIBRARY_FIXTURE_BYTES.len() as u64,
                },
            }])
            .expect("Libraries recovery known-good inventory")
            .with_test_standalone_leaf_repair_source(0, repair_source)
            .expect("Libraries recovery standalone leaf source"),
        );
        (state, active_inventory)
    }

    fn test_registered_recovery_app_state(
        root: &Path,
        instance_id: &str,
        instance_name: &str,
    ) -> AppState {
        let paths = test_paths(root);
        let library_dir = root.join("library");
        fs::create_dir_all(paths.instances_dir.join(instance_id))
            .expect("registered recovery instance directory");
        fs::create_dir_all(&library_dir).expect("registered recovery managed root");
        let config = Arc::new(
            ConfigStore::from_config(
                paths.clone(),
                AppConfig {
                    library_dir: library_dir.to_string_lossy().into_owned(),
                    ..AppConfig::default()
                },
            )
            .expect("configure registered recovery root"),
        );
        let mut instance = test_recovery_launch_instance();
        instance.id = instance_id.to_string();
        instance.name = instance_name.to_string();
        let instances = Arc::new(
            InstanceStore::from_snapshot(
                paths.clone(),
                InstanceRegistrySnapshot::new(vec![instance], instance_id.to_string(), Vec::new())
                    .expect("registered recovery registry snapshot"),
            )
            .expect("load registered recovery instances"),
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
                    .expect("registered recovery performance manager"),
            ),
            startup_warnings: Vec::new(),
            frontend_dir: root.join("frontend"),
        })
    }

    #[cfg(unix)]
    fn test_version_bundle_recovery_app_state(
        root: &Path,
        instance_id: &str,
    ) -> (AppState, PathBuf, Vec<u8>) {
        const CLIENT_BYTES: &[u8] = b"axial managed VersionBundle client fixture";
        const LOG_ID: &str = "guardian-version-bundle.xml";
        const LOG_BYTES: &[u8] = b"<Configuration/>";

        let paths = test_paths(root);
        let library_dir = root.join("library");
        fs::create_dir_all(paths.instances_dir.join(instance_id))
            .expect("VersionBundle recovery instance directory");
        let mut instance = test_recovery_launch_instance();
        instance.id = instance_id.to_string();
        instance.name = "VersionBundle recovery".to_string();
        let version_id = instance.version_id.clone();
        let version_json = serde_json::to_vec(&serde_json::json!({
            "id": version_id.as_str(),
            "type": "release",
            "mainClass": "org.axial.GuardianFixture"
        }))
        .expect("VersionBundle recovery metadata");
        let version_dir = library_dir.join("versions").join(&version_id);
        fs::create_dir_all(&version_dir).expect("VersionBundle recovery version directory");
        fs::write(
            version_dir.join(format!("{version_id}.json")),
            &version_json,
        )
        .expect("VersionBundle recovery metadata");
        let client_path = version_dir.join(format!("{version_id}.jar"));
        fs::write(&client_path, vec![7_u8; CLIENT_BYTES.len()])
            .expect("same-size wrong-content VersionBundle client");
        let log_path = library_dir.join("assets/log_configs").join(LOG_ID);
        fs::create_dir_all(log_path.parent().expect("VersionBundle log parent"))
            .expect("VersionBundle log directory");
        fs::write(&log_path, LOG_BYTES).expect("VersionBundle log config");

        let config = Arc::new(
            ConfigStore::from_config(
                paths.clone(),
                AppConfig {
                    library_dir: library_dir.to_string_lossy().into_owned(),
                    ..AppConfig::default()
                },
            )
            .expect("configure VersionBundle recovery root"),
        );
        let instances = Arc::new(
            InstanceStore::from_snapshot(
                paths.clone(),
                InstanceRegistrySnapshot::new(vec![instance], instance_id.to_string(), Vec::new())
                    .expect("VersionBundle recovery registry snapshot"),
            )
            .expect("load VersionBundle recovery instances"),
        );
        let state = AppState::new(AppStateInit {
            app_name: "Axial".to_string(),
            version: "test".to_string(),
            config,
            instances,
            installs: Arc::new(InstallStore::new()),
            sessions: Arc::new(SessionStore::new()),
            performance: Arc::new(
                PerformanceManager::load_for_startup(&paths.config_dir)
                    .expect("VersionBundle recovery performance manager"),
            ),
            startup_warnings: Vec::new(),
            frontend_dir: root.join("frontend"),
        });
        state.activate_known_good_inventory_for_test(
            instance_id,
            KnownGoodInventory::from_test_entries([
                TestKnownGoodEntry {
                    root: TestKnownGoodRoot::Versions,
                    path: format!("{version_id}/{version_id}.json"),
                    kind: KnownGoodArtifactKind::VersionMetadata,
                    integrity: TestKnownGoodIntegrity::Sha1 {
                        digest: format!("{:x}", Sha1::digest(&version_json)),
                        size: version_json.len() as u64,
                    },
                },
                TestKnownGoodEntry {
                    root: TestKnownGoodRoot::Versions,
                    path: format!("{version_id}/{version_id}.jar"),
                    kind: KnownGoodArtifactKind::ClientJar,
                    integrity: TestKnownGoodIntegrity::Sha1 {
                        digest: format!("{:x}", Sha1::digest(CLIENT_BYTES)),
                        size: CLIENT_BYTES.len() as u64,
                    },
                },
                TestKnownGoodEntry {
                    root: TestKnownGoodRoot::Assets,
                    path: format!("log_configs/{LOG_ID}"),
                    kind: KnownGoodArtifactKind::LogConfig,
                    integrity: TestKnownGoodIntegrity::Sha1 {
                        digest: format!("{:x}", Sha1::digest(LOG_BYTES)),
                        size: LOG_BYTES.len() as u64,
                    },
                },
            ])
            .expect("VersionBundle recovery inventory"),
        );
        (state, client_path, CLIENT_BYTES.to_vec())
    }

    fn write_user_owned_launch_sentinels(
        state: &AppState,
        instance_id: &str,
    ) -> Vec<(PathBuf, Vec<u8>)> {
        [
            ("saves/world/level.dat", b"world".as_slice()),
            ("mods/user.jar", b"mod".as_slice()),
            ("config/user.toml", b"config".as_slice()),
            ("resourcepacks/user.zip", b"resourcepack".as_slice()),
            ("custom/opaque/nested.bin", b"unknown-owned".as_slice()),
        ]
        .into_iter()
        .map(|(relative, contents)| {
            let path = state.instances().game_dir(instance_id).join(relative);
            fs::create_dir_all(path.parent().expect("user-owned launch sentinel parent"))
                .expect("user-owned launch sentinel directory");
            fs::write(&path, contents).expect("write user-owned launch sentinel");
            (path, contents.to_vec())
        })
        .collect()
    }

    fn assert_user_owned_launch_sentinels(sentinels: &[(PathBuf, Vec<u8>)]) {
        for (path, contents) in sentinels {
            assert_eq!(
                fs::read(path).expect("user-owned launch sentinel remains readable"),
                *contents
            );
        }
    }

    async fn prepare_whole_instance_launch_fixture(
        state: &AppState,
    ) -> axial_minecraft::ManagedWholeInstanceFixtureForTest {
        #[cfg(unix)]
        const RUNTIME_BYTES: &[u8] = br#"#!/bin/sh
if [ "$1" = "-XshowSettings:property" ]; then
  echo 'openjdk version "21.0.3"' >&2
  exit 0
fi
count=0
if [ -f guardian-whole-instance-process-count ]; then
  count=$(cat guardian-whole-instance-process-count)
fi
count=$((count + 1))
printf '%s' "$count" > guardian-whole-instance-process-count
printf '%s\n' '[Render thread/INFO]: Created: 1024x512x4 minecraft:textures/atlas/blocks.png-atlas' >&2
sleep 1
exit 0
"#;
        #[cfg(not(unix))]
        const RUNTIME_BYTES: &[u8] = b"axial whole-instance Runtime fixture";

        let runtime_url = serve_whole_instance_runtime_bytes(RUNTIME_BYTES).await;
        axial_minecraft::prepare_managed_whole_instance_fixture_for_test(
            PathBuf::from(state.library_dir().expect("whole-instance managed root")),
            "1.21.1",
            RuntimeId::from("java-runtime-delta"),
            &runtime_url,
            RUNTIME_BYTES,
        )
        .await
        .expect("prepare whole-instance launch fixture")
    }

    async fn serve_whole_instance_runtime_bytes(bytes: &'static [u8]) -> String {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("whole-instance Runtime fixture server");
        let address = listener
            .local_addr()
            .expect("whole-instance Runtime fixture address");
        tokio::spawn(async move {
            for _ in 0..2 {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let mut request = [0_u8; 1024];
                let _ = socket.read(&mut request).await;
                let headers = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    bytes.len()
                );
                if socket.write_all(headers.as_bytes()).await.is_ok() {
                    let _ = socket.write_all(bytes).await;
                }
            }
        });
        format!("http://{address}/java")
    }

    struct MassTamperCorruption {
        deleted: PathBuf,
        replaced: PathBuf,
        tampered: PathBuf,
    }

    fn tamper_every_managed_inventory_entry(
        state: &AppState,
        inventory: &KnownGoodInventory,
    ) -> MassTamperCorruption {
        let library_root = PathBuf::from(state.library_dir().expect("mass-tamper managed root"));
        let mut directories = Vec::new();
        let mut paths = std::collections::BTreeSet::new();
        let mut deleted = None;
        let mut replaced = None;
        let mut tampered = None;
        for entry in inventory.entries() {
            let physical =
                known_good_entry_path(&library_root, state.managed_runtime_cache(), entry);
            let path = physical.root().join(physical.relative());
            assert!(paths.insert(path.clone()), "inventory paths must be unique");
            match entry.integrity() {
                KnownGoodIntegrity::Directory => {
                    fs::create_dir_all(&path).expect("seed managed directory before tampering");
                    directories.push(path);
                }
                KnownGoodIntegrity::Sha1 { size, .. }
                | KnownGoodIntegrity::ExactBytes { size, .. } => {
                    fs::create_dir_all(path.parent().expect("managed entry parent"))
                        .expect("seed managed entry parent");
                    let wrong_size = (*size).max(1) as usize;
                    match entry.kind() {
                        KnownGoodArtifactKind::Library if deleted.is_none() => {
                            fs::write(&path, vec![0x3c; wrong_size])
                                .expect("seed managed file before deletion");
                            fs::remove_file(&path).expect("delete managed file");
                            deleted = Some(path);
                        }
                        KnownGoodArtifactKind::VersionMetadata if replaced.is_none() => {
                            fs::write(&path, vec![0x3c; wrong_size])
                                .expect("seed managed file before identity replacement");
                            fs::remove_file(&path).expect("remove managed file before replacement");
                            fs::write(&path, vec![0xa5; wrong_size])
                                .expect("replace managed file with a new identity");
                            replaced = Some(path);
                        }
                        KnownGoodArtifactKind::ClientJar if tampered.is_none() => {
                            fs::write(&path, vec![0x3c; wrong_size])
                                .expect("seed managed file before in-place tampering");
                            let mut file = fs::OpenOptions::new()
                                .write(true)
                                .truncate(true)
                                .open(&path)
                                .expect("open managed file for in-place tampering");
                            file.write_all(&vec![0x5a; wrong_size])
                                .expect("tamper existing managed file");
                            tampered = Some(path);
                        }
                        _ => {
                            fs::write(&path, vec![0xa5; wrong_size]).expect("tamper managed file");
                        }
                    }
                }
                KnownGoodIntegrity::LinkTarget(_) => {
                    fs::create_dir_all(path.parent().expect("managed link parent"))
                        .expect("seed managed link parent");
                    fs::write(&path, b"not-a-managed-link").expect("tamper managed link");
                }
            }
        }
        directories.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
        for path in directories {
            if path.exists() {
                fs::remove_dir_all(&path).expect("remove managed directory for tampering");
            }
            fs::write(&path, b"not-a-managed-directory").expect("tamper managed directory");
        }
        let corruption = MassTamperCorruption {
            deleted: deleted.expect("mass-tamper inventory must contain a managed Library"),
            replaced: replaced.expect("mass-tamper inventory must contain VersionMetadata"),
            tampered: tampered.expect("mass-tamper inventory must contain a ClientJar"),
        };
        assert_eq!(
            std::collections::BTreeSet::from([
                corruption.deleted.as_path(),
                corruption.replaced.as_path(),
                corruption.tampered.as_path(),
            ])
            .len(),
            3,
            "delete, replace, and tamper must target distinct managed entries"
        );
        assert_eq!(paths.len(), inventory.entries().len());
        for entry in inventory.entries() {
            let physical =
                known_good_entry_path(&library_root, state.managed_runtime_cache(), entry);
            let path = physical.root().join(physical.relative());
            if path == corruption.deleted {
                assert_eq!(
                    fs::symlink_metadata(&path)
                        .expect_err("deleted managed entry must remain absent")
                        .kind(),
                    std::io::ErrorKind::NotFound
                );
                continue;
            }
            let metadata = fs::symlink_metadata(&path).expect("tampered managed entry metadata");
            match entry.integrity() {
                KnownGoodIntegrity::Sha1 { digest, .. }
                | KnownGoodIntegrity::ExactBytes { digest, .. } => {
                    let actual = format!(
                        "{:x}",
                        Sha1::digest(fs::read(&path).expect("tampered managed entry bytes"))
                    );
                    assert_ne!(actual, digest.as_str());
                }
                KnownGoodIntegrity::Directory => assert!(!metadata.is_dir()),
                KnownGoodIntegrity::LinkTarget(_) => assert!(!metadata.file_type().is_symlink()),
            }
        }
        corruption
    }

    fn registered_artifact_repair_decision(
        operation_id: &str,
        target: TargetDescriptor,
    ) -> crate::guardian::GuardianDecision {
        crate::guardian::GuardianDecision::for_test(
            Some(OperationId::new(operation_id)),
            GuardianMode::Managed,
            GuardianActionKind::Repair,
            vec![DiagnosisId::LauncherManagedArtifactCorrupt],
            Some(crate::guardian::GuardianActionPlan::new(
                StabilizationSystem::Guardian,
                crate::guardian::ActionPlanPrerequisite {
                    diagnosis_id: DiagnosisId::LauncherManagedArtifactCorrupt,
                    ownership: OwnershipClass::LauncherManaged,
                    confidence: crate::guardian::GuardianConfidence::Confirmed,
                    affected_targets: vec![target.clone()],
                    candidate_actions: vec![GuardianActionKind::Repair],
                },
                vec![crate::guardian::GuardianAction {
                    kind: GuardianActionKind::Repair,
                    target: Some(target),
                    reason: DiagnosisId::LauncherManagedArtifactCorrupt,
                }],
            )),
        )
    }

    #[cfg(unix)]
    fn test_fabric_crash_app_state(root: &Path, instance: &Instance) -> AppState {
        let paths = test_paths(root);
        fs::create_dir_all(paths.instances_dir.join(&instance.id))
            .expect("registered launch instance directory");
        let config = Arc::new(
            ConfigStore::from_config(
                paths.clone(),
                AppConfig {
                    library_dir: root.join("library").to_string_lossy().to_string(),
                    ..AppConfig::default()
                },
            )
            .expect("configure registered launch library"),
        );
        let snapshot =
            InstanceRegistrySnapshot::new(vec![instance.clone()], instance.id.clone(), Vec::new())
                .expect("registered launch instance snapshot");
        let instances = Arc::new(
            InstanceStore::from_snapshot(paths.clone(), snapshot)
                .expect("load registered launch instance"),
        );
        let state = AppState::new(AppStateInit {
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
        });
        activate_fabric_crash_known_good(&state, root, &instance.id);
        state
    }

    #[cfg(unix)]
    fn activate_fabric_crash_known_good(state: &AppState, root: &Path, instance_id: &str) {
        use std::os::unix::fs::PermissionsExt;

        let library = root.join("library");
        let version_dir = library.join("versions").join(CRASH_E2E_FABRIC_VERSION_ID);
        let version_json = version_dir.join(format!("{CRASH_E2E_FABRIC_VERSION_ID}.json"));
        let client_jar = version_dir.join(format!("{CRASH_E2E_FABRIC_VERSION_ID}.jar"));
        let runtime_root = state
            .managed_runtime_cache()
            .component_root("java-runtime-delta")
            .expect("runtime root");
        let runtime_java = if cfg!(target_os = "macos") {
            runtime_root.join("jre.bundle/Contents/Home/bin/java")
        } else {
            runtime_root.join("bin/java")
        };
        fs::create_dir_all(runtime_java.parent().expect("runtime Java parent"))
            .expect("runtime Java directory");
        let runtime_java_bytes = b"java";
        fs::write(&runtime_java, runtime_java_bytes).expect("runtime Java");
        let mut permissions = fs::metadata(&runtime_java)
            .expect("runtime Java metadata")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&runtime_java, permissions).expect("runtime Java executable");
        let runtime_proof = runtime_root.join(".axial-runtime-manifest.json");
        let runtime_marker = runtime_root.join(".axial-ready");
        let runtime_java_relative = runtime_java
            .strip_prefix(&runtime_root)
            .expect("runtime Java relative path")
            .to_string_lossy()
            .replace('\\', "/");
        let runtime_manifest = serde_json::json!({
            "files": {
                runtime_java_relative.clone(): {
                    "type": "file",
                    "downloads": {
                        "raw": {
                            "url": "https://example.invalid/java",
                            "sha1": hex::encode(Sha1::digest(runtime_java_bytes)),
                            "size": runtime_java_bytes.len()
                        }
                    }
                }
            }
        });
        fs::write(
            &runtime_proof,
            serde_json::to_vec(&runtime_manifest).expect("runtime manifest JSON"),
        )
        .expect("runtime proof");
        fs::write(&runtime_marker, b"ready").expect("runtime marker");

        let runtime_kind = || TestKnownGoodRoot::ManagedRuntime {
            component: "java-runtime-delta".to_string(),
        };
        let file = |entry_root, path, kind, physical_path: &Path| TestKnownGoodEntry {
            root: entry_root,
            path,
            kind,
            integrity: TestKnownGoodIntegrity::File {
                size: fs::metadata(physical_path).expect("known-good file").len(),
            },
        };
        let library_entries = CRASH_E2E_FABRIC_LIBRARIES.iter().map(|(_, relative)| {
            file(
                TestKnownGoodRoot::Libraries,
                (*relative).to_string(),
                KnownGoodArtifactKind::Library,
                &library.join("libraries").join(relative),
            )
        });
        let mut entries = vec![
            file(
                TestKnownGoodRoot::Versions,
                format!("{CRASH_E2E_FABRIC_VERSION_ID}/{CRASH_E2E_FABRIC_VERSION_ID}.json"),
                KnownGoodArtifactKind::VersionMetadata,
                &version_json,
            ),
            file(
                TestKnownGoodRoot::Versions,
                format!("{CRASH_E2E_FABRIC_VERSION_ID}/{CRASH_E2E_FABRIC_VERSION_ID}.jar"),
                KnownGoodArtifactKind::ClientJar,
                &client_jar,
            ),
            file(
                runtime_kind(),
                ".axial-runtime-manifest.json".to_string(),
                KnownGoodArtifactKind::RuntimeManifestProof,
                &runtime_proof,
            ),
            file(
                runtime_kind(),
                ".axial-ready".to_string(),
                KnownGoodArtifactKind::RuntimeReadyMarker,
                &runtime_marker,
            ),
            file(
                runtime_kind(),
                runtime_java_relative,
                KnownGoodArtifactKind::RuntimeExecutable,
                &runtime_java,
            ),
        ];
        entries.extend(library_entries);
        state.activate_known_good_inventory_for_test(
            instance_id,
            KnownGoodInventory::from_test_entries(entries).expect("Fabric crash inventory"),
        );
    }

    fn test_app_state_with_telemetry(root: &Path) -> AppState {
        let paths = test_paths(root);
        let config_store = ConfigStore::from_config(
            paths.clone(),
            AppConfig {
                telemetry_enabled: true,
                telemetry_install_id: TEST_TELEMETRY_INSTALL_ID.to_string(),
                ..AppConfig::default()
            },
        )
        .expect("seed telemetry config");
        let config = Arc::new(config_store);
        let instances = Arc::new(
            InstanceStore::from_snapshot(paths.clone(), InstanceRegistrySnapshot::default())
                .expect("load instances"),
        );
        let telemetry = Arc::new(TelemetryHub::new(
            config.clone(),
            Some(TEST_TELEMETRY_KEY.to_string()),
            DEFAULT_POSTHOG_HOST.to_string(),
        ));

        AppState::new_with_telemetry(
            AppStateInit {
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
            },
            telemetry,
        )
    }

    fn test_app_state_with_library(root: &Path) -> AppState {
        let paths = test_paths(root);
        let config_store = ConfigStore::from_config(
            paths.clone(),
            AppConfig {
                library_dir: paths.library_dir.to_string_lossy().to_string(),
                ..AppConfig::default()
            },
        )
        .expect("set test library");
        let config = Arc::new(config_store);
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

    fn test_asset_launch_plan(
        asset_index_id: &str,
        requires_virtual_asset_repair: bool,
    ) -> axial_launcher::VanillaLaunchPlan {
        axial_launcher::VanillaLaunchPlan {
            version: serde_json::from_value(serde_json::json!({
                "id": "test",
                "assetIndex": { "id": asset_index_id }
            }))
            .expect("test version"),
            requires_virtual_asset_repair,
            libraries: Vec::new(),
            client_jar_path: None,
            natives_dir: None,
            classpath: String::new(),
            jvm_args: Vec::new(),
            game_args: Vec::new(),
            main_class: String::new(),
            command: Vec::new(),
            game_dir: PathBuf::new(),
        }
    }

    fn write_runner_asset_index(root: &Path, asset_index_id: &str, contents: &str) {
        let indexes_dir = assets_dir(root).join("indexes");
        fs::create_dir_all(&indexes_dir).expect("asset indexes directory");
        fs::write(indexes_dir.join(format!("{asset_index_id}.json")), contents)
            .expect("asset index");
    }

    #[cfg(unix)]
    fn write_out_of_memory_launch_fixture(root: &Path) -> String {
        let java_path = write_out_of_memory_launch_fixture_with_boot(root, false);
        write_fabric_crash_install(root);
        java_path
    }

    #[cfg(unix)]
    fn write_delayed_boot_launch_fixture(root: &Path) -> String {
        write_crashing_java_fixture(
            root,
            "delayed-boot-java",
            r#"#!/bin/sh
if [ "$1" = "-XshowSettings:property" ]; then
  echo 'openjdk version "21.0.3"' >&2
  exit 0
fi
sleep 0.2
printf '%s\n' '[Render thread/INFO]: Created: 1024x512x4 minecraft:textures/atlas/blocks.png-atlas' >&2
sleep 1
exit 0
"#,
        )
    }

    #[cfg(unix)]
    async fn serve_registered_library_once(
        body: &'static [u8],
    ) -> (String, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind deleted-Libraries source server");
        let address = listener
            .local_addr()
            .expect("deleted-Libraries source address");
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener
                .accept()
                .await
                .expect("accept deleted-Libraries source request");
            let mut request = [0_u8; 2048];
            let count = socket
                .read(&mut request)
                .await
                .expect("read deleted-Libraries source request");
            assert!(request[..count].starts_with(b"GET "));
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("write deleted-Libraries source headers");
            socket
                .write_all(body)
                .await
                .expect("write deleted-Libraries source body");
            socket
                .shutdown()
                .await
                .expect("close deleted-Libraries source response");
        });
        (format!("http://{address}/fixture-1.0.0.jar"), server)
    }

    #[cfg(unix)]
    fn write_deleted_library_launch_fixture(
        root: &Path,
        library_path: &Path,
        process_count_path: &Path,
    ) -> String {
        let library_path = shell_path_literal(library_path);
        let process_count_path = shell_path_literal(process_count_path);
        write_crashing_java_fixture(
            root,
            "deleted-library-java",
            &format!(
                r#"#!/bin/sh
if [ "$1" = "-XshowSettings:property" ]; then
  echo 'openjdk version "21.0.3"' >&2
  exit 0
fi
count=0
if [ -f {process_count} ]; then
  count=$(cat {process_count})
fi
count=$((count + 1))
printf '%s' "$count" > {process_count}
if [ ! -f {library} ]; then
  mkdir -p crash-reports
  cat > crash-reports/crash-guardian-missing-library.txt <<'CRASH'
---- Minecraft Crash Report ----
Description: Loading game
net.minecraftforge.fml.common.MissingModsException: missing launcher-managed library
CRASH
  printf '%s\n' 'net.minecraftforge.fml.common.MissingModsException: missing launcher-managed library' >&2
  exit 1
fi
printf '%s\n' '[Render thread/INFO]: Created: 1024x512x4 minecraft:textures/atlas/blocks.png-atlas' >&2
sleep 1
exit 0
"#,
                process_count = process_count_path,
                library = library_path,
            ),
        )
    }

    #[cfg(unix)]
    fn write_version_bundle_launch_fixture(
        root: &Path,
        client_path: &Path,
        expected_client: &[u8],
        process_count_path: &Path,
    ) -> String {
        let expected_path = root.join("expected-version-bundle-client.jar");
        fs::write(&expected_path, expected_client).expect("expected VersionBundle client fixture");
        let client_path = shell_path_literal(client_path);
        let expected_path = shell_path_literal(&expected_path);
        let process_count_path = shell_path_literal(process_count_path);
        let bin_dir = root.join("wrong-content-client-java").join("bin");
        fs::create_dir_all(&bin_dir).expect("VersionBundle Java bin directory");
        let java_path = bin_dir.join("java");
        fs::write(
            &java_path,
            format!(
                r#"#!/bin/sh
if [ "$1" = "-XshowSettings:property" ]; then
  echo 'openjdk version "21.0.3"' >&2
  exit 0
fi
count=0
if [ -f {process_count} ]; then
  count=$(cat {process_count})
fi
count=$((count + 1))
printf '%s' "$count" > {process_count}
if ! cmp -s {client} {expected}; then
  printf '%s\n' 'java.lang.SecurityException: Invalid signature file digest for Manifest main attributes' >&2
  exit 1
fi
printf '%s\n' '[Render thread/INFO]: Created: 1024x512x4 minecraft:textures/atlas/blocks.png-atlas' >&2
sleep 1
exit 0
"#,
                process_count = process_count_path,
                client = client_path,
                expected = expected_path,
            ),
        )
        .expect("write VersionBundle Java fixture");
        let mut permissions = fs::metadata(&java_path)
            .expect("VersionBundle Java metadata")
            .permissions();
        use std::os::unix::fs::PermissionsExt as _;
        permissions.set_mode(0o755);
        fs::set_permissions(&java_path, permissions).expect("VersionBundle Java executable");
        java_path.to_string_lossy().into_owned()
    }

    #[cfg(unix)]
    fn shell_path_literal(path: &Path) -> String {
        format!(
            "'{}'",
            path.to_str()
                .expect("test shell fixture path must be UTF-8")
                .replace('\'', "'\"'\"'")
        )
    }

    #[cfg(unix)]
    fn write_post_boot_out_of_memory_launch_fixture(root: &Path) -> String {
        write_out_of_memory_launch_fixture_with_boot(root, true)
    }

    #[cfg(unix)]
    fn write_post_boot_mod_crash_launch_fixture(root: &Path) -> String {
        let java_path = write_crashing_java_fixture(
            root,
            "mod-crash-java",
            r#"#!/bin/sh
if [ "$1" = "-XshowSettings:property" ]; then
  echo 'openjdk version "21.0.3"' >&2
  exit 0
fi
printf '%s\n' '[Render thread/INFO]: Created: 1024x512x4 minecraft:textures/atlas/blocks.png-atlas' >&2
mkdir -p crash-reports
cat > crash-reports/crash-guardian-mod.txt <<'CRASH'
---- Minecraft Crash Report ----
Description: Mod loading error has occurred
java.lang.IllegalStateException: registry failed
-- MOD examplemachines --
Failure message: Example Machines (examplemachines) encountered an error
Mod Version: 3.2.1
JVM Flags: -Duser.home=/home/alice -Dtoken=raw-secret-token
CRASH
printf '%s\n' 'java.lang.IllegalStateException: registry failed' >&2
exit 1
"#,
        );
        write_fabric_crash_install(root);
        java_path
    }

    #[cfg(unix)]
    fn write_fabric_crash_install(root: &Path) {
        let libraries = CRASH_E2E_FABRIC_LIBRARIES
            .into_iter()
            .map(|(name, relative_path)| {
                let path = root.join("library").join("libraries").join(relative_path);
                fs::create_dir_all(path.parent().expect("Fabric library parent"))
                    .expect("Fabric library directory");
                write_readable_test_jar(&path);
                let bytes = fs::read(path).expect("read Fabric crash fixture library");
                serde_json::json!({
                    "name": name,
                    "downloads": {
                        "artifact": {
                            "path": relative_path,
                            "sha1": hex::encode(Sha1::digest(&bytes)),
                            "size": bytes.len()
                        }
                    }
                })
            })
            .collect::<Vec<_>>();
        let version_dir = root
            .join("library")
            .join("versions")
            .join(CRASH_E2E_FABRIC_VERSION_ID);
        fs::create_dir_all(&version_dir).expect("Fabric crash fixture version directory");
        fs::write(
            version_dir.join(format!("{CRASH_E2E_FABRIC_VERSION_ID}.json")),
            serde_json::to_vec(&serde_json::json!({
                "id": CRASH_E2E_FABRIC_VERSION_ID,
                "inheritsFrom": "1.21.1",
                "axialMaterialized": true,
                "type": "release",
                "mainClass": "net.fabricmc.loader.impl.launch.knot.KnotClient",
                "assetIndex": {},
                "arguments": { "jvm": [], "game": [] },
                "libraries": libraries
            }))
            .expect("encode Fabric crash fixture version"),
        )
        .expect("write Fabric crash fixture version");
        fs::copy(
            root.join("library/versions/1.21.1/1.21.1.jar"),
            version_dir.join(format!("{CRASH_E2E_FABRIC_VERSION_ID}.jar")),
        )
        .expect("materialize Fabric crash fixture client");
    }

    #[cfg(unix)]
    fn write_readable_test_jar(path: &Path) {
        let file = fs::File::create(path).expect("create Fabric crash fixture library");
        let mut archive = zip::ZipWriter::new(file);
        archive
            .start_file(
                "META-INF/guardian-fixture",
                zip::write::SimpleFileOptions::default(),
            )
            .expect("start Fabric crash fixture jar entry");
        archive
            .write_all(b"fixture")
            .expect("write Fabric crash fixture jar entry");
        archive
            .finish()
            .expect("finish Fabric crash fixture library");
    }

    #[cfg(unix)]
    fn write_out_of_memory_launch_fixture_with_boot(root: &Path, boot_first: bool) -> String {
        let boot_marker = if boot_first {
            "printf '%s\\n' '[Render thread/INFO]: Created: 1024x512x4 minecraft:textures/atlas/blocks.png-atlas' >&2\nsleep 0.1\n"
        } else {
            ""
        };
        write_crashing_java_fixture(
            root,
            "oom-java",
            &format!(
                r#"#!/bin/sh
if [ "$1" = "-XshowSettings:property" ]; then
  echo 'openjdk version "21.0.3"' >&2
  exit 0
fi
{boot_marker}
mkdir -p crash-reports
cat > crash-reports/crash-guardian-oom.txt <<'CRASH'
---- Minecraft Crash Report ----
Description: Rendering game
java.lang.OutOfMemoryError: Java heap space
JVM Flags: -Duser.home=/home/alice -Dtoken=raw-secret-token
Player: SecretPlayer
CRASH
printf '%s\n' 'java.lang.OutOfMemoryError: Java heap space /home/alice/.axial/secret --accessToken raw-secret-token -Xmx8192M -Dtoken=raw provider_payload=provider-secret account_id=account-secret username=SecretPlayer eyJheader123456789.abcdEFGH12345678.ijklMNOP12345678' >&2
printf '%s\n' 'at SecretMod.crash(/home/alice/SecretMod.java:42)' >&2
exit 1
"#
            ),
        )
    }

    #[cfg(unix)]
    fn write_crashing_java_fixture(root: &Path, runtime_dir: &str, script: &str) -> String {
        use std::os::unix::fs::PermissionsExt;

        let version_dir = root.join("library").join("versions").join("1.21.1");
        fs::create_dir_all(&version_dir).expect("crash fixture version directory");
        fs::write(
            version_dir.join("1.21.1.json"),
            serde_json::to_vec(&serde_json::json!({
                "id": "1.21.1",
                "type": "release",
                "mainClass": "net.minecraft.client.main.Main",
                "assetIndex": {},
                "javaVersion": { "component": "java-runtime-delta", "majorVersion": 21 },
                "libraries": []
            }))
            .expect("encode crash fixture version"),
        )
        .expect("write crash fixture version");
        fs::write(version_dir.join("1.21.1.jar"), b"client jar")
            .expect("write crash fixture client jar");
        fs::create_dir_all(root.join("instance")).expect("crash fixture game directory");

        let bin_dir = root.join(runtime_dir).join("bin");
        fs::create_dir_all(&bin_dir).expect("crash fixture Java bin directory");
        let java_path = bin_dir.join("java");
        fs::write(&java_path, script).expect("write crash fixture Java");
        let mut permissions = fs::metadata(&java_path)
            .expect("crash fixture Java metadata")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&java_path, permissions).expect("make crash fixture Java executable");
        java_path.to_string_lossy().to_string()
    }

    fn assert_out_of_memory_observation(
        entry: &crate::state::failure_memory::GuardianFailureMemoryEntry,
        expected_instance_id: &str,
    ) {
        assert_eq!(entry.diagnosis_id.as_str(), "out_of_memory");
        assert_eq!(entry.domain, GuardianDomain::Startup);
        assert_eq!(entry.mode, GuardianMode::Managed);
        assert_eq!(entry.target.system, StabilizationSystem::Guardian);
        assert_eq!(entry.target.kind, TargetKind::Instance);
        assert_eq!(entry.target.id, expected_instance_id);
        assert_eq!(entry.ownership, OwnershipClass::UserOwned);
        assert_eq!(entry.occurrence_count, 1);
        assert_eq!(entry.last_action_kind, None);
        assert_eq!(entry.last_action_outcome, None);
        assert_eq!(entry.repair_attempt_count, 0);
        assert_eq!(entry.suppression_until, None);
        assert_eq!(entry.target_content_hash, None);
        assert_eq!(entry.user_intent_hash, None);
    }

    fn assert_oom_preflight_guidance(
        preflight: &super::super::session::LaunchPreflightResponse,
        fact: &crate::guardian::GuardianFact,
    ) {
        let current_memory_mb = fact
            .fields
            .iter()
            .find(|field| field.key == "current_memory_mb")
            .map(|field| field.value.as_str())
            .expect("OOM fact current memory");
        let suggested_memory_mb = fact
            .fields
            .iter()
            .find(|field| field.key == "suggested_memory_mb")
            .map(|field| field.value.as_str());
        let expected = suggested_memory_mb.map_or_else(
            || {
                "Guardian could not verify safe headroom for a larger memory allocation. Close another session or free memory before relaunching."
                    .to_string()
            },
            |suggested| {
                format!(
                    "Increase this instance's maximum memory from {current_memory_mb} MB to {suggested} MB before relaunching."
                )
            },
        );
        assert!(
            preflight
                .guardian
                .guidance()
                .iter()
                .any(|guidance| guidance == &expected),
            "missing OOM next-launch guidance: {expected}"
        );
    }

    fn guardian_fact_field<'a>(
        fact: &'a crate::guardian::GuardianFact,
        key: &str,
    ) -> Option<&'a str> {
        fact.fields
            .iter()
            .find(|field| field.key == key)
            .map(|field| field.value.as_str())
    }

    fn assert_no_out_of_memory_sensitive_decoys(payload: &str) {
        for fragment in [
            "/home/alice",
            "C:\\Users\\Alice",
            "--accessToken",
            "raw-secret-token",
            "-Xmx8192M",
            "-Dtoken=raw",
            "provider_payload",
            "provider-secret",
            "account_id=account-secret",
            "username=SecretPlayer",
            "SecretPlayer",
            "SecretMod.crash",
            "eyJheader123456789",
        ] {
            assert!(
                !payload.contains(fragment),
                "OOM public or persisted payload leaked {fragment:?}: {payload}"
            );
        }
    }

    fn assert_no_sensitive_stage_evidence(text: &str) {
        for fragment in [
            "/home/alice",
            "/home/",
            "C:\\Users",
            "Alice",
            ".jdks",
            ".minecraft",
            "java.exe",
            "--accessToken",
            "-Xmx",
            "-cp",
            "token",
            "SecretPlayer",
        ] {
            assert!(
                !text.contains(fragment),
                "stage evidence leaked fragment {fragment:?}: {text}"
            );
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
