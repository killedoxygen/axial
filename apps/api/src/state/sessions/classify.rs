use croopor_launcher::{LaunchFailureClass, LaunchState};

pub(super) fn parse_launch_state(state: &str) -> LaunchState {
    match state {
        "queued" => LaunchState::Queued,
        "planning" => LaunchState::Planning,
        "validating" => LaunchState::Validating,
        "ensuring_runtime" => LaunchState::EnsuringRuntime,
        "downloading_runtime" => LaunchState::DownloadingRuntime,
        "preparing" => LaunchState::Preparing,
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

pub(super) fn classify_failure_text(text: &str) -> LaunchFailureClass {
    let lower = text.trim().to_lowercase();
    if lower.is_empty() {
        return LaunchFailureClass::Unknown;
    }
    if lower.contains("unrecognized vm option") || lower.contains("unsupported vm option") {
        return LaunchFailureClass::JvmUnsupportedOption;
    }
    if lower.contains("must be enabled via -xx:+unlockexperimentalvmoptions") {
        return LaunchFailureClass::JvmExperimentalUnlock;
    }
    if lower.contains("unlock option must precede") || lower.contains("must precede") {
        return LaunchFailureClass::JvmOptionOrdering;
    }
    if lower.contains("unsupportedclassversionerror")
        || lower.contains("compiled by a more recent version of the java runtime")
        || lower.contains("requires java")
    {
        return LaunchFailureClass::JavaRuntimeMismatch;
    }
    if lower.contains("resolutionexception: modules")
        || lower.contains("export package")
        || lower.contains("modulelayerhandler.buildlayer")
    {
        return LaunchFailureClass::ClasspathModuleConflict;
    }
    if lower.contains("bootstraplauncher")
        || lower.contains("modlauncher")
        || lower.contains("nosuchelementexception: no value present")
    {
        return LaunchFailureClass::LoaderBootstrapFailure;
    }
    if lower.contains("microsoft account")
        || lower.contains("check your microsoft account")
        || lower.contains("multiplayer is disabled")
    {
        return LaunchFailureClass::AuthModeIncompatible;
    }
    LaunchFailureClass::Unknown
}
