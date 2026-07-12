mod failure;
mod metadata;
mod prewarm;
mod proof;
mod recovery;
mod spawn;
mod status;

use crate::application::guardian_conversion::api_guardian_mode;
use crate::application::launch_application_stage_evidence;
use crate::execution::launch::{
    LaunchCommandPreparationRequest, launch_command_stage_evidence, prepare_launch_command,
};
use crate::guardian::{
    GuardianCopyRequest, GuardianLaunchRecoveryPlan, GuardianObservedLaunchFailurePhase,
    GuardianPrepareFailureRequest, GuardianPresetAdjustmentRequest,
    GuardianStartupFailureObservation, GuardianStartupFailureRequest, author_guardian_copy,
    guardian_prelaunch_preset_adjustment_directive, guardian_prepare_failure_outcome,
    guardian_startup_failure_outcome, guardian_summary_with_blocked_outcome,
    guardian_summary_with_observed_outcome, is_guardian_launch_crash_class,
    record_launch_failure_observation,
};
use crate::logging::{append_trace, timestamp_utc};
use crate::observability::telemetry::{
    TelemetryErrorArea, TelemetryErrorKind, TelemetryErrorLevel, TelemetryEvent,
    TelemetryLaunchOutcome,
};
use crate::state::launch_reports::LaunchProofContext;
use crate::state::{
    AppState, LaunchEvent, LaunchFailureTermination, LaunchFailureTerminationErrorClass,
    LaunchStatusEvent, OperationJournalStoreError, StartupOutcome,
};
use axial_launcher::{
    GuardianSummary, LaunchFailureClass, LaunchSessionExitReason, LaunchSessionOutcome,
    LaunchSessionOutcomeKind, LaunchState, PreparedLaunchAttempt, build_healing_summary,
    prepare_launch_attempt_with_events,
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

pub(crate) async fn launch_session(
    state: AppState,
    task: super::session::LaunchSessionTask,
    producer: crate::state::ProducerLease,
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
    let result = launch_session_inner(state.clone(), task, &producer).await;
    let disposition =
        terminalize_unhandled_launch_error(&state, &producer, &session_id, result).await;
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
                break state.sessions().get(&session_id).await;
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
                    }
                    Err(error_class) => {
                        trace_unconfirmed_launch_failure_termination(error_class);
                    }
                }
            });
            LaunchTerminalizationDisposition::Retained(Err(error))
        }
        LaunchFailureTermination::Unconfirmed(error_class) => {
            trace_unconfirmed_launch_failure_termination(error_class);
            LaunchTerminalizationDisposition::Retained(Err(error))
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

async fn launch_session_inner(
    state: AppState,
    task: super::session::LaunchSessionTask,
    producer: &crate::state::ProducerLease,
) -> Result<LaunchSuccess, LaunchRequestError> {
    launch_session_inner_with_control(state, task, producer, &LaunchLoopControl::default()).await
}

async fn launch_session_inner_with_control(
    state: AppState,
    task: super::session::LaunchSessionTask,
    producer: &crate::state::ProducerLease,
    control: &LaunchLoopControl,
) -> Result<LaunchSuccess, LaunchRequestError> {
    let super::session::LaunchSessionTask {
        application,
        preflight_stage_evidence,
        mut instance,
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
            prepare_launch_attempt_with_events(
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
                    &mut instance,
                    &intent.username,
                    intent.max_memory_mb,
                    intent.min_memory_mb,
                    &launched_at,
                )
                .await
                {
                    tracing::warn!(?stage, "launch metadata persistence failed");
                }
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
                let observation = if stalled {
                    GuardianStartupFailureObservation::Stalled
                } else {
                    GuardianStartupFailureObservation::Exited {
                        failure_class: terminal_record
                            .as_ref()
                            .and_then(|record| record.failure.as_ref().map(|failure| failure.class))
                            .unwrap_or(LaunchFailureClass::Unknown),
                    }
                };
                let guardian_mode = api_guardian_mode(intent.guardian.mode);
                let startup_outcome =
                    guardian_startup_failure_outcome(GuardianStartupFailureRequest {
                        mode: guardian_mode,
                        observation,
                        crash_evidence: terminal_record
                            .as_ref()
                            .and_then(|record| record.crash_evidence.as_ref()),
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
                    });
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
}

impl LaunchLoopControl {
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

pub fn trace_launch_event(session_id: &str, message: &str) {
    append_trace("launch", session_id, message);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guardian::{
        GuardianActionKind, GuardianDomain, GuardianMode, guardian_user_outcome_for_test,
    };
    use crate::observability::telemetry::{DEFAULT_POSTHOG_HOST, TelemetryHub};
    use crate::state::contracts::{
        OperationPhase, OwnershipClass, StabilizationSystem, TargetKind,
    };
    use crate::state::failure_memory::{FailureMemorySnapshot, failure_memory_path};
    use crate::state::{AppStateInit, InstallStore, LaunchEvent, SessionStore};
    use axial_config::{
        AppConfig, AppPaths, ConfigStore, Instance, InstanceRegistrySnapshot, InstanceStore,
    };
    use axial_launcher::{
        CrashEvidence, LaunchAuthContext, LaunchGuardianContext, LaunchIntent, LaunchSessionRecord,
        OverrideOrigin, SessionId,
    };
    use axial_performance::PerformanceManager;
    use std::fs;
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::Duration;
    use std::time::{SystemTime, UNIX_EPOCH};

    const TEST_TELEMETRY_INSTALL_ID: &str = "123e4567-e89b-12d3-a456-426614174000";
    const TEST_TELEMETRY_KEY: &str = "phc_test";
    const CRASH_E2E_INSTANCE_ID: &str = "0123456789abcdef";
    const CRASH_E2E_FABRIC_VERSION_ID: &str = "fabric-loader-0.16.10-1.21.1";

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
        };
        let producer = state.try_claim_producer().expect("claim launch producer");

        let result = tokio::time::timeout(
            Duration::from_secs(10),
            launch_session_inner_with_control(
                state.clone(),
                test_recovery_launch_task(session_id, &root),
                &producer,
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
    async fn out_of_memory_startup_exit_persists_bounded_proof_and_failure_memory() {
        let root = unique_test_dir("launch-out-of-memory-e2e");
        let paths = test_paths(&root);
        let session_id = "launch-out-of-memory-e2e";
        let java_path = write_out_of_memory_launch_fixture(&root);
        assert_scanner_recognizes_fabric_crash_install(&root);
        let mut task = test_recovery_launch_task(session_id, &root);
        retarget_test_launch_task(&mut task, CRASH_E2E_INSTANCE_ID);
        align_fabric_crash_task(&mut task, &java_path);
        task.instance.max_memory_mb = 1024;
        task.intent.max_memory_mb = 1024;
        let state = test_app_state_with_registered_launch_instance(&root, &task.instance);
        task.intent.game_dir = Some(state.instances().game_dir(&task.instance.id));
        task.launched_at = "2026-01-01T00:00:00.000Z".to_string();
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
        let producer = state.try_claim_producer().expect("claim OOM producer");

        let result = tokio::time::timeout(
            Duration::from_secs(10),
            launch_session_inner(state.clone(), task, &producer),
        )
        .await
        .expect("OOM launch deadline");
        let error = match result {
            Ok(_) => panic!("OOM launch must fail"),
            Err(error) => error,
        };

        assert_eq!(error.message, "Guardian blocked launch startup.");
        assert!(error.message.chars().count() <= 180);
        let guardian = error.guardian.as_ref().expect("OOM Guardian summary");
        assert_eq!(guardian.decision, axial_launcher::GuardianDecision::Blocked);
        assert_eq!(guardian.message.as_deref(), Some(error.message.as_str()));
        assert!(guardian.details.iter().any(|detail| {
            detail == "Minecraft exited before startup completed after running out of memory."
        }));
        assert!(guardian.guidance.iter().any(|detail| {
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
            preflight.guardian.decision,
            axial_launcher::GuardianDecision::Warned
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
            preflight.guardian.details.iter().any(|detail| {
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
            low_end_preflight.guardian.decision,
            axial_launcher::GuardianDecision::Warned
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
            low_end_preflight.guardian.details.iter().any(|detail| {
                detail.contains("out-of-memory crash") && detail.contains("today")
            })
        );
        assert!(low_end_preflight.guardian.guidance.iter().any(|guidance| {
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
        let mut task = test_recovery_launch_task(session_id, &root);
        task.instance.java_path = java_path.clone();
        task.intent.requested_java = java_path;
        task.intent.game_dir = Some(root.join("instance"));
        task.launched_at = "2026-01-01T00:00:00.000Z".to_string();
        let producer = state
            .try_claim_producer()
            .expect("claim post-boot OOM producer");

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
        let mut task = test_recovery_launch_task(session_id, &root);
        retarget_test_launch_task(&mut task, CRASH_E2E_INSTANCE_ID);
        align_fabric_crash_task(&mut task, &java_path);
        let state = test_app_state_with_registered_launch_instance(&root, &task.instance);
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
        let producer = state
            .try_claim_producer()
            .expect("claim mod crash producer");

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
            preflight.guardian.decision,
            axial_launcher::GuardianDecision::Warned
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
            preflight.guardian.details.iter().any(|detail| {
                detail.contains("mod-attributed crash") && detail.contains("today")
            })
        );
        assert!(preflight.guardian.guidance.iter().any(|guidance| {
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
            normal_preflight.guardian.decision,
            axial_launcher::GuardianDecision::Warned
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
            normal_preflight.guardian.details.iter().any(|detail| {
                detail.contains("mod-attributed crash") && detail.contains("today")
            })
        );
        assert!(normal_preflight.guardian.guidance.iter().any(|guidance| {
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
        let task = test_recovery_launch_task(session_id, &root);
        let proof_context = LaunchProofContext::from_intent(&task.intent);

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
        let task = test_recovery_launch_task(session_id, &root);
        let proof_context = LaunchProofContext::from_intent(&task.intent);
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
                    guardian: GuardianSummary::new(axial_launcher::GuardianMode::Managed),
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
        let mut guardian = GuardianSummary::new(axial_launcher::GuardianMode::Managed);
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
        assert_eq!(guardian.decision, axial_launcher::GuardianDecision::Blocked);
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
        let result =
            match terminalize_unhandled_launch_error(&state, &producer, session_id, Err(error))
                .await
            {
                LaunchTerminalizationDisposition::Complete(result)
                | LaunchTerminalizationDisposition::Retained(result)
                | LaunchTerminalizationDisposition::Settled(result) => result,
            };

        assert!(result.is_err());
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
        let result =
            match terminalize_unhandled_launch_error(&state, &producer, session_id, Err(error))
                .await
            {
                LaunchTerminalizationDisposition::Complete(result)
                | LaunchTerminalizationDisposition::Retained(result)
                | LaunchTerminalizationDisposition::Settled(result) => result,
            };

        assert!(result.is_err());
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
        let result =
            match terminalize_unhandled_launch_error(&state, &producer, session_id, Err(error))
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

    fn test_recovery_launch_task(
        session_id: &str,
        root: &Path,
    ) -> super::super::session::LaunchSessionTask {
        super::super::session::LaunchSessionTask {
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
            instance: Instance {
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
            },
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
            guardian: GuardianSummary::new(axial_launcher::GuardianMode::Managed),
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
        assert_eq!(fabric_loader.build_id, "fabric:1.21.1:0.16.10");
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

    fn test_app_state_with_registered_launch_instance(
        root: &Path,
        instance: &Instance,
    ) -> AppState {
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
                "type": "release",
                "mainClass": "net.fabricmc.loader.impl.launch.knot.KnotClient",
                "assetIndex": {},
                "arguments": { "jvm": [], "game": [] },
                "libraries": [
                    {
                        "name": "net.fabricmc:fabric-loader:0.16.10",
                        "axialChecksumlessAllowed": true
                    },
                    {
                        "name": "net.fabricmc:intermediary:1.21.1",
                        "axialChecksumlessAllowed": true
                    }
                ]
            }))
            .expect("encode Fabric crash fixture version"),
        )
        .expect("write Fabric crash fixture version");
        fs::write(
            version_dir.join(".axial-loader.json"),
            serde_json::to_vec(&serde_json::json!({
                "schema_version": 1,
                "component_id": "net.fabricmc.fabric-loader",
                "component_name": "Fabric",
                "build_id": "fabric:1.21.1:0.16.10",
                "minecraft_version": "1.21.1",
                "loader_version": "0.16.10"
            }))
            .expect("encode Fabric crash fixture metadata"),
        )
        .expect("write Fabric crash fixture metadata");

        for relative_path in [
            "net/fabricmc/fabric-loader/0.16.10/fabric-loader-0.16.10.jar",
            "net/fabricmc/intermediary/1.21.1/intermediary-1.21.1.jar",
        ] {
            let path = root.join("library").join("libraries").join(relative_path);
            fs::create_dir_all(path.parent().expect("Fabric library parent"))
                .expect("Fabric library directory");
            write_readable_test_jar(&path);
        }
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
                .guidance
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
