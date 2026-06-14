use super::policy;
use super::runner::trace_launch_event;
use crate::logging::timestamp_utc;
use crate::routes::{
    accounts,
    auth::{AuthRefreshFailure, refresh_active_auth},
};
use crate::state::launch_reports::{LaunchBenchmarkMetadata, LaunchProofResourceBudget};
use crate::state::{
    ActiveMinecraftAccountState, AppState, LaunchSessionRecord, LauncherAccountKind,
    LauncherAccountRecord,
};
use axum::{Json, http::StatusCode};
use croopor_config::{AppConfig, Instance, LAUNCH_AUTH_MODE_ONLINE, validate_username};
use croopor_launcher::{
    GuardianMode, GuardianSummary, LAUNCH_DISK_HEADROOM_MB, LAUNCH_MEMORY_HEADROOM_MB,
    LaunchAuthContext, LaunchCpuLoadWarningFacts, LaunchFailureClass, LaunchGuardianContext,
    LaunchIntent, LaunchReadiness, LaunchReadinessRequest, LaunchResourceWarningFacts, LaunchState,
    LaunchWarningFacts, failure_class_name, inspect_launch_readiness, summarize_launch_warnings,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::{Path, PathBuf};
use sysinfo::{Disks, ProcessRefreshKind, ProcessesToUpdate, System, get_current_pid};

#[derive(Clone, Debug, Deserialize)]
pub(super) struct LaunchRequest {
    pub instance_id: String,
    pub username: Option<String>,
    pub max_memory_mb: Option<i32>,
    pub min_memory_mb: Option<i32>,
    pub client_started_at_ms: Option<i64>,
}

pub(super) struct LaunchSessionTask {
    pub instance: Instance,
    pub config: AppConfig,
    pub intent: LaunchIntent,
    pub guardian: GuardianSummary,
    pub launched_at: String,
    pub benchmark: Option<LaunchBenchmarkMetadata>,
    pub resource_budget: Option<LaunchProofResourceBudget>,
}

pub(super) struct PreparedLaunch {
    pub task: LaunchSessionTask,
}

#[derive(Clone, Debug)]
struct LaunchPreflightFacts {
    config: AppConfig,
    max_memory_mb: i32,
    raw_min_memory_mb: i32,
    min_memory_mb: i32,
    requested_java: String,
    requested_preset: String,
    guardian: LaunchGuardianContext,
    guardian_summary: GuardianSummary,
    readiness: LaunchReadiness,
    resource_budget: LaunchProofResourceBudget,
}

#[derive(Clone, Copy)]
struct LaunchAuthRefreshOptions;

#[derive(Clone, Debug, Serialize)]
pub(super) struct LaunchPreflightResponse {
    pub status: &'static str,
    pub guardian: GuardianSummary,
    pub mode: GuardianMode,
    pub memory: LaunchPreflightMemory,
    pub overrides: LaunchPreflightOverrides,
    pub readiness: LaunchReadiness,
    pub resource_budget: LaunchPreflightResourceBudget,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct LaunchPreflightMemory {
    pub max_memory_mb: i32,
    pub min_memory_mb: i32,
    pub min_clamped: bool,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct LaunchPreflightOverrides {
    pub java: LaunchPreflightOverride,
    pub preset: LaunchPreflightOverride,
    pub raw_jvm_args: LaunchPreflightOverride,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct LaunchPreflightOverride {
    pub present: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin: Option<croopor_launcher::OverrideOrigin>,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct LaunchPreflightResourceBudget {
    pub active_session_count: usize,
    pub active_install_count: usize,
    pub active_memory_allocation_mb: u64,
    pub requested_memory_mb: Option<i32>,
    pub estimated_remaining_memory_mb: Option<i64>,
    pub memory_pressure: bool,
    pub cpu_pressure: bool,
    pub install_pressure: bool,
    pub disk_pressure: bool,
}

pub(super) async fn prepare_launch_session(
    state: &AppState,
    payload: LaunchRequest,
) -> Result<PreparedLaunch, (StatusCode, Json<serde_json::Value>)> {
    prepare_launch_session_with_auth_refresh(state, payload, None).await
}

async fn prepare_launch_session_with_auth_refresh(
    state: &AppState,
    payload: LaunchRequest,
    auth_refresh: Option<LaunchAuthRefreshOptions>,
) -> Result<PreparedLaunch, (StatusCode, Json<serde_json::Value>)> {
    let library_dir = state.library_dir().ok_or_else(|| {
        (
            StatusCode::PRECONDITION_FAILED,
            Json(json!({ "error": "Croopor library is not configured" })),
        )
    })?;
    let library_dir = PathBuf::from(library_dir);

    let instance = state.instances().get(&payload.instance_id).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "instance not found" })),
        )
    })?;
    if state.sessions().has_active_instance(&instance.id).await {
        return Err((
            StatusCode::CONFLICT,
            Json(json!({ "error": "instance already has an active session" })),
        ));
    }
    state
        .instances()
        .ensure_instance_layout(&instance.id, Some(&library_dir))
        .map_err(launch_layout_error_response)?;
    let game_dir = state.instances().game_dir(&instance.id);

    let config = state.config().current();
    let requested_offline_username = payload
        .username
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(config.username.as_str());
    let requested_offline_username = validate_username(requested_offline_username)
        .map_err(|error| (StatusCode::BAD_REQUEST, Json(json!({ "error": error }))))?;
    let active_account =
        accounts::sync_active_offline_account_from_username(state, &requested_offline_username)
            .map_err(launch_account_store_error_response)?;
    let requested_username = active_account
        .as_ref()
        .filter(|account| account.kind == LauncherAccountKind::Offline)
        .map(|account| account.display_name.as_str())
        .unwrap_or(requested_offline_username.as_str())
        .to_string();
    let offline_username = validate_username(&requested_username)
        .map_err(|error| (StatusCode::BAD_REQUEST, Json(json!({ "error": error }))))?;
    let auth = if let Some(auth_refresh) = auth_refresh {
        launch_auth_context_for_config_with_refresh(
            state,
            &config,
            active_account.as_ref(),
            &offline_username,
            auth_refresh,
        )
        .await?
    } else {
        launch_auth_context_for_config(state, &config, active_account.as_ref(), &offline_username)
            .await?
    };
    let online_launch = active_account
        .as_ref()
        .is_some_and(|account| account.kind == LauncherAccountKind::Microsoft)
        || (active_account.is_none() && config.launch_auth_mode == LAUNCH_AUTH_MODE_ONLINE);
    if online_launch {
        super::super::skin::flush_pending_saved_skin_applies_for_launch(state).await?;
    }
    let username = offline_username.clone();
    let preflight = build_launch_preflight_facts(
        state,
        &instance,
        &config,
        &library_dir,
        &game_dir,
        payload.max_memory_mb,
        payload.min_memory_mb,
    )
    .await;
    if !preflight.readiness.launchable {
        return Err(launch_readiness_error_response(preflight.readiness));
    }

    let launched_at = timestamp_utc();
    let session_id = policy::generate_session_id();

    let intent = LaunchIntent {
        session_id: session_id.0.clone(),
        library_dir: library_dir.clone(),
        instance_id: instance.id.clone(),
        version_id: instance.version_id.clone(),
        username: username.clone(),
        auth,
        requested_java: preflight.requested_java.clone(),
        requested_preset: preflight.requested_preset.clone(),
        extra_jvm_args: policy::split_jvm_args(&instance.extra_jvm_args),
        max_memory_mb: preflight.max_memory_mb,
        min_memory_mb: preflight.min_memory_mb,
        resolution: policy::selected_resolution(&instance, &config),
        launcher_name: "croopor".to_string(),
        launcher_version: state.version().to_string(),
        game_dir: Some(game_dir),
        guardian: preflight.guardian.clone(),
        performance_mode: policy::selected_performance_mode(&instance, &config),
    };

    state
        .sessions()
        .insert(LaunchSessionRecord {
            session_id: session_id.clone(),
            instance_id: instance.id.clone(),
            version_id: instance.version_id.clone(),
            launched_at: Some(launched_at.clone()),
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
            guardian: serde_json::to_value(&preflight.guardian_summary).ok(),
            stages: Vec::new(),
        })
        .await;
    trace_launch_event(
        &session_id.0,
        &format!(
            "launch requested for instance {} version {} client_started_at_ms={:?}",
            instance.id, instance.version_id, payload.client_started_at_ms
        ),
    );

    Ok(PreparedLaunch {
        task: LaunchSessionTask {
            instance: instance.clone(),
            config: preflight.config.clone(),
            intent,
            guardian: preflight.guardian_summary,
            launched_at,
            benchmark: None,
            resource_budget: Some(preflight.resource_budget),
        },
    })
}

fn launch_layout_error_response(
    _error: impl std::fmt::Display,
) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({
            "error": "Could not prepare the instance folder. Check app data permissions and try again."
        })),
    )
}

fn launch_readiness_error_response(
    readiness: LaunchReadiness,
) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::PRECONDITION_FAILED,
        Json(json!({
            "error": "Installed version is not ready to launch",
            "readiness": readiness,
        })),
    )
}

pub(super) async fn prepare_launch_preflight(
    state: &AppState,
    instance_id: String,
) -> Result<LaunchPreflightResponse, (StatusCode, Json<serde_json::Value>)> {
    let library_dir = state.library_dir().ok_or_else(|| {
        (
            StatusCode::PRECONDITION_FAILED,
            Json(json!({ "error": "Croopor library is not configured" })),
        )
    })?;
    let library_dir = PathBuf::from(library_dir);

    let instance = state.instances().get(&instance_id).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "instance not found" })),
        )
    })?;
    let game_dir = state.instances().game_dir(&instance.id);
    let config = state.config().current();
    let facts = build_launch_preflight_facts(
        state,
        &instance,
        &config,
        &library_dir,
        &game_dir,
        None,
        None,
    )
    .await;

    Ok(facts.into_response())
}

