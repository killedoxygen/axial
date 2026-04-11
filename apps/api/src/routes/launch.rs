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
    HealingEvent, HealingEventKind, LaunchFailureClass, LaunchState, RuntimeSelection, SessionId,
    VanillaLaunchRequest, boot_throttle_args, cleanup_natives_dir, gc_preset_args,
    plan_vanilla_launch, resolve_preset, resolve_runtime,
};
use croopor_minecraft::{
    JavaRuntimeInfo, JavaVersion, find_java_runtime, probe_java_runtime_info, resolve_version,
};
use serde::{Deserialize, Serialize};
use std::{
    convert::Infallible,
    path::{Path as FsPath, PathBuf},
    time::{Duration, SystemTime},
};
use tokio::process::Command;

#[derive(Debug, Deserialize)]
struct LaunchRequest {
    instance_id: String,
    username: Option<String>,
    max_memory_mb: Option<i32>,
    min_memory_mb: Option<i32>,
}

#[derive(Debug, Default, Serialize)]
struct LegacyHealingSummary {
    #[serde(skip_serializing_if = "Option::is_none")]
    requested_preset: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    effective_preset: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    requested_java_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    effective_java_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    auth_mode: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    warnings: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    fallback_applied: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    retry_count: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    failure_class: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    advanced_overrides: Option<bool>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    events: Vec<HealingEvent>,
}

#[derive(Debug, Clone)]
struct Recovery {
    description: String,
    action: RecoveryAction,
}

#[derive(Debug, Clone)]
enum RecoveryAction {
    DowngradePreset(String),
    DisableCustomGc,
    SwitchManagedRuntime,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/launch", post(handle_launch))
        .route("/api/v1/launch/{id}/events", get(handle_launch_events))
        .route("/api/v1/launch/{id}/command", get(handle_launch_command))
        .route("/api/v1/launch/{id}/kill", post(handle_launch_kill))
}

