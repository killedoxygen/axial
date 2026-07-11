mod failure;
mod metadata;
mod prewarm;
mod proof;
mod recovery;
mod spawn;
mod status;

use crate::application::{launch_application_stage_evidence, launch_boundary_stage_evidence};
use crate::execution::launch::{
    LaunchCommandPreparationRequest, launch_command_stage_evidence, prepare_launch_command,
};
use crate::guardian::{
    GuardianLaunchRecoveryPlan, GuardianLaunchRecoveryStatus, GuardianPrepareFailureRequest,
    GuardianPresetAdjustmentRequest, GuardianStartupFailureObservation,
    GuardianStartupFailureRequest, GuardianUserOutcome,
    guardian_prelaunch_preset_adjustment_directive, guardian_prepare_failure_outcome,
    guardian_startup_failure_outcome,
};
use crate::logging::append_trace;
use crate::observability::telemetry::{
    TelemetryErrorArea, TelemetryErrorKind, TelemetryErrorLevel, TelemetryEvent,
    TelemetryLaunchOutcome,
};
use crate::state::launch_reports::LaunchProofContext;
use crate::state::{
    AppState, LaunchFailureTermination, LaunchFailureTerminationErrorClass,
    OperationJournalStoreError, StartupOutcome,
};
use axial_launcher::{
    GuardianSummary, LaunchFailureClass, LaunchSessionExitReason, LaunchSessionOutcome,
    LaunchState, PreparedLaunchAttempt, build_healing_summary, prepare_launch_attempt_with_events,
};
use axial_minecraft::download::repair_virtual_assets_from_index;
use axial_minecraft::paths::assets_dir;
use failure::{fail_launch, fail_launch_for_journal, fail_launch_with_outcome};
use metadata::persist_launch_metadata;
use prewarm::{format_prewarm_run_summary, prewarm_launch_plan};
use proof::persist_launch_proof_with_context;
use recovery::{
    apply_prepare_recovery_directive, apply_startup_recovery_directive,
    block_guardian_for_suppressed_launch_recovery, block_guardian_with_user_outcome,
    plan_guardian_launch_recovery_directive, record_failed_self_healing_if_any,
    record_guardian_launch_recovery_attempt, record_prelaunch_preset_adjustment_directive,
    record_successful_self_healing_if_any, suppressed_launch_recovery_outcome,
};
use spawn::{
    launch_command_target, launch_spawn_failed_stage_evidence, launch_spawn_stage_evidence,
};
use status::{emit_status, launch_state_for_preparation_event, serialize_guardian};
use tokio::process::Command;

pub use failure::sanitize_live_launch_failure_message;
pub(in crate::application::launch) use proof::persist_launch_proof;

pub(super) async fn persist_launch_proof_for_reservation_failure(
    state: &AppState,
    session_id: &str,
    launched_at: Option<&str>,
    proof_context: &LaunchProofContext,
) {
    persist_launch_proof_with_context(
        state,
        session_id,
        launched_at,
        "failed",
        Some(proof_context),
    )
    .await;
}

const STARTUP_OBSERVATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

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

pub(crate) async fn launch_session(
    state: AppState,
    task: super::session::LaunchSessionTask,
) -> Result<LaunchSuccess, LaunchRequestError> {
    let session_id = task.intent.session_id.clone();
    let sessions = state.sessions().clone();
    let result = launch_session_inner(state.clone(), task).await;
    match terminalize_unhandled_launch_error(&state, &session_id, result).await {
        LaunchTerminalizationDisposition::Complete(result) => {
            sessions.release_terminal_retention_hold(&session_id).await;
            result
        }
        LaunchTerminalizationDisposition::Retained(result)
        | LaunchTerminalizationDisposition::Settled(result) => result,
    }
}

