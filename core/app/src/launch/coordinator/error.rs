use super::*;

pub(super) fn bundle_read_error(error: std::io::Error) -> LaunchError {
    if error.kind() == std::io::ErrorKind::WouldBlock {
        LaunchError::InstanceBusy
    } else {
        LaunchError::LibraryUnavailable
    }
}

pub(in crate::launch) fn install_read_error(error: InstallError) -> LaunchError {
    match error {
        InstallError::NotReady
        | InstallError::ClientJarMissing
        | InstallError::ClientJarCorrupt
        | InstallError::VersionJsonMissing
        | InstallError::LibrariesMissing
        | InstallError::LibrariesCorrupt
        | InstallError::AssetIndexMissing
        | InstallError::AssetIndexCorrupt => LaunchError::InstallUnavailable,
        InstallError::Busy => LaunchError::InstanceBusy,
        InstallError::AtCapacity => LaunchError::AtCapacity,
        InstallError::Closed => LaunchError::Closed,
        _ => LaunchError::LibraryUnavailable,
    }
}

pub(super) fn game_libraries_error(
    error: crate::install::artifacts::GameLibrariesError,
) -> LaunchError {
    use crate::install::artifacts::GameLibrariesError;

    match error {
        GameLibrariesError::Install(error) => install_read_error(error),
        GameLibrariesError::Physical(error) => physical_work_error(error),
    }
}

pub(in crate::launch) fn physical_work_error(
    error: axial_resource::PhysicalWorkError,
) -> LaunchError {
    use axial_resource::PhysicalWorkError;
    match error {
        PhysicalWorkError::Closed => LaunchError::Closed,
        PhysicalWorkError::Cancelled => LaunchError::Cancelled,
        PhysicalWorkError::ScratchLimit
        | PhysicalWorkError::WorkerLimit
        | PhysicalWorkError::Unavailable => LaunchError::AtCapacity,
        PhysicalWorkError::TaskStopped => LaunchError::PreparationFailed,
    }
}

pub(super) fn instance_error(error: InstanceError) -> LaunchError {
    match error {
        InstanceError::NotFound => LaunchError::InstanceNotFound,
        InstanceError::Busy => LaunchError::InstanceBusy,
        InstanceError::LibraryUnavailable => LaunchError::LibraryUnavailable,
        InstanceError::Closed => LaunchError::Closed,
        _ => LaunchError::InstanceChanged,
    }
}

pub(super) fn session_error(error: SessionError) -> LaunchError {
    match error {
        SessionError::Busy => LaunchError::InstanceBusy,
        SessionError::Closing => LaunchError::Closed,
        SessionError::AtCapacity => LaunchError::AtCapacity,
        SessionError::NotFound => LaunchError::PreparationFailed,
    }
}

/// Only bounded domain text can cross the transport boundary.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaunchError {
    #[error("The launch request is invalid.")]
    InvalidRequest,
    #[error("The launch intent identity is invalid.")]
    InvalidIntent,
    #[error("This launch intent already belongs to a different request.")]
    IntentConflict,
    #[error(
        "This launch was interrupted. Its process outcome is not known; it will not be repeated."
    )]
    Interrupted,
    #[error(
        "The durable launch status is unavailable. Inspect the existing attempt before retrying."
    )]
    IntentUnavailable,
    #[error(
        "Launch history reconciliation is incomplete. Restart to continue. Existing files are preserved."
    )]
    RecoveryIncomplete,
    #[error("The launch request capacity is full. Wait for accepted work to finish.")]
    AtCapacity,
    #[error("Enter a username between 3 and 16 letters, numbers or underscores.")]
    InvalidUsername,
    #[error("The requested memory bounds are invalid.")]
    InvalidMemory,
    #[error("The instance was not found.")]
    InstanceNotFound,
    #[error("The instance is busy. Wait for its current operation to finish.")]
    InstanceBusy,
    #[error("The instance changed while launch was being prepared. Try again.")]
    InstanceChanged,
    #[error("The selected account changed while launch was being prepared. Try again.")]
    AccountChanged,
    #[error("Select an account before launching.")]
    AccountUnavailable,
    #[error("Online launch requires a verified Minecraft Java account. Sign in again.")]
    OnlineAccountUnavailable,
    #[error("The saved profile change could not be settled. Open Skins before retrying.")]
    ProfileUnsettled,
    #[error("The selected library is unavailable. Reconnect it before launching.")]
    LibraryUnavailable,
    #[error("Could not verify installed versions. Check the library folder and try again.")]
    InstalledVersionsDegraded,
    #[error("The installed version requires a completed installation before launch.")]
    InstallUnavailable,
    #[error("The selected Java runtime could not be verified. Check Java settings.")]
    RuntimeUnavailable,
    #[error("The launch settings changed while launch was being prepared. Try again.")]
    SettingsChanged,
    #[error("Performance changes require settlement before this instance can launch.")]
    PerformanceUnsettled,
    #[error("The selected Performance mode is not available for this launch.")]
    PerformanceUnavailable,
    #[error(
        "The launch command could not be validated. Check the instance settings and installation."
    )]
    PlanRejected,
    #[error("Launch was cancelled before a game process was started.")]
    Cancelled,
    #[error("Launches are unavailable while the application is shutting down.")]
    Closed,
    #[error("The launch could not be prepared. Its status is retained for inspection.")]
    PreparationFailed,
    #[error("{0}")]
    #[serde(untagged, skip_deserializing)]
    RuntimeFailure(#[serde(serialize_with = "serialize_runtime_failure")] JavaDiscoveryError),
}

pub(super) fn serialize_runtime_failure<S: serde::Serializer>(
    _: &JavaDiscoveryError,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    // The public code stays fixed; intent persistence retains the typed cause separately.
    LaunchError::RuntimeUnavailable.serialize(serializer)
}

#[derive(Clone, Debug, Serialize)]
pub struct LaunchErrorResponse {
    pub error: String,
    pub code: LaunchError,
}

impl From<LaunchError> for LaunchErrorResponse {
    fn from(code: LaunchError) -> Self {
        Self {
            error: code.to_string(),
            code,
        }
    }
}
