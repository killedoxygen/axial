use crate::logging::append_trace;
use crate::state::{AppState, LaunchStatusEvent, StartupOutcome};
use croopor_config::{AppConfig, Instance};
use croopor_launcher::{
    LaunchFailureClass, LaunchState, RecoveryAction, build_healing_summary, failure_class_name,
    format_failure_class, launch_state_name, prepare_launch_attempt, recovery_for_failure,
};
use serde_json::Value;
use tokio::process::Command;

pub(super) async fn run_launch_session(state: AppState, task: super::task::LaunchSessionTask) {
    let super::task::LaunchSessionTask {
        mut instance,
        config,
        intent,
        launched_at,
    } = task;
    let session_id = intent.session_id.clone();
    let mut attempt = croopor_launcher::service::AttemptOverrides::default();

    loop {
        trace_launch_event(&session_id, "run_launch_session entered");
        emit_status(
            &state,
            &session_id,
            LaunchState::Validating,
            None,
            None,
            None,
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
                emit_terminal_failure(
                    &state,
                    &session_id,
                    failure_class,
                    &error.message,
                    error.healing,
                )
                .await;
                return;
            }
        };
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

        if prepared.runtime.bypassed_requested_runtime {
            state
                .sessions()
                .emit_log(
                    &session_id,
                    "system",
                    "Croopor bypassed the selected Java override and will use the managed runtime."
                        .to_string(),
                )
                .await;
        }
        if prepared.runtime.effective_source == "managed" {
            emit_status(
                &state,
                &session_id,
                LaunchState::EnsuringRuntime,
                None,
                None,
                prepared.healing.clone(),
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
            emit_terminal_failure(
                &state,
                &session_id,
                LaunchFailureClass::Unknown,
                "launch plan did not produce a runnable command",
                prepared.healing.clone(),
            )
            .await;
            return;
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
        };

        let launched = match state.sessions().start_process(record, command).await {
            Ok(record) => record,
            Err(error) => {
                trace_launch_event(&session_id, &format!("spawn failed: {error}"));
                emit_terminal_failure(
                    &state,
                    &session_id,
                    LaunchFailureClass::Unknown,
                    &format!("failed to start launch process: {error}"),
                    prepared.healing.clone(),
                )
                .await;
                return;
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
                return;
            }
            StartupOutcome::Exited | StartupOutcome::Stalled => {
                let stalled = matches!(outcome, StartupOutcome::Stalled);
                if stalled {
                    let _ = state.sessions().kill(&session_id).await;
                }

                let failed_record = state.sessions().get(&session_id).await;
                let failure_class = if stalled {
                    LaunchFailureClass::StartupStalled
                } else {
                    failed_record
                        .as_ref()
                        .and_then(|record| record.failure.as_ref().map(|failure| failure.class))
                        .unwrap_or(LaunchFailureClass::Unknown)
                };
                if attempt.retry_count == 0
                    && let Some(recovery) = recovery_for_failure(
                        failure_class,
                        &intent.version_id,
                        &prepared.runtime.effective_info,
                        &intent.requested_java,
                        intent.advanced_overrides,
                        attempt.disable_custom_gc,
                        &prepared.effective_preset,
                    )
                {
                    state
                        .sessions()
                        .emit_log(&session_id, "system", recovery.description.clone())
                        .await;
                    attempt.retry_count += 1;
                    attempt.fallback_applied = Some(recovery.description.clone());
                    match recovery.action {
                        RecoveryAction::DowngradePreset(next_preset) => {
                            attempt.preset_override = Some(next_preset);
                            attempt.disable_custom_gc = false;
                        }
                        RecoveryAction::DisableCustomGc => {
                            attempt.preset_override = None;
                            attempt.disable_custom_gc = true;
                        }
                        RecoveryAction::SwitchManagedRuntime => {
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
                        advanced_overrides: intent.advanced_overrides,
                        fallback_applied: attempt.fallback_applied.as_deref(),
                        retry_count: attempt.retry_count,
                        failure_class: Some(failure_class),
                    });
                let message = if failure_class == LaunchFailureClass::StartupStalled {
                    "launch stopped before startup: no startup activity observed".to_string()
                } else {
                    format!(
                        "launch failed during startup: {}",
                        format_failure_class(failure_class)
                    )
                };
                emit_terminal_failure(&state, &session_id, failure_class, &message, healing).await;
                return;
            }
        }
    }
}

pub(super) fn trace_launch_event(session_id: &str, message: &str) {
    append_trace("launch", session_id, message);
}

async fn emit_status(
    state: &AppState,
    session_id: &str,
    launch_state: LaunchState,
    pid: Option<u32>,
    failure_class: Option<LaunchFailureClass>,
    healing: Option<croopor_launcher::LaunchHealingSummary>,
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
