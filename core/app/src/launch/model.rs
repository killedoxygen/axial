//! Private launch inputs and retained command authority. None are wire DTOs.

use crate::{
    install::{queue::InstalledVersionReceipt, vanilla::PreparedNatives},
    runtime::probe::JavaProbeReceipt,
};
use axial_minecraft::managed_path::ManagedLibraryOperation;
use axial_minecraft::{ManagedRuntimeLaunchReceipt, VersionBundleReadGuard};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    fmt,
    path::{Path, PathBuf},
    sync::Arc,
};

#[derive(Clone)]
pub(crate) struct LaunchAuthContext {
    pub player_name: String,
    pub uuid: String,
    pub access_token: String,
    pub client_id: String,
    pub xuid: String,
    pub user_type: String,
}

impl LaunchAuthContext {
    pub(crate) fn offline(player_name: impl Into<String>) -> Self {
        let player_name = player_name.into();
        Self {
            uuid: axial_minecraft::offline_uuid(&player_name),
            player_name,
            access_token: "0".into(),
            client_id: String::new(),
            xuid: String::new(),
            user_type: "msa".into(),
        }
    }

    pub(super) fn is_offline(&self) -> bool {
        self.access_token == "0"
            && self.client_id.is_empty()
            && self.xuid.is_empty()
            && self.uuid == axial_minecraft::offline_uuid(&self.player_name)
    }
}

impl fmt::Debug for LaunchAuthContext {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.write_str("LaunchAuthContext { identity: [redacted], credentials: [redacted] }")
    }
}

/// Values already selected by the settings/coordinator owner. The planner never
/// changes a saved or explicit override to make a launch succeed.
#[derive(Clone)]
pub(crate) struct LaunchOptions {
    pub min_memory_mb: Option<i32>,
    pub max_memory_mb: Option<i32>,
    pub resolution: Option<(u32, u32)>,
    pub extra_jvm_args: Vec<String>,
    pub jvm_preset: String,
    pub low_impact_startup: bool,
    pub logical_cores: usize,
    pub total_memory_mb: Option<u64>,
    pub loader: String,
    pub is_modded: bool,
    pub launcher_name: String,
    pub launcher_version: String,
}

impl Default for LaunchOptions {
    fn default() -> Self {
        Self {
            min_memory_mb: None,
            max_memory_mb: None,
            resolution: None,
            extra_jvm_args: Vec::new(),
            jvm_preset: String::new(),
            low_impact_startup: false,
            logical_cores: 4,
            total_memory_mb: None,
            loader: String::new(),
            is_modded: false,
            launcher_name: "Axial".into(),
            launcher_version: env!("CARGO_PKG_VERSION").into(),
        }
    }
}

/// Constructed only by launch coordination after retaining the registered
/// instance lease and its library generation. Paths are projections of those
/// capabilities, never values deserialized from a launch request.
pub(crate) struct LaunchPlanRequest {
    pub library_operation: ManagedLibraryOperation,
    pub library_dir: PathBuf,
    pub target_version_id: String,
    pub game_dir: PathBuf,
    pub auth: LaunchAuthContext,
    pub runtime: JavaProbeReceipt,
    pub managed_launch: Option<ManagedRuntimeLaunchReceipt>,
    pub settings: LaunchOptions,
    pub installed: InstalledVersionReceipt,
    pub version_guard: VersionBundleReadGuard,
    pub prepared_natives: Option<Arc<PreparedNatives>>,
}

/// There is no public constructor, deserializer, or raw command export. The
/// session retains this object through process and output settlement.
pub(crate) struct ValidatedLaunchCommand {
    pub(super) program: PathBuf,
    pub(super) args: Vec<String>,
    pub(super) env: BTreeMap<OsString, OsString>,
    pub(super) cwd: PathBuf,
    pub(super) library_operation: ManagedLibraryOperation,
    pub(super) library_dir: PathBuf,
    pub(super) version_guard: VersionBundleReadGuard,
    pub(super) installed: InstalledVersionReceipt,
    pub(super) prepared_natives: Option<Arc<PreparedNatives>>,
    pub(super) runtime: JavaProbeReceipt,
    pub(super) managed_launch: Option<ManagedRuntimeLaunchReceipt>,
}