async fn handle_launch(
    State(state): State<AppState>,
    Json(payload): Json<LaunchRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let mc_dir = state.mc_dir().ok_or_else(|| {
        (
            StatusCode::PRECONDITION_FAILED,
            Json(serde_json::json!({ "error": "minecraft directory not configured" })),
        )
    })?;
    let mc_dir = PathBuf::from(mc_dir);

    let mut instance = state.instances().get(&payload.instance_id).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "instance not found" })),
        )
    })?;
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
    let launched_at = chrono::DateTime::<chrono::Utc>::from(SystemTime::now()).to_rfc3339();
    let base_extra_jvm_args = split_jvm_args(&instance.extra_jvm_args);

    let version = resolve_version(&mc_dir, &instance.version_id).map_err(internal_error)?;
    let mut runtime =
        resolve_runtime_selection(&mc_dir, &version.java_version, &requested_java, false).map_err(
            |error| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({
                        "error": format!("resolve java: {error}"),
                        "healing": build_healing_summary(HealingSummaryInput {
                            requested_java_path: &requested_java,
                            requested_preset: &requested_preset,
                            effective_java_path: None,
                            effective_preset: None,
                            advanced_overrides,
                            fallback_applied: None,
                            retry_count: 0,
                            failure_class: Some(LaunchFailureClass::JavaRuntimeMismatch),
                        })
                    })),
                )
            },
        )?;

    if runtime.effective_path.is_empty() {
        return Err(internal_error("effective runtime path is empty"));
    }
    if runtime.effective_info.major == 0 && version.java_version.major_version > 0 {
        runtime.effective_info.major = version.java_version.major_version as u32;
    }

    let loader = infer_loader(&instance.version_id);
    let is_modded = loader != "vanilla" || !version.inherits_from.trim().is_empty();
    let mut effective_preset = resolve_preset(
        &requested_preset,
        &instance.version_id,
        loader,
        is_modded,
        &runtime.effective_info,
    );

    if advanced_overrides {
        if let Err((class, message)) = validate_manual_java_override(
            &requested_java,
            &runtime,
            version.java_version.major_version,
        ) {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": message,
                    "healing": build_healing_summary(HealingSummaryInput {
                        requested_java_path: &requested_java,
                        requested_preset: &requested_preset,
                        effective_java_path: Some(runtime.effective_path.as_str()),
                        effective_preset: Some(effective_preset.as_str()),
                        advanced_overrides,
                        fallback_applied: None,
                        retry_count: 0,
                        failure_class: Some(class),
                    }),
                })),
            ));
        }
        if let Err((class, message)) =
            validate_manual_jvm_args(&base_extra_jvm_args, &runtime.effective_info)
        {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": message,
                    "healing": build_healing_summary(HealingSummaryInput {
                        requested_java_path: &requested_java,
                        requested_preset: &requested_preset,
                        effective_java_path: Some(runtime.effective_path.as_str()),
                        effective_preset: Some(effective_preset.as_str()),
                        advanced_overrides,
                        fallback_applied: None,
                        retry_count: 0,
                        failure_class: Some(class),
                    }),
                })),
            ));
        }
    }

    let mut retry_count = 0u32;
    let mut fallback_applied: Option<String> = None;
    let mut disable_custom_gc = false;

    loop {
        let session_id = SessionId(generate_session_id());
        let healing = build_healing_summary(HealingSummaryInput {
            requested_java_path: &requested_java,
            requested_preset: &requested_preset,
            effective_java_path: Some(runtime.effective_path.as_str()),
            effective_preset: Some(effective_preset.as_str()),
            advanced_overrides,
            fallback_applied: fallback_applied.as_deref(),
            retry_count,
            failure_class: None,
        });

        let mut extra_jvm_args = boot_throttle_args(runtime.effective_info.major);
        if !effective_preset.trim().is_empty() && !disable_custom_gc {
            extra_jvm_args.extend(gc_preset_args(&effective_preset, &runtime.effective_info));
        }
        extra_jvm_args.extend(base_extra_jvm_args.iter().cloned());

        let plan = plan_vanilla_launch(&VanillaLaunchRequest {
            session_id: session_id.0.clone(),
            mc_dir: mc_dir.clone(),
            version_id: instance.version_id.clone(),
            username: username.clone(),
            runtime: runtime.clone(),
            game_dir: Some(state.instances().game_dir(&instance.id)),
            launcher_name: "croopor".to_string(),
            launcher_version: state.version().to_string(),
            min_memory_mb: Some(min_memory_mb),
            max_memory_mb: Some(max_memory_mb),
            extra_jvm_args,
            resolution: selected_resolution(&instance, &config),
        })
        .map_err(internal_error)?;

        if plan.command.len() < 2 {
            return Err(internal_error(
                "launch plan did not produce a runnable command",
            ));
        }

        let mut command = Command::new(&plan.command[0]);
        command.args(&plan.command[1..]);
        command.current_dir(&plan.game_dir);

        let record = LaunchSessionRecord {
            session_id: session_id.clone(),
            instance_id: instance.id.clone(),
            version_id: instance.version_id.clone(),
            state: LaunchState::Starting,
            pid: None,
            exit_code: None,
            command: plan.command.clone(),
            java_path: Some(runtime.effective_path.clone()),
            natives_dir: plan
                .natives_dir
                .as_ref()
                .map(|path| path.to_string_lossy().to_string()),
            failure: None,
            healing: healing
                .as_ref()
                .map(|value| serde_json::to_value(value).unwrap_or_default()),
        };

        let launched = state
            .sessions()
            .start_process(record, command)
            .await
            .map_err(|error| {
                if let Some(natives_dir) = &plan.natives_dir {
                    let _ = cleanup_natives_dir(natives_dir);
                }
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({ "error": format!("failed to start launch process: {error}") })),
                )
            })?;

        let outcome = state
            .sessions()
            .wait_for_startup(&session_id.0, Duration::from_secs(5))
            .await;

        match outcome {
            StartupOutcome::Stable | StartupOutcome::TimedOut => {
                persist_launch_metadata(
                    &state,
                    &mut instance,
                    &config,
                    &username,
                    max_memory_mb,
                    min_memory_mb,
                    &launched_at,
                );

                return Ok(Json(serde_json::json!({
                    "status": "launching",
                    "session_id": launched.session_id.0,
                    "instance_id": launched.instance_id,
                    "pid": launched.pid,
                    "launched_at": launched_at,
                    "healing": launched.healing,
                })));
            }
            StartupOutcome::Exited | StartupOutcome::Stalled => {
                let stalled = matches!(outcome, StartupOutcome::Stalled);
                if stalled {
                    let _ = state.sessions().kill(&session_id.0).await;
                }

                let failed_record = state.sessions().get(&session_id.0).await;
                let failure_class = if stalled {
                    LaunchFailureClass::StartupStalled
                } else {
                    failed_record
                        .as_ref()
                        .and_then(|record| record.failure.as_ref().map(|failure| failure.class))
                        .unwrap_or(LaunchFailureClass::Unknown)
                };

                if let Some(natives_dir) = failed_record
                    .as_ref()
                    .and_then(|record| record.natives_dir.as_ref())
                {
                    let _ = cleanup_natives_dir(FsPath::new(natives_dir));
                }
                state.sessions().remove(&session_id.0).await;

                if retry_count == 0
                    && let Some(recovery) = recovery_for_failure(
                        failure_class,
                        &instance.version_id,
                        &runtime.effective_info,
                        &requested_java,
                        advanced_overrides,
                        disable_custom_gc,
                        &effective_preset,
                    )
                {
                    retry_count += 1;
                    fallback_applied = Some(recovery.description.clone());
                    match recovery.action {
                        RecoveryAction::DowngradePreset(next_preset) => {
                            effective_preset = next_preset;
                            disable_custom_gc = false;
                        }
                        RecoveryAction::DisableCustomGc => {
                            effective_preset.clear();
                            disable_custom_gc = true;
                        }
                        RecoveryAction::SwitchManagedRuntime => {
                            runtime = resolve_runtime_selection(
                                &mc_dir,
                                &version.java_version,
                                &requested_java,
                                true,
                            )
                            .map_err(|error| {
                                (
                                    StatusCode::INTERNAL_SERVER_ERROR,
                                    Json(serde_json::json!({
                                        "error": format!("resolve java: {error}")
                                    })),
                                )
                            })?;
                            if runtime.effective_info.major == 0
                                && version.java_version.major_version > 0
                            {
                                runtime.effective_info.major =
                                    version.java_version.major_version as u32;
                            }
                            effective_preset = resolve_preset(
                                &requested_preset,
                                &instance.version_id,
                                loader,
                                is_modded,
                                &runtime.effective_info,
                            );
                            disable_custom_gc = false;
                        }
                    }
                    continue;
                }

                let healing = build_healing_summary(HealingSummaryInput {
                    requested_java_path: &requested_java,
                    requested_preset: &requested_preset,
                    effective_java_path: Some(runtime.effective_path.as_str()),
                    effective_preset: Some(effective_preset.as_str()),
                    advanced_overrides,
                    fallback_applied: fallback_applied.as_deref(),
                    retry_count,
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
                return Err((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({
                        "error": message,
                        "healing": healing,
                    })),
                ));
            }
        }
    }
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

