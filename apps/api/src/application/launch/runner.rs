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
    GuardianStartupFailureRequest, guardian_prelaunch_preset_adjustment_directive,
    guardian_prepare_failure_outcome, guardian_startup_failure_outcome,
};
use crate::logging::append_trace;
use crate::observability::telemetry::{
    TelemetryErrorArea, TelemetryErrorKind, TelemetryErrorLevel, TelemetryEvent,
    TelemetryLaunchOutcome,
};
use crate::state::launch_reports::LaunchProofContext;
use crate::state::{AppState, StartupOutcome};
use croopor_launcher::{
    GuardianSummary, LaunchFailureClass, LaunchSessionExitReason, LaunchSessionOutcome,
    LaunchState, PreparedLaunchAttempt, build_healing_summary, prepare_launch_attempt_with_events,
};
use croopor_minecraft::download::repair_virtual_assets_from_index;
use croopor_minecraft::paths::assets_dir;
use failure::{fail_launch, fail_launch_with_outcome};
use metadata::persist_launch_metadata;
use prewarm::{format_prewarm_run_summary, prewarm_launch_plan};
use proof::persist_launch_proof_best_effort_with_context;
use recovery::{
    apply_prepare_recovery_directive, apply_startup_recovery_directive,
    block_guardian_for_suppressed_launch_recovery, block_guardian_with_user_outcome,
    plan_guardian_launch_recovery_directive, record_failed_self_healing_if_any,
    record_guardian_launch_recovery_attempt, record_prelaunch_preset_adjustment_directive,
    suppressed_launch_recovery_outcome,
};
use spawn::{
    launch_command_target, launch_spawn_failed_stage_evidence, launch_spawn_stage_evidence,
};
use status::{emit_status, launch_state_for_preparation_event, serialize_guardian};
use tokio::process::Command;

pub use failure::sanitize_live_launch_failure_message;
pub use proof::persist_launch_proof_best_effort;

const STARTUP_OBSERVATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

pub struct LaunchSuccess {
    pub session_id: String,
    pub instance_id: String,
    pub pid: u32,
    pub launched_at: String,
    pub max_memory_mb: i32,
    pub min_memory_mb: i32,
    pub healing: Option<croopor_launcher::LaunchHealingSummary>,
    pub guardian: Option<GuardianSummary>,
}

pub struct LaunchRequestError {
    pub message: String,
    pub healing: Option<croopor_launcher::LaunchHealingSummary>,
    pub guardian: Option<GuardianSummary>,
}

pub async fn launch_session(
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
    let mut attempt = croopor_launcher::service::AttemptOverrides::default();
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
                trace_launch_event(&session_id, &format!("prepare failed: {}", error.message));
                let failure_class = error.failure_class.unwrap_or(LaunchFailureClass::Unknown);
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
                    ) {
                        Ok(recovery_plan) => recovery_plan,
                        Err(_) => {
                            emit_pending_launch_failure(&state, &mut launch_completion_pending);
                            return Err(LaunchRequestError {
                                message: prepare_outcome.user_outcome.summary.clone(),
                                healing: error.healing.clone(),
                                guardian: Some(guardian.clone()),
                            });
                        }
                    };
                    let recovery_outcome = record_guardian_launch_recovery_attempt(
                        &state,
                        &session_id,
                        &recovery_plan,
                        failure_class,
                    );
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
                record_failed_self_healing_if_any(
                    &state,
                    &session_id,
                    last_recovery_plan.as_ref(),
                    failure_class,
                )
                .await;
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
                state
                    .sessions()
                    .record_stage_evidence(&session_id, launch_command_stage_evidence(&error.facts))
                    .await;
                trace_launch_event(
                    &session_id,
                    &format!("launch command preparation failed: {}", error),
                );
                record_failed_self_healing_if_any(
                    &state,
                    &session_id,
                    last_recovery_plan.as_ref(),
                    LaunchFailureClass::Unknown,
                )
                .await;
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
            trace_launch_event(
                &session_id,
                &format!("legacy virtual asset repair failed: {error}"),
            );
            record_failed_self_healing_if_any(
                &state,
                &session_id,
                last_recovery_plan.as_ref(),
                LaunchFailureClass::Unknown,
            )
            .await;
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
            session_id: croopor_launcher::SessionId(session_id.clone()),
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
                record_failed_self_healing_if_any(
                    &state,
                    &session_id,
                    last_recovery_plan.as_ref(),
                    LaunchFailureClass::Unknown,
                )
                .await;
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
                persist_launch_proof_best_effort_with_context(
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
                    ) {
                        Ok(recovery_plan) => recovery_plan,
                        Err(_) => {
                            emit_launch_completed(
                                &state,
                                &mut launch_completion_pending,
                                TelemetryLaunchOutcome::Failure,
                            );
                            return Err(LaunchRequestError {
                                message: startup_outcome.user_outcome.summary.clone(),
                                healing: startup_failure_healing(
                                    &intent,
                                    &prepared,
                                    &attempt,
                                    failure_class,
                                ),
                                guardian: Some(guardian.clone()),
                            });
                        }
                    };
                    let recovery_outcome = record_guardian_launch_recovery_attempt(
                        &state,
                        &session_id,
                        &recovery_plan,
                        failure_class,
                    );
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
                record_failed_self_healing_if_any(
                    &state,
                    &session_id,
                    last_recovery_plan.as_ref(),
                    failure_class,
                )
                .await;
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
    intent: &croopor_launcher::LaunchIntent,
    plan: &croopor_launcher::VanillaLaunchPlan,
) -> Result<(), croopor_minecraft::download::DownloadError> {
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
    mode: croopor_launcher::GuardianMode,
) -> crate::guardian::GuardianMode {
    match mode {
        croopor_launcher::GuardianMode::Managed => crate::guardian::GuardianMode::Managed,
        croopor_launcher::GuardianMode::Custom => crate::guardian::GuardianMode::Custom,
    }
}

fn startup_failure_healing(
    intent: &croopor_launcher::LaunchIntent,
    prepared: &PreparedLaunchAttempt,
    attempt: &croopor_launcher::service::AttemptOverrides,
    failure_class: LaunchFailureClass,
) -> Option<croopor_launcher::LaunchHealingSummary> {
    build_healing_summary(croopor_launcher::service::HealingSummaryInput {
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
    use crate::observability::telemetry::{DEFAULT_POSTHOG_HOST, TelemetryHub};
    use crate::state::{AppStateInit, InstallStore, LaunchEvent, SessionStore};
    use croopor_config::{AppConfig, AppPaths, ConfigStore, InstanceStore};
    use croopor_launcher::{LaunchSessionRecord, SessionId};
    use croopor_performance::PerformanceManager;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::Duration;
    use std::time::{SystemTime, UNIX_EPOCH};

    const TEST_TELEMETRY_INSTALL_ID: &str = "123e4567-e89b-12d3-a456-426614174000";
    const TEST_TELEMETRY_KEY: &str = "phc_test";

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
        state.sessions().insert(test_record(session_id)).await;
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
            app_name: "Croopor".to_string(),
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
                app_name: "Croopor".to_string(),
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
