use super::policy;
use super::runner::trace_launch_event;
use crate::logging::timestamp_utc;
use crate::state::launch_reports::{
    LAUNCH_DISK_HEADROOM_MB, LaunchBenchmarkMetadata, LaunchProofResourceBudget,
};
use crate::state::{AppState, LaunchSessionRecord};
use axum::{Json, http::StatusCode};
use croopor_config::{AppConfig, Instance, validate_username};
use croopor_launcher::{
    GuardianMode, GuardianSummary, LaunchGuardianContext, LaunchIntent, LaunchState,
};
use serde::Deserialize;
use serde_json::json;
use std::path::{Path, PathBuf};
use sysinfo::{Disks, ProcessRefreshKind, ProcessesToUpdate, System, get_current_pid};

const OS_MEMORY_HEADROOM_MB: u64 = 2048;
const LOW_MEMORY_ALLOCATION_WARNING_THRESHOLD_MB: i32 = 2048;
const MEMORY_CLAMP_WARNING: &str = "Minimum memory was higher than maximum memory, so Croopor clamped the launch minimum to match the maximum allocation.";
const MEMORY_CLAMP_GUIDANCE: &str = "Lower the minimum memory setting or raise the maximum memory allocation if this was intentional.";

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

pub(super) async fn prepare_launch_session(
    state: &AppState,
    payload: LaunchRequest,
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
        .map_err(|error| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("failed to prepare instance layout: {error}") })),
            )
        })?;
    let game_dir = state.instances().game_dir(&instance.id);

    let config = state.config().current();
    let requested_username = payload
        .username
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(config.username.as_str())
        .to_string();
    let username = validate_username(&requested_username)
        .map_err(|error| (StatusCode::BAD_REQUEST, Json(json!({ "error": error }))))?;
    let memory_evidence = capture_launch_memory_evidence();
    let memory_defaults = policy::derived_launch_memory_defaults(
        &instance,
        &config,
        payload.max_memory_mb,
        payload.min_memory_mb,
        memory_evidence.host_total_memory_mb,
    );
    let max_memory_mb =
        policy::effective_max_memory(&instance, &config, payload.max_memory_mb, memory_defaults);
    let raw_min_memory_mb =
        policy::selected_raw_min_memory(&instance, &config, payload.min_memory_mb, memory_defaults);
    let min_memory_mb = policy::effective_min_memory(
        &instance,
        &config,
        payload.min_memory_mb,
        max_memory_mb,
        memory_defaults,
    );
    let requested_java = policy::selected_java_override(&instance, &config);
    let requested_preset = policy::selected_jvm_preset(&instance, &config);
    let launched_at = timestamp_utc();
    let session_id = policy::generate_session_id();
    let guardian = LaunchGuardianContext {
        mode: policy::selected_guardian_mode(&config),
        java_override_origin: policy::java_override_origin(&instance, &config),
        preset_override_origin: policy::preset_override_origin(&instance, &config),
        raw_jvm_args_origin: policy::raw_jvm_args_origin(&instance),
    };
    let resource_budget = capture_resource_budget_snapshot(
        memory_evidence,
        capture_launch_disk_evidence([library_dir.as_path(), game_dir.as_path()]),
        capture_launch_cpu_load_evidence(),
        host_cpu_threads(),
        state.sessions().active_session_count().await,
        state.installs().active_install_count().await,
        state.sessions().active_memory_allocation_mb().await,
        max_memory_mb,
    );
    let mut guardian_summary = GuardianSummary::new(guardian.mode);
    if let Some(guidance) = memory_clamp_warning_guidance(raw_min_memory_mb, max_memory_mb) {
        guardian_summary.warn_with_guidance(guidance);
    }
    if let Some(guidance) = low_memory_allocation_warning_guidance(max_memory_mb) {
        guardian_summary.warn_with_guidance(guidance);
    }
    if let Some(guidance) = memory_budget_warning_guidance(&resource_budget) {
        guardian_summary.warn_with_guidance(guidance);
    }
    if let Some(guidance) = cpu_pressure_warning_guidance(&resource_budget) {
        guardian_summary.warn_with_guidance(guidance);
    }
    if let Some(guidance) = install_pressure_warning_guidance(&resource_budget) {
        guardian_summary.warn_with_guidance(guidance);
    }
    if let Some(guidance) = disk_pressure_warning_guidance(&resource_budget) {
        guardian_summary.warn_with_guidance(guidance);
    }
    if let Some(guidance) = custom_risky_override_warning_guidance(&guardian) {
        guardian_summary.warn_with_guidance(guidance);
    }

    let intent = LaunchIntent {
        session_id: session_id.0.clone(),
        library_dir: library_dir.clone(),
        instance_id: instance.id.clone(),
        version_id: instance.version_id.clone(),
        username: username.clone(),
        requested_java: requested_java.clone(),
        requested_preset: requested_preset.clone(),
        extra_jvm_args: policy::split_jvm_args(&instance.extra_jvm_args),
        max_memory_mb,
        min_memory_mb,
        resolution: policy::selected_resolution(&instance, &config),
        launcher_name: "croopor".to_string(),
        launcher_version: state.version().to_string(),
        game_dir: Some(game_dir),
        guardian: guardian.clone(),
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
            guardian: serde_json::to_value(&guardian_summary).ok(),
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
            config: config.clone(),
            intent,
            guardian: guardian_summary,
            launched_at,
            benchmark: None,
            resource_budget: Some(resource_budget),
        },
    })
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
        return LaunchCpuLoadEvidence {
            host_cpu_load_1m_x100: load_to_x100(load.one),
            host_cpu_load_5m_x100: load_to_x100(load.five),
            host_cpu_load_15m_x100: load_to_x100(load.fifteen),
        };
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