struct HealingSummaryInput<'a> {
    requested_java_path: &'a str,
    requested_preset: &'a str,
    effective_java_path: Option<&'a str>,
    effective_preset: Option<&'a str>,
    advanced_overrides: bool,
    fallback_applied: Option<&'a str>,
    retry_count: u32,
    failure_class: Option<LaunchFailureClass>,
}

fn build_healing_summary(input: HealingSummaryInput<'_>) -> Option<LegacyHealingSummary> {
    let requested_java_path = (!input.requested_java_path.trim().is_empty())
        .then(|| input.requested_java_path.to_string());
    let requested_preset = (!input.requested_preset.trim().is_empty())
        .then(|| input.requested_preset.trim().to_string());
    let effective_java_path = input
        .effective_java_path
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string);
    let effective_preset = input
        .effective_preset
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string);
    let fallback_applied = input
        .fallback_applied
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string);
    let failure_class_name = input
        .failure_class
        .map(failure_class_name)
        .map(str::to_string);

    let mut warnings = Vec::new();
    let mut events = Vec::new();

    if let Some(requested) = requested_preset.as_ref() {
        let effective = effective_preset.as_deref().unwrap_or("none");
        if requested != effective {
            let detail = format!(
                "Requested JVM preset \"{requested}\" was downgraded to \"{effective}\" for compatibility"
            );
            warnings.push(detail.clone());
            events.push(HealingEvent {
                kind: HealingEventKind::PresetDowngraded,
                detail: Some(detail),
            });
        }
    }
    if let (Some(requested), Some(effective)) =
        (requested_java_path.as_ref(), effective_java_path.as_ref())
        && requested != effective
    {
        let detail =
            "Requested Java override was bypassed in favor of a safer managed runtime".to_string();
        warnings.push(detail.clone());
        events.push(HealingEvent {
            kind: HealingEventKind::RuntimeBypassed,
            detail: Some(format!("requested={requested} effective={effective}")),
        });
    }
    if let Some(detail) = fallback_applied.as_ref() {
        events.push(HealingEvent {
            kind: HealingEventKind::FallbackApplied,
            detail: Some(detail.clone()),
        });
    }
    if matches!(
        input.failure_class,
        Some(LaunchFailureClass::StartupStalled)
    ) {
        events.push(HealingEvent {
            kind: HealingEventKind::StartupStalled,
            detail: Some("no startup activity observed".to_string()),
        });
    }

    let summary = LegacyHealingSummary {
        requested_preset,
        effective_preset,
        requested_java_path,
        effective_java_path,
        auth_mode: Some("offline".to_string()),
        warnings,
        fallback_applied,
        retry_count: (input.retry_count > 0).then_some(input.retry_count),
        failure_class: failure_class_name,
        advanced_overrides: Some(input.advanced_overrides),
        events,
    };

    if summary.requested_preset.is_none()
        && summary.effective_preset.is_none()
        && summary.requested_java_path.is_none()
        && summary.effective_java_path.is_none()
        && summary.warnings.is_empty()
        && summary.fallback_applied.is_none()
        && summary.retry_count.is_none()
        && summary.failure_class.is_none()
        && summary.events.is_empty()
        && !input.advanced_overrides
    {
        None
    } else {
        Some(summary)
    }
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

