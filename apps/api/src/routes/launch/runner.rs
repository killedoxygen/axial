use crate::logging::append_trace;
use crate::state::{AppState, LaunchStatusEvent, StartupOutcome};
use croopor_config::{AppConfig, Instance};
use croopor_launcher::{
    GuardianInterventionKind, GuardianSummary, LaunchFailureClass, LaunchState, PreLaunchAction,
    PreLaunchDecision, RecoveryAction, build_healing_summary, decide_prepare_failure,
    failure_class_name, format_failure_class, guidance_for_failure, launch_state_name,
    prepare_launch_attempt, recovery_plan_for_startup_failure,
};
use serde_json::Value;
use tokio::process::Command;

pub(super) struct LaunchSuccess {
    pub session_id: String,
    pub instance_id: String,
    pub pid: u32,
    pub launched_at: String,
    pub healing: Option<croopor_launcher::LaunchHealingSummary>,
    pub guardian: Option<GuardianSummary>,
}

pub(super) struct LaunchRequestError {
    pub message: String,
    pub healing: Option<croopor_launcher::LaunchHealingSummary>,
    pub guardian: Option<GuardianSummary>,
}

pub(super) async fn launch_session(
    state: AppState,
    task: super::task::LaunchSessionTask,
) -> Result<LaunchSuccess, LaunchRequestError> {
    let super::task::LaunchSessionTask {
        mut instance,
        config,
        intent,
        launched_at,
    } = task;
    let session_id = intent.session_id.clone();
    let mut attempt = croopor_launcher::service::AttemptOverrides::default();
    let mut guardian = GuardianSummary::new(intent.guardian.mode);

    loop {
        trace_launch_event(&session_id, "launch_session entered");
        emit_status(
            &state,
            &session_id,
            LaunchState::Validating,
            None,
            None,
            None,
            Some(guardian.clone()),
        )
        .await;
        state
            .sessions()
            .emit_log(
                &session_id,
                "system",
                format!("Preparing launch for {}.", instance.name),
            )
            .await;

        let prepared = match prepare_launch_attempt(&intent, &attempt).await {
            Ok(prepared) => prepared,
            Err(error) => {
                trace_launch_event(&session_id, &format!("prepare failed: {}", error.message));
                let failure_class = error.failure_class.unwrap_or(LaunchFailureClass::Unknown);
                match decide_prepare_failure(
                    &intent.guardian,
                    failure_class,
                    &error.message,
                    &intent.requested_java,
                    &intent.extra_jvm_args,
                    attempt.runtime_intervention_applied,
                    attempt.raw_jvm_args_intervention_applied,
                ) {
                    PreLaunchDecision::Allow => {}
                    PreLaunchDecision::Intervene {
                        action,
                        kind,
                        description,
                    } => {
                        state
                            .sessions()
                            .emit_log(&session_id, "system", description.clone())
                            .await;
                        guardian.record_intervention(kind, description.clone(), false);
                        match action {
                            PreLaunchAction::ForceManagedRuntime => {
                                attempt.record_runtime_intervention(description);
                            }
                            PreLaunchAction::StripRawJvmArgs => {
                                attempt.record_raw_jvm_args_intervention(description);
                            }
                        }
                        continue;
                    }
                    PreLaunchDecision::Block {
                        class,
                        message,
                        guidance,
                    } => {
                        guardian.block_with_guidance(guidance);
                        return Err(fail_launch(
                            &state,
                            &session_id,
                            class,
                            &message,
                            error.healing,
                            Some(guardian.clone()),
                        )
                        .await);
                    }
                }
                guardian.block_with_guidance(guidance_for_failure(failure_class, &intent.guardian));
                return Err(fail_launch(
                    &state,
                    &session_id,
                    failure_class,
                    &error.message,
                    error.healing,
                    Some(guardian.clone()),
                )
                .await);
            }
        };
        for intervention in &prepared.guardian_interventions {
            if let Some(detail) = intervention.detail.as_deref() {
                guardian.record_intervention(
                    intervention.kind,
                    detail,
                    intervention.silent.unwrap_or(false),
                );
            }
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

        if prepared.runtime.effective_source == "managed" {
            emit_status(
                &state,
                &session_id,
                LaunchState::EnsuringRuntime,
                None,
                None,
                prepared.healing.clone(),
                Some(guardian.clone()),
            )
            .await;
        }

        emit_status(
            &state,
            &session_id,
            LaunchState::Planning,
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

        if prepared.plan.command.len() < 2 {
            return Err(fail_launch(
                &state,
                &session_id,
                LaunchFailureClass::Unknown,
                "launch plan did not produce a runnable command",
                prepared.healing.clone(),
                Some(guardian.clone()),
            )
            .await);
        }

        let mut command = Command::new(&prepared.plan.command[0]);
        command.args(&prepared.plan.command[1..]);
        command.current_dir(&prepared.plan.game_dir);

        let record = crate::state::LaunchSessionRecord {
            session_id: croopor_launcher::SessionId(session_id.clone()),
            instance_id: intent.instance_id.clone(),
            version_id: intent.version_id.clone(),
            state: LaunchState::Starting,
            pid: None,
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
        };

        let launched = match state.sessions().start_process(record, command).await {
            Ok(record) => record,
            Err(error) => {
                trace_launch_event(&session_id, &format!("spawn failed: {error}"));
                return Err(fail_launch(
                    &state,
                    &session_id,
                    LaunchFailureClass::Unknown,
                    &format!("failed to start launch process: {error}"),
                    prepared.healing.clone(),
                    Some(guardian.clone()),
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
            .wait_for_startup(&session_id, std::time::Duration::from_secs(5))
            .await;

        match outcome {
            StartupOutcome::Stable | StartupOutcome::TimedOut => {
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
                    healing: prepared.healing.clone(),
                    guardian: Some(guardian.clone()),
                });
            }
            StartupOutcome::Exited | StartupOutcome::Stalled => {
                let stalled = matches!(outcome, StartupOutcome::Stalled);
                if stalled {
                    let _ = state.sessions().kill(&session_id).await;
                }

                let failure_class = if stalled {
                    LaunchFailureClass::StartupStalled
                } else {
                    state
                        .sessions()
                        .observed_failure(&session_id)
                        .await
                        .map(|failure| failure.class)
                        .unwrap_or(LaunchFailureClass::Unknown)
                };
                if !attempt.startup_recovery_applied
                    && let Some(recovery) = recovery_plan_for_startup_failure(
                        failure_class,
                        &intent.version_id,
                        &prepared.runtime.effective_info,
                        &intent.requested_java,
                        &intent.guardian,
                        attempt.disable_custom_gc,
                        &prepared.effective_preset,
                    )
                {
                    state
                        .sessions()
                        .emit_log(&session_id, "system", recovery.description.clone())
                        .await;
                    attempt.record_startup_recovery(recovery.description.clone());
                    match recovery.action {
                        RecoveryAction::DowngradePreset(next_preset) => {
                            guardian.record_intervention(
                                GuardianInterventionKind::DowngradePreset,
                                recovery.description.clone(),
                                false,
                            );
                            attempt.preset_override = Some(next_preset);
                            attempt.disable_custom_gc = false;
                        }
                        RecoveryAction::DisableCustomGc => {
                            guardian.record_intervention(
                                GuardianInterventionKind::DisableCustomGc,
                                recovery.description.clone(),
                                false,
                            );
                            attempt.preset_override = None;
                            attempt.disable_custom_gc = true;
                        }
                        RecoveryAction::SwitchManagedRuntime => {
                            guardian.record_intervention(
                                GuardianInterventionKind::SwitchManagedRuntime,
                                recovery.description.clone(),
                                false,
                            );
                            attempt.force_managed_runtime = true;
                            attempt.preset_override = None;
                            attempt.disable_custom_gc = false;
                        }
                    }
                    continue;
                }

                let healing =
                    build_healing_summary(croopor_launcher::service::HealingSummaryInput {
                        requested_java_path: &intent.requested_java,
                        requested_preset: &intent.requested_preset,
                        effective_java_path: Some(prepared.runtime.effective_path.as_str()),
                        effective_preset: Some(prepared.effective_preset.as_str()),
                        fallback_applied: attempt.fallback_applied.as_deref(),
                        retry_count: attempt.retry_count,
                        failure_class: Some(failure_class),
                    });
                guardian.block_with_guidance(guidance_for_failure(failure_class, &intent.guardian));
                let message = if failure_class == LaunchFailureClass::StartupStalled {
                    "launch stopped before startup: no startup activity observed".to_string()
                } else {
                    format!(
                        "launch failed during startup: {}",
                        format_failure_class(failure_class)
                    )
                };
                return Err(fail_launch(
                    &state,
                    &session_id,
                    failure_class,
                    &message,
                    healing,
                    Some(guardian.clone()),
                )
                .await);
            }
        }
    }
}

pub(super) fn trace_launch_event(session_id: &str, message: &str) {
    append_trace("launch", session_id, message);
}

async fn fail_launch(
    state: &AppState,
    session_id: &str,
    failure_class: LaunchFailureClass,
    message: &str,
    healing: Option<croopor_launcher::LaunchHealingSummary>,
    guardian: Option<GuardianSummary>,
) -> LaunchRequestError {
    emit_terminal_failure(
        state,
        session_id,
        failure_class,
        message,
        healing.clone(),
        guardian.clone(),
    )
    .await;
    state.sessions().remove(session_id).await;
    LaunchRequestError {
        message: message.to_string(),
        healing,
        guardian,
    }
}

async fn emit_status(
    state: &AppState,
    session_id: &str,
    launch_state: LaunchState,
    pid: Option<u32>,
    failure_class: Option<LaunchFailureClass>,
    healing: Option<croopor_launcher::LaunchHealingSummary>,
    guardian: Option<GuardianSummary>,
) {
    state
        .sessions()
        .emit_status(
            session_id,
            LaunchStatusEvent {
                state: launch_state_name(launch_state).to_string(),
                pid,
                exit_code: None,
                failure_class: failure_class.map(failure_class_name).map(str::to_string),
                failure_detail: None,
                healing: serialize_healing(healing),
                guardian: serialize_guardian(guardian),
            },
        )
        .await;
}

async fn emit_terminal_failure(
    state: &AppState,
    session_id: &str,
    failure_class: LaunchFailureClass,
    message: &str,
    healing: Option<croopor_launcher::LaunchHealingSummary>,
    guardian: Option<GuardianSummary>,
) {
    state
        .sessions()
        .emit_log(session_id, "system", message.to_string())
        .await;
    state
        .sessions()
        .emit_status(
            session_id,
            LaunchStatusEvent {
                state: "exited".to_string(),
                pid: None,
                exit_code: Some(-1),
                failure_class: Some(failure_class_name(failure_class).to_string()),
                failure_detail: Some(message.to_string()),
                healing: serialize_healing(healing),
                guardian: serialize_guardian(guardian),
            },
        )
        .await;
}

fn persist_launch_metadata(
    state: &AppState,
    instance: &mut Instance,
    config: &AppConfig,
    username: &str,
    max_memory_mb: i32,
    min_memory_mb: i32,
    launched_at: &str,
) {
    instance.last_played_at = launched_at.to_string();
    let _ = state.instances().update(instance.clone());
    let _ = state.instances().set_last_instance_id(instance.id.clone());

    let mut next = config.clone();
    next.username = username.to_string();
    if max_memory_mb > 0 {
        next.max_memory_mb = max_memory_mb;
    }
    if min_memory_mb > 0 {
        next.min_memory_mb = min_memory_mb;
    }
    let _ = state.config().update(next);
}

fn serialize_healing(healing: Option<croopor_launcher::LaunchHealingSummary>) -> Option<Value> {
    healing.and_then(|value| serde_json::to_value(value).ok())
}

fn serialize_guardian(guardian: Option<GuardianSummary>) -> Option<Value> {
    guardian.and_then(|value| serde_json::to_value(value).ok())
}
