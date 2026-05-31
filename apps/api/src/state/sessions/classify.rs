use croopor_launcher::{LaunchFailureClass, LaunchState};

pub(super) fn parse_launch_state(state: &str) -> LaunchState {
    match state {
        "queued" => LaunchState::Queued,
        "planning" => LaunchState::Planning,
        "validating" => LaunchState::Validating,
        "ensuring_runtime" => LaunchState::EnsuringRuntime,
        "downloading_runtime" => LaunchState::DownloadingRuntime,
        "preparing" => LaunchState::Preparing,
        "prewarming" => LaunchState::Prewarming,
        "starting" => LaunchState::Starting,
        "monitoring" => LaunchState::Monitoring,
        "running" => LaunchState::Running,
        "degraded" => LaunchState::Degraded,
        "failed" => LaunchState::Failed,
        "exited" => LaunchState::Exited,
        _ => LaunchState::Idle,
    }
}

pub(super) fn is_terminal_state(state: LaunchState) -> bool {
    matches!(state, LaunchState::Failed | LaunchState::Exited)
}

pub(super) fn parse_failure_class(raw: &str) -> LaunchFailureClass {
    LaunchFailureClass::from_name(raw).unwrap_or(LaunchFailureClass::Unknown)
}

pub(super) fn boot_marker_detected(text: &str) -> bool {
    const BOOT_MARKERS: [&str; 3] = ["Setting user:", "LWJGL Version", "[Render thread"];
    BOOT_MARKERS.iter().any(|marker| text.contains(marker))
}