fn internal_error(error: impl std::fmt::Display) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({ "error": error.to_string() })),
    )
}

fn generate_session_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|value| value.as_nanos())
        .unwrap_or_default();
    format!("{:032x}", nanos)
}

fn snapshot_status(record: &LaunchSessionRecord) -> LaunchStatusEvent {
    LaunchStatusEvent {
        state: launch_state_name(record.state).to_string(),
        pid: record.pid,
        exit_code: record.exit_code,
        failure_class: record
            .failure
            .as_ref()
            .map(|failure| failure_class_name(failure.class).to_string()),
        failure_detail: record
            .failure
            .as_ref()
            .and_then(|failure| failure.detail.clone()),
        healing: record.healing.clone(),
    }
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

fn is_terminal_status(status: &LaunchStatusEvent) -> bool {
    matches!(status.state.as_str(), "failed" | "exited")
}

fn is_terminal_state(state: LaunchState) -> bool {
    matches!(state, LaunchState::Failed | LaunchState::Exited)
}

fn launch_state_name(state: LaunchState) -> &'static str {
    match state {
        LaunchState::Idle => "idle",
        LaunchState::Planning => "planning",
        LaunchState::Validating => "validating",
        LaunchState::Preparing => "preparing",
        LaunchState::Starting => "starting",
        LaunchState::Monitoring => "monitoring",
        LaunchState::Running => "running",
        LaunchState::Degraded => "degraded",
        LaunchState::Failed => "failed",
        LaunchState::Exited => "exited",
    }
}

