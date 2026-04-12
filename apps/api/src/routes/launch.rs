use crate::logging::{append_trace, timestamp_utc};
use crate::state::{AppState, LaunchEvent, LaunchSessionRecord, LaunchStatusEvent, StartupOutcome};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    response::sse::{Event, Sse},
    routing::{get, post},
};
use croopor_config::{AppConfig, Instance};
use croopor_launcher::{
    LaunchFailureClass, LaunchIntent, LaunchState, RecoveryAction, SessionId,
    build_healing_summary, failure_class_name, format_failure_class, is_terminal_state,
    is_terminal_status, launch_state_name, prepare_launch_attempt, recovery_for_failure,
    snapshot_status,
};
use serde::Deserialize;
use std::{
    convert::Infallible,
    path::PathBuf,
    time::{Duration, SystemTime},
};
use tokio::process::Command;

#[derive(Debug, Deserialize)]
struct LaunchRequest {
    instance_id: String,
    username: Option<String>,
    max_memory_mb: Option<i32>,
    min_memory_mb: Option<i32>,
    client_started_at_ms: Option<i64>,
}

struct LaunchSessionTask {
    instance: Instance,
    config: AppConfig,
    intent: LaunchIntent,
    launched_at: String,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/launch", post(handle_launch))
        .route("/api/v1/launch/{id}/events", get(handle_launch_events))
        .route("/api/v1/launch/{id}/status", get(handle_launch_status))
        .route("/api/v1/launch/{id}/command", get(handle_launch_command))
        .route("/api/v1/launch/{id}/kill", post(handle_launch_kill))
}

async fn handle_launch(
    State(state): State<AppState>,
    Json(payload): Json<LaunchRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let library_dir = state.library_dir().ok_or_else(|| {
        (
            StatusCode::PRECONDITION_FAILED,
            Json(serde_json::json!({ "error": "Croopor library is not configured" })),
        )
    })?;
    let library_dir = PathBuf::from(library_dir);

    let instance = state.instances().get(&payload.instance_id).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "instance not found" })),
        )
    })?;
    if state.sessions().has_active_instance(&instance.id).await {
        return Err((
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "error": "instance already has an active session" })),
        ));
    }

    let config = state.config().current();
    let username = payload
        .username
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(config.username.as_str())
        .to_string();
    let max_memory_mb = effective_max_memory(&instance, &config, payload.max_memory_mb);
    let min_memory_mb =
        effective_min_memory(&instance, &config, payload.min_memory_mb, max_memory_mb);
    let requested_java = selected_java_override(&instance, &config);
    let requested_preset = selected_jvm_preset(&instance, &config);
    let advanced_overrides = has_advanced_overrides(&instance);
    let launched_at = timestamp_utc();
    let session_id = SessionId(generate_session_id());

    let intent = LaunchIntent {
        session_id: session_id.0.clone(),
        library_dir: library_dir.clone(),
        instance_id: instance.id.clone(),
        version_id: instance.version_id.clone(),
        username: username.clone(),
        requested_java: requested_java.clone(),
        requested_preset: requested_preset.clone(),
        extra_jvm_args: split_jvm_args(&instance.extra_jvm_args),
        max_memory_mb,
        min_memory_mb,
        resolution: selected_resolution(&instance, &config),
        launcher_name: "croopor".to_string(),
        launcher_version: state.version().to_string(),
        game_dir: Some(state.instances().game_dir(&instance.id)),
        advanced_overrides,
        performance_mode: selected_performance_mode(&instance, &config),
    };

    state
        .sessions()
        .insert(LaunchSessionRecord {
            session_id: session_id.clone(),
            instance_id: instance.id.clone(),
            version_id: instance.version_id.clone(),
            state: LaunchState::Queued,
            pid: None,
            exit_code: None,
            command: Vec::new(),
            java_path: None,
            natives_dir: None,
            failure: None,
            healing: None,
        })
        .await;
    trace_launch_event(
        &session_id.0,
        &format!(
            "launch accepted for instance {} version {} client_started_at_ms={:?}",
            instance.id, instance.version_id, payload.client_started_at_ms
        ),
    );

    let state_task = state.clone();
    let task = LaunchSessionTask {
        instance: instance.clone(),
        config: config.clone(),
        intent,
        launched_at: launched_at.clone(),
    };
    tokio::spawn(async move {
        run_launch_session(state_task, task).await;
    });

    Ok(Json(serde_json::json!({
        "status": "accepted",
        "session_id": session_id.0,
        "instance_id": instance.id,
        "pid": 0,
        "launched_at": launched_at,
        "healing": null,
    })))
}

