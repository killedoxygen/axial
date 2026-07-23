use super::diagnosis::Diagnosis;
use crate::observability::EvidenceField;
use crate::state::contracts::{
    OperationId, OperationPhase, OwnershipClass, StabilizationSystem, TargetDescriptor,
};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};

macro_rules! guardian_modes {
    ($($variant:ident),+ $(,)?) => {
        #[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
        pub enum GuardianMode {
            $($variant),+
        }

        impl GuardianMode {
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];

            pub(crate) fn from_config(value: &str) -> Self {
                match value.trim() {
                    "custom" => Self::Custom,
                    "disabled" => Self::Disabled,
                    _ => Self::Managed,
                }
            }
        }
    };
}

guardian_modes!(Managed, Custom, Disabled);

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum GuardianFactId {
    AgentHookFailed,
    AgentUnavailable,
    ArtifactChecksumMismatch,
    ArtifactHashMismatch,
    ArtifactMissing,
    ArtifactQuarantined,
    ArtifactSizeDrift,
    ArtifactSizeMismatch,
    AssetIndexMissing,
    AtomicPromotionCompleted,
    AtomicPromotionFailed,
    AuthModeIncompatible,
    BootMarkerObserved,
    BootMilestoneOverdue,
    BootMilestoneReached,
    ClasspathModuleConflict,
    ClientJarMissing,
    CustomJavaOverridePresent,
    CustomJvmArgsPresent,
    CustomJvmPresetPresent,
    DownloadInterrupted,
    DownloadProviderUnavailable,
    DownloadWrittenToTemp,
    ExitCodeNonzero,
    ExitCodeUnknown,
    ExitCodeZero,
    FilesystemPermissionDenied,
    FrameBudgetExceeded,
    GcPauseStorm,
    GraphicsDriverCrash,
    HeapPressureCritical,
    IncompleteInstall,
    InstallDependencyFailed,
    InstallExecutionFailed,
    InstallProcessorFailed,
    InstalledVersionsDegraded,
    JavaMajorMismatch,
    JavaOverrideEmpty,
    JavaOverrideMissing,
    JavaOverrideUndefinedSentinel,
    JavaProbeFailed,
    JavaUpdateTooOld,
    JvmArgAgentOverride,
    JvmArgExperimentalUnlockMissing,
    JvmArgMemoryConflict,
    JvmArgReservedLauncherFlag,
    JvmArgUnlockOrderInvalid,
    JvmArgUnsafeClasspathOverride,
    JvmArgUnsafeNativePathOverride,
    JvmArgUnsupported,
    JvmArgUnsupportedGc,
    JvmArgsEmpty,
    JvmArgsParseFailed,
    JvmPresetCompatibilityAdjusted,
    LaunchFailureClassified,
    LaunchJvmPresetDowngradeAvailable,
    LaunchJvmStripAvailable,
    LaunchMemoryAllocationLow,
    LaunchMemoryMinClamped,
    LaunchResourceCpuPressure,
    LaunchResourceDiskPressure,
    LaunchResourceInstallPressure,
    LaunchResourceMemoryPressure,
    LaunchRuntimeFallbackAvailable,
    LauncherManagedArtifactSignatureCorruption,
    LauncherStopRequested,
    LibrariesMissing,
    LoaderBootstrapFailure,
    ManagedRuntimeCorrupt,
    ManagedRuntimeMissing,
    ManagedRuntimeReadyMarkerMissing,
    ManagedRuntimeRepairApplied,
    ManagedRuntimeRosettaRequired,
    ManagedRuntimeUnavailableForPlatform,
    MissingDependency,
    ModAttributedCrash,
    ModTransformationFailure,
    NoStructuredFact(OperationPhase),
    OutOfMemory,
    ParentVersionMissing,
    PerformanceFallbackSelected,
    PerformanceHealthInvalid,
    PerformanceRulesInvalid,
    PerformanceUserOwnedConflict,
    PersistedStateRepairAvailable,
    PersistedStateSchemaInvalid,
    PrimitiveRefused,
    ProcessExited,
    ProcessExitedAfterBoot,
    ProcessExitedBeforeBoot,
    ProcessKilled,
    ProcessSpawned,
    ProviderDataInvalid,
    RecentRepairFailed,
    RecentStartupFailure,
    RegisteredArtifactRepairAvailable,
    RegisteredComponentRebuildFailed,
    RepairSuppressedUntil,
    StartupWindowExpired,
    TempFileWriteFailed,
    UnknownLaunchFailure,
    UserModSetDrift,
    VersionJsonMissing,
    WatchdogActionObserved,
    WatchdogKilledProcess,
}