fn failure_class_name(class: LaunchFailureClass) -> &'static str {
    match class {
        LaunchFailureClass::Unknown => "unknown",
        LaunchFailureClass::JvmUnsupportedOption => "jvm_unsupported_option",
        LaunchFailureClass::JvmExperimentalUnlock => "jvm_experimental_unlock_required",
        LaunchFailureClass::JvmOptionOrdering => "jvm_option_ordering",
        LaunchFailureClass::JavaRuntimeMismatch => "java_runtime_mismatch",
        LaunchFailureClass::ClasspathModuleConflict => "classpath_or_module_conflict",
        LaunchFailureClass::AuthModeIncompatible => "auth_mode_incompatible",
        LaunchFailureClass::LoaderBootstrapFailure => "loader_bootstrap_failure",
        LaunchFailureClass::StartupStalled => "startup_stalled",
    }
}

fn format_failure_class(class: LaunchFailureClass) -> &'static str {
    match class {
        LaunchFailureClass::Unknown => "unknown startup failure",
        LaunchFailureClass::JvmUnsupportedOption => "unsupported JVM option",
        LaunchFailureClass::JvmExperimentalUnlock => "experimental JVM option requires unlock",
        LaunchFailureClass::JvmOptionOrdering => "JVM option ordering conflict",
        LaunchFailureClass::JavaRuntimeMismatch => "Java runtime mismatch",
        LaunchFailureClass::ClasspathModuleConflict => "classpath or module conflict",
        LaunchFailureClass::AuthModeIncompatible => "auth mode incompatibility",
        LaunchFailureClass::LoaderBootstrapFailure => "loader bootstrap failure",
        LaunchFailureClass::StartupStalled => "startup stalled",
    }
}

fn infer_loader(version_id: &str) -> &'static str {
    let version = version_id.to_ascii_lowercase();
    if version.contains("neoforge") {
        "neoforge"
    } else if version.contains("fabric") {
        "fabric"
    } else if version.contains("forge") {
        "forge"
    } else if version.contains("quilt") {
        "quilt"
    } else {
        "vanilla"
    }
}

fn resolve_runtime_selection(
    mc_dir: &FsPath,
    java_version: &JavaVersion,
    requested_java: &str,
    force_managed: bool,
) -> Result<RuntimeSelection, croopor_minecraft::JavaRuntimeLookupError> {
    resolve_runtime::<croopor_minecraft::JavaRuntimeLookupError, _>(
        java_version,
        requested_java.to_string(),
        force_managed,
        Some(|override_value: &str| {
            let runtime = find_java_runtime(mc_dir, java_version, override_value)?;
            let info =
                probe_java_runtime_info(FsPath::new(&runtime.path), Some(&runtime.component))?;
            Ok((Some(runtime), info))
        }),
    )
    .map_err(|error| match error {
        croopor_launcher::RuntimeSelectionError::Resolve(error) => error,
        croopor_launcher::RuntimeSelectionError::MissingResolver => {
            croopor_minecraft::JavaRuntimeLookupError::Probe(
                "runtime resolver is required".to_string(),
            )
        }
    })
}

fn validate_manual_java_override(
    requested_java: &str,
    runtime: &RuntimeSelection,
    required_major: i32,
) -> Result<(), (LaunchFailureClass, String)> {
    if requested_java.trim().is_empty() || requested_java.trim() != runtime.effective_path.trim() {
        return Ok(());
    }
    if required_major > 0
        && runtime.effective_info.major > 0
        && runtime.effective_info.major as i32 != required_major
    {
        return Err((
            LaunchFailureClass::JavaRuntimeMismatch,
            format!(
                "explicit Java override targets Java {} but this version requires Java {}",
                runtime.effective_info.major, required_major
            ),
        ));
    }
    if required_major == 8
        && runtime.effective_info.major == 8
        && runtime.effective_info.update > 0
        && runtime.effective_info.update < 312
    {
        return Err((
            LaunchFailureClass::JavaRuntimeMismatch,
            format!(
                "explicit Java 8 override is too old for legacy support (8u{} detected; use 8u312 or newer)",
                runtime.effective_info.update
            ),
        ));
    }
    Ok(())
}

