use super::reports::LaunchProofResourceBudget;
use std::path::Path;
use sysinfo::{Disks, ProcessRefreshKind, ProcessesToUpdate, System, get_current_pid};

const MEMORY_HEADROOM_MB: u64 = 2048;
const DISK_HEADROOM_MB: u64 = 2048;
const MIB: u64 = 1024 * 1024;

pub(super) struct HostResources {
    pub total_memory_mb: u64,
    pub available_memory_mb: u64,
    pub used_memory_mb: u64,
    pub cpu_threads: Option<usize>,
}

pub(super) fn capture_host() -> HostResources {
    let mut host = System::new();
    host.refresh_memory();
    HostResources {
        total_memory_mb: host.total_memory() / MIB,
        available_memory_mb: host.available_memory() / MIB,
        used_memory_mb: host.used_memory() / MIB,
        cpu_threads: std::thread::available_parallelism().ok().map(usize::from),
    }
}

pub(super) fn capture(
    host: &HostResources,
    (active_session_count, active_memory_allocation_mb): (usize, u64),
    active_install_count: usize,
    requested_memory_mb: i32,
    paths: [&Path; 2],
) -> LaunchProofResourceBudget {
    let positive = |value| (value > 0).then_some(value);
    let total = positive(host.total_memory_mb);
    let requested_memory_mb = (requested_memory_mb > 0).then_some(requested_memory_mb);
    let remaining = total.zip(requested_memory_mb).map(|(total, requested)| {
        (i128::from(total) - i128::from(active_memory_allocation_mb) - i128::from(requested))
            .clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
    });
    let mut process = System::new();
    let launcher_process_memory_mb = get_current_pid().ok().and_then(|pid| {
        process.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[pid]),
            true,
            ProcessRefreshKind::nothing().with_memory().without_tasks(),
        );
        process
            .process(pid)
            .and_then(|process| positive(process.memory() / MIB))
    });
    #[cfg(unix)]
    let loads = {
        let load = System::load_average();
        [
            load_x100(load.one),
            load_x100(load.five),
            load_x100(load.fifteen),
        ]
    };
    #[cfg(not(unix))]
    let loads = [None; 3];
    let disks = Disks::new_with_refreshed_list();
    let launch_disk_available_mb = paths
        .into_iter()
        .filter_map(|path| {
            let path = path.canonicalize().ok()?;
            disks
                .list()
                .iter()
                .filter_map(|disk| {
                    let mount = disk.mount_point().canonicalize().ok()?;
                    path.starts_with(&mount)
                        .then_some((mount.components().count(), disk.available_space() / MIB))
                })
                .max_by_key(|(depth, _)| *depth)
                .map(|(_, available)| available)
        })
        .min();
    LaunchProofResourceBudget {
        host_total_memory_mb: total,
        host_available_memory_mb: positive(host.available_memory_mb),
        host_used_memory_mb: positive(host.used_memory_mb),
        host_cpu_threads: host.cpu_threads,
        host_cpu_load_1m_x100: loads[0],
        host_cpu_load_5m_x100: loads[1],
        host_cpu_load_15m_x100: loads[2],
        launcher_process_memory_mb,
        active_session_count,
        active_install_count,
        active_memory_allocation_mb,
        requested_memory_mb,
        estimated_remaining_memory_mb: remaining,
        memory_headroom_mb: MEMORY_HEADROOM_MB,
        memory_pressure: remaining.is_some_and(|remaining| remaining < MEMORY_HEADROOM_MB as i64),
        cpu_pressure: cpu_pressure(host.cpu_threads, active_session_count, loads),
        install_pressure: active_install_count > 0,
        launch_disk_available_mb,
        launch_disk_headroom_mb: DISK_HEADROOM_MB,
        disk_pressure: launch_disk_available_mb
            .is_some_and(|available| available < DISK_HEADROOM_MB),
    }
}

fn cpu_pressure(threads: Option<usize>, sessions: usize, loads: [Option<u64>; 3]) -> bool {
    let Some(threads) = threads.filter(|threads| *threads > 0) else {
        return false;
    };
    let (session_limit, load_percent) = if threads <= 4 {
        (1, 75)
    } else if threads <= 8 {
        (2, 85)
    } else {
        (4, 95)
    };
    sessions >= session_limit
        || loads.into_iter().flatten().next().is_some_and(|load| {
            load >= u64::try_from(threads)
                .unwrap_or(u64::MAX / 100)
                .saturating_mul(load_percent)
        })
}

#[cfg(any(unix, test))]
fn load_x100(value: f64) -> Option<u64> {
    (value.is_finite() && value >= 0.0)
        .then(|| (value * 100.0).round().clamp(0.0, u64::MAX as f64) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_snapshot_preserves_overcommit_and_unknown_observations() {
        let root = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
        let host = HostResources {
            total_memory_mb: 4096,
            available_memory_mb: 1024,
            used_memory_mb: 3072,
            cpu_threads: None,
        };
        let budget = capture(&host, (2, 2048), 1, 4096, [root.path(), root.path()]);
        assert_eq!(budget.host_total_memory_mb, Some(4096));
        assert_eq!(budget.host_available_memory_mb, Some(1024));
        assert_eq!(budget.host_used_memory_mb, Some(3072));
        assert_eq!(budget.host_cpu_threads, None);
        assert_eq!(budget.estimated_remaining_memory_mb, Some(-2048));
        assert_eq!(
            (
                budget.active_session_count,
                budget.active_memory_allocation_mb
            ),
            (2, 2048)
        );
        assert!(budget.memory_pressure && budget.install_pressure && !budget.cpu_pressure);
        let missing = root.path().join("missing");
        let host = HostResources {
            total_memory_mb: 0,
            available_memory_mb: 0,
            used_memory_mb: 0,
            cpu_threads: None,
        };
        let budget = capture(&host, (0, 0), 0, 0, [&missing, &missing]);
        assert_eq!(budget.host_total_memory_mb, None);
        assert_eq!(budget.host_available_memory_mb, None);
        assert_eq!(budget.host_used_memory_mb, None);
        assert_eq!(budget.requested_memory_mb, None);
        assert_eq!(budget.estimated_remaining_memory_mb, None);
        assert_eq!(budget.launch_disk_available_mb, None);
        assert!(
            !budget.memory_pressure
                && !budget.cpu_pressure
                && !budget.install_pressure
                && !budget.disk_pressure
        );
    }

    #[test]
    fn cpu_pressure_retains_thread_tiers_and_first_observed_load() {
        for (threads, sessions, threshold) in [(4, 1, 300), (8, 2, 680), (16, 4, 1520)] {
            assert!(!cpu_pressure(Some(threads), sessions - 1, [None; 3]));
            assert!(cpu_pressure(Some(threads), sessions, [None; 3]));
            assert!(!cpu_pressure(
                Some(threads),
                0,
                [Some(threshold - 1), Some(threshold), None]
            ));
            assert!(cpu_pressure(
                Some(threads),
                0,
                [None, Some(threshold), None]
            ));
        }
        assert!(!cpu_pressure(None, 10, [Some(1000); 3]));
        assert!(!cpu_pressure(Some(0), 10, [Some(1000); 3]));
        for (load, expected) in [
            (0.0, Some(0)),
            (1.234, Some(123)),
            (-1.0, None),
            (f64::NAN, None),
            (f64::INFINITY, None),
        ] {
            assert_eq!(load_x100(load), expected);
        }
    }
}