async fn build_launch_preflight_facts(
    state: &AppState,
    instance: &Instance,
    config: &AppConfig,
    library_dir: &Path,
    game_dir: &Path,
    requested_max_memory_mb: Option<i32>,
    requested_min_memory_mb: Option<i32>,
) -> LaunchPreflightFacts {
    let memory_evidence = capture_launch_memory_evidence();
    let memory_defaults = policy::derived_launch_memory_defaults(
        instance,
        config,
        requested_max_memory_mb,
        requested_min_memory_mb,
        memory_evidence.host_total_memory_mb,
    );
    let max_memory_mb =
        policy::effective_max_memory(instance, config, requested_max_memory_mb, memory_defaults);
    let raw_min_memory_mb =
        policy::selected_raw_min_memory(instance, config, requested_min_memory_mb, memory_defaults);
    let min_memory_mb = policy::effective_min_memory(
        instance,
        config,
        requested_min_memory_mb,
        max_memory_mb,
        memory_defaults,
    );
    let requested_java = policy::selected_java_override(instance, config);
    let requested_preset = policy::selected_jvm_preset(instance, config);
    let guardian = LaunchGuardianContext {
        mode: policy::selected_guardian_mode(config),
        java_override_origin: policy::java_override_origin(instance, config),
        preset_override_origin: policy::preset_override_origin(instance, config),
        raw_jvm_args_origin: policy::raw_jvm_args_origin(instance),
    };
    let resource_budget = capture_resource_budget_snapshot(
        memory_evidence,
        capture_launch_disk_evidence([library_dir, game_dir]),
        capture_launch_cpu_load_evidence(),
        host_cpu_threads(),
        ActiveLaunchResourceUse {
            session_count: state.sessions().active_session_count().await,
            install_count: state.installs().active_install_count().await,
            memory_allocation_mb: state.sessions().active_memory_allocation_mb().await,
        },
        max_memory_mb,
    );
    let guardian_summary = guardian_summary_for_preflight(
        raw_min_memory_mb,
        max_memory_mb,
        &resource_budget,
        &guardian,
    );
    let readiness = inspect_launch_readiness(&LaunchReadinessRequest {
        library_dir: library_dir.to_path_buf(),
        version_id: instance.version_id.clone(),
        requested_java: requested_java.clone(),
        guardian_mode: guardian.mode,
    });

    LaunchPreflightFacts {
        config: config.clone(),
        max_memory_mb,
        raw_min_memory_mb,
        min_memory_mb,
        requested_java,
        requested_preset,
        guardian,
        guardian_summary,
        readiness,
        resource_budget,
    }
}

async fn launch_auth_context_for_config(
    state: &AppState,
    config: &AppConfig,
    active_account: Option<&LauncherAccountRecord>,
    offline_username: &str,
) -> Result<LaunchAuthContext, (StatusCode, Json<serde_json::Value>)> {
    launch_auth_context_for_config_with_refresh(
        state,
        config,
        active_account,
        offline_username,
        LaunchAuthRefreshOptions,
    )
    .await
}

async fn launch_auth_context_for_config_with_refresh(
    state: &AppState,
    config: &AppConfig,
    active_account: Option<&LauncherAccountRecord>,
    offline_username: &str,
    auth_refresh: LaunchAuthRefreshOptions,
) -> Result<LaunchAuthContext, (StatusCode, Json<serde_json::Value>)> {
    if let Some(account) = active_account {
        return match account.kind {
            LauncherAccountKind::Offline => Ok(LaunchAuthContext::offline(&account.display_name)),
            LauncherAccountKind::Microsoft => {
                let Some(login_id) = account.login_id.as_deref() else {
                    return Err(online_auth_refresh_unavailable_response(
                        "refresh_failed",
                        "account_login_missing",
                    ));
                };
                match state.auth_logins().switch_active_account(login_id).await {
                    Ok(true) => {}
                    Ok(false) => {
                        return Err(online_auth_refresh_unavailable_response(
                            "refresh_failed",
                            "account_auth_missing",
                        ));
                    }
                    Err(_) => {
                        return Err(online_auth_refresh_unavailable_response(
                            "refresh_failed",
                            "account_selection_failed",
                        ));
                    }
                }
                online_launch_auth_context_with_refresh(state, auth_refresh).await
            }
        };
    }

    if config.launch_auth_mode != LAUNCH_AUTH_MODE_ONLINE {
        return Ok(LaunchAuthContext::offline(offline_username));
    }

    online_launch_auth_context_with_refresh(state, auth_refresh).await
}

async fn online_launch_auth_context_with_refresh(
    state: &AppState,
    _auth_refresh: LaunchAuthRefreshOptions,
) -> Result<LaunchAuthContext, (StatusCode, Json<serde_json::Value>)> {
    if let Some(auth) = state
        .auth_logins()
        .active_current_minecraft_account_state()
        .await
        .and_then(online_launch_auth_context)
    {
        return Ok(auth);
    }

    refresh_active_auth(state.auth_logins())
        .await
        .map_err(online_auth_refresh_failure_response)?;

    state
        .auth_logins()
        .active_current_minecraft_account_state()
        .await
        .and_then(online_launch_auth_context)
        .ok_or_else(|| {
            online_auth_refresh_unavailable_response("refresh_failed", "refreshed_account_unusable")
        })
}

fn launch_account_store_error_response(
    error: std::io::Error,
) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({
            "error": error.to_string(),
            "failure_class": failure_class_name(LaunchFailureClass::AuthModeIncompatible),
            "status": "account_persistence_failed",
        })),
    )
}

fn online_launch_auth_context(
    account_state: ActiveMinecraftAccountState,
) -> Option<LaunchAuthContext> {
    let account = account_state.account;
    if !account.owns_minecraft_java
        || account.access_token.trim().is_empty()
        || account.profile.name.trim().is_empty()
        || account.profile.id.trim().is_empty()
    {
        return None;
    }

    Some(LaunchAuthContext {
        player_name: account.profile.name,
        uuid: account.profile.id,
        access_token: account.access_token,
        client_id: String::new(),
        xuid: String::new(),
        user_type: "msa".to_string(),
    })
}

fn online_auth_refresh_failure_response(
    error: AuthRefreshFailure,
) -> (StatusCode, Json<serde_json::Value>) {
    online_auth_unavailable_response_with_refresh(Some((
        error.launch_status_id(),
        error.launch_reason_id(),
    )))
}

fn online_auth_refresh_unavailable_response(
    refresh_status: &'static str,
    refresh_reason: &'static str,
) -> (StatusCode, Json<serde_json::Value>) {
    online_auth_unavailable_response_with_refresh(Some((refresh_status, refresh_reason)))
}

fn online_auth_unavailable_response_with_refresh(
    refresh: Option<(&'static str, &'static str)>,
) -> (StatusCode, Json<serde_json::Value>) {
    let mut response = json!({
        "error": "Online launch requires an active verified Minecraft Java account",
        "failure_class": failure_class_name(LaunchFailureClass::AuthModeIncompatible),
        "launch_auth_mode": LAUNCH_AUTH_MODE_ONLINE,
        "online_mode_ready": false,
    });
    if let Some((refresh_status, refresh_reason)) = refresh {
        response["auth_refresh_status"] = json!(refresh_status);
        response["auth_refresh_reason"] = json!(refresh_reason);
    }

    (StatusCode::PRECONDITION_FAILED, Json(response))
}

fn guardian_summary_for_preflight(
    raw_min_memory_mb: i32,
    max_memory_mb: i32,
    resource_budget: &LaunchProofResourceBudget,
    guardian: &LaunchGuardianContext,
) -> GuardianSummary {
    summarize_launch_warnings(
        guardian,
        &LaunchWarningFacts {
            raw_min_memory_mb,
            max_memory_mb,
            resource: launch_resource_warning_facts(resource_budget),
        },
    )
}

impl LaunchPreflightFacts {
    fn into_response(self) -> LaunchPreflightResponse {
        LaunchPreflightResponse {
            status: "ready",
            mode: self.guardian.mode,
            guardian: self.guardian_summary,
            memory: LaunchPreflightMemory {
                max_memory_mb: self.max_memory_mb,
                min_memory_mb: self.min_memory_mb,
                min_clamped: self.raw_min_memory_mb > self.max_memory_mb,
            },
            overrides: LaunchPreflightOverrides {
                java: LaunchPreflightOverride::from_origin(self.guardian.java_override_origin),
                preset: LaunchPreflightOverride::from_origin(self.guardian.preset_override_origin),
                raw_jvm_args: LaunchPreflightOverride::from_origin(
                    self.guardian.raw_jvm_args_origin,
                ),
            },
            readiness: self.readiness,
            resource_budget: LaunchPreflightResourceBudget::from_budget(&self.resource_budget),
        }
    }
}

impl LaunchPreflightOverride {
    fn from_origin(origin: Option<croopor_launcher::OverrideOrigin>) -> Self {
        Self {
            present: origin.is_some(),
            origin,
        }
    }
}

