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
        let mut values = [0.0; 3];
        // The native function writes at most the supplied three elements.
        let observed = unsafe { libc::getloadavg(values.as_mut_ptr(), 3) };
        observed_loads(values, observed)
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
                    path.starts_with(&mount).then_some((mount, disk))
                })
                .max_by_key(|(mount, _)| mount.components().count())
                .and_then(|selected| {
                    #[cfg(any(target_os = "linux", target_os = "macos"))]
                    {
                        available_disk_bytes(&selected.0)
                    }
                    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
                    {
                        Some(selected.1.available_space())
                    }
                })
                .map(|available| available / MIB)
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

#[cfg(target_os = "linux")]
fn available_disk_bytes(path: &Path) -> Option<u64> {
    let disk = rustix::fs::statvfs(path).ok()?;
    disk.f_bavail.checked_mul(disk.f_frsize)
}

#[cfg(target_os = "macos")]
fn available_disk_bytes(path: &Path) -> Option<u64> {
    use core_foundation::{
        array::CFArray,
        base::{CFType, TCFType},
        dictionary::CFDictionary,
        number::CFNumber,
        string::{CFString, CFStringRef},
        url::{CFURL, CFURLCopyResourcePropertiesForKeys, kCFURLVolumeAvailableCapacityKey},
    };
    unsafe extern "C" {
        static kCFURLVolumeAvailableCapacityForImportantUsageKey: CFStringRef;
    }

    let url = CFURL::from_path(path, true)?;
    // These process-lifetime framework constants are retained by their wrappers.
    let important =
        unsafe { CFString::wrap_under_get_rule(kCFURLVolumeAvailableCapacityForImportantUsageKey) };
    let available = unsafe { CFString::wrap_under_get_rule(kCFURLVolumeAvailableCapacityKey) };
    let keys = CFArray::from_CFTypes(&[important.clone(), available.clone()]);
    // The Copy result is owned; a null result carries no capacity observation.
    let properties = unsafe {
        CFURLCopyResourcePropertiesForKeys(
            url.as_concrete_TypeRef(),
            keys.as_concrete_TypeRef(),
            std::ptr::null_mut(),
        )
    };
    if properties.is_null() {
        return None;
    }
    let properties: CFDictionary<CFString, CFType> =
        unsafe { CFDictionary::wrap_under_create_rule(properties) };
    let bytes = |key: &CFString| {
        let value = properties.find(key)?.downcast::<CFNumber>()?.to_i64()?;
        u64::try_from(value).ok()
    };
    let preferred = bytes(&important);
    preferred
        .filter(|value| *value > 0)
        .or_else(|| bytes(&available))
        .or(preferred)
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
fn observed_loads(values: [f64; 3], count: i32) -> [Option<u64>; 3] {
    std::array::from_fn(|index| {
        ((0..=3).contains(&count) && index < count as usize)
            .then(|| load_x100(values[index]))
            .flatten()
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

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "requires the isolated native capacity probe"]
    fn native_capacity_observation_retains_failure_zero_and_reclaimable_space() {
        let mode = std::env::var("AXIAL_CAPACITY_PROBE").expect("native probe mode required");
        let expected = match mode.as_str() {
            "missing" | "negative" | "wrong_type" | "failure" => None,
            "zero" | "important_zero_only" => Some(0),
            "important" => Some(4096),
            "fallback" | "missing_important" => Some(2048),
            _ => panic!("unknown native probe mode"),
        };
        let root = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
        let disks = Disks::new_with_refreshed_list();
        assert!(
            disks.list().iter().any(|disk| {
                disk.mount_point()
                    .canonicalize()
                    .is_ok_and(|mount| root.path().starts_with(mount))
            }),
            "the capacity probe must retain the fixture's eligible disk"
        );
        let budget = capture(&capture_host(), (0, 0), 0, 2048, [root.path(), root.path()]);
        assert_eq!(budget.launch_disk_available_mb, expected);
        assert_eq!(
            budget.disk_pressure,
            expected.is_some_and(|value| value < 2048)
        );
        let payload = serde_json::to_value(budget).unwrap();
        assert_eq!(
            payload.get("launch_disk_available_mb"),
            expected.map(serde_json::Value::from).as_ref()
        );
    }

    #[cfg(target_os = "linux")]
    fn run_observation_child(test: &str, marker: &str) {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test, "--nocapture", "--test-threads=1"])
            .env(marker, "1")
            .stdin(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                _ => {
                    let _ = child.kill();
                    child.wait().expect("observation child must be reaped");
                    panic!("{test} child timed out or could not be observed");
                }
            }
        };
        assert!(status.success(), "{test} child failed: {status}");
    }

    #[cfg(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    #[test]
    fn disk_observation_failure_is_not_reported_as_available_capacity() {
        use std::os::unix::ffi::OsStrExt;

        const CHILD: &str = "AXIAL_DISK_DENIAL_FIXTURE_CHILD";
        if std::env::var_os(CHILD).is_none() {
            run_observation_child(
                "launch::resources::tests::disk_observation_failure_is_not_reported_as_available_capacity",
                CHILD,
            );
            return;
        }

        let root = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
        let path = std::ffi::CString::new(root.path().as_os_str().as_bytes()).unwrap();
        let native_sample = || {
            let mut observation = std::mem::MaybeUninit::<libc::statvfs>::uninit();
            // The canonical fixture path and output storage outlive this native call.
            let result = unsafe { libc::statvfs(path.as_ptr(), observation.as_mut_ptr()) };
            if result == 0 {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error().raw_os_error())
            }
        };
        native_sample().expect("fixture filesystem must be observable before denial");
        let host = capture_host();
        let before = capture(&host, (0, 0), 0, 2048, [root.path(), root.path()]);
        assert!(
            before.launch_disk_available_mb.is_some(),
            "fixture filesystem must have a reported disk sample before denial"
        );

        let instruction = |code: u32, jt, jf, k| libc::sock_filter {
            code: code as u16,
            jt,
            jf,
            k,
        };
        let mut filter = [
            instruction(
                libc::BPF_LD | libc::BPF_W | libc::BPF_ABS,
                0,
                0,
                std::mem::offset_of!(libc::seccomp_data, nr) as u32,
            ),
            instruction(
                libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K,
                2,
                0,
                libc::SYS_statfs as u32,
            ),
            instruction(
                libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K,
                1,
                0,
                libc::SYS_fstatfs as u32,
            ),
            instruction(libc::BPF_RET | libc::BPF_K, 0, 0, libc::SECCOMP_RET_ALLOW),
            instruction(
                libc::BPF_RET | libc::BPF_K,
                0,
                0,
                libc::SECCOMP_RET_ERRNO | libc::EIO as u32,
            ),
        ];
        let program = libc::sock_fprog {
            len: filter.len() as u16,
            filter: filter.as_mut_ptr(),
        };
        // No TSYNC: only this isolated helper thread is restricted until it exits.
        assert_eq!(
            unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) },
            0,
            "disk-denial fixture could not disable privilege gains"
        );
        assert_eq!(
            unsafe {
                libc::prctl(
                    libc::PR_SET_SECCOMP,
                    libc::SECCOMP_MODE_FILTER,
                    &program as *const libc::sock_fprog,
                    0,
                    0,
                )
            },
            0,
            "disk-denial fixture requires an available seccomp filter"
        );
        let denied_before = native_sample();
        let budget = capture(&host, (0, 0), 0, 2048, [root.path(), root.path()]);
        let denied_after = native_sample();
        root.close().unwrap();
        assert_eq!(denied_before, Err(Some(libc::EIO)));
        assert_eq!(denied_after, Err(Some(libc::EIO)));
        assert_eq!(
            budget.launch_disk_available_mb, None,
            "confirmed native disk-observation failure must not become a capacity sample"
        );
        assert!(!budget.disk_pressure);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn descriptor_exhaustion_retains_native_load_availability() {
        const CHILD: &str = "AXIAL_LOAD_DENIAL_FIXTURE_CHILD";
        if std::env::var_os(CHILD).is_none() {
            run_observation_child(
                "launch::resources::tests::descriptor_exhaustion_retains_native_load_availability",
                CHILD,
            );
            return;
        }

        let root = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
        let host = capture_host();
        let loads = |budget: &LaunchProofResourceBudget| {
            [
                budget.host_cpu_load_1m_x100,
                budget.host_cpu_load_5m_x100,
                budget.host_cpu_load_15m_x100,
            ]
        };
        let before = capture(&host, (0, 0), 0, 2048, [root.path(), root.path()]);
        assert!(loads(&before).into_iter().all(|load| load.is_some()));
        assert!(!std::fs::read_to_string("/proc/loadavg").unwrap().is_empty());
        let mut original = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        assert_eq!(
            unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut original) },
            0
        );
        let denied = libc::rlimit {
            rlim_cur: 0,
            rlim_max: original.rlim_max,
        };
        // Only this isolated child's soft limit changes, after sampler initialization.
        assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &denied) }, 0);
        let observation = std::panic::catch_unwind(|| {
            let open_error = std::fs::File::open("/proc/loadavg")
                .err()
                .and_then(|error| error.raw_os_error());
            let mut values = [0.0; 3];
            let observed = unsafe { libc::getloadavg(values.as_mut_ptr(), 3) };
            let budget = (open_error == Some(libc::EMFILE))
                .then(|| capture(&host, (0, 0), 0, 2048, [root.path(), root.path()]));
            (open_error, observed, budget)
        });
        assert_eq!(
            unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &original) },
            0
        );
        let (open_error, observed, budget) =
            observation.expect("resource capture panicked during denial");
        assert_eq!(open_error, Some(libc::EMFILE));
        let budget = budget.expect("capture requires the confirmed load-read denial");
        assert_eq!(
            loads(&budget).map(|load| load.is_some()),
            std::array::from_fn(|index| index < observed.max(0) as usize),
            "file-read failure alone does not establish native sampler unavailability"
        );
        let after = capture(&host, (0, 0), 0, 2048, [root.path(), root.path()]);
        assert!(loads(&after).into_iter().all(|load| load.is_some()));
    }

    #[test]
    fn native_load_count_preserves_zero_and_omits_unobserved_slots() {
        for (count, expected) in [
            (-1, [None; 3]),
            (0, [None; 3]),
            (1, [Some(0), None, None]),
            (2, [Some(0), Some(123), None]),
            (3, [Some(0), Some(123), Some(250)]),
            (4, [None; 3]),
        ] {
            assert_eq!(observed_loads([0.0, 1.234, 2.5], count), expected);
        }
        assert_eq!(
            observed_loads([f64::NAN, f64::INFINITY, -1.0], 3),
            [None; 3]
        );
    }

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
        let mut budget = capture(&host, (0, 0), 0, 0, [&missing, &missing]);
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
        [
            budget.host_cpu_load_1m_x100,
            budget.host_cpu_load_5m_x100,
            budget.host_cpu_load_15m_x100,
        ] = observed_loads([0.0, 1.0, 2.0], 1);
        let payload = serde_json::to_value(budget).unwrap();
        assert_eq!(payload["host_cpu_load_1m_x100"], 0);
        assert!(payload.get("host_cpu_load_5m_x100").is_none());
        assert!(payload.get("host_cpu_load_15m_x100").is_none());
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