fn capture_resource_budget_snapshot(
    memory_evidence: LaunchMemoryEvidence,
    disk_evidence: LaunchDiskEvidence,
    cpu_load_evidence: LaunchCpuLoadEvidence,
    host_cpu_threads: Option<usize>,
    active_session_count: usize,
    active_install_count: usize,
    active_memory_allocation_mb: u64,
    requested_allocation_mb: i32,
) -> LaunchProofResourceBudget {
    let requested_memory_mb = positive_i32(requested_allocation_mb);
    LaunchProofResourceBudget {
        host_total_memory_mb: memory_evidence.host_total_memory_mb,
        host_available_memory_mb: memory_evidence.host_available_memory_mb,
        host_used_memory_mb: memory_evidence.host_used_memory_mb,
        host_cpu_threads,
        host_cpu_load_1m_x100: cpu_load_evidence.host_cpu_load_1m_x100,
        host_cpu_load_5m_x100: cpu_load_evidence.host_cpu_load_5m_x100,
        host_cpu_load_15m_x100: cpu_load_evidence.host_cpu_load_15m_x100,
        launcher_process_memory_mb: memory_evidence.launcher_process_memory_mb,
        active_session_count,
        active_install_count,
        active_memory_allocation_mb,
        requested_memory_mb,
        estimated_remaining_memory_mb: estimated_remaining_memory_mb(
            memory_evidence.host_total_memory_mb,
            active_memory_allocation_mb,
            requested_memory_mb,
        ),
        memory_headroom_mb: OS_MEMORY_HEADROOM_MB,
        memory_pressure: memory_budget_pressure(
            memory_evidence.host_total_memory_mb,
            active_memory_allocation_mb,
            requested_memory_mb,
        ),
        cpu_pressure: cpu_pressure(host_cpu_threads, active_session_count, cpu_load_evidence),
        install_pressure: active_install_count > 0,
        launch_disk_available_mb: disk_evidence.launch_disk_available_mb,
        launch_disk_headroom_mb: LAUNCH_DISK_HEADROOM_MB,
        disk_pressure: disk_pressure(disk_evidence.launch_disk_available_mb),
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

fn memory_budget_pressure(
    total_memory_mb: Option<u64>,
    active_allocation_mb: u64,
    requested_allocation_mb: Option<i32>,
) -> bool {
    let Some(total_memory_mb) = total_memory_mb else {
        return false;
    };
    let Some(requested_allocation_mb) =
        requested_allocation_mb.and_then(|value| u64::try_from(value).ok())
    else {
        return false;
    };
    let remaining_mb = total_memory_mb
        .saturating_sub(active_allocation_mb.saturating_add(requested_allocation_mb));
    remaining_mb < OS_MEMORY_HEADROOM_MB
}

fn memory_budget_warning_guidance(
    resource_budget: &LaunchProofResourceBudget,
) -> Option<Vec<String>> {
    if !resource_budget.memory_pressure {
        return None;
    }
    Some(vec![
        "Launch memory budget is tight: active sessions plus this launch may leave less than 2 GB for the OS.".to_string(),
        "Close another running session or lower this instance's memory allocation if startup or gameplay becomes unstable.".to_string(),
    ])
}

fn memory_clamp_warning_guidance(
    raw_min_memory_mb: i32,
    max_memory_mb: i32,
) -> Option<Vec<String>> {
    (raw_min_memory_mb > max_memory_mb).then(|| {
        vec![
            MEMORY_CLAMP_WARNING.to_string(),
            MEMORY_CLAMP_GUIDANCE.to_string(),
        ]
    })
}

fn low_memory_allocation_warning_guidance(max_memory_mb: i32) -> Option<Vec<String>> {
    (max_memory_mb > 0 && max_memory_mb < LOW_MEMORY_ALLOCATION_WARNING_THRESHOLD_MB).then(|| {
        vec![
            format!(
                "Launch memory allocation is very low: this instance is limited to less than 2 GB of RAM ({max_memory_mb} MB selected)."
            ),
            "Raise the maximum memory allocation if Minecraft crashes during startup, stalls while loading, or exits with out-of-memory errors.".to_string(),
        ]
    })
}

fn disk_pressure(launch_disk_available_mb: Option<u64>) -> bool {
    launch_disk_available_mb.is_some_and(|available_mb| available_mb < LAUNCH_DISK_HEADROOM_MB)
}

fn disk_pressure_warning_guidance(
    resource_budget: &LaunchProofResourceBudget,
) -> Option<Vec<String>> {
    if !resource_budget.disk_pressure {
        return None;
    }
    let available_mb = resource_budget.launch_disk_available_mb?;

    Some(vec![
        format!(
            "Launch disk space is tight: launch-relevant storage reports less than 2 GB free ({available_mb} MB available)."
        ),
        "Free disk space before launching if caches, natives, or prewarm steps become unreliable."
            .to_string(),
    ])
}

fn cpu_pressure(
    cpu_threads: Option<usize>,
    active_launch_count: usize,
    cpu_load_evidence: LaunchCpuLoadEvidence,
) -> bool {
    active_launch_cpu_pressure(cpu_threads, active_launch_count)
        || measured_cpu_load_pressure(cpu_threads, cpu_load_evidence)
}

fn active_launch_cpu_pressure(cpu_threads: Option<usize>, active_launch_count: usize) -> bool {
    let Some(cpu_threads) = cpu_threads.filter(|value| *value > 0) else {
        return false;
    };
    let queued_launch_count = active_launch_count.saturating_add(1);
    if cpu_threads <= 4 {
        active_launch_count >= 1
    } else if cpu_threads <= 8 {
        queued_launch_count >= 3
    } else {
        queued_launch_count >= 5
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CpuLoadPressureEvidence {
    window_label: &'static str,
    load_x100: u64,
}

fn measured_cpu_load_pressure(
    cpu_threads: Option<usize>,
    cpu_load_evidence: LaunchCpuLoadEvidence,
) -> bool {
    measured_cpu_load_pressure_evidence(cpu_threads, cpu_load_evidence).is_some()
}

fn measured_cpu_load_pressure_evidence(
    cpu_threads: Option<usize>,
    cpu_load_evidence: LaunchCpuLoadEvidence,
) -> Option<CpuLoadPressureEvidence> {
    let cpu_threads = cpu_threads.filter(|value| *value > 0)?;
    let sample = most_recent_cpu_load_sample(cpu_load_evidence)?;
    let threshold_x100 = measured_cpu_load_threshold_x100(cpu_threads);
    (sample.load_x100 >= threshold_x100).then_some(sample)
}

fn most_recent_cpu_load_sample(
    cpu_load_evidence: LaunchCpuLoadEvidence,
) -> Option<CpuLoadPressureEvidence> {
    cpu_load_evidence
        .host_cpu_load_1m_x100
        .map(|load_x100| CpuLoadPressureEvidence {
            window_label: "1-minute",
            load_x100,
        })
        .or_else(|| {
            cpu_load_evidence
                .host_cpu_load_5m_x100
                .map(|load_x100| CpuLoadPressureEvidence {
                    window_label: "5-minute",
                    load_x100,
                })
        })
        .or_else(|| {
            cpu_load_evidence
                .host_cpu_load_15m_x100
                .map(|load_x100| CpuLoadPressureEvidence {
                    window_label: "15-minute",
                    load_x100,
                })
        })
}

fn measured_cpu_load_threshold_x100(cpu_threads: usize) -> u64 {
    let headroom_percent = if cpu_threads <= 4 {
        75_u64
    } else if cpu_threads <= 8 {
        85
    } else {
        95
    };
    u64::try_from(cpu_threads)
        .unwrap_or(u64::MAX / 100)
        .saturating_mul(headroom_percent)
}

fn cpu_load_evidence_from_budget(
    resource_budget: &LaunchProofResourceBudget,
) -> LaunchCpuLoadEvidence {
    LaunchCpuLoadEvidence {
        host_cpu_load_1m_x100: resource_budget.host_cpu_load_1m_x100,
        host_cpu_load_5m_x100: resource_budget.host_cpu_load_5m_x100,
        host_cpu_load_15m_x100: resource_budget.host_cpu_load_15m_x100,
    }
}

fn format_load_x100(value: u64) -> String {
    format!("{}.{:02}", value / 100, value % 100)
}

fn cpu_pressure_warning_guidance(
    resource_budget: &LaunchProofResourceBudget,
) -> Option<Vec<String>> {
    if !resource_budget.cpu_pressure {
        return None;
    }
    let cpu_threads = resource_budget.host_cpu_threads?;
    let active_session_count = resource_budget.active_session_count;
    let cpu_load_evidence = cpu_load_evidence_from_budget(resource_budget);
    let load_pressure =
        measured_cpu_load_pressure_evidence(resource_budget.host_cpu_threads, cpu_load_evidence);
    let launch_pressure =
        active_launch_cpu_pressure(resource_budget.host_cpu_threads, active_session_count);
    let mut guidance = Vec::new();

    if let Some(load_pressure) = load_pressure {
        guidance.push(format!(
            "Host CPU load is already high: {} load average is {} on {cpu_threads} CPU threads before launch.",
            load_pressure.window_label,
            format_load_x100(load_pressure.load_x100)
        ));
        guidance.push(
            "Close CPU-heavy apps or wait for background work to settle if startup feels sluggish."
                .to_string(),
        );
    }

    if launch_pressure {
        guidance.push(format!(
            "Launch concurrency may be tight: this device reports {cpu_threads} CPU threads, and other active launch sessions before this one: {active_session_count}."
        ));
        guidance.push(
            "Multiple launches can saturate low-end CPUs; wait for another launch to finish if startup feels sluggish.".to_string(),
        );
    }

    (!guidance.is_empty()).then_some(guidance)
}

fn install_pressure_warning_guidance(
    resource_budget: &LaunchProofResourceBudget,
) -> Option<Vec<String>> {
    if !resource_budget.install_pressure {
        return None;
    }
    let active_install_count = resource_budget.active_install_count;

    Some(vec![
        format!(
            "Active install/download sessions: {active_install_count}. Launching now can add disk and network pressure during startup."
        ),
        "On low-end devices, wait for the install or download to finish if startup feels slow."
            .to_string(),
    ])
}

fn custom_risky_override_warning_guidance(guardian: &LaunchGuardianContext) -> Option<Vec<String>> {
    if !matches!(guardian.mode, GuardianMode::Custom) || !guardian.has_risky_overrides() {
        return None;
    }

    let mut guidance = Vec::new();
    if guardian.has_java_override() {
        guidance.push(
            "Guardian Custom mode will keep the selected Java override for this launch."
                .to_string(),
        );
    }
    if guardian.has_raw_jvm_args() {
        guidance.push(
            "Guardian Custom mode will keep explicit JVM args; remove them first if startup becomes unstable."
                .to_string(),
        );
    }
    guidance.push(
        "Switch Guardian back to Managed if you want Croopor to adjust unsafe choices.".to_string(),
    );
    Some(guidance)
}

fn positive_i32(value: i32) -> Option<i32> {
    (value > 0).then_some(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{AppStateInit, InstallStore, SessionStore};
    use croopor_config::{AppConfig, AppPaths, ConfigStore, InstanceStore};
    use croopor_launcher::{GuardianDecision, OverrideOrigin, SessionId};
    use croopor_performance::PerformanceManager;
    use std::{fs, path::PathBuf, sync::Arc};

    #[tokio::test]
    async fn prepare_launch_session_ensures_instance_layout_before_building_intent() {
        let fixture = TestFixture::new("prepare-ensures-layout");
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
    async fn custom_mode_with_java_override_warns_before_queue() {
        let fixture = TestFixture::new("prepare-custom-java-warning");
        fixture.set_guardian_mode("custom");
        let instance_id = fixture.add_instance("Survival", "1.21.1");
        fixture.update_instance(&instance_id, |instance| {
            instance.java_path = "/manual/java/bin/java".to_string();
        });

        let prepared = fixture
            .prepare(instance_id.clone(), None)
            .await
            .expect("prepare launch session");

        assert_eq!(prepared.task.guardian.decision, GuardianDecision::Warned);
        assert_eq!(prepared.task.intent.requested_java, "/manual/java/bin/java");
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
    async fn managed_mode_with_manual_overrides_skips_custom_warning_at_queue_time() {
        let fixture = TestFixture::new("prepare-managed-overrides-no-custom-warning");
        fixture.set_guardian_mode("managed");
        let instance_id = fixture.add_instance("Survival", "1.21.1");
        fixture.update_instance(&instance_id, |instance| {
            instance.java_path = "/manual/java/bin/java".to_string();
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
        assert_eq!(prepared.task.intent.requested_java, "/manual/java/bin/java");
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
        assert_eq!(memory_clamp_warning_guidance(1024, 4096), None);
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
        fixture.update_instance(&instance_id, |instance| {
            instance.java_path = "/manual/java/bin/java".to_string();
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
        fixture.update_instance(&instance_id, |instance| {
            instance.java_path = "/manual/java/bin/java".to_string();
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
        fixture.update_instance(&instance_id, |instance| {
            instance.java_path = "/manual/java/bin/java".to_string();
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
        fixture.update_instance(&instance_id, |instance| {
            instance.java_path = "/manual/java/bin/java".to_string();
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
    fn memory_budget_warning_is_conservative_and_host_independent() {
        assert_eq!(
            memory_budget_warning_guidance(&test_budget(None, None, 0, 0, 4096, 4096)),
            None
        );
        assert_eq!(
            memory_budget_warning_guidance(&test_budget(Some(16_384), None, 0, 0, 4096, 4096)),
            None
        );

        let warning =
            memory_budget_warning_guidance(&test_budget(Some(8192), None, 0, 0, 3072, 4096))
                .expect("warning guidance");
        assert_eq!(
            warning,
            vec![
                "Launch memory budget is tight: active sessions plus this launch may leave less than 2 GB for the OS.",
                "Close another running session or lower this instance's memory allocation if startup or gameplay becomes unstable.",
            ]
        );

        assert_eq!(
            memory_budget_warning_guidance(&test_budget(Some(4096), None, 0, 0, 2048, 0)),
            None
        );
    }

    #[test]
    fn low_memory_allocation_warning_uses_strict_positive_threshold() {
        assert_eq!(low_memory_allocation_warning_guidance(0), None);
        assert_eq!(low_memory_allocation_warning_guidance(-512), None);
        assert_eq!(
            low_memory_allocation_warning_guidance(LOW_MEMORY_ALLOCATION_WARNING_THRESHOLD_MB),
            None
        );
        assert_eq!(
            low_memory_allocation_warning_guidance(LOW_MEMORY_ALLOCATION_WARNING_THRESHOLD_MB + 1),
            None
        );

        assert_eq!(
            low_memory_allocation_warning_guidance(
                LOW_MEMORY_ALLOCATION_WARNING_THRESHOLD_MB - 1
            ),
            Some(vec![
                "Launch memory allocation is very low: this instance is limited to less than 2 GB of RAM (2047 MB selected).".to_string(),
                "Raise the maximum memory allocation if Minecraft crashes during startup, stalls while loading, or exits with out-of-memory errors.".to_string(),
            ])
        );
    }

    #[test]
    fn cpu_pressure_warning_is_conservative_and_host_independent() {
        assert_eq!(
            cpu_pressure_warning_guidance(&test_budget(None, None, 4, 0, 0, 0)),
            None
        );
        assert_eq!(
            cpu_pressure_warning_guidance(&test_budget(None, Some(4), 0, 0, 0, 0)),
            None
        );
        assert_eq!(
            cpu_pressure_warning_guidance(&test_budget(None, Some(8), 1, 0, 0, 0)),
            None
        );
        assert_eq!(
            cpu_pressure_warning_guidance(&test_budget(None, Some(16), 3, 0, 0, 0)),
            None
        );

        assert!(cpu_pressure_warning_guidance(&test_budget(None, Some(4), 1, 0, 0, 0)).is_some());
        assert!(cpu_pressure_warning_guidance(&test_budget(None, Some(8), 2, 0, 0, 0)).is_some());
        assert!(cpu_pressure_warning_guidance(&test_budget(None, Some(16), 4, 0, 0, 0)).is_some());
    }

    #[test]
    fn measured_cpu_load_pressure_is_conservative_and_host_independent() {
        assert!(!measured_cpu_load_pressure(
            None,
            LaunchCpuLoadEvidence {
                host_cpu_load_1m_x100: Some(300),
                ..LaunchCpuLoadEvidence::default()
            }
        ));
        assert!(!measured_cpu_load_pressure(
            Some(4),
            LaunchCpuLoadEvidence::default()
        ));
        assert!(!measured_cpu_load_pressure(
            Some(4),
            LaunchCpuLoadEvidence {
                host_cpu_load_1m_x100: Some(299),
                ..LaunchCpuLoadEvidence::default()
            }
        ));
        assert!(measured_cpu_load_pressure(
            Some(4),
            LaunchCpuLoadEvidence {
                host_cpu_load_1m_x100: Some(300),
                ..LaunchCpuLoadEvidence::default()
            }
        ));
        assert!(!measured_cpu_load_pressure(
            Some(8),
            LaunchCpuLoadEvidence {
                host_cpu_load_1m_x100: Some(679),
                ..LaunchCpuLoadEvidence::default()
            }
        ));
        assert!(measured_cpu_load_pressure(
            Some(8),
            LaunchCpuLoadEvidence {
                host_cpu_load_1m_x100: Some(680),
                ..LaunchCpuLoadEvidence::default()
            }
        ));
        assert!(!measured_cpu_load_pressure(
            Some(16),
            LaunchCpuLoadEvidence {
                host_cpu_load_1m_x100: Some(1519),
                ..LaunchCpuLoadEvidence::default()
            }
        ));
        assert!(measured_cpu_load_pressure(
            Some(16),
            LaunchCpuLoadEvidence {
                host_cpu_load_1m_x100: Some(1520),
                ..LaunchCpuLoadEvidence::default()
            }
        ));
    }

    #[test]
    fn measured_cpu_load_pressure_uses_most_recent_available_load_sample() {
        assert!(!measured_cpu_load_pressure(
            Some(4),
            LaunchCpuLoadEvidence {
                host_cpu_load_1m_x100: Some(100),
                host_cpu_load_5m_x100: Some(300),
                host_cpu_load_15m_x100: Some(300),
            }
        ));
        assert!(measured_cpu_load_pressure(
            Some(4),
            LaunchCpuLoadEvidence {
                host_cpu_load_1m_x100: None,
                host_cpu_load_5m_x100: Some(300),
                host_cpu_load_15m_x100: Some(100),
            }
        ));
    }

    #[test]
    fn measured_cpu_load_warning_guidance_distinguishes_host_load_from_launch_concurrency() {
        let warning = cpu_pressure_warning_guidance(&test_budget_with_cpu_load(
            LaunchCpuLoadEvidence {
                host_cpu_load_1m_x100: Some(300),
                ..LaunchCpuLoadEvidence::default()
            },
            Some(4),
            0,
        ))
        .expect("measured load warning");
        assert_eq!(
            warning,
            vec![
                "Host CPU load is already high: 1-minute load average is 3.00 on 4 CPU threads before launch.",
                "Close CPU-heavy apps or wait for background work to settle if startup feels sluggish.",
            ]
        );

        let warning = cpu_pressure_warning_guidance(&test_budget_with_cpu_load(
            LaunchCpuLoadEvidence::default(),
            Some(4),
            1,
        ))
        .expect("concurrent launch warning");
        assert_eq!(
            warning,
            vec![
                "Launch concurrency may be tight: this device reports 4 CPU threads, and other active launch sessions before this one: 1.",
                "Multiple launches can saturate low-end CPUs; wait for another launch to finish if startup feels sluggish.",
            ]
        );
    }

    #[test]
    fn disk_pressure_warning_is_conservative_and_host_independent() {
        let unknown = test_budget_with_disk(None);
        assert!(!unknown.disk_pressure);
        assert_eq!(disk_pressure_warning_guidance(&unknown), None);

        let clear = test_budget_with_disk(Some(LAUNCH_DISK_HEADROOM_MB));
        assert!(!clear.disk_pressure);
        assert_eq!(disk_pressure_warning_guidance(&clear), None);

        let pressured = test_budget_with_disk(Some(1400));
        assert!(pressured.disk_pressure);
        assert_eq!(
            disk_pressure_warning_guidance(&pressured),
            Some(vec![
                "Launch disk space is tight: launch-relevant storage reports less than 2 GB free (1400 MB available).".to_string(),
                "Free disk space before launching if caches, natives, or prewarm steps become unreliable.".to_string(),
            ])
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
            1,
            1,
            3072,
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
        assert_eq!(pressured.memory_headroom_mb, OS_MEMORY_HEADROOM_MB);
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
            0,
            0,
            0,
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

    fn test_paths(root: &std::path::Path) -> AppPaths {
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

    fn test_budget(
        host_total_memory_mb: Option<u64>,
        host_cpu_threads: Option<usize>,
        active_session_count: usize,
        active_install_count: usize,
        active_memory_allocation_mb: u64,
        requested_memory_mb: i32,
    ) -> LaunchProofResourceBudget {
        test_budget_with_memory(
            LaunchMemoryEvidence {
                host_total_memory_mb,
                ..LaunchMemoryEvidence::default()
            },
            host_cpu_threads,
            active_session_count,
            active_install_count,
            active_memory_allocation_mb,
            requested_memory_mb,
        )
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
            active_session_count,
            active_install_count,
            active_memory_allocation_mb,
            requested_memory_mb,
        )
    }

    fn test_budget_with_disk(launch_disk_available_mb: Option<u64>) -> LaunchProofResourceBudget {
        test_budget_with_memory_and_disk(
            LaunchMemoryEvidence::default(),
            LaunchDiskEvidence {
                launch_disk_available_mb,
            },
            LaunchCpuLoadEvidence::default(),
            None,
            0,
            0,
            0,
            4096,
        )
    }

    fn test_budget_with_cpu_load(
        cpu_load_evidence: LaunchCpuLoadEvidence,
        host_cpu_threads: Option<usize>,
        active_session_count: usize,
    ) -> LaunchProofResourceBudget {
        test_budget_with_memory_and_disk(
            LaunchMemoryEvidence::default(),
            LaunchDiskEvidence::default(),
            cpu_load_evidence,
            host_cpu_threads,
            active_session_count,
            0,
            0,
            4096,
        )
    }

    fn test_budget_with_memory_and_disk(
        memory_evidence: LaunchMemoryEvidence,
        disk_evidence: LaunchDiskEvidence,
        cpu_load_evidence: LaunchCpuLoadEvidence,
        host_cpu_threads: Option<usize>,
        active_session_count: usize,
        active_install_count: usize,
        active_memory_allocation_mb: u64,
        requested_memory_mb: i32,
    ) -> LaunchProofResourceBudget {
        capture_resource_budget_snapshot(
            memory_evidence,
            disk_evidence,
            cpu_load_evidence,
            host_cpu_threads,
            active_session_count,
            active_install_count,
            active_memory_allocation_mb,
            requested_memory_mb,
        )
    }

    fn assert_has_memory_clamp_warning(guardian: &GuardianSummary) {
        for expected in [MEMORY_CLAMP_WARNING, MEMORY_CLAMP_GUIDANCE] {
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
        for unexpected in [MEMORY_CLAMP_WARNING, MEMORY_CLAMP_GUIDANCE] {
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
