use super::LaunchPreflightResourceBudget;
use crate::guardian::GuardianPreflightResourceSignals;
use crate::state::launch_reports::LaunchProofResourceBudget;
use axial_launcher::{LAUNCH_DISK_HEADROOM_MB, LAUNCH_MEMORY_HEADROOM_MB};
use std::path::Path;
use sysinfo::{Disks, ProcessRefreshKind, ProcessesToUpdate, System, get_current_pid};

const LOW_MEMORY_ALLOCATION_WARNING_THRESHOLD_MB: i32 = 2048;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct LaunchMemoryEvidence {
    pub(super) host_total_memory_mb: Option<u64>,
    pub(super) host_available_memory_mb: Option<u64>,
    pub(super) host_used_memory_mb: Option<u64>,
    pub(super) launcher_process_memory_mb: Option<u64>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct LaunchDiskEvidence {
    pub(super) launch_disk_available_mb: Option<u64>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct LaunchCpuLoadEvidence {
    pub(super) host_cpu_load_1m_x100: Option<u64>,
    pub(super) host_cpu_load_5m_x100: Option<u64>,
    pub(super) host_cpu_load_15m_x100: Option<u64>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct ActiveLaunchResourceUse {
    pub(super) session_count: usize,
    pub(super) install_count: usize,
    pub(super) memory_allocation_mb: u64,
}

pub(super) fn preflight_resource_signals(
    raw_min_memory_mb: i32,
    max_memory_mb: i32,
    resource_budget: &LaunchProofResourceBudget,
) -> GuardianPreflightResourceSignals {
    GuardianPreflightResourceSignals {
        memory_clamped: raw_min_memory_mb > max_memory_mb,
        low_memory_allocation: max_memory_mb > 0
            && max_memory_mb < LOW_MEMORY_ALLOCATION_WARNING_THRESHOLD_MB,
        memory_pressure: resource_budget.memory_pressure,
        cpu_pressure: resource_budget.cpu_pressure,
        install_pressure: resource_budget.install_pressure,
        disk_pressure: resource_budget.disk_pressure,
    }
}

pub(super) fn capture_launch_memory_evidence() -> LaunchMemoryEvidence {
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

pub(super) fn capture_launch_disk_evidence<'a>(
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

pub(super) fn capture_launch_cpu_load_evidence() -> LaunchCpuLoadEvidence {
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
pub(super) fn load_to_x100(value: f64) -> Option<u64> {
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

pub(super) fn host_cpu_threads() -> Option<usize> {
    std::thread::available_parallelism().ok().map(usize::from)
}

pub(super) fn capture_resource_budget_snapshot(
    memory_evidence: LaunchMemoryEvidence,
    disk_evidence: LaunchDiskEvidence,
    cpu_load_evidence: LaunchCpuLoadEvidence,
    host_cpu_threads: Option<usize>,
    active: ActiveLaunchResourceUse,
    requested_allocation_mb: i32,
) -> LaunchProofResourceBudget {
    // Captured before launch work starts so Guardian and proof records use the same pressure view.
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
        memory_pressure: memory_pressure(
            memory_evidence.host_total_memory_mb,
            active.memory_allocation_mb,
            requested_memory_mb,
        ),
        cpu_pressure: active_launch_cpu_pressure(host_cpu_threads, active.session_count)
            || measured_cpu_load_pressure(host_cpu_threads, cpu_load_evidence),
        install_pressure: active.install_count > 0,
        launch_disk_available_mb: disk_evidence.launch_disk_available_mb,
        launch_disk_headroom_mb: LAUNCH_DISK_HEADROOM_MB,
        disk_pressure: disk_evidence
            .launch_disk_available_mb
            .is_some_and(|available_mb| available_mb < LAUNCH_DISK_HEADROOM_MB),
    }
}

fn memory_pressure(
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
    remaining_mb < LAUNCH_MEMORY_HEADROOM_MB
}

pub(super) fn active_launch_cpu_pressure(
    cpu_threads: Option<usize>,
    active_launch_count: usize,
) -> bool {
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

pub(super) fn measured_cpu_load_pressure(
    cpu_threads: Option<usize>,
    cpu_load: LaunchCpuLoadEvidence,
) -> bool {
    let Some(cpu_threads) = cpu_threads.filter(|value| *value > 0) else {
        return false;
    };
    let Some(load_x100) = cpu_load
        .host_cpu_load_1m_x100
        .or(cpu_load.host_cpu_load_5m_x100)
        .or(cpu_load.host_cpu_load_15m_x100)
    else {
        return false;
    };
    load_x100 >= measured_cpu_load_threshold_x100(cpu_threads)
}

pub(super) fn measured_cpu_load_threshold_x100(cpu_threads: usize) -> u64 {
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

impl LaunchPreflightResourceBudget {
    pub(super) fn from_budget(resource_budget: &LaunchProofResourceBudget) -> Self {
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