impl GuardianFactId {
    pub const ALL: [Self; 116] = [
        Self::AgentHookFailed,
        Self::AgentUnavailable,
        Self::ArtifactChecksumMismatch,
        Self::ArtifactHashMismatch,
        Self::ArtifactMissing,
        Self::ArtifactQuarantined,
        Self::ArtifactSizeDrift,
        Self::ArtifactSizeMismatch,
        Self::AssetIndexMissing,
        Self::AtomicPromotionCompleted,
        Self::AtomicPromotionFailed,
        Self::AuthModeIncompatible,
        Self::BootMarkerObserved,
        Self::BootMilestoneOverdue,
        Self::BootMilestoneReached,
        Self::ClasspathModuleConflict,
        Self::ClientJarMissing,
        Self::CustomJavaOverridePresent,
        Self::CustomJvmArgsPresent,
        Self::CustomJvmPresetPresent,
        Self::DownloadInterrupted,
        Self::DownloadProviderUnavailable,
        Self::DownloadWrittenToTemp,
        Self::ExitCodeNonzero,
        Self::ExitCodeUnknown,
        Self::ExitCodeZero,
        Self::FilesystemPermissionDenied,
        Self::FrameBudgetExceeded,
        Self::GcPauseStorm,
        Self::GraphicsDriverCrash,
        Self::HeapPressureCritical,
        Self::IncompleteInstall,
        Self::InstallDependencyFailed,
        Self::InstallExecutionFailed,
        Self::InstallProcessorFailed,
        Self::InstalledVersionsDegraded,
        Self::JavaMajorMismatch,
        Self::JavaOverrideEmpty,
        Self::JavaOverrideMissing,
        Self::JavaOverrideUndefinedSentinel,
        Self::JavaProbeFailed,
        Self::JavaUpdateTooOld,
        Self::JvmArgAgentOverride,
        Self::JvmArgExperimentalUnlockMissing,
        Self::JvmArgMemoryConflict,
        Self::JvmArgReservedLauncherFlag,
        Self::JvmArgUnlockOrderInvalid,
        Self::JvmArgUnsafeClasspathOverride,
        Self::JvmArgUnsafeNativePathOverride,
        Self::JvmArgUnsupported,
        Self::JvmArgUnsupportedGc,
        Self::JvmArgsEmpty,
        Self::JvmArgsParseFailed,
        Self::JvmPresetCompatibilityAdjusted,
        Self::LaunchFailureClassified,
        Self::LaunchJvmPresetDowngradeAvailable,
        Self::LaunchJvmStripAvailable,
        Self::LaunchMemoryAllocationLow,
        Self::LaunchMemoryMinClamped,
        Self::LaunchResourceCpuPressure,
        Self::LaunchResourceDiskPressure,
        Self::LaunchResourceInstallPressure,
        Self::LaunchResourceMemoryPressure,
        Self::LaunchRuntimeFallbackAvailable,
        Self::LauncherManagedArtifactSignatureCorruption,
        Self::LauncherStopRequested,
        Self::LibrariesMissing,
        Self::LoaderBootstrapFailure,
        Self::ManagedRuntimeCorrupt,
        Self::ManagedRuntimeMissing,
        Self::ManagedRuntimeReadyMarkerMissing,
        Self::ManagedRuntimeRepairApplied,
        Self::ManagedRuntimeRosettaRequired,
        Self::ManagedRuntimeUnavailableForPlatform,
        Self::MissingDependency,
        Self::ModAttributedCrash,
        Self::ModTransformationFailure,
        Self::NoStructuredFact(OperationPhase::Completed),
        Self::NoStructuredFact(OperationPhase::Downloading),
        Self::NoStructuredFact(OperationPhase::Failed),
        Self::NoStructuredFact(OperationPhase::Installing),
        Self::NoStructuredFact(OperationPhase::Launching),
        Self::NoStructuredFact(OperationPhase::Planning),
        Self::NoStructuredFact(OperationPhase::Preparing),
        Self::NoStructuredFact(OperationPhase::Repairing),
        Self::NoStructuredFact(OperationPhase::RollingBack),
        Self::NoStructuredFact(OperationPhase::Running),
        Self::NoStructuredFact(OperationPhase::Startup),
        Self::NoStructuredFact(OperationPhase::Validating),
        Self::OutOfMemory,
        Self::ParentVersionMissing,
        Self::PerformanceFallbackSelected,
        Self::PerformanceHealthInvalid,
        Self::PerformanceRulesInvalid,
        Self::PerformanceUserOwnedConflict,
        Self::PersistedStateRepairAvailable,
        Self::PersistedStateSchemaInvalid,
        Self::PrimitiveRefused,
        Self::ProcessExited,
        Self::ProcessExitedAfterBoot,
        Self::ProcessExitedBeforeBoot,
        Self::ProcessKilled,
        Self::ProcessSpawned,
        Self::ProviderDataInvalid,
        Self::RecentRepairFailed,
        Self::RecentStartupFailure,
        Self::RegisteredArtifactRepairAvailable,
        Self::RegisteredComponentRebuildFailed,
        Self::RepairSuppressedUntil,
        Self::StartupWindowExpired,
        Self::TempFileWriteFailed,
        Self::UnknownLaunchFailure,
        Self::UserModSetDrift,
        Self::VersionJsonMissing,
        Self::WatchdogActionObserved,
        Self::WatchdogKilledProcess,
    ];

    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::AgentHookFailed => "agent_hook_failed",
            Self::AgentUnavailable => "agent_unavailable",
            Self::ArtifactChecksumMismatch => "artifact_checksum_mismatch",
            Self::ArtifactHashMismatch => "artifact_hash_mismatch",
            Self::ArtifactMissing => "artifact_missing",
            Self::ArtifactQuarantined => "artifact_quarantined",
            Self::ArtifactSizeDrift => "artifact_size_drift",
            Self::ArtifactSizeMismatch => "artifact_size_mismatch",
            Self::AssetIndexMissing => "asset_index_missing",
            Self::AtomicPromotionCompleted => "atomic_promotion_completed",
            Self::AtomicPromotionFailed => "atomic_promotion_failed",
            Self::AuthModeIncompatible => "auth_mode_incompatible",
            Self::BootMarkerObserved => "boot_marker_observed",
            Self::BootMilestoneOverdue => "boot_milestone_overdue",
            Self::BootMilestoneReached => "boot_milestone_reached",
            Self::ClasspathModuleConflict => "classpath_module_conflict",
            Self::ClientJarMissing => "client_jar_missing",
            Self::CustomJavaOverridePresent => "custom_java_override_present",
            Self::CustomJvmArgsPresent => "custom_jvm_args_present",
            Self::CustomJvmPresetPresent => "custom_jvm_preset_present",
            Self::DownloadInterrupted => "download_interrupted",
            Self::DownloadProviderUnavailable => "download_provider_unavailable",
            Self::DownloadWrittenToTemp => "download_written_to_temp",
            Self::ExitCodeNonzero => "exit_code_nonzero",
            Self::ExitCodeUnknown => "exit_code_unknown",
            Self::ExitCodeZero => "exit_code_zero",
            Self::FilesystemPermissionDenied => "filesystem_permission_denied",
            Self::FrameBudgetExceeded => "frame_budget_exceeded",
            Self::GcPauseStorm => "gc_pause_storm",
            Self::GraphicsDriverCrash => "graphics_driver_crash",
            Self::HeapPressureCritical => "heap_pressure_critical",
            Self::IncompleteInstall => "incomplete_install",
            Self::InstallDependencyFailed => "install_dependency_failed",
            Self::InstallExecutionFailed => "install_execution_failed",
            Self::InstallProcessorFailed => "install_processor_failed",
            Self::InstalledVersionsDegraded => "installed_versions_degraded",
            Self::JavaMajorMismatch => "java_major_mismatch",
            Self::JavaOverrideEmpty => "java_override_empty",
            Self::JavaOverrideMissing => "java_override_missing",
            Self::JavaOverrideUndefinedSentinel => "java_override_undefined_sentinel",
            Self::JavaProbeFailed => "java_probe_failed",
            Self::JavaUpdateTooOld => "java_update_too_old",
            Self::JvmArgAgentOverride => "jvm_arg_agent_override",
            Self::JvmArgExperimentalUnlockMissing => "jvm_arg_experimental_unlock_missing",
            Self::JvmArgMemoryConflict => "jvm_arg_memory_conflict",
            Self::JvmArgReservedLauncherFlag => "jvm_arg_reserved_launcher_flag",
            Self::JvmArgUnlockOrderInvalid => "jvm_arg_unlock_order_invalid",
            Self::JvmArgUnsafeClasspathOverride => "jvm_arg_unsafe_classpath_override",
            Self::JvmArgUnsafeNativePathOverride => "jvm_arg_unsafe_native_path_override",
            Self::JvmArgUnsupported => "jvm_arg_unsupported",
            Self::JvmArgUnsupportedGc => "jvm_arg_unsupported_gc",
            Self::JvmArgsEmpty => "jvm_args_empty",
            Self::JvmArgsParseFailed => "jvm_args_parse_failed",
            Self::JvmPresetCompatibilityAdjusted => "jvm_preset_compatibility_adjusted",
            Self::LaunchFailureClassified => "launch_failure_classified",
            Self::LaunchJvmPresetDowngradeAvailable => "launch_jvm_preset_downgrade_available",
            Self::LaunchJvmStripAvailable => "launch_jvm_strip_available",
            Self::LaunchMemoryAllocationLow => "launch_memory_allocation_low",
            Self::LaunchMemoryMinClamped => "launch_memory_min_clamped",
            Self::LaunchResourceCpuPressure => "launch_resource_cpu_pressure",
            Self::LaunchResourceDiskPressure => "launch_resource_disk_pressure",
            Self::LaunchResourceInstallPressure => "launch_resource_install_pressure",
            Self::LaunchResourceMemoryPressure => "launch_resource_memory_pressure",
            Self::LaunchRuntimeFallbackAvailable => "launch_runtime_fallback_available",
            Self::LauncherManagedArtifactSignatureCorruption => {
                "launcher_managed_artifact_signature_corruption"
            }
            Self::LauncherStopRequested => "launcher_stop_requested",
            Self::LibrariesMissing => "libraries_missing",
            Self::LoaderBootstrapFailure => "loader_bootstrap_failure",
            Self::ManagedRuntimeCorrupt => "managed_runtime_corrupt",
            Self::ManagedRuntimeMissing => "managed_runtime_missing",
            Self::ManagedRuntimeReadyMarkerMissing => "managed_runtime_ready_marker_missing",
            Self::ManagedRuntimeRepairApplied => "managed_runtime_repair_applied",
            Self::ManagedRuntimeRosettaRequired => "managed_runtime_rosetta_required",
            Self::ManagedRuntimeUnavailableForPlatform => {
                "managed_runtime_unavailable_for_platform"
            }
            Self::MissingDependency => "missing_dependency",
            Self::ModAttributedCrash => "mod_attributed_crash",
            Self::ModTransformationFailure => "mod_transformation_failure",
            Self::NoStructuredFact(phase) => match phase {
                OperationPhase::Startup => "no_structured_fact_startup",
                OperationPhase::Planning => "no_structured_fact_planning",
                OperationPhase::Validating => "no_structured_fact_validating",
                OperationPhase::Downloading => "no_structured_fact_downloading",
                OperationPhase::Installing => "no_structured_fact_installing",
                OperationPhase::Preparing => "no_structured_fact_preparing",
                OperationPhase::Launching => "no_structured_fact_launching",
                OperationPhase::Running => "no_structured_fact_running",
                OperationPhase::Repairing => "no_structured_fact_repairing",
                OperationPhase::RollingBack => "no_structured_fact_rolling_back",
                OperationPhase::Completed => "no_structured_fact_completed",
                OperationPhase::Failed => "no_structured_fact_failed",
            },
            Self::OutOfMemory => "out_of_memory",
            Self::ParentVersionMissing => "parent_version_missing",
            Self::PerformanceFallbackSelected => "performance_fallback_selected",
            Self::PerformanceHealthInvalid => "performance_health_invalid",
            Self::PerformanceRulesInvalid => "performance_rules_invalid",
            Self::PerformanceUserOwnedConflict => "performance_user_owned_conflict",
            Self::PersistedStateRepairAvailable => "persisted_state_repair_available",
            Self::PersistedStateSchemaInvalid => "persisted_state_schema_invalid",
            Self::PrimitiveRefused => "primitive_refused",
            Self::ProcessExited => "process_exited",
            Self::ProcessExitedAfterBoot => "process_exited_after_boot",
            Self::ProcessExitedBeforeBoot => "process_exited_before_boot",
            Self::ProcessKilled => "process_killed",
            Self::ProcessSpawned => "process_spawned",
            Self::ProviderDataInvalid => "provider_data_invalid",
            Self::RecentRepairFailed => "recent_repair_failed",
            Self::RecentStartupFailure => "recent_startup_failure",
            Self::RegisteredArtifactRepairAvailable => "registered_artifact_repair_available",
            Self::RegisteredComponentRebuildFailed => "registered_component_rebuild_failed",
            Self::RepairSuppressedUntil => "repair_suppressed_until",
            Self::StartupWindowExpired => "startup_window_expired",
            Self::TempFileWriteFailed => "temp_file_write_failed",
            Self::UnknownLaunchFailure => "unknown_launch_failure",
            Self::UserModSetDrift => "user_mod_set_drift",
            Self::VersionJsonMissing => "version_json_missing",
            Self::WatchdogActionObserved => "watchdog_action_observed",
            Self::WatchdogKilledProcess => "watchdog_killed_process",
        }
    }

    fn from_wire(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|candidate| candidate.as_str() == value)
    }
}