async fn run_launch_session(state: AppState, task: LaunchSessionTask) {
    let LaunchSessionTask {
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

        let record = LaunchSessionRecord {
            session_id: SessionId(session_id.clone()),
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
            .wait_for_startup(&session_id, Duration::from_secs(5))
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

fn trace_launch_event(session_id: &str, message: &str) {
    append_trace("launch", session_id, message);
}

async fn handle_launch_events(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<
    Sse<impl futures_util::Stream<Item = Result<Event, Infallible>>>,
    (StatusCode, Json<serde_json::Value>),
> {
    let snapshot = state.sessions().get(&id).await.ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "session not found" })),
        )
    })?;
    let mut receiver = state.sessions().subscribe(&id).await.ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "session not found" })),
        )
    })?;

    let stream = async_stream::stream! {
        yield Ok(status_event(&snapshot_status(&snapshot)));
        if is_terminal_state(snapshot.state) {
            return;
        }

        loop {
            match receiver.recv().await {
                Ok(LaunchEvent::Status(status)) => {
                    let terminal = is_terminal_status(&status);
                    yield Ok(status_event(&status));
                    if terminal {
                        return;
                    }
                }
                Ok(LaunchEvent::Log(log)) => {
                    yield Ok(log_event(&log));
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            }
        }
    };

    Ok(Sse::new(stream))
}

async fn handle_launch_command(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let record = state.sessions().get(&id).await.ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "session not found" })),
        )
    })?;

    Ok(Json(serde_json::json!({
        "command": record.command,
        "java_path": record.java_path,
        "session_id": record.session_id.0,
        "healing": record.healing,
    })))
}

async fn handle_launch_status(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let record = state.sessions().get(&id).await.ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "session not found" })),
        )
    })?;

    let status = snapshot_status(&record);
    Ok(Json(serde_json::json!({
        "state": status.state,
        "pid": status.pid,
        "exit_code": status.exit_code,
        "failure_class": status.failure_class,
        "failure_detail": status.failure_detail,
        "healing": status.healing,
        "session_id": record.session_id.0,
    })))
}

async fn handle_launch_kill(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    state.sessions().kill(&id).await.map_err(|error| {
        let status = if error.kind() == std::io::ErrorKind::NotFound {
            StatusCode::NOT_FOUND
        } else {
            StatusCode::INTERNAL_SERVER_ERROR
        };
        (
            status,
            Json(serde_json::json!({ "error": error.to_string() })),
        )
    })?;

    Ok(Json(serde_json::json!({ "status": "killed" })))
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
                healing: healing.and_then(|value| serde_json::to_value(value).ok()),
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
                healing: healing.and_then(|value| serde_json::to_value(value).ok()),
            },
        )
        .await;
}

fn selected_java_override(instance: &Instance, config: &AppConfig) -> String {
    if !instance.java_path.trim().is_empty() {
        instance.java_path.trim().to_string()
    } else {
        config.java_path_override.trim().to_string()
    }
}

fn selected_jvm_preset(instance: &Instance, config: &AppConfig) -> String {
    if !instance.jvm_preset.trim().is_empty() {
        instance.jvm_preset.trim().to_string()
    } else {
        config.jvm_preset.trim().to_string()
    }
}

fn selected_performance_mode(instance: &Instance, config: &AppConfig) -> String {
    if !instance.performance_mode.trim().is_empty() {
        instance.performance_mode.trim().to_string()
    } else {
        config.performance_mode.trim().to_string()
    }
}

fn selected_resolution(instance: &Instance, config: &AppConfig) -> Option<(u32, u32)> {
    let width = if instance.window_width > 0 {
        instance.window_width
    } else {
        config.window_width
    };
    let height = if instance.window_height > 0 {
        instance.window_height
    } else {
        config.window_height
    };
    if width > 0 && height > 0 {
        Some((width as u32, height as u32))
    } else {
        None
    }
}

fn effective_max_memory(instance: &Instance, config: &AppConfig, requested: Option<i32>) -> i32 {
    if instance.max_memory_mb > 0 {
        instance.max_memory_mb
    } else if requested.unwrap_or_default() > 0 {
        requested.unwrap_or_default()
    } else {
        config.max_memory_mb
    }
}

fn effective_min_memory(
    instance: &Instance,
    config: &AppConfig,
    requested: Option<i32>,
    max_memory_mb: i32,
) -> i32 {
    let min_memory_mb = if instance.min_memory_mb > 0 {
        instance.min_memory_mb
    } else if requested.unwrap_or_default() > 0 {
        requested.unwrap_or_default()
    } else {
        config.min_memory_mb
    };
    min_memory_mb.min(max_memory_mb).max(0)
}

fn split_jvm_args(extra_jvm_args: &str) -> Vec<String> {
    extra_jvm_args
        .split_whitespace()
        .map(str::to_string)
        .collect()
}

fn has_advanced_overrides(instance: &Instance) -> bool {
    !instance.java_path.trim().is_empty()
        || !instance.jvm_preset.trim().is_empty()
        || !instance.extra_jvm_args.trim().is_empty()
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

fn generate_session_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|value| value.as_nanos())
        .unwrap_or_default();
    format!("{:032x}", nanos)
}

fn status_event(status: &LaunchStatusEvent) -> Event {
    Event::default()
        .event("status")
        .data(serde_json::to_string(status).unwrap_or_else(|_| "{}".to_string()))
}

fn log_event(log: &crate::state::LaunchLogEvent) -> Event {
    Event::default()
        .event("log")
        .data(serde_json::to_string(log).unwrap_or_else(|_| "{}".to_string()))
}