impl LaunchPreflightResourceBudget {
    fn from_budget(resource_budget: &LaunchProofResourceBudget) -> Self {
        Self {
            active_session_count: resource_budget.active_session_count,
            active_install_count: resource_budget.active_install_count,
            active_memory_allocation_mb: resource_budget.active_memory_allocation_mb,
            requested_memory_mb: resource_budget.requested_memory_mb,
            estimated_remaining_memory_mb: resource_budget.estimated_remaining_memory_mb,
            memory_pressure: resource_budget.memory_pressure,
            cpu_pressure: resource_budget.cpu_pressure,
            install_pressure: resource_budget.install_pressure,
            disk_pressure: resource_budget.disk_pressure,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct LaunchMemoryEvidence {
    host_total_memory_mb: Option<u64>,
    host_available_memory_mb: Option<u64>,
    host_used_memory_mb: Option<u64>,
    launcher_process_memory_mb: Option<u64>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct LaunchDiskEvidence {
    launch_disk_available_mb: Option<u64>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct LaunchCpuLoadEvidence {
    host_cpu_load_1m_x100: Option<u64>,
    host_cpu_load_5m_x100: Option<u64>,
    host_cpu_load_15m_x100: Option<u64>,
}

fn capture_launch_memory_evidence() -> LaunchMemoryEvidence {
    let mut system = System::new();
    system.refresh_memory();
    let launcher_process_memory_mb = current_process_memory_mb(&mut system);

    LaunchMemoryEvidence {
        host_total_memory_mb: bytes_to_positive_mb(system.total_memory()),
        host_available_memory_mb: bytes_to_positive_mb(system.available_memory()),
        host_used_memory_mb: bytes_to_positive_mb(system.used_memory()),
        launcher_process_memory_mb,
    }
}

fn current_process_memory_mb(system: &mut System) -> Option<u64> {
    let pid = get_current_pid().ok()?;
    let process_refresh = ProcessRefreshKind::nothing().with_memory().without_tasks();
    system.refresh_processes_specifics(ProcessesToUpdate::Some(&[pid]), true, process_refresh);
    system
        .process(pid)
        .and_then(|process| bytes_to_positive_mb(process.memory()))
}

fn bytes_to_positive_mb(value: u64) -> Option<u64> {
    let value = value / (1024 * 1024);
    (value > 0).then_some(value)
}

fn capture_launch_disk_evidence<'a>(
    candidate_paths: impl IntoIterator<Item = &'a Path>,
) -> LaunchDiskEvidence {
    let disks = Disks::new_with_refreshed_list();
    let launch_disk_available_mb = candidate_paths
        .into_iter()
        .filter_map(|path| disk_available_mb_for_path(&disks, path))
        .min();

    LaunchDiskEvidence {
        launch_disk_available_mb,
    }
}

fn capture_launch_cpu_load_evidence() -> LaunchCpuLoadEvidence {
    #[cfg(unix)]
    {
        let load = System::load_average();
        LaunchCpuLoadEvidence {
            host_cpu_load_1m_x100: load_to_x100(load.one),
            host_cpu_load_5m_x100: load_to_x100(load.five),
            host_cpu_load_15m_x100: load_to_x100(load.fifteen),
        }
    }

    #[cfg(not(unix))]
    {
        LaunchCpuLoadEvidence::default()
    }
}

#[cfg(unix)]
fn load_to_x100(value: f64) -> Option<u64> {
    if value.is_finite() && value >= 0.0 {
        Some((value * 100.0).round().clamp(0.0, u64::MAX as f64) as u64)
    } else {
        None
    }
}

fn disk_available_mb_for_path(disks: &Disks, path: &Path) -> Option<u64> {
    let path = path.canonicalize().ok()?;
    disks
        .list()
        .iter()
        .filter_map(|disk| {
            let mount_point = disk.mount_point().canonicalize().ok()?;
            path.starts_with(&mount_point).then(|| {
                (
                    mount_point.components().count(),
                    disk.available_space() / (1024 * 1024),
                )
            })
        })
        .max_by_key(|(mount_depth, _)| *mount_depth)
        .map(|(_, available_mb)| available_mb)
}

fn host_cpu_threads() -> Option<usize> {
    std::thread::available_parallelism().ok().map(usize::from)
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct ActiveLaunchResourceUse {
    session_count: usize,
    install_count: usize,
    memory_allocation_mb: u64,
}

fn capture_resource_budget_snapshot(
    memory_evidence: LaunchMemoryEvidence,
    disk_evidence: LaunchDiskEvidence,
    cpu_load_evidence: LaunchCpuLoadEvidence,
    host_cpu_threads: Option<usize>,
    active: ActiveLaunchResourceUse,
    requested_allocation_mb: i32,
) -> LaunchProofResourceBudget {
    let requested_memory_mb = positive_i32(requested_allocation_mb);
    let warning_facts = LaunchResourceWarningFacts {
        host_total_memory_mb: memory_evidence.host_total_memory_mb,
        host_cpu_threads,
        cpu_load: LaunchCpuLoadWarningFacts {
            host_cpu_load_1m_x100: cpu_load_evidence.host_cpu_load_1m_x100,
            host_cpu_load_5m_x100: cpu_load_evidence.host_cpu_load_5m_x100,
            host_cpu_load_15m_x100: cpu_load_evidence.host_cpu_load_15m_x100,
        },
        active_session_count: active.session_count,
        active_install_count: active.install_count,
        active_memory_allocation_mb: active.memory_allocation_mb,
        requested_memory_mb,
        launch_disk_available_mb: disk_evidence.launch_disk_available_mb,
        memory_headroom_mb: LAUNCH_MEMORY_HEADROOM_MB,
        launch_disk_headroom_mb: LAUNCH_DISK_HEADROOM_MB,
    };
    LaunchProofResourceBudget {
        host_total_memory_mb: memory_evidence.host_total_memory_mb,
        host_available_memory_mb: memory_evidence.host_available_memory_mb,
        host_used_memory_mb: memory_evidence.host_used_memory_mb,
        host_cpu_threads,
        host_cpu_load_1m_x100: cpu_load_evidence.host_cpu_load_1m_x100,
        host_cpu_load_5m_x100: cpu_load_evidence.host_cpu_load_5m_x100,
        host_cpu_load_15m_x100: cpu_load_evidence.host_cpu_load_15m_x100,
        launcher_process_memory_mb: memory_evidence.launcher_process_memory_mb,
        active_session_count: active.session_count,
        active_install_count: active.install_count,
        active_memory_allocation_mb: active.memory_allocation_mb,
        requested_memory_mb,
        estimated_remaining_memory_mb: estimated_remaining_memory_mb(
            memory_evidence.host_total_memory_mb,
            active.memory_allocation_mb,
            requested_memory_mb,
        ),
        memory_headroom_mb: LAUNCH_MEMORY_HEADROOM_MB,
        memory_pressure: warning_facts.memory_pressure(),
        cpu_pressure: warning_facts.cpu_pressure(),
        install_pressure: warning_facts.install_pressure(),
        launch_disk_available_mb: disk_evidence.launch_disk_available_mb,
        launch_disk_headroom_mb: LAUNCH_DISK_HEADROOM_MB,
        disk_pressure: warning_facts.disk_pressure(),
    }
}

fn launch_resource_warning_facts(
    resource_budget: &LaunchProofResourceBudget,
) -> LaunchResourceWarningFacts {
    LaunchResourceWarningFacts {
        host_total_memory_mb: resource_budget.host_total_memory_mb,
        host_cpu_threads: resource_budget.host_cpu_threads,
        cpu_load: LaunchCpuLoadWarningFacts {
            host_cpu_load_1m_x100: resource_budget.host_cpu_load_1m_x100,
            host_cpu_load_5m_x100: resource_budget.host_cpu_load_5m_x100,
            host_cpu_load_15m_x100: resource_budget.host_cpu_load_15m_x100,
        },
        active_session_count: resource_budget.active_session_count,
        active_install_count: resource_budget.active_install_count,
        active_memory_allocation_mb: resource_budget.active_memory_allocation_mb,
        requested_memory_mb: resource_budget.requested_memory_mb,
        launch_disk_available_mb: resource_budget.launch_disk_available_mb,
        memory_headroom_mb: resource_budget.memory_headroom_mb,
        launch_disk_headroom_mb: resource_budget.launch_disk_headroom_mb,
    }
}

fn estimated_remaining_memory_mb(
    total_memory_mb: Option<u64>,
    active_allocation_mb: u64,
    requested_allocation_mb: Option<i32>,
) -> Option<i64> {
    let requested_allocation_mb = u64::try_from(requested_allocation_mb?).ok()?;
    // Signed estimate preserves overcommit amount instead of saturating negative headroom to zero.
    let remaining = i128::from(total_memory_mb?)
        - i128::from(active_allocation_mb)
        - i128::from(requested_allocation_mb);
    Some(remaining.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64)
}

fn positive_i32(value: i32) -> Option<i32> {
    (value > 0).then_some(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{
        AppStateInit, AuthLoginMinecraftProfile, InstallStore, NewAuthLoginMinecraftAccount,
        NewAuthLoginMsaToken, SessionStore,
    };
    use axum::Json;
    use croopor_config::{AppConfig, AppPaths, ConfigStore, InstanceStore};
    use croopor_launcher::{GuardianDecision, LaunchReadinessReasonId, OverrideOrigin, SessionId};
    use croopor_performance::PerformanceManager;
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::Arc,
    };

    #[test]
    fn launch_layout_error_response_keeps_500_and_hides_unix_path_material() {
        let (status, Json(body)) = launch_layout_error_response(
            "/Users/alice/Library/Application Support/Croopor/instances/survival: Permission denied (os error 13)",
        );

        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            body,
            serde_json::json!({
                "error": "Could not prepare the instance folder. Check app data permissions and try again."
            })
        );
        let body = body.to_string();
        for fragment in [
            "/Users/alice",
            "Library/Application Support",
            "instances/survival",
            "Permission denied",
            "os error 13",
        ] {
            assert!(
                !body.contains(fragment),
                "launch layout error exposed raw fragment {fragment}"
            );
        }
    }

    #[test]
    fn launch_layout_error_response_hides_windows_path_material_and_raw_io_text() {
        let (status, Json(body)) = launch_layout_error_response(
            r"C:\Users\Alice\AppData\Roaming\Croopor\instances\creative: Access is denied. Read-only file system (os error 5)",
        );

        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            body["error"],
            "Could not prepare the instance folder. Check app data permissions and try again."
        );
        let body = body.to_string();
        for fragment in [
            r"C:\Users\Alice",
            "AppData",
            r"instances\creative",
            "Access is denied",
            "Read-only file system",
            "os error 5",
        ] {
            assert!(
                !body.contains(fragment),
                "launch layout error exposed raw fragment {fragment}"
            );
        }
    }

    #[tokio::test]
    async fn prepare_launch_session_ensures_instance_layout_before_building_intent() {
        let fixture = TestFixture::new("prepare-ensures-layout");
        fixture.write_ready_install("1.21.1");
        fs::write(
            fixture.paths.library_dir.join("options.txt"),
            "shared options",
        )
        .expect("write options");
        fs::write(
            fixture.paths.library_dir.join("servers.dat"),
            "shared servers",
        )
        .expect("write servers");
        let instance_id = fixture.add_instance("Survival", "1.21.1");
        let game_dir = fixture.state.instances().game_dir(&instance_id);
        let _ = fs::remove_dir_all(game_dir.join("screenshots"));
        let _ = fs::remove_dir_all(game_dir.join("logs"));

        let prepared = prepare_launch_session(
            &fixture.state,
            LaunchRequest {
                instance_id: instance_id.clone(),
                username: None,
                max_memory_mb: None,
                min_memory_mb: None,
                client_started_at_ms: None,
            },
        )
        .await
        .expect("prepare launch session");

        assert_eq!(prepared.task.intent.game_dir, Some(game_dir.clone()));
        assert_eq!(prepared.task.intent.auth.player_name, "Player");
        assert_eq!(
            prepared.task.intent.auth.uuid,
            croopor_minecraft::offline_uuid("Player")
        );
        assert_eq!(prepared.task.intent.auth.access_token, "null");
        assert_eq!(prepared.task.intent.auth.user_type, "legacy");
        assert!(game_dir.join("screenshots").is_dir());
        assert!(game_dir.join("logs").is_dir());
        assert_eq!(
            fs::read_to_string(game_dir.join("options.txt")).expect("read options"),
            "shared options"
        );
        assert_eq!(
            fs::read_to_string(game_dir.join("servers.dat")).expect("read servers"),
            "shared servers"
        );
    }

    #[tokio::test]
    async fn prepare_launch_session_syncs_active_offline_account_from_config_username() {
        let fixture = TestFixture::new("prepare-syncs-offline-account-name");
        fixture.write_ready_install("1.21.1");
        fixture
            .state
            .accounts()
            .create_offline_account("OldName")
            .expect("create offline account");
        let mut config = fixture.state.config().current();
        config.username = "NewName".to_string();
        fixture
            .state
            .config()
            .replace_in_memory(config)
            .expect("set config username");
        let instance_id = fixture.add_instance("Survival", "1.21.1");

        let prepared = prepare_launch_session(
            &fixture.state,
            LaunchRequest {
                instance_id,
                username: None,
                max_memory_mb: None,
                min_memory_mb: None,
                client_started_at_ms: None,
            },
        )
        .await
        .expect("prepare launch session");

        assert_eq!(prepared.task.intent.auth.player_name, "NewName");
        assert_eq!(prepared.task.intent.username, "NewName");
        let active = fixture
            .state
            .accounts()
            .active_account()
            .expect("active account")
            .expect("active account");
        assert_eq!(active.display_name, "NewName");
    }

    #[tokio::test]
    async fn prepare_launch_session_rejects_invalid_offline_request_username_as_bad_request() {
        let fixture = TestFixture::new("prepare-invalid-offline-name");
        fixture
            .state
            .accounts()
            .create_offline_account("LocalUser")
            .expect("create offline account");
        let instance_id = fixture.add_instance("Survival", "1.21.1");

        let error = match prepare_launch_session(
            &fixture.state,
            LaunchRequest {
                instance_id,
                username: Some("Bad Name!".to_string()),
                max_memory_mb: None,
                min_memory_mb: None,
                client_started_at_ms: None,
            },
        )
        .await
        {
            Ok(_) => panic!("invalid username should fail"),
            Err(error) => error,
        };

        assert_eq!(error.0, StatusCode::BAD_REQUEST);
        assert_eq!(
            error.1.0["error"],
            "Letters, numbers, and underscores only."
        );
    }

    #[tokio::test]
    async fn prepare_launch_session_uses_online_auth_context_from_active_minecraft_account() {
        let fixture = TestFixture::new("prepare-online-auth");
        fixture.set_launch_auth_mode("online");
        let instance_id = fixture.add_instance("Survival", "1.21.1");
        fixture.add_active_minecraft_account(true).await;
        let prepared = fixture
            .prepare(instance_id, None)
            .await
            .expect("prepare launch session");

        assert_eq!(prepared.task.config.username, "Player");
        assert_eq!(prepared.task.intent.username, "Player");
        assert_eq!(prepared.task.intent.auth.player_name, "ProfileName");
        assert_eq!(
            prepared.task.intent.auth.uuid,
            "4f9c7f7d0b1245d9a5c2f03a8c120001"
        );
        assert_eq!(
            prepared.task.intent.auth.access_token,
            "minecraft-access-token"
        );
        assert_eq!(prepared.task.intent.auth.user_type, "msa");
        assert_eq!(prepared.task.intent.auth.client_id, "");
        assert_eq!(prepared.task.intent.auth.xuid, "");
    }

    #[tokio::test]
    async fn prepare_launch_session_rejects_online_auth_missing_refresh_token_boundedly() {
        let fixture = TestFixture::new("prepare-online-auth-no-refresh");
        fixture.set_launch_auth_mode("online");
        let instance_id = fixture.add_instance("Survival", "1.21.1");
        let error = match fixture.prepare(instance_id, None).await {
            Ok(_) => panic!("online auth without refresh token should fail"),
            Err(error) => error,
        };

        assert_eq!(error.0, StatusCode::PRECONDITION_FAILED);
        assert_eq!(error.1.0["failure_class"], "auth_mode_incompatible");
        assert_eq!(error.1.0["launch_auth_mode"], "online");
        assert_eq!(error.1.0["online_mode_ready"], false);
        assert_eq!(error.1.0["auth_refresh_status"], "sign_in_required");
        assert_eq!(error.1.0["auth_refresh_reason"], "refresh_token_missing");
        assert_launch_error_is_token_safe(&error.1.0);
    }

    #[tokio::test]
    async fn prepare_launch_session_rejects_online_auth_without_verified_account_boundedly() {
        let fixture = TestFixture::new("prepare-online-auth-missing");
        fixture.set_launch_auth_mode("online");
        let instance_id = fixture.add_instance("Survival", "1.21.1");

        let error = match fixture.prepare(instance_id.clone(), None).await {
            Ok(_) => panic!("online auth without account should fail"),
            Err(error) => error,
        };

        assert_eq!(error.0, StatusCode::PRECONDITION_FAILED);
        assert_eq!(error.1.0["failure_class"], "auth_mode_incompatible");
        assert_eq!(error.1.0["launch_auth_mode"], "online");
        assert_eq!(error.1.0["online_mode_ready"], false);
        let text = error.1.0.to_string();
        for material in [
            "minecraft-access-token",
            "msa-access-token",
            "provider-secret-payload",
        ] {
            assert!(
                !text.contains(material),
                "public launch error exposed sensitive material {material}"
            );
        }
        assert!(
            !fixture
                .state
                .sessions()
                .has_active_instance(&instance_id)
                .await
        );
    }

    #[tokio::test]
    async fn prepare_launch_session_rejects_online_auth_without_java_ownership() {
        let fixture = TestFixture::new("prepare-online-auth-unowned");
        fixture.set_launch_auth_mode("online");
        let instance_id = fixture.add_instance("Survival", "1.21.1");
        fixture.add_active_minecraft_account(false).await;

        let error = match fixture.prepare(instance_id, None).await {
            Ok(_) => panic!("online auth without ownership should fail"),
            Err(error) => error,
        };

        assert_eq!(error.0, StatusCode::PRECONDITION_FAILED);
        assert_eq!(error.1.0["failure_class"], "auth_mode_incompatible");
        assert_eq!(error.1.0["online_mode_ready"], false);
        let text = error.1.0.to_string();
        assert!(!text.contains("minecraft-access-token"));
    }

    #[tokio::test]
    async fn prepare_launch_session_rejects_same_instance_active_launch() {
        let fixture = TestFixture::new("prepare-active-conflict");
        let instance_id = fixture.add_instance("Survival", "1.21.1");
        fixture
            .state
            .sessions()
            .insert(LaunchSessionRecord {
                session_id: SessionId("active-session".to_string()),
                instance_id: instance_id.clone(),
                version_id: "1.21.1".to_string(),
                launched_at: Some(timestamp_utc()),
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
                stages: Vec::new(),
            })
            .await;

        let error = match prepare_launch_session(
            &fixture.state,
            LaunchRequest {
                instance_id,
                username: None,
                max_memory_mb: None,
                min_memory_mb: None,
                client_started_at_ms: None,
            },
        )
        .await
        {
            Ok(_) => panic!("active instance should conflict"),
            Err(error) => error,
        };

        assert_eq!(error.0, StatusCode::CONFLICT);
        assert_eq!(
            error.1.0,
            serde_json::json!({ "error": "instance already has an active session" })
        );
    }

    #[tokio::test]
    async fn omitted_request_memory_uses_backend_derived_defaults_for_fresh_builtin_global() {
        let fixture = TestFixture::new("prepare-derived-memory-defaults");
        let instance_id = fixture.add_instance("Modded", "fabric-loader-0.16.10-1.21.1");
        let config = fixture.state.config().current();
        let expected_defaults = policy::derived_launch_memory_defaults(
            &fixture
                .state
                .instances()
                .get(&instance_id)
                .expect("instance"),
            &config,
            None,
            None,
            capture_launch_memory_evidence().host_total_memory_mb,
        );

        let prepared = fixture
            .prepare_with_memory(instance_id, None, None)
            .await
            .expect("prepare launch session");

        if let Some(defaults) = expected_defaults {
            assert_eq!(prepared.task.intent.max_memory_mb, defaults.max_memory_mb);
            assert_eq!(prepared.task.intent.min_memory_mb, defaults.min_memory_mb);
            assert_ne!(
                prepared.task.intent.min_memory_mb,
                AppConfig::default().min_memory_mb
            );
        } else {
            assert_eq!(prepared.task.intent.max_memory_mb, config.max_memory_mb);
            assert_eq!(prepared.task.intent.min_memory_mb, config.min_memory_mb);
        }
    }

    #[tokio::test]
    async fn launch_preflight_ready_payload_for_managed_instance_does_not_create_session() {
        let fixture = TestFixture::new("preflight-managed-ready");
        let instance_id = fixture.add_instance("Survival", "1.21.1");

        let preflight = prepare_launch_preflight(&fixture.state, instance_id.clone())
            .await
            .expect("prepare preflight");

        assert_eq!(preflight.status, "ready");
        assert_eq!(preflight.mode, GuardianMode::Managed);
        assert_eq!(preflight.guardian.mode, GuardianMode::Managed);
        assert!(!preflight.overrides.java.present);
        assert_eq!(preflight.overrides.java.origin, None);
        assert!(!preflight.overrides.preset.present);
        assert_eq!(preflight.overrides.preset.origin, None);
        assert!(!preflight.overrides.raw_jvm_args.present);
        assert_eq!(preflight.overrides.raw_jvm_args.origin, None);
        assert!(preflight.memory.max_memory_mb > 0);
        assert!(preflight.memory.min_memory_mb >= 0);
        assert!(!preflight.memory.min_clamped);
        assert!(!preflight.readiness.launchable);
        assert_readiness_reason(&preflight, LaunchReadinessReasonId::VersionJsonMissing);
        assert_eq!(fixture.state.sessions().active_session_count().await, 0);
        assert!(
            !fixture
                .state
                .sessions()
                .has_active_instance(&instance_id)
                .await
        );
    }

    #[tokio::test]
    async fn launch_preflight_readiness_reports_missing_version_json() {
        let fixture = TestFixture::new("preflight-readiness-missing-version-json");
        let instance_id = fixture.add_instance("Survival", "1.21.1");

        let preflight = prepare_launch_preflight(&fixture.state, instance_id)
            .await
            .expect("prepare preflight");

        assert!(!preflight.readiness.launchable);
        assert_eq!(preflight.readiness.reasons.len(), 1);
        assert_readiness_reason(&preflight, LaunchReadinessReasonId::VersionJsonMissing);
    }

    #[tokio::test]
    async fn launch_preflight_readiness_reports_missing_client_jar() {
        let fixture = TestFixture::new("preflight-readiness-missing-client-jar");
        fixture.write_version_json(
            "1.21.1",
            serde_json::json!({
                "id": "1.21.1",
                "type": "release",
                "mainClass": "net.minecraft.client.main.Main",
                "assetIndex": {},
                "javaVersion": { "component": "java-runtime-delta", "majorVersion": 21 },
                "libraries": []
            }),
        );
        let instance_id = fixture.add_instance("Survival", "1.21.1");

        let preflight = prepare_launch_preflight(&fixture.state, instance_id)
            .await
            .expect("prepare preflight");

        assert!(!preflight.readiness.launchable);
        assert_readiness_reason(&preflight, LaunchReadinessReasonId::ClientJarMissing);
        assert_readiness_reason(&preflight, LaunchReadinessReasonId::ManagedRuntimeMissing);
    }

    #[tokio::test]
    async fn launch_preflight_readiness_reports_incomplete_install_marker() {
        let fixture = TestFixture::new("preflight-readiness-incomplete-install");
        fixture.write_ready_install("1.21.1");
        fs::write(
            fixture
                .paths
                .library_dir
                .join("versions")
                .join("1.21.1")
                .join(".incomplete"),
            b"installing",
        )
        .expect("incomplete marker");
        let instance_id = fixture.add_instance("Survival", "1.21.1");

        let preflight = prepare_launch_preflight(&fixture.state, instance_id)
            .await
            .expect("prepare preflight");

        assert!(!preflight.readiness.launchable);
        assert_eq!(preflight.readiness.reasons.len(), 1);
        assert_readiness_reason(&preflight, LaunchReadinessReasonId::IncompleteInstall);
    }

    #[tokio::test]
    async fn prepare_launch_session_rejects_incomplete_install_without_session() {
        let fixture = TestFixture::new("prepare-rejects-incomplete-install");
        fixture.write_ready_install("1.21.1");
        fs::write(
            fixture
                .paths
                .library_dir
                .join("versions")
                .join("1.21.1")
                .join(".incomplete"),
            b"installing",
        )
        .expect("incomplete marker");
        let instance_id = fixture.add_instance("Survival", "1.21.1");

        let error = match prepare_launch_session(
            &fixture.state,
            LaunchRequest {
                instance_id: instance_id.clone(),
                username: None,
                max_memory_mb: None,
                min_memory_mb: None,
                client_started_at_ms: None,
            },
        )
        .await
        {
            Ok(_) => panic!("incomplete install should not queue"),
            Err(error) => error,
        };

        assert_eq!(error.0, StatusCode::PRECONDITION_FAILED);
        assert_eq!(
            error.1.0["error"],
            "Installed version is not ready to launch"
        );
        assert_eq!(error.1.0["readiness"]["launchable"], false);
        assert_eq!(
            error.1.0["readiness"]["reasons"][0]["id"],
            "incomplete_install"
        );
        assert_eq!(fixture.state.sessions().active_session_count().await, 0);
        assert!(
            !fixture
                .state
                .sessions()
                .has_active_instance(&instance_id)
                .await
        );
        let payload = error.1.0.to_string();
        assert!(!payload.contains(&fixture.root.to_string_lossy().to_string()));
        assert!(!payload.contains(".incomplete"));
    }

    #[tokio::test]
    async fn prepare_launch_session_rejects_incomplete_parent_without_session() {
        let fixture = TestFixture::new("prepare-rejects-incomplete-parent");
        fixture.write_ready_install("1.21.1");
        fixture.write_child_version("fabric-loader-0.16.10-1.21.1", "1.21.1");
        fs::write(
            fixture
                .paths
                .library_dir
                .join("versions")
                .join("1.21.1")
                .join(".incomplete"),
            b"installing",
        )
        .expect("incomplete marker");
        let instance_id = fixture.add_instance("Modded", "fabric-loader-0.16.10-1.21.1");

        let error = match prepare_launch_session(
            &fixture.state,
            LaunchRequest {
                instance_id: instance_id.clone(),
                username: None,
                max_memory_mb: None,
                min_memory_mb: None,
                client_started_at_ms: None,
            },
        )
        .await
        {
            Ok(_) => panic!("incomplete parent install should not queue"),
            Err(error) => error,
        };

        assert_eq!(error.0, StatusCode::PRECONDITION_FAILED);
        assert_eq!(
            error.1.0["readiness"]["reasons"][0]["id"],
            "incomplete_install"
        );
        assert_eq!(fixture.state.sessions().active_session_count().await, 0);
        assert!(
            !fixture
                .state
                .sessions()
                .has_active_instance(&instance_id)
                .await
        );
    }

    #[tokio::test]
    async fn launch_preflight_custom_override_warns_with_bounded_override_payload() {
        let fixture = TestFixture::new("preflight-custom-bounded");
        fixture.set_guardian_mode("custom");
        let instance_id = fixture.add_instance("Survival", "1.21.1");
        fixture.update_instance(&instance_id, |instance| {
            instance.java_path = "/Users/SecretUser/.jdks/manual/bin/java".to_string();
            instance.extra_jvm_args = "-Dtoken=secret-token -XX:+UseZGC".to_string();
        });

        let preflight = prepare_launch_preflight(&fixture.state, instance_id)
            .await
            .expect("prepare preflight");

        assert_eq!(preflight.status, "ready");
        assert_eq!(preflight.mode, GuardianMode::Custom);
        assert_eq!(preflight.guardian.decision, GuardianDecision::Warned);
        assert_eq!(
            preflight.overrides.java.origin,
            Some(OverrideOrigin::Instance)
        );
        assert_eq!(
            preflight.overrides.raw_jvm_args.origin,
            Some(OverrideOrigin::Instance)
        );
        assert_readiness_reason(&preflight, LaunchReadinessReasonId::JavaOverrideMissing);
        assert!(preflight.guardian.guidance.iter().any(|detail| detail
            == "Guardian Custom mode will keep the selected Java override for this launch."));
        assert!(preflight.guardian.guidance.iter().any(|detail| detail
            == "Guardian Custom mode will keep explicit JVM args; remove them first if startup becomes unstable."));

        let payload = serde_json::to_string(&preflight).expect("serialize preflight");
        assert!(!payload.contains("/Users/SecretUser"));
        assert!(!payload.contains("manual/bin/java"));
        assert!(!payload.contains("-Dtoken"));
        assert!(!payload.contains("secret-token"));
        assert!(!payload.contains("requested_java"));
        assert!(!payload.contains("requested_preset"));
        assert!(!payload.contains("java_path"));
        assert!(!payload.contains("command"));
        assert!(!payload.contains("username"));
        for reason in &preflight.readiness.reasons {
            assert!(!reason.message.contains("/Users/SecretUser"));
            assert!(!reason.message.contains("manual/bin/java"));
            assert!(!reason.message.contains("secret-token"));
        }
    }

    #[tokio::test]
    async fn launch_preflight_memory_clamp_warning_is_reflected() {
        let fixture = TestFixture::new("preflight-memory-clamp");
        let instance_id = fixture.add_instance("Survival", "1.21.1");
        fixture.update_instance(&instance_id, |instance| {
            instance.max_memory_mb = 1024;
            instance.min_memory_mb = 2048;
        });

        let preflight = prepare_launch_preflight(&fixture.state, instance_id)
            .await
            .expect("prepare preflight");

        assert_eq!(preflight.memory.max_memory_mb, 1024);
        assert_eq!(preflight.memory.min_memory_mb, 1024);
        assert!(preflight.memory.min_clamped);
        assert_eq!(preflight.guardian.decision, GuardianDecision::Warned);
        assert_has_memory_clamp_warning(&preflight.guardian);
    }

    #[tokio::test]
    async fn launch_preflight_resource_warning_path_is_reflected() {
        let fixture = TestFixture::new("preflight-resource-warning");
        let instance_id = fixture.add_instance("Survival", "1.21.1");

        let preflight = prepare_launch_preflight(&fixture.state, instance_id)
            .await
            .expect("prepare preflight");

        assert_eq!(
            preflight.resource_budget.active_session_count,
            fixture.state.sessions().active_session_count().await
        );
        assert_eq!(
            preflight.resource_budget.active_install_count,
            fixture.state.installs().active_install_count().await
        );
        assert_eq!(
            preflight.resource_budget.requested_memory_mb,
            Some(preflight.memory.max_memory_mb)
        );
    }

    #[tokio::test]
    async fn custom_mode_with_java_override_warns_before_queue() {
        let fixture = TestFixture::new("prepare-custom-java-warning");
        fixture.set_guardian_mode("custom");
        let instance_id = fixture.add_instance("Survival", "1.21.1");
        let java_path = fixture.write_manual_java_override();
        fixture.update_instance(&instance_id, |instance| {
            instance.java_path = java_path.clone();
        });

        let prepared = fixture
            .prepare(instance_id.clone(), None)
            .await
            .expect("prepare launch session");

        assert_eq!(prepared.task.guardian.decision, GuardianDecision::Warned);
        assert_eq!(prepared.task.intent.requested_java, java_path);
        assert_eq!(
            prepared.task.intent.guardian.java_override_origin,
            Some(OverrideOrigin::Instance)
        );
        assert!(prepared.task.guardian.guidance.iter().any(|detail| detail
            == "Guardian Custom mode will keep the selected Java override for this launch."));
        assert!(prepared.task.guardian.guidance.iter().any(|detail| detail
            == "Switch Guardian back to Managed if you want Croopor to adjust unsafe choices."));
    }

    #[tokio::test]
    async fn custom_mode_with_raw_jvm_args_warns_before_queue() {
        let fixture = TestFixture::new("prepare-custom-jvm-warning");
        fixture.set_guardian_mode("custom");
        let instance_id = fixture.add_instance("Survival", "1.21.1");
        fixture.update_instance(&instance_id, |instance| {
            instance.extra_jvm_args = "-XX:+UseZGC -Ddemo=true".to_string();
        });

        let prepared = fixture
            .prepare(instance_id.clone(), None)
            .await
            .expect("prepare launch session");

        assert_eq!(prepared.task.guardian.decision, GuardianDecision::Warned);
        assert_eq!(
            prepared.task.intent.extra_jvm_args,
            vec!["-XX:+UseZGC", "-Ddemo=true"]
        );
        assert_eq!(
            prepared.task.intent.guardian.raw_jvm_args_origin,
            Some(OverrideOrigin::Instance)
        );
        assert!(
            prepared
                .task
                .guardian
                .guidance
                .iter()
                .any(|detail| detail
                    == "Guardian Custom mode will keep explicit JVM args; remove them first if startup becomes unstable.")
        );
    }

    #[tokio::test]
    async fn custom_mode_with_instance_jvm_preset_warns_before_queue() {
        let fixture = TestFixture::new("prepare-custom-preset-warning");
        fixture.set_guardian_mode("custom");
        let instance_id = fixture.add_instance("Survival", "1.21.1");
        fixture.update_instance(&instance_id, |instance| {
            instance.jvm_preset = "graalvm".to_string();
        });

        let prepared = fixture
            .prepare(instance_id.clone(), None)
            .await
            .expect("prepare launch session");

        assert_eq!(prepared.task.guardian.decision, GuardianDecision::Warned);
        assert_eq!(prepared.task.intent.requested_preset, "graalvm");
        assert_eq!(
            prepared.task.intent.guardian.preset_override_origin,
            Some(OverrideOrigin::Instance)
        );
        assert!(prepared.task.guardian.guidance.iter().any(|detail| detail
            == "Guardian Custom mode will keep the selected JVM preset for this launch."));
        assert!(prepared.task.guardian.details.iter().any(|detail| detail
            == "Guardian Custom mode will keep the selected JVM preset for this launch."));
    }

    #[tokio::test]
    async fn custom_mode_with_global_jvm_preset_warns_before_queue() {
        let fixture = TestFixture::new("prepare-custom-global-preset-warning");
        fixture.set_guardian_mode("custom");
        fixture.set_global_jvm_preset("performance");
        let instance_id = fixture.add_instance("Survival", "1.21.1");

        let prepared = fixture
            .prepare(instance_id.clone(), None)
            .await
            .expect("prepare launch session");

        assert_eq!(prepared.task.guardian.decision, GuardianDecision::Warned);
        assert_eq!(prepared.task.intent.requested_preset, "performance");
        assert_eq!(
            prepared.task.intent.guardian.preset_override_origin,
            Some(OverrideOrigin::Global)
        );
        assert!(prepared.task.guardian.guidance.iter().any(|detail| detail
            == "Guardian Custom mode will keep the selected JVM preset for this launch."));
    }

    #[tokio::test]
    async fn managed_mode_with_manual_overrides_skips_custom_warning_at_queue_time() {
        let fixture = TestFixture::new("prepare-managed-overrides-no-custom-warning");
        fixture.set_guardian_mode("managed");
        let instance_id = fixture.add_instance("Survival", "1.21.1");
        let java_path = fixture.write_manual_java_override();
        fixture.update_instance(&instance_id, |instance| {
            instance.java_path = java_path.clone();
            instance.jvm_preset = "graalvm".to_string();
            instance.extra_jvm_args = "-XX:+UseZGC".to_string();
        });

        let prepared = fixture
            .prepare(instance_id.clone(), None)
            .await
            .expect("prepare launch session");

        assert!(
            !prepared
                .task
                .guardian
                .guidance
                .iter()
                .any(|detail| detail.starts_with("Guardian Custom mode will keep"))
        );
        assert!(
            !prepared
                .task
                .guardian
                .details
                .iter()
                .any(|detail| detail.starts_with("Guardian Custom mode will keep"))
        );
        assert_eq!(prepared.task.intent.requested_java, java_path);
        assert_eq!(prepared.task.intent.requested_preset, "graalvm");
        assert_eq!(prepared.task.intent.extra_jvm_args, vec!["-XX:+UseZGC"]);
    }

    #[tokio::test]
    async fn instance_min_above_max_warns_and_clamps_intent_min_to_max() {
        let fixture = TestFixture::new("prepare-instance-memory-clamp-warning");
        let instance_id = fixture.add_instance("Survival", "1.21.1");
        fixture.update_instance(&instance_id, |instance| {
            instance.max_memory_mb = 1024;
            instance.min_memory_mb = 2048;
        });

        let prepared = fixture
            .prepare(instance_id.clone(), None)
            .await
            .expect("prepare launch session");

        assert_eq!(prepared.task.intent.max_memory_mb, 1024);
        assert_eq!(prepared.task.intent.min_memory_mb, 1024);
        assert_eq!(prepared.task.guardian.decision, GuardianDecision::Warned);
        assert_has_memory_clamp_warning(&prepared.task.guardian);
    }

    #[tokio::test]
    async fn request_min_above_request_max_warns_for_api_callers() {
        let fixture = TestFixture::new("prepare-request-memory-clamp-warning");
        let instance_id = fixture.add_instance("Survival", "1.21.1");

        let prepared = fixture
            .prepare_with_memory(instance_id.clone(), Some(1024), Some(2048))
            .await
            .expect("prepare launch session");

        assert_eq!(prepared.task.intent.max_memory_mb, 1024);
        assert_eq!(prepared.task.intent.min_memory_mb, 1024);
        assert_eq!(prepared.task.guardian.decision, GuardianDecision::Warned);
        assert_has_memory_clamp_warning(&prepared.task.guardian);
    }

    #[tokio::test]
    async fn normal_min_at_or_below_max_does_not_add_clamp_warning() {
        let fixture = TestFixture::new("prepare-no-memory-clamp-warning");
        let instance_id = fixture.add_instance("Survival", "1.21.1");

        let prepared = fixture
            .prepare_with_memory(instance_id.clone(), Some(4096), Some(1024))
            .await
            .expect("prepare launch session");

        assert_eq!(prepared.task.intent.max_memory_mb, 4096);
        assert_eq!(prepared.task.intent.min_memory_mb, 1024);
        assert_no_memory_clamp_warning(&prepared.task.guardian);
    }

    #[tokio::test]
    async fn low_max_memory_warns_without_changing_intent_memory_values() {
        let fixture = TestFixture::new("prepare-low-max-memory-warning");
        let instance_id = fixture.add_instance("Survival", "1.21.1");

        let prepared = fixture
            .prepare_with_memory(instance_id.clone(), Some(1024), Some(512))
            .await
            .expect("prepare launch session");

        assert_eq!(prepared.task.intent.max_memory_mb, 1024);
        assert_eq!(prepared.task.intent.min_memory_mb, 512);
        assert_eq!(prepared.task.guardian.decision, GuardianDecision::Warned);
        assert_has_low_memory_allocation_warning(&prepared.task.guardian, 1024);
        assert_no_memory_clamp_warning(&prepared.task.guardian);
    }

    #[tokio::test]
    async fn memory_clamp_warning_merges_with_custom_override_warning() {
        let fixture = TestFixture::new("prepare-memory-clamp-custom-merged-warning");
        fixture.set_guardian_mode("custom");
        let instance_id = fixture.add_instance("Survival", "1.21.1");
        let java_path = fixture.write_manual_java_override();
        fixture.update_instance(&instance_id, |instance| {
            instance.java_path = java_path;
        });

        let prepared = fixture
            .prepare_with_memory(instance_id.clone(), Some(1024), Some(2048))
            .await
            .expect("prepare launch session");

        assert_eq!(prepared.task.intent.max_memory_mb, 1024);
        assert_eq!(prepared.task.intent.min_memory_mb, 1024);
        assert_eq!(prepared.task.guardian.decision, GuardianDecision::Warned);
        assert_has_memory_clamp_warning(&prepared.task.guardian);
        assert!(prepared.task.guardian.guidance.iter().any(|detail| detail
            == "Guardian Custom mode will keep the selected Java override for this launch."));
        assert!(prepared.task.guardian.details.iter().any(|detail| detail
            == "Guardian Custom mode will keep the selected Java override for this launch."));
    }

    #[tokio::test]
    async fn low_memory_warning_merges_with_custom_override_warning() {
        let fixture = TestFixture::new("prepare-low-memory-custom-merged-warning");
        fixture.set_guardian_mode("custom");
        let instance_id = fixture.add_instance("Survival", "1.21.1");
        let java_path = fixture.write_manual_java_override();
        fixture.update_instance(&instance_id, |instance| {
            instance.java_path = java_path;
        });

        let prepared = fixture
            .prepare_with_memory(instance_id.clone(), Some(1024), Some(512))
            .await
            .expect("prepare launch session");

        assert_eq!(prepared.task.intent.max_memory_mb, 1024);
        assert_eq!(prepared.task.intent.min_memory_mb, 512);
        assert_eq!(prepared.task.guardian.decision, GuardianDecision::Warned);
        assert_has_low_memory_allocation_warning(&prepared.task.guardian, 1024);
        assert!(prepared.task.guardian.guidance.iter().any(|detail| detail
            == "Guardian Custom mode will keep the selected Java override for this launch."));
        assert!(prepared.task.guardian.details.iter().any(|detail| detail
            == "Guardian Custom mode will keep the selected Java override for this launch."));
    }

    #[tokio::test]
    async fn memory_warning_and_custom_override_warning_merge_before_queue() {
        let fixture = TestFixture::new("prepare-merged-warning");
        fixture.set_guardian_mode("custom");
        let instance_id = fixture.add_instance("Survival", "1.21.1");
        let java_path = fixture.write_manual_java_override();
        fixture.update_instance(&instance_id, |instance| {
            instance.java_path = java_path;
        });

        let prepared = fixture
            .prepare(instance_id.clone(), Some(i32::MAX))
            .await
            .expect("prepare launch session");

        let resource_budget = prepared
            .task
            .resource_budget
            .as_ref()
            .expect("resource budget snapshot");
        assert_eq!(resource_budget.active_session_count, 0);
        assert_eq!(resource_budget.active_install_count, 0);
        assert_eq!(resource_budget.active_memory_allocation_mb, 0);
        assert_eq!(resource_budget.requested_memory_mb, Some(i32::MAX));
        assert!(resource_budget.memory_pressure);
        assert!(!resource_budget.install_pressure);
        assert_eq!(prepared.task.guardian.decision, GuardianDecision::Warned);
        for expected in [
            "Launch memory budget is tight: active sessions plus this launch may leave less than 2 GB for the OS.",
            "Guardian Custom mode will keep the selected Java override for this launch.",
            "Switch Guardian back to Managed if you want Croopor to adjust unsafe choices.",
        ] {
            assert!(
                prepared
                    .task
                    .guardian
                    .guidance
                    .iter()
                    .any(|detail| detail == expected),
                "missing guidance: {expected}"
            );
            assert!(
                prepared
                    .task
                    .guardian
                    .details
                    .iter()
                    .any(|detail| detail == expected),
                "missing detail: {expected}"
            );
        }
    }

    #[tokio::test]
    async fn resource_budget_warnings_merge_with_existing_guardian_guidance_before_queue() {
        let fixture = TestFixture::new("prepare-resource-merged-warning");
        fixture.set_guardian_mode("custom");
        let instance_id = fixture.add_instance("Survival", "1.21.1");
        let java_path = fixture.write_manual_java_override();
        fixture.update_instance(&instance_id, |instance| {
            instance.java_path = java_path;
        });
        for index in 0..4 {
            fixture
                .add_active_launch(&format!("active-launch-{index}"), 1024)
                .await;
        }
        fixture.add_active_install("active-install").await;

        let prepared = fixture
            .prepare(instance_id.clone(), Some(i32::MAX))
            .await
            .expect("prepare launch session");

        let resource_budget = prepared
            .task
            .resource_budget
            .as_ref()
            .expect("resource budget snapshot");
        assert_eq!(resource_budget.active_session_count, 4);
        assert_eq!(resource_budget.active_install_count, 1);
        assert_eq!(resource_budget.active_memory_allocation_mb, 4096);
        assert_eq!(resource_budget.requested_memory_mb, Some(i32::MAX));
        assert!(resource_budget.memory_pressure);
        assert!(resource_budget.install_pressure);
        assert_eq!(prepared.task.guardian.decision, GuardianDecision::Warned);
        for expected in [
            "Launch memory budget is tight: active sessions plus this launch may leave less than 2 GB for the OS.",
            "Multiple launches can saturate low-end CPUs; wait for another launch to finish if startup feels sluggish.",
            "Active install/download sessions: 1. Launching now can add disk and network pressure during startup.",
            "Guardian Custom mode will keep the selected Java override for this launch.",
        ] {
            assert!(
                prepared
                    .task
                    .guardian
                    .guidance
                    .iter()
                    .any(|detail| detail == expected),
                "missing guidance: {expected}"
            );
        }
        assert!(
            prepared
                .task
                .guardian
                .guidance
                .iter()
                .any(|detail| detail.starts_with("Launch concurrency may be tight:"))
        );
    }

    #[test]
    fn resource_budget_snapshot_marks_pressure_flags_and_signed_remaining_memory() {
        let pressured = test_budget_with_memory_and_disk(
            LaunchMemoryEvidence {
                host_total_memory_mb: Some(8192),
                host_available_memory_mb: Some(1536),
                host_used_memory_mb: Some(6656),
                launcher_process_memory_mb: Some(128),
            },
            LaunchDiskEvidence {
                launch_disk_available_mb: Some(1024),
            },
            LaunchCpuLoadEvidence {
                host_cpu_load_1m_x100: Some(142),
                host_cpu_load_5m_x100: Some(81),
                host_cpu_load_15m_x100: Some(43),
            },
            Some(4),
            ActiveLaunchResourceUse {
                session_count: 1,
                install_count: 1,
                memory_allocation_mb: 3072,
            },
            4096,
        );

        assert_eq!(pressured.host_total_memory_mb, Some(8192));
        assert_eq!(pressured.host_available_memory_mb, Some(1536));
        assert_eq!(pressured.host_used_memory_mb, Some(6656));
        assert_eq!(pressured.host_cpu_threads, Some(4));
        assert_eq!(pressured.host_cpu_load_1m_x100, Some(142));
        assert_eq!(pressured.host_cpu_load_5m_x100, Some(81));
        assert_eq!(pressured.host_cpu_load_15m_x100, Some(43));
        assert_eq!(pressured.launcher_process_memory_mb, Some(128));
        assert_eq!(pressured.active_session_count, 1);
        assert_eq!(pressured.active_install_count, 1);
        assert_eq!(pressured.active_memory_allocation_mb, 3072);
        assert_eq!(pressured.requested_memory_mb, Some(4096));
        assert_eq!(pressured.estimated_remaining_memory_mb, Some(1024));
        assert_eq!(pressured.memory_headroom_mb, LAUNCH_MEMORY_HEADROOM_MB);
        assert!(pressured.memory_pressure);
        assert!(pressured.cpu_pressure);
        assert!(pressured.install_pressure);
        assert_eq!(pressured.launch_disk_available_mb, Some(1024));
        assert_eq!(pressured.launch_disk_headroom_mb, LAUNCH_DISK_HEADROOM_MB);
        assert!(pressured.disk_pressure);

        let overcommitted = test_budget_with_memory(
            LaunchMemoryEvidence {
                host_total_memory_mb: Some(4096),
                ..LaunchMemoryEvidence::default()
            },
            Some(16),
            0,
            0,
            1024,
            8192,
        );
        assert_eq!(overcommitted.estimated_remaining_memory_mb, Some(-5120));
        assert!(overcommitted.memory_pressure);
        assert!(!overcommitted.cpu_pressure);
        assert!(!overcommitted.install_pressure);
        assert_eq!(overcommitted.launch_disk_available_mb, None);
        assert_eq!(
            overcommitted.launch_disk_headroom_mb,
            LAUNCH_DISK_HEADROOM_MB
        );
        assert!(!overcommitted.disk_pressure);

        let cpu_load_pressured = test_budget_with_memory_and_disk(
            LaunchMemoryEvidence::default(),
            LaunchDiskEvidence::default(),
            LaunchCpuLoadEvidence {
                host_cpu_load_1m_x100: Some(1520),
                ..LaunchCpuLoadEvidence::default()
            },
            Some(16),
            ActiveLaunchResourceUse::default(),
            4096,
        );
        assert!(cpu_load_pressured.cpu_pressure);
    }

    #[test]
    fn cpu_load_conversion_is_instant_and_optional() {
        assert_eq!(load_to_x100(0.0), Some(0));
        assert_eq!(load_to_x100(0.424), Some(42));
        assert_eq!(load_to_x100(0.425), Some(43));
        assert_eq!(load_to_x100(12.5), Some(1250));
        assert_eq!(load_to_x100(f64::NAN), None);
        assert_eq!(load_to_x100(f64::INFINITY), None);
        assert_eq!(load_to_x100(-0.1), None);
    }

    struct TestFixture {
        state: AppState,
        paths: AppPaths,
        root: PathBuf,
    }

    impl TestFixture {
        fn new(name: &str) -> Self {
            let root = test_root(name);
            let paths = test_paths(&root);
            fs::create_dir_all(&paths.library_dir).expect("create library dir");
            let config = Arc::new(ConfigStore::load_from(paths.clone()).expect("load config"));
            config
                .replace_in_memory(AppConfig {
                    library_dir: paths.library_dir.to_string_lossy().to_string(),
                    ..AppConfig::default()
                })
                .expect("set library dir");
            let instances =
                Arc::new(InstanceStore::load_from(paths.clone()).expect("load instances"));
            let state = AppState::new(AppStateInit {
                app_name: "Croopor".to_string(),
                version: "test".to_string(),
                config,
                instances,
                installs: Arc::new(InstallStore::new()),
                sessions: Arc::new(SessionStore::new()),
                performance: Arc::new(PerformanceManager::new().expect("performance manager")),
                startup_warnings: Vec::new(),
                frontend_dir: root.join("frontend"),
            });

            Self { state, paths, root }
        }

        fn add_instance(&self, name: &str, version_id: &str) -> String {
            self.state
                .instances()
                .add(
                    name.to_string(),
                    version_id.to_string(),
                    String::new(),
                    String::new(),
                    None,
                )
                .expect("add instance")
                .id
        }

        fn write_ready_install(&self, version_id: &str) {
            self.write_version_json(
                version_id,
                serde_json::json!({
                    "id": version_id,
                    "type": "release",
                    "mainClass": "net.minecraft.client.main.Main",
                    "assetIndex": {},
                    "javaVersion": { "component": "java-runtime-delta", "majorVersion": 21 },
                    "libraries": []
                }),
            );
            let version_dir = self.paths.library_dir.join("versions").join(version_id);
            fs::write(version_dir.join(format!("{version_id}.jar")), b"client jar")
                .expect("write client jar");
            self.write_ready_runtime("java-runtime-delta");
        }

        fn write_child_version(&self, version_id: &str, parent_id: &str) {
            self.write_version_json(
                version_id,
                serde_json::json!({
                    "id": version_id,
                    "inheritsFrom": parent_id,
                    "type": "release",
                    "mainClass": "net.minecraft.client.main.Main",
                    "assetIndex": {},
                    "libraries": []
                }),
            );
        }

        fn write_ready_runtime(&self, component: &str) {
            let runtime_bin = self
                .paths
                .library_dir
                .join("runtime")
                .join(component)
                .join("bin");
            fs::create_dir_all(&runtime_bin).expect("runtime bin");
            let java_name = if cfg!(target_os = "windows") {
                "javaw.exe"
            } else {
                "java"
            };
            fs::write(runtime_bin.join(java_name), b"java").expect("runtime java");
        }

        fn write_manual_java_override(&self) -> String {
            let bin_dir = self.root.join("manual-java").join("bin");
            fs::create_dir_all(&bin_dir).expect("manual java bin");
            let java_path = bin_dir.join(if cfg!(target_os = "windows") {
                "javaw.exe"
            } else {
                "java"
            });
            fs::write(&java_path, b"java").expect("manual java");
            java_path.to_string_lossy().to_string()
        }

        fn update_instance(&self, id: &str, update: impl FnOnce(&mut Instance)) {
            let mut instance = self.state.instances().get(id).expect("instance");
            update(&mut instance);
            self.state
                .instances()
                .update(instance)
                .expect("update instance");
        }

        fn set_guardian_mode(&self, mode: &str) {
            let mut config = self.state.config().current();
            config.guardian_mode = mode.to_string();
            self.state
                .config()
                .replace_in_memory(config)
                .expect("set guardian mode");
        }

        fn set_launch_auth_mode(&self, mode: &str) {
            let mut config = self.state.config().current();
            config.launch_auth_mode = mode.to_string();
            self.state
                .config()
                .replace_in_memory(config)
                .expect("set launch auth mode");
        }

        fn set_global_jvm_preset(&self, preset: &str) {
            let mut config = self.state.config().current();
            config.jvm_preset = preset.to_string();
            self.state
                .config()
                .replace_in_memory(config)
                .expect("set global jvm preset");
        }

        fn write_version_json(&self, version_id: &str, value: serde_json::Value) {
            let version_dir = self.paths.library_dir.join("versions").join(version_id);
            fs::create_dir_all(&version_dir).expect("version dir");
            fs::write(
                version_dir.join(format!("{version_id}.json")),
                serde_json::to_vec(&value).expect("version json"),
            )
            .expect("write version json");
        }

        async fn prepare(
            &self,
            instance_id: String,
            max_memory_mb: Option<i32>,
        ) -> Result<PreparedLaunch, (StatusCode, Json<serde_json::Value>)> {
            self.prepare_with_memory(instance_id, max_memory_mb, None)
                .await
        }

        async fn prepare_with_memory(
            &self,
            instance_id: String,
            max_memory_mb: Option<i32>,
            min_memory_mb: Option<i32>,
        ) -> Result<PreparedLaunch, (StatusCode, Json<serde_json::Value>)> {
            if let Some(instance) = self.state.instances().get(&instance_id) {
                self.write_ready_install(&instance.version_id);
            }
            prepare_launch_session(
                &self.state,
                LaunchRequest {
                    instance_id,
                    username: None,
                    max_memory_mb,
                    min_memory_mb,
                    client_started_at_ms: None,
                },
            )
            .await
        }

        async fn add_active_launch(&self, session_id: &str, max_memory_mb: u64) {
            self.state
                .sessions()
                .insert(LaunchSessionRecord {
                    session_id: SessionId(session_id.to_string()),
                    instance_id: format!("{session_id}-instance"),
                    version_id: "1.21.1".to_string(),
                    launched_at: Some(timestamp_utc()),
                    benchmark: None,
                    state: LaunchState::Queued,
                    pid: None,
                    process_started_at_ms: None,
                    boot_completed_at_ms: None,
                    boot_duration_ms: None,
                    priority: None,
                    exit_code: None,
                    command: vec!["java".to_string(), format!("-Xmx{max_memory_mb}M")],
                    java_path: None,
                    natives_dir: None,
                    failure: None,
                    healing: None,
                    guardian: None,
                    stages: Vec::new(),
                })
                .await;
        }

        async fn add_active_install(&self, install_id: &str) {
            self.state.installs().insert(install_id.to_string()).await;
        }

        async fn add_active_minecraft_account(&self, owns_minecraft_java: bool) {
            self.state
                .auth_logins()
                .replace_with_msa_and_minecraft_account(
                    NewAuthLoginMsaToken {
                        access_token: "msa-access-token".to_string(),
                        refresh_token: None,
                        id_token: None,
                        token_type: "Bearer".to_string(),
                        expires_in: 3600,
                        scope: Some("XboxLive.signin offline_access".to_string()),
                    },
                    NewAuthLoginMinecraftAccount {
                        access_token: "minecraft-access-token".to_string(),
                        token_type: Some("Bearer".to_string()),
                        expires_in: 86400,
                        profile: AuthLoginMinecraftProfile {
                            id: "4f9c7f7d0b1245d9a5c2f03a8c120001".to_string(),
                            name: "ProfileName".to_string(),
                            skins: Vec::new(),
                            capes: Vec::new(),
                        },
                        owns_minecraft_java,
                    },
                )
                .await;
        }
    }

    impl Drop for TestFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn test_root(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "croopor-api-launch-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|value| value.as_nanos())
                .unwrap_or_default()
        ));
        fs::create_dir_all(&path).expect("create test root");
        path
    }

    fn test_paths(root: &Path) -> AppPaths {
        let config_dir = root.join("config");
        AppPaths {
            config_file: config_dir.join("config.json"),
            instances_file: config_dir.join("instances.json"),
            instances_dir: root.join("instances"),
            music_dir: root.join("music"),
            library_dir: root.join("library"),
            config_dir,
        }
    }

    fn assert_readiness_reason(
        preflight: &LaunchPreflightResponse,
        expected: LaunchReadinessReasonId,
    ) {
        assert!(
            preflight
                .readiness
                .reasons
                .iter()
                .any(|reason| reason.id == expected),
            "missing readiness reason {expected:?}: {:?}",
            preflight.readiness.reasons
        );
    }

    fn assert_launch_error_is_token_safe(value: &serde_json::Value) {
        assert_no_sensitive_public_field_keys(value);
        let text = value.to_string();
        for material in [
            "new-msa-access-token",
            "new-msa-refresh-token",
            "old-msa-access-token",
            "old-msa-refresh-token",
            "minecraft-access-token",
            "xbl-token",
            "xsts-token",
            "provider-secret-payload",
        ] {
            assert!(
                !text.contains(material),
                "public launch JSON exposed sensitive material {material}"
            );
        }
    }

    fn assert_no_sensitive_public_field_keys(value: &serde_json::Value) {
        match value {
            serde_json::Value::Object(map) => {
                for (key, value) in map {
                    assert!(
                        !matches!(
                            key.as_str(),
                            "access_token" | "refresh_token" | "id_token" | "device_code"
                        ),
                        "public launch JSON exposed {key}"
                    );
                    assert_no_sensitive_public_field_keys(value);
                }
            }
            serde_json::Value::Array(values) => {
                for value in values {
                    assert_no_sensitive_public_field_keys(value);
                }
            }
            _ => {}
        }
    }

    fn test_budget_with_memory(
        memory_evidence: LaunchMemoryEvidence,
        host_cpu_threads: Option<usize>,
        active_session_count: usize,
        active_install_count: usize,
        active_memory_allocation_mb: u64,
        requested_memory_mb: i32,
    ) -> LaunchProofResourceBudget {
        test_budget_with_memory_and_disk(
            memory_evidence,
            LaunchDiskEvidence::default(),
            LaunchCpuLoadEvidence::default(),
            host_cpu_threads,
            ActiveLaunchResourceUse {
                session_count: active_session_count,
                install_count: active_install_count,
                memory_allocation_mb: active_memory_allocation_mb,
            },
            requested_memory_mb,
        )
    }

    fn test_budget_with_memory_and_disk(
        memory_evidence: LaunchMemoryEvidence,
        disk_evidence: LaunchDiskEvidence,
        cpu_load_evidence: LaunchCpuLoadEvidence,
        host_cpu_threads: Option<usize>,
        active: ActiveLaunchResourceUse,
        requested_memory_mb: i32,
    ) -> LaunchProofResourceBudget {
        capture_resource_budget_snapshot(
            memory_evidence,
            disk_evidence,
            cpu_load_evidence,
            host_cpu_threads,
            active,
            requested_memory_mb,
        )
    }

    fn assert_has_memory_clamp_warning(guardian: &GuardianSummary) {
        for expected in [
            "Minimum memory was higher than maximum memory, so Croopor clamped the launch minimum to match the maximum allocation.",
            "Lower the minimum memory setting or raise the maximum memory allocation if this was intentional.",
        ] {
            assert!(
                guardian.guidance.iter().any(|detail| detail == expected),
                "missing clamp guidance: {expected}"
            );
            assert!(
                guardian.details.iter().any(|detail| detail == expected),
                "missing clamp detail: {expected}"
            );
        }
    }

    fn assert_no_memory_clamp_warning(guardian: &GuardianSummary) {
        for unexpected in [
            "Minimum memory was higher than maximum memory, so Croopor clamped the launch minimum to match the maximum allocation.",
            "Lower the minimum memory setting or raise the maximum memory allocation if this was intentional.",
        ] {
            assert!(
                !guardian.guidance.iter().any(|detail| detail == unexpected),
                "unexpected clamp guidance: {unexpected}"
            );
            assert!(
                !guardian.details.iter().any(|detail| detail == unexpected),
                "unexpected clamp detail: {unexpected}"
            );
        }
    }

    fn assert_has_low_memory_allocation_warning(guardian: &GuardianSummary, max_memory_mb: i32) {
        for expected in [
            format!(
                "Launch memory allocation is very low: this instance is limited to less than 2 GB of RAM ({max_memory_mb} MB selected)."
            ),
            "Raise the maximum memory allocation if Minecraft crashes during startup, stalls while loading, or exits with out-of-memory errors.".to_string(),
        ] {
            assert!(
                guardian.guidance.iter().any(|detail| detail == &expected),
                "missing low-memory guidance: {expected}"
            );
            assert!(
                guardian.details.iter().any(|detail| detail == &expected),
                "missing low-memory detail: {expected}"
            );
        }
    }
}