impl Serialize for GuardianFactId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for GuardianFactId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::from_wire(&value).ok_or_else(|| D::Error::custom("unknown Guardian fact id"))
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct GuardianFact {
    pub operation_id: Option<OperationId>,
    pub id: GuardianFactId,
    pub domain: GuardianDomain,
    pub phase: OperationPhase,
    pub reliability: FactReliability,
    pub severity: Option<GuardianSeverity>,
    pub confidence: Option<GuardianConfidence>,
    pub ownership: OwnershipClass,
    pub target: Option<TargetDescriptor>,
    pub fields: Vec<EvidenceField>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum FactReliability {
    DirectStructured,
    ValidatedProbe,
    ProcessLifecycle,
    ExactClassifier,
    HeuristicClassifier,
    ExpectedMarkerAbsence,
    UserReported,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum DiagnosisId {
    ArtifactOwnershipUnsafe,
    AtomicPromotionFailed,
    DownloadUnavailable,
    FilesystemPermissionDenied,
    InstallArtifactMetadataInvalid,
    InstallDependencyFailed,
    InstallExecutionFailed,
    InstallProcessorFailed,
    JavaOverrideUnavailable,
    JavaProbeFailed,
    JavaRuntimeMajorMismatch,
    JavaRuntimeUpdateTooOld,
    JvmArgUnsafeOverride,
    JvmArgUnsupported,
    JvmArgsEmpty,
    JvmArgsMalformed,
    LauncherManagedArtifactCorrupt,
    LauncherManagedArtifactSignatureCorrupt,
    ManagedRuntimeCorrupt,
    ManagedRuntimeMissing,
    ManagedRuntimeRosettaRequired,
    ManagedRuntimeUnavailableForPlatform,
    PerformanceFallbackSelected,
    PerformanceRulesInvalid,
    PerformanceUserOwnedConflict,
    PersistedStateSchemaInvalid,
    ProcessLifecycleObserved,
    TempFileWriteFailed,
    InstalledVersionMetadataMissing,
    ParentVersionMetadataMissing,
    InstallIncomplete,
    ClientJarMissing,
    LibrariesMissing,
    AssetIndexMissing,
    LaunchMemoryMinClamped,
    LaunchMemoryAllocationLow,
    LaunchResourceMemoryPressure,
    LaunchResourceCpuPressure,
    LaunchResourceInstallPressure,
    LaunchResourceDiskPressure,
    CustomJavaOverridePresent,
    CustomJvmPresetPresent,
    CustomJvmArgsPresent,
    PerformanceHealthInvalid,
    JvmPresetAdjusted,
    LaunchPrepareFailed,
    StartupStalled,
    OutOfMemory,
    GraphicsDriverCrash,
    MissingDependency,
    ModTransformationFailure,
    ModAttributedCrash,
    ClasspathModuleConflict,
    AuthModeIncompatible,
    LoaderBootstrapFailure,
    StartupFailedUnknown,
    JavaRuntimeRecovery,
    JvmPresetRecovery,
    LaunchFailureUnknown,
    JvmUnsupportedOption,
    JvmExperimentalUnlock,
    JvmOptionOrdering,
    JavaRuntimeMismatch,
    LauncherManagedArtifactSignature,
    UnknownFailure(OperationPhase),
}

impl DiagnosisId {
    pub const ALL: [Self; 76] = [
        Self::ArtifactOwnershipUnsafe,
        Self::AtomicPromotionFailed,
        Self::DownloadUnavailable,
        Self::FilesystemPermissionDenied,
        Self::InstallArtifactMetadataInvalid,
        Self::InstallDependencyFailed,
        Self::InstallExecutionFailed,
        Self::InstallProcessorFailed,
        Self::JavaOverrideUnavailable,
        Self::JavaProbeFailed,
        Self::JavaRuntimeMajorMismatch,
        Self::JavaRuntimeUpdateTooOld,
        Self::JvmArgUnsafeOverride,
        Self::JvmArgUnsupported,
        Self::JvmArgsEmpty,
        Self::JvmArgsMalformed,
        Self::LauncherManagedArtifactCorrupt,
        Self::LauncherManagedArtifactSignatureCorrupt,
        Self::ManagedRuntimeCorrupt,
        Self::ManagedRuntimeMissing,
        Self::ManagedRuntimeRosettaRequired,
        Self::ManagedRuntimeUnavailableForPlatform,
        Self::PerformanceFallbackSelected,
        Self::PerformanceRulesInvalid,
        Self::PerformanceUserOwnedConflict,
        Self::PersistedStateSchemaInvalid,
        Self::ProcessLifecycleObserved,
        Self::TempFileWriteFailed,
        Self::InstalledVersionMetadataMissing,
        Self::ParentVersionMetadataMissing,
        Self::InstallIncomplete,
        Self::ClientJarMissing,
        Self::LibrariesMissing,
        Self::AssetIndexMissing,
        Self::LaunchMemoryMinClamped,
        Self::LaunchMemoryAllocationLow,
        Self::LaunchResourceMemoryPressure,
        Self::LaunchResourceCpuPressure,
        Self::LaunchResourceInstallPressure,
        Self::LaunchResourceDiskPressure,
        Self::CustomJavaOverridePresent,
        Self::CustomJvmPresetPresent,
        Self::CustomJvmArgsPresent,
        Self::PerformanceHealthInvalid,
        Self::JvmPresetAdjusted,
        Self::LaunchPrepareFailed,
        Self::StartupStalled,
        Self::OutOfMemory,
        Self::GraphicsDriverCrash,
        Self::MissingDependency,
        Self::ModTransformationFailure,
        Self::ModAttributedCrash,
        Self::ClasspathModuleConflict,
        Self::AuthModeIncompatible,
        Self::LoaderBootstrapFailure,
        Self::StartupFailedUnknown,
        Self::JavaRuntimeRecovery,
        Self::JvmPresetRecovery,
        Self::LaunchFailureUnknown,
        Self::JvmUnsupportedOption,
        Self::JvmExperimentalUnlock,
        Self::JvmOptionOrdering,
        Self::JavaRuntimeMismatch,
        Self::LauncherManagedArtifactSignature,
        Self::UnknownFailure(OperationPhase::Startup),
        Self::UnknownFailure(OperationPhase::Planning),
        Self::UnknownFailure(OperationPhase::Validating),
        Self::UnknownFailure(OperationPhase::Downloading),
        Self::UnknownFailure(OperationPhase::Installing),
        Self::UnknownFailure(OperationPhase::Preparing),
        Self::UnknownFailure(OperationPhase::Launching),
        Self::UnknownFailure(OperationPhase::Running),
        Self::UnknownFailure(OperationPhase::Repairing),
        Self::UnknownFailure(OperationPhase::RollingBack),
        Self::UnknownFailure(OperationPhase::Completed),
        Self::UnknownFailure(OperationPhase::Failed),
    ];

    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::ArtifactOwnershipUnsafe => "artifact_ownership_unsafe",
            Self::AtomicPromotionFailed => "atomic_promotion_failed",
            Self::DownloadUnavailable => "download_unavailable",
            Self::FilesystemPermissionDenied => "filesystem_permission_denied",
            Self::InstallArtifactMetadataInvalid => "install_artifact_metadata_invalid",
            Self::InstallDependencyFailed => "install_dependency_failed",
            Self::InstallExecutionFailed => "install_execution_failed",
            Self::InstallProcessorFailed => "install_processor_failed",
            Self::JavaOverrideUnavailable => "java_override_unavailable",
            Self::JavaProbeFailed => "java_probe_failed",
            Self::JavaRuntimeMajorMismatch => "java_runtime_major_mismatch",
            Self::JavaRuntimeUpdateTooOld => "java_runtime_update_too_old",
            Self::JvmArgUnsafeOverride => "jvm_arg_unsafe_override",
            Self::JvmArgUnsupported => "jvm_arg_unsupported",
            Self::JvmArgsEmpty => "jvm_args_empty",
            Self::JvmArgsMalformed => "jvm_args_malformed",
            Self::LauncherManagedArtifactCorrupt => "launcher_managed_artifact_corrupt",
            Self::LauncherManagedArtifactSignatureCorrupt => {
                "launcher_managed_artifact_signature_corrupt"
            }
            Self::ManagedRuntimeCorrupt => "managed_runtime_corrupt",
            Self::ManagedRuntimeMissing => "managed_runtime_missing",
            Self::ManagedRuntimeRosettaRequired => "managed_runtime_rosetta_required",
            Self::ManagedRuntimeUnavailableForPlatform => {
                "managed_runtime_unavailable_for_platform"
            }
            Self::PerformanceFallbackSelected => "performance_fallback_selected",
            Self::PerformanceRulesInvalid => "performance_rules_invalid",
            Self::PerformanceUserOwnedConflict => "performance_user_owned_conflict",
            Self::PersistedStateSchemaInvalid => "persisted_state_schema_invalid",
            Self::ProcessLifecycleObserved => "process_lifecycle_observed",
            Self::TempFileWriteFailed => "temp_file_write_failed",
            Self::InstalledVersionMetadataMissing => "installed_version_metadata_missing",
            Self::ParentVersionMetadataMissing => "parent_version_metadata_missing",
            Self::InstallIncomplete => "install_incomplete",
            Self::ClientJarMissing => "client_jar_missing",
            Self::LibrariesMissing => "libraries_missing",
            Self::AssetIndexMissing => "asset_index_missing",
            Self::LaunchMemoryMinClamped => "launch_memory_min_clamped",
            Self::LaunchMemoryAllocationLow => "launch_memory_allocation_low",
            Self::LaunchResourceMemoryPressure => "launch_resource_memory_pressure",
            Self::LaunchResourceCpuPressure => "launch_resource_cpu_pressure",
            Self::LaunchResourceInstallPressure => "launch_resource_install_pressure",
            Self::LaunchResourceDiskPressure => "launch_resource_disk_pressure",
            Self::CustomJavaOverridePresent => "custom_java_override_present",
            Self::CustomJvmPresetPresent => "custom_jvm_preset_present",
            Self::CustomJvmArgsPresent => "custom_jvm_args_present",
            Self::PerformanceHealthInvalid => "performance_health_invalid",
            Self::JvmPresetAdjusted => "jvm_preset_adjusted",
            Self::LaunchPrepareFailed => "launch_prepare_failed",
            Self::StartupStalled => "startup_stalled",
            Self::OutOfMemory => "out_of_memory",
            Self::GraphicsDriverCrash => "graphics_driver_crash",
            Self::MissingDependency => "missing_dependency",
            Self::ModTransformationFailure => "mod_transformation_failure",
            Self::ModAttributedCrash => "mod_attributed_crash",
            Self::ClasspathModuleConflict => "classpath_module_conflict",
            Self::AuthModeIncompatible => "auth_mode_incompatible",
            Self::LoaderBootstrapFailure => "loader_bootstrap_failure",
            Self::StartupFailedUnknown => "startup_failed_unknown",
            Self::JavaRuntimeRecovery => "java_runtime_recovery",
            Self::JvmPresetRecovery => "jvm_preset_recovery",
            Self::LaunchFailureUnknown => "unknown",
            Self::JvmUnsupportedOption => "jvm_unsupported_option",
            Self::JvmExperimentalUnlock => "jvm_experimental_unlock",
            Self::JvmOptionOrdering => "jvm_option_ordering",
            Self::JavaRuntimeMismatch => "java_runtime_mismatch",
            Self::LauncherManagedArtifactSignature => "launcher_managed_artifact_signature",
            Self::UnknownFailure(phase) => match phase {
                OperationPhase::Startup => "unknown_failure_startup",
                OperationPhase::Planning => "unknown_failure_planning",
                OperationPhase::Validating => "unknown_failure_validating",
                OperationPhase::Downloading => "unknown_failure_downloading",
                OperationPhase::Installing => "unknown_failure_installing",
                OperationPhase::Preparing => "unknown_failure_preparing",
                OperationPhase::Launching => "unknown_failure_launching",
                OperationPhase::Running => "unknown_failure_running",
                OperationPhase::Repairing => "unknown_failure_repairing",
                OperationPhase::RollingBack => "unknown_failure_rolling_back",
                OperationPhase::Completed => "unknown_failure_completed",
                OperationPhase::Failed => "unknown_failure_failed",
            },
        }
    }

    fn from_wire(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|candidate| candidate.as_str() == value)
    }
}