async fn terminalize_unhandled_launch_error(
    state: &AppState,
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
            let terminal_error = finalize_unhandled_launch_error(state, session_id, error).await;
            lease.release().await;
            LaunchTerminalizationDisposition::Settled(Err(terminal_error))
        }
        LaunchFailureTermination::Pending(pending) => {
            trace_unconfirmed_launch_failure_termination(pending.error_class());
            let deferred_state = state.clone();
            let deferred_session_id = session_id.to_string();
            let deferred_error = error.clone();
            tokio::spawn(async move {
                match pending.wait_for_settlement().await {
                    Ok(lease) => {
                        let _ = finalize_unhandled_launch_error(
                            &deferred_state,
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
    session_id: &str,
    error: LaunchRequestError,
) -> LaunchRequestError {
    state.telemetry().emit(TelemetryEvent::launch_completed(
        TelemetryLaunchOutcome::Failure,
    ));
    fail_launch_for_journal(
        state,
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
) -> Result<LaunchSuccess, LaunchRequestError> {
    let super::session::LaunchSessionTask {
        application,
        boundary,
        mut instance,
        config,
        intent,
        mut guardian,
        launched_at,
        benchmark,
        resource_budget,
    } = task;
    let session_id = intent.session_id.clone();
    trace_launch_event(
        &session_id,
        &format!("application command staged: {:?}", application.command.kind),
    );
    trace_launch_event(
        &session_id,
        &format!(
            "application launch boundary staged: safety={:?} performance_mode={}",
            boundary.guardian_decision.kind, boundary.performance_mode
        ),
    );
    let mut initial_evidence = launch_application_stage_evidence(&application);
    initial_evidence.extend(launch_boundary_stage_evidence(&boundary));
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
        let preparation_status_task = tokio::spawn(async move {
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
        });
        let prepared_result = prepare_launch_attempt_with_events(&intent, &attempt, move |event| {
            let _ = preparation_event_sender.send(event);
        })
        .await;
        drop(preparation_event_tx);
        let _ = preparation_status_task.await;

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
                        mode: launch_policy_guardian_mode(intent.guardian.mode),
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
                    let recovery_plan = match plan_guardian_launch_recovery_directive(
                        &session_id,
                        &intent,
                        directive,
                        launch_policy_guardian_mode(intent.guardian.mode),
                        failure_class,
                    ) {
                        Ok(recovery_plan) => recovery_plan,
                        Err(_) => {
                            emit_pending_launch_failure(&state, &mut launch_completion_pending);
                            return Err(fail_rejected_launch_recovery_plan(
                                &state,
                                &session_id,
                                Some(&proof_context),
                                failure_class,
                                &prepare_outcome.user_outcome,
                                error.healing.clone(),
                                &mut guardian,
                            )
                            .await);
                        }
                    };
                    let recovery_outcome = record_guardian_launch_recovery_attempt(
                        &state,
                        &session_id,
                        &recovery_plan,
                        failure_class,
                    )
                    .await
                    .map_err(guardian_journal_error)?;
                    if recovery_outcome.status == GuardianLaunchRecoveryStatus::Suppressed {
                        let recovery_user_outcome =
                            suppressed_launch_recovery_outcome(&recovery_plan);
                        let message = recovery_user_outcome.summary.clone();
                        state
                            .sessions()
                            .emit_log(&session_id, "system", message.clone())
                            .await;
                        block_guardian_for_suppressed_launch_recovery(
                            &mut guardian,
                            &recovery_user_outcome,
                        );
                        emit_pending_launch_failure(&state, &mut launch_completion_pending);
                        return Err(fail_launch(
                            &state,
                            &session_id,
                            Some(&proof_context),
                            failure_class,
                            &message,
                            error.healing,
                            Some(guardian.clone()),
                        )
                        .await);
                    }
                    state
                        .sessions()
                        .emit_log(
                            &session_id,
                            "system",
                            recovery_plan.directive.description.clone(),
                        )
                        .await;
                    apply_prepare_recovery_directive(&mut guardian, &mut attempt, &recovery_plan);
                    last_recovery_plan = Some(recovery_plan);
                    continue;
                }
                block_guardian_with_user_outcome(&mut guardian, &prepare_outcome.user_outcome);
                emit_pending_launch_failure(&state, &mut launch_completion_pending);
                return Err(fail_launch(
                    &state,
                    &session_id,
                    Some(&proof_context),
                    failure_class,
                    &error.message,
                    error.healing,
                    Some(guardian.clone()),
                )
                .await);
            }
        };
        if let Some(directive) =
            guardian_prelaunch_preset_adjustment_directive(GuardianPresetAdjustmentRequest {
                mode: launch_policy_guardian_mode(intent.guardian.mode),
                requested_preset: &intent.requested_preset,
                effective_preset: &prepared.effective_preset,
                explicit_jvm_preset_present: intent.guardian.has_named_preset(),
            })
            && let Ok(plan) = plan_guardian_launch_recovery_directive(
                &session_id,
                &intent,
                directive,
                launch_policy_guardian_mode(intent.guardian.mode),
                LaunchFailureClass::Unknown,
            )
        {
            record_prelaunch_preset_adjustment_directive(&mut guardian, &plan);
        }

        trace_launch_event(
            &session_id,
            &format!(
                "prepare finished total={}ms version={}ms runtime={}ms planning={}ms",
                prepared.metrics.total_ms,
                prepared.metrics.version_ms,
                prepared.metrics.runtime_ms,
                prepared.metrics.planning_ms,
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
                emit_pending_launch_failure(&state, &mut launch_completion_pending);
                return Err(fail_launch(
                    &state,
                    &session_id,
                    Some(&proof_context),
                    LaunchFailureClass::Unknown,
                    &error.to_string(),
                    prepared.healing.clone(),
                    Some(guardian.clone()),
                )
                .await);
            }
        };

        if let Err(error) =
            repair_legacy_virtual_assets_before_launch(&intent, &prepared.plan).await
        {
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
            emit_pending_launch_failure(&state, &mut launch_completion_pending);
            return Err(fail_launch(
                &state,
                &session_id,
                Some(&proof_context),
                LaunchFailureClass::Unknown,
                &error.to_string(),
                prepared.healing.clone(),
                Some(guardian.clone()),
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
                emit_launch_completed(
                    &state,
                    &mut launch_completion_pending,
                    TelemetryLaunchOutcome::Failure,
                );
                state.telemetry().emit(TelemetryEvent::error_captured(
                    TelemetryErrorKind::LaunchSpawnFailed,
                    TelemetryErrorArea::Launch,
                    TelemetryErrorLevel::Error,
                    LaunchSessionExitReason::SpawnFailed.summary(),
                ));
                trace_launch_event(&session_id, &format!("spawn failed: {error}"));
                return Err(fail_launch_with_outcome(
                    &state,
                    &session_id,
                    Some(&proof_context),
                    LaunchFailureClass::Unknown,
                    &format!("failed to start launch process: {error}"),
                    prepared.healing.clone(),
                    Some(guardian.clone()),
                    Some(LaunchSessionOutcome::from_reason(
                        LaunchSessionExitReason::SpawnFailed,
                    )),
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
                    &session_id,
                    Some(launched_at.as_str()),
                    "running",
                    Some(&proof_context),
                )
                .await;
                persist_launch_metadata(
                    &state,
                    &mut instance,
                    &config,
                    &intent.username,
                    intent.max_memory_mb,
                    intent.min_memory_mb,
                    &launched_at,
                );
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

                let observation = if stalled {
                    GuardianStartupFailureObservation::Stalled
                } else {
                    GuardianStartupFailureObservation::Exited {
                        failure_class: state
                            .sessions()
                            .observed_failure_for_exit(&session_id)
                            .await
                            .unwrap_or(LaunchFailureClass::Unknown),
                    }
                };
                let startup_outcome =
                    guardian_startup_failure_outcome(GuardianStartupFailureRequest {
                        mode: launch_policy_guardian_mode(intent.guardian.mode),
                        observation,
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
                    let recovery_plan = match plan_guardian_launch_recovery_directive(
                        &session_id,
                        &intent,
                        directive,
                        launch_policy_guardian_mode(intent.guardian.mode),
                        failure_class,
                    ) {
                        Ok(recovery_plan) => recovery_plan,
                        Err(_) => {
                            let healing = startup_failure_healing(
                                &intent,
                                &prepared,
                                &attempt,
                                failure_class,
                            );
                            emit_launch_completed(
                                &state,
                                &mut launch_completion_pending,
                                TelemetryLaunchOutcome::Failure,
                            );
                            return Err(fail_rejected_launch_recovery_plan(
                                &state,
                                &session_id,
                                Some(&proof_context),
                                failure_class,
                                &startup_outcome.user_outcome,
                                healing,
                                &mut guardian,
                            )
                            .await);
                        }
                    };
                    let recovery_outcome = record_guardian_launch_recovery_attempt(
                        &state,
                        &session_id,
                        &recovery_plan,
                        failure_class,
                    )
                    .await
                    .map_err(guardian_journal_error)?;
                    if recovery_outcome.status == GuardianLaunchRecoveryStatus::Suppressed {
                        let recovery_user_outcome =
                            suppressed_launch_recovery_outcome(&recovery_plan);
                        let message = recovery_user_outcome.summary.clone();
                        state
                            .sessions()
                            .emit_log(&session_id, "system", message.clone())
                            .await;
                        block_guardian_for_suppressed_launch_recovery(
                            &mut guardian,
                            &recovery_user_outcome,
                        );
                        let healing =
                            startup_failure_healing(&intent, &prepared, &attempt, failure_class);
                        emit_launch_completed(
                            &state,
                            &mut launch_completion_pending,
                            TelemetryLaunchOutcome::Failure,
                        );
                        return Err(fail_launch(
                            &state,
                            &session_id,
                            Some(&proof_context),
                            failure_class,
                            &message,
                            healing,
                            Some(guardian.clone()),
                        )
                        .await);
                    }
                    state
                        .sessions()
                        .emit_log(
                            &session_id,
                            "system",
                            recovery_plan.directive.description.clone(),
                        )
                        .await;
                    apply_startup_recovery_directive(&mut guardian, &mut attempt, &recovery_plan);
                    last_recovery_plan = Some(recovery_plan);
                    continue;
                }

                let healing = startup_failure_healing(&intent, &prepared, &attempt, failure_class);
                block_guardian_with_user_outcome(&mut guardian, &startup_outcome.user_outcome);
                emit_launch_completed(
                    &state,
                    &mut launch_completion_pending,
                    TelemetryLaunchOutcome::Failure,
                );
                return Err(fail_launch(
                    &state,
                    &session_id,
                    Some(&proof_context),
                    failure_class,
                    &startup_outcome.user_outcome.summary,
                    healing,
                    Some(guardian.clone()),
                )
                .await);
            }
        }
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

async fn fail_rejected_launch_recovery_plan(
    state: &AppState,
    session_id: &str,
    proof_context: Option<&LaunchProofContext>,
    failure_class: LaunchFailureClass,
    user_outcome: &GuardianUserOutcome,
    healing: Option<axial_launcher::LaunchHealingSummary>,
    guardian: &mut GuardianSummary,
) -> LaunchRequestError {
    block_guardian_with_user_outcome(guardian, user_outcome);
    fail_launch(
        state,
        session_id,
        proof_context,
        failure_class,
        &user_outcome.summary,
        healing,
        Some(guardian.clone()),
    )
    .await
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

async fn repair_legacy_virtual_assets_before_launch(
    intent: &axial_launcher::LaunchIntent,
    plan: &axial_launcher::VanillaLaunchPlan,
) -> Result<(), axial_minecraft::download::DownloadError> {
    let asset_index_id = plan.version.asset_index.id.trim();
    if asset_index_id.is_empty() {
        return Ok(());
    }
    let asset_index_path = assets_dir(&intent.library_dir)
        .join("indexes")
        .join(format!("{asset_index_id}.json"));
    repair_virtual_assets_from_index(&intent.library_dir, &asset_index_path).await?;
    Ok(())
}

fn launch_policy_guardian_mode(
    mode: axial_launcher::GuardianMode,
) -> crate::guardian::GuardianMode {
    match mode {
        axial_launcher::GuardianMode::Managed => crate::guardian::GuardianMode::Managed,
        axial_launcher::GuardianMode::Custom => crate::guardian::GuardianMode::Custom,
    }
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
    use crate::guardian::GuardianDecisionKind;
    use crate::observability::telemetry::{DEFAULT_POSTHOG_HOST, TelemetryHub};
    use crate::state::contracts::OperationPhase;
    use crate::state::{AppStateInit, InstallStore, LaunchEvent, SessionStore};
    use axial_config::{AppConfig, AppPaths, ConfigStore, InstanceStore};
    use axial_launcher::{LaunchSessionRecord, SessionId};
    use axial_performance::PerformanceManager;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::Duration;
    use std::time::{SystemTime, UNIX_EPOCH};

    const TEST_TELEMETRY_INSTALL_ID: &str = "123e4567-e89b-12d3-a456-426614174000";
    const TEST_TELEMETRY_KEY: &str = "phc_test";

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
        let user_outcome = GuardianUserOutcome {
            decision: GuardianDecisionKind::Block,
            phase: OperationPhase::Preparing,
            summary: "Guardian blocked launch recovery planning.".to_string(),
            details: vec!["The recovery directive could not be planned safely.".to_string()],
            guidance: Vec::new(),
        };

        let error = fail_rejected_launch_recovery_plan(
            &state,
            session_id,
            None,
            LaunchFailureClass::Unknown,
            &user_outcome,
            None,
            &mut guardian,
        )
        .await;

        assert_eq!(error.message, user_outcome.summary);
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

        let result = match terminalize_unhandled_launch_error(&state, session_id, Err(error)).await
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

        let result = match terminalize_unhandled_launch_error(&state, session_id, Err(error)).await
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
            .add(
                "Termination rejection".to_string(),
                "1.21.1".to_string(),
                String::new(),
                String::new(),
                None,
            )
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

        let result = match terminalize_unhandled_launch_error(&state, session_id, Err(error)).await
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

    fn test_app_state(root: &Path) -> AppState {
        let paths = test_paths(root);
        let config = Arc::new(ConfigStore::load_from(paths.clone()).expect("load config"));
        let instances = Arc::new(InstanceStore::load_from(paths.clone()).expect("load instances"));
        AppState::new(AppStateInit {
            app_name: "Axial".to_string(),
            version: "test".to_string(),
            config,
            instances,
            installs: Arc::new(InstallStore::new()),
            sessions: Arc::new(SessionStore::new()),
            performance: Arc::new(PerformanceManager::new().expect("performance manager")),
            startup_warnings: Vec::new(),
            frontend_dir: root.join("frontend"),
        })
    }

    fn test_app_state_with_telemetry(root: &Path) -> AppState {
        let paths = test_paths(root);
        let config_store = ConfigStore::load_from(paths.clone()).expect("load config");
        config_store
            .update(AppConfig {
                telemetry_enabled: true,
                telemetry_install_id: TEST_TELEMETRY_INSTALL_ID.to_string(),
                ..AppConfig::default()
            })
            .expect("seed telemetry config");
        let config = Arc::new(config_store);
        let instances = Arc::new(InstanceStore::load_from(paths.clone()).expect("load instances"));
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
                performance: Arc::new(PerformanceManager::new().expect("performance manager")),
                startup_warnings: Vec::new(),
                frontend_dir: root.join("frontend"),
            },
            telemetry,
        )
    }

    fn test_app_state_with_library(root: &Path) -> AppState {
        let paths = test_paths(root);
        let config_store = ConfigStore::load_from(paths.clone()).expect("load config");
        config_store
            .replace_in_memory(AppConfig {
                library_dir: paths.library_dir.to_string_lossy().to_string(),
                ..AppConfig::default()
            })
            .expect("set test library");
        let config = Arc::new(config_store);
        let instances = Arc::new(InstanceStore::load_from(paths.clone()).expect("load instances"));
        AppState::new(AppStateInit {
            app_name: "Axial".to_string(),
            version: "test".to_string(),
            config,
            instances,
            installs: Arc::new(InstallStore::new()),
            sessions: Arc::new(SessionStore::new()),
            performance: Arc::new(PerformanceManager::new().expect("performance manager")),
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
            healing: None,
            guardian: None,
            outcome: None,
            stages: Vec::new(),
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
