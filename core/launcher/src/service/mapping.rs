use crate::process::{LaunchSessionRecord, LaunchStatusEvent};
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
        LaunchState::Recovering => "recovering",
        LaunchState::Running => "running",
        LaunchState::Degraded => "degraded",
        LaunchState::Settling => "settling",
        LaunchState::Failed => "failed",
        LaunchState::Exited => "exited",
    }
}

pub fn launch_stage_label(stage: &str) -> &'static str {
    match stage {
        "idle" => "Idle",
        "queued" => "Queued",
        "planning" => "Planning launch",
        "validating" => "Validating launch",
        "ensuring_runtime" => "Ensuring runtime",
        "downloading_runtime" => "Downloading runtime",
        "preparing" => "Preparing files",
        "starting" => "Starting process",
        "monitoring" => "Monitoring startup",
        "recovering" => "Recovering startup",
        "running" => "Running",
        "degraded" => "Degraded",
        "settling" => "Finalizing session",
        "failed" => "Failed",
        "exited" => "Exited",
        _ => "Launch stage",
    }
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
        LaunchFailureClass::OutOfMemory => "out of memory",
        LaunchFailureClass::GraphicsDriverCrash => "graphics driver crash",
        LaunchFailureClass::MissingDependency => "missing dependency",
        LaunchFailureClass::ModTransformationFailure => "mod transformation failure",
        LaunchFailureClass::ModAttributedCrash => "mod-attributed crash",
        LaunchFailureClass::ClasspathModuleConflict => "classpath or module conflict",
        LaunchFailureClass::LauncherManagedArtifactSignature => {
            "launcher-managed artifact signature corruption"
        }
        LaunchFailureClass::AuthModeIncompatible => "auth mode incompatibility",
        LaunchFailureClass::LoaderBootstrapFailure => "loader bootstrap failure",
        LaunchFailureClass::StartupStalled => "startup stalled",
    }
}

pub fn snapshot_status(record: &LaunchSessionRecord) -> LaunchStatusEvent {
    LaunchStatusEvent {
        state: launch_state_name(record.state).to_string(),
        benchmark: record.benchmark.clone(),
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
        crash_evidence: record.crash_evidence.clone(),
        healing: record.healing.clone(),
        guardian: record.guardian.clone(),
        outcome: record.outcome.clone(),
        notice: None,
        evidence: Vec::new(),
        stages: record.stages.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::{launch_stage_label, launch_state_name};
    use crate::LaunchState;

    #[test]
    fn recovering_is_a_named_nonterminal_launch_state() {
        assert_eq!(launch_state_name(LaunchState::Recovering), "recovering");
        assert_eq!(launch_stage_label("recovering"), "Recovering startup");
    }

    #[test]
    fn p00_b08_contract_launcher_mapping_keeps_preparing_and_starting_adjacent() {
        assert_eq!(launch_state_name(LaunchState::Preparing), "preparing");
        assert_eq!(launch_stage_label("preparing"), "Preparing files");
        assert_eq!(launch_state_name(LaunchState::Starting), "starting");
        assert_eq!(launch_stage_label("starting"), "Starting process");

        let retired_stage = ["pre", "warming"].concat();
        assert_eq!(launch_stage_label(&retired_stage), "Launch stage");
    }
}
