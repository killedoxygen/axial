use crate::process::LaunchStatusEvent;
use crate::types::{LaunchFailureClass, LaunchState};

pub fn launch_state_name(state: LaunchState) -> &'static str {
    match state {
        LaunchState::Idle => "idle",
        LaunchState::Queued => "queued",
        LaunchState::Planning => "planning",
        LaunchState::Validating => "validating",
        LaunchState::EnsuringRuntime => "ensuring_runtime",
        LaunchState::DownloadingRuntime => "downloading_runtime",
        LaunchState::Preparing => "preparing",
        LaunchState::Starting => "starting",
        LaunchState::Monitoring => "monitoring",
        LaunchState::Running => "running",
        LaunchState::Degraded => "degraded",
        LaunchState::Failed => "failed",
        LaunchState::Exited => "exited",
    }
}

pub fn is_terminal_status(status: &LaunchStatusEvent) -> bool {
    matches!(status.state.as_str(), "failed" | "exited")
}

pub fn is_terminal_state(state: LaunchState) -> bool {
    matches!(state, LaunchState::Failed | LaunchState::Exited)
}

pub fn failure_class_name(class: LaunchFailureClass) -> &'static str {
    class.as_str()
}

pub fn format_failure_class(class: LaunchFailureClass) -> &'static str {
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

pub fn snapshot_status(
    record: &crate::process::LaunchSessionRecord,
) -> crate::process::LaunchStatusEvent {
    crate::process::LaunchStatusEvent {
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