impl Serialize for DiagnosisId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for DiagnosisId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::from_wire(&value).ok_or_else(|| D::Error::custom("unknown Guardian diagnosis id"))
    }
}

impl std::fmt::Display for DiagnosisId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum GuardianDomain {
    Config,
    Library,
    Runtime,
    Jvm,
    Install,
    Download,
    Performance,
    Launch,
    Startup,
    Session,
    Filesystem,
    Network,
    Auth,
    State,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum GuardianSeverity {
    Info,
    Warning,
    Degraded,
    Repairable,
    Recoverable,
    Blocking,
    Critical,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum GuardianConfidence {
    Low,
    Medium,
    High,
    Confirmed,
    Certain,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SafetyCase {
    pub operation_id: Option<OperationId>,
    pub mode: GuardianMode,
    pub phase: OperationPhase,
    pub diagnoses: Vec<Diagnosis>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ActionPlanPrerequisite {
    pub diagnosis_id: DiagnosisId,
    pub ownership: OwnershipClass,
    pub confidence: GuardianConfidence,
    pub affected_targets: Vec<TargetDescriptor>,
    pub candidate_actions: Vec<GuardianActionKind>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct GuardianActionPlan {
    pub owner: StabilizationSystem,
    pub prerequisite: ActionPlanPrerequisite,
    pub actions: Vec<GuardianAction>,
}

impl GuardianActionPlan {
    pub fn new(
        owner: StabilizationSystem,
        prerequisite: ActionPlanPrerequisite,
        actions: Vec<GuardianAction>,
    ) -> Self {
        Self {
            owner,
            prerequisite,
            actions,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct GuardianAction {
    pub kind: GuardianActionKind,
    pub target: Option<TargetDescriptor>,
    pub reason: DiagnosisId,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum GuardianActionKind {
    Allow,
    Warn,
    Repair,
    Retry,
    Strip,
    Downgrade,
    Fallback,
    Quarantine,
    AskUser,
    Block,
    RecordOnly,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SafetyOutcome {
    pub decision: GuardianActionKind,
    pub summary: String,
    pub detail: Option<String>,
    pub diagnoses: Vec<DiagnosisId>,
}

#[cfg(test)]
mod tests {
    use super::GuardianMode;

    #[test]
    fn config_modes_have_one_canonical_guardian_parser() {
        assert_eq!(GuardianMode::from_config("managed"), GuardianMode::Managed);
        assert_eq!(GuardianMode::from_config(" custom "), GuardianMode::Custom);
        assert_eq!(
            GuardianMode::from_config("disabled"),
            GuardianMode::Disabled
        );
        assert_eq!(GuardianMode::from_config("unknown"), GuardianMode::Managed);
        assert_eq!(GuardianMode::from_config(""), GuardianMode::Managed);
    }
}