fn validate_manual_jvm_args(
    args: &[String],
    info: &JavaRuntimeInfo,
) -> Result<(), (LaunchFailureClass, String)> {
    if args.is_empty() {
        return Ok(());
    }
    let unlock_index = args
        .iter()
        .position(|arg| arg == "-XX:+UnlockExperimentalVMOptions");
    for (index, arg) in args.iter().enumerate() {
        match () {
            _ if arg == "-XX:+UseShenandoahGC"
                && !croopor_launcher::jvm::supports_shenandoah(info) =>
            {
                return Err((
                    LaunchFailureClass::JvmUnsupportedOption,
                    "explicit JVM args request Shenandoah on an unsupported runtime".to_string(),
                ));
            }
            _ if arg == "-XX:+UseZGC" && !croopor_launcher::jvm::supports_zgc(info) => {
                return Err((
                    LaunchFailureClass::JvmUnsupportedOption,
                    "explicit JVM args request ZGC on an unsupported runtime".to_string(),
                ));
            }
            _ if arg == "-XX:+ZGenerational"
                && !croopor_launcher::jvm::supports_generational_zgc(info) =>
            {
                return Err((
                    LaunchFailureClass::JvmUnsupportedOption,
                    "explicit JVM args request Generational ZGC on an unsupported runtime"
                        .to_string(),
                ));
            }
            _ if arg.starts_with("-XX:G1NewSizePercent=")
                || arg.starts_with("-XX:G1MaxNewSizePercent=") =>
            {
                if !croopor_launcher::jvm::supports_hotspot_tuning(info) {
                    return Err((
                        LaunchFailureClass::JvmUnsupportedOption,
                        "explicit JVM args request experimental G1 tuning on an unsupported runtime"
                            .to_string(),
                    ));
                }
                if unlock_index.is_none() {
                    return Err((
                        LaunchFailureClass::JvmExperimentalUnlock,
                        "explicit JVM args require -XX:+UnlockExperimentalVMOptions".to_string(),
                    ));
                }
                if unlock_index.is_some_and(|unlock| unlock > index) {
                    return Err((
                        LaunchFailureClass::JvmOptionOrdering,
                        "explicit JVM args place -XX:+UnlockExperimentalVMOptions after dependent flags"
                            .to_string(),
                    ));
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn recovery_for_failure(
    class: LaunchFailureClass,
    version_id: &str,
    info: &JavaRuntimeInfo,
    requested_java: &str,
    advanced_overrides: bool,
    disable_custom_gc: bool,
    effective_preset: &str,
) -> Option<Recovery> {
    if advanced_overrides {
        return None;
    }

    match class {
        LaunchFailureClass::JvmUnsupportedOption
        | LaunchFailureClass::JvmExperimentalUnlock
        | LaunchFailureClass::JvmOptionOrdering => {
            if !effective_preset.trim().is_empty() {
                let preset = conservative_healing_preset(version_id, info);
                if !preset.is_empty() && preset != effective_preset {
                    return Some(Recovery {
                        description: format!(
                            "Automatic retry: downgraded JVM preset to \"{preset}\" after startup failure"
                        ),
                        action: RecoveryAction::DowngradePreset(preset),
                    });
                }
            }
            if !disable_custom_gc {
                return Some(Recovery {
                    description: "Automatic retry: disabled custom GC flags after startup failure"
                        .to_string(),
                    action: RecoveryAction::DisableCustomGc,
                });
            }
        }
        LaunchFailureClass::JavaRuntimeMismatch => {
            if !requested_java.trim().is_empty() {
                return Some(Recovery {
                    description: "Automatic retry: switched to managed Java after runtime mismatch"
                        .to_string(),
                    action: RecoveryAction::SwitchManagedRuntime,
                });
            }
        }
        _ => {}
    }
    None
}

fn conservative_healing_preset(version_id: &str, info: &JavaRuntimeInfo) -> String {
    if info.major <= 8 || is_legacy_version_family(version_id) {
        "legacy".to_string()
    } else {
        "performance".to_string()
    }
}

fn is_legacy_version_family(version_id: &str) -> bool {
    let base = version_id.split("-forge-").next().unwrap_or(version_id);
    let numbers = base
        .split('.')
        .filter_map(|part| part.parse::<u32>().ok())
        .collect::<Vec<_>>();
    matches!(numbers.as_slice(), [1, minor, ..] if *minor <= 12)
}
