use super::super::resources::{
    active_launch_cpu_pressure, measured_cpu_load_pressure, measured_cpu_load_threshold_x100,
};
use super::*;

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

#[test]
fn cpu_concurrency_pressure_preserves_thread_tier_thresholds() {
    assert!(!active_launch_cpu_pressure(Some(4), 0));
    assert!(active_launch_cpu_pressure(Some(4), 1));

    assert!(!active_launch_cpu_pressure(Some(8), 1));
    assert!(active_launch_cpu_pressure(Some(8), 2));

    assert!(!active_launch_cpu_pressure(Some(16), 3));
    assert!(active_launch_cpu_pressure(Some(16), 4));
}

#[test]
fn measured_cpu_pressure_uses_most_recent_sample_and_thread_tier_thresholds() {
    assert_eq!(measured_cpu_load_threshold_x100(4), 300);
    assert_eq!(measured_cpu_load_threshold_x100(8), 680);
    assert_eq!(measured_cpu_load_threshold_x100(16), 1520);

    let loads = |one, five, fifteen| LaunchCpuLoadEvidence {
        host_cpu_load_1m_x100: one,
        host_cpu_load_5m_x100: five,
        host_cpu_load_15m_x100: fifteen,
    };

    assert!(!measured_cpu_load_pressure(
        Some(4),
        loads(Some(299), Some(300), Some(300)),
    ));
    assert!(measured_cpu_load_pressure(
        Some(4),
        loads(None, Some(300), Some(299)),
    ));
    assert!(measured_cpu_load_pressure(
        Some(4),
        loads(None, None, Some(300)),
    ));
    assert!(measured_cpu_load_pressure(
        Some(8),
        loads(Some(680), None, None),
    ));
    assert!(measured_cpu_load_pressure(
        Some(16),
        loads(Some(1520), None, None),
    ));
}