impl ValidatedLaunchCommand {
    pub(crate) fn program(&self) -> &Path {
        &self.program
    }
    pub(crate) fn args(&self) -> &[String] {
        &self.args
    }
    pub(crate) fn env(&self) -> &BTreeMap<OsString, OsString> {
        &self.env
    }
    pub(crate) fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// Fingerprints and retained capabilities only. Never starts Java.
    pub(crate) fn revalidate(&self) -> Result<(), LaunchPlanError> {
        self.library_operation
            .validate_read_projection(&self.library_dir)
            .map_err(|_| LaunchPlanError::LibraryChanged)?;
        self.version_guard
            .revalidate()
            .map_err(|_| LaunchPlanError::LibraryChanged)?;
        self.runtime
            .revalidate()
            .map_err(|_| LaunchPlanError::RuntimeChanged)?;
        if let Some(receipt) = &self.managed_launch {
            receipt
                .validate_program(&self.program)
                .map_err(|_| LaunchPlanError::RuntimeChanged)?;
        }
        self.installed
            .revalidate()
            .map_err(|_| LaunchPlanError::ArtifactChanged)?;
        if let Some(natives) = &self.prepared_natives {
            natives
                .revalidate()
                .map_err(|_| LaunchPlanError::ArtifactChanged)?;
        }
        Ok(())
    }
}

impl fmt::Debug for ValidatedLaunchCommand {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.debug_struct("ValidatedLaunchCommand")
            .field("program", &"[private]")
            .field("arguments", &"[redacted]")
            .field("working_directory", &"[private]")
            .finish_non_exhaustive()
    }
}

/// Deliberately contains no raw paths, arguments, provider errors, or tokens.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum LaunchPlanError {
    #[error("The selected library changed. Refresh the instance before launching.")]
    LibraryChanged,
    #[error("The selected version is invalid.")]
    InvalidVersion,
    #[error(
        "Installed version metadata is missing or invalid. Install this version before launching."
    )]
    VersionUnavailable,
    #[error("Parent version metadata is missing. Install the base version before launching.")]
    ParentUnavailable,
    #[error("The game directory is unavailable.")]
    GameDirectoryUnavailable,
    #[error("Game files are missing. Install this version before launching.")]
    ArtifactMissing,
    #[error("Game file integrity could not be verified. Install this version before launching.")]
    ArtifactInvalid,
    #[error("Game files changed during preparation. Try launching again.")]
    ArtifactChanged,
    #[error("Native libraries have not been prepared. Finish installation before launching.")]
    NativesUnavailable,
    #[error("Legacy game assets have not been prepared. Finish installation before launching.")]
    LegacyAssetsUnavailable,
    #[error("The selected Java runtime changed or is unavailable. Select a runtime again.")]
    RuntimeChanged,
    #[error("The selected Java runtime is incompatible with this version.")]
    RuntimeIncompatible,
    #[error("The selected account is not ready to launch.")]
    InvalidAccount,
    #[error("The launch memory settings are invalid.")]
    InvalidMemory,
    #[error("The game window dimensions are invalid.")]
    InvalidResolution,
    #[error("The custom JVM arguments are invalid. Check their quoting and values.")]
    InvalidJvmArguments,
    #[error(
        "Custom JVM arguments override launcher-managed memory, paths, agents, or entrypoints. Remove those arguments before launching."
    )]
    ReservedJvmArgument,
    #[error("The selected JVM options are unsupported by this Java runtime.")]
    UnsupportedJvmOption,
    #[error(
        "Experimental JVM options require UnlockExperimentalVMOptions before dependent arguments."
    )]
    JvmOptionOrdering,
    #[error("The selected JVM preset is incompatible with this Java runtime or game version.")]
    IncompatiblePreset,
    #[error("The installed launch command is invalid or contains unresolved variables.")]
    InvalidCommand,
    #[error("The installed version exceeds launch inspection limits.")]
    InspectionLimit,
}
