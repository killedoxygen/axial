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

impl GuardianMode {
    pub const fn failure_memory_id(self) -> &'static str {
        match self {
            Self::Managed => "Managed",
            Self::Custom => "Custom",
            Self::Disabled => "Disabled",
        }
    }
}

macro_rules! stable_phase_id_registry {
    (
        $error:literal;
        $count:literal;
        pub enum $name:ident {
            before { $($before:ident => $before_id:literal),* $(,)? }
            phase $phase_variant:ident($phase_type:ty) {
                $($phase:ident => $phase_id:literal),* $(,)?
            }
            after { $($after:ident => $after_id:literal),* $(,)? }
        }
    ) => {
        #[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
        pub enum $name {
            $($before,)*
            $phase_variant($phase_type),
            $($after,)*
        }

        impl $name {
            pub const ALL: [Self; $count] = [
                $(Self::$before,)*
                $(Self::$phase_variant(OperationPhase::$phase),)*
                $(Self::$after,)*
            ];

            pub const fn as_str(&self) -> &'static str {
                match self {
                    $(Self::$before => $before_id,)*
                    Self::$phase_variant(phase) => match phase {
                        $(OperationPhase::$phase => $phase_id,)*
                    },
                    $(Self::$after => $after_id,)*
                }
            }

            fn from_wire(value: &str) -> Option<Self> {
                Self::ALL
                    .into_iter()
                    .find(|candidate| candidate.as_str() == value)
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str(self.as_str())
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                serializer.serialize_str(self.as_str())
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                Self::from_wire(&value).ok_or_else(|| D::Error::custom($error))
            }
        }
    };
}

stable_phase_id_registry! {
    "unknown Guardian fact id";
    117;
    pub enum GuardianFactId {
        before {
        AgentHookFailed => "agent_hook_failed",
        AgentUnavailable => "agent_unavailable",
        ArtifactChecksumMismatch => "artifact_checksum_mismatch",
        ArtifactHashMismatch => "artifact_hash_mismatch",
        ArtifactMissing => "artifact_missing",
        ArtifactQuarantined => "artifact_quarantined",
        ArtifactSizeDrift => "artifact_size_drift",
        ArtifactSizeMismatch => "artifact_size_mismatch",
        AssetIndexMissing => "asset_index_missing",
        AtomicPromotionCompleted => "atomic_promotion_completed",
        AtomicPromotionFailed => "atomic_promotion_failed",
        AuthModeIncompatible => "auth_mode_incompatible",
        BootMarkerObserved => "boot_marker_observed",
        BootMilestoneOverdue => "boot_milestone_overdue",
        BootMilestoneReached => "boot_milestone_reached",
        ClasspathModuleConflict => "classpath_module_conflict",
        ClientJarMissing => "client_jar_missing",
        CustomJavaOverridePresent => "custom_java_override_present",
        CustomJvmArgsPresent => "custom_jvm_args_present",
        CustomJvmPresetPresent => "custom_jvm_preset_present",
        DownloadInterrupted => "download_interrupted",
        DownloadProviderUnavailable => "download_provider_unavailable",
        DownloadTempDiscarded => "download_temp_discarded",
        DownloadWrittenToTemp => "download_written_to_temp",
        ExitCodeNonzero => "exit_code_nonzero",
        ExitCodeUnknown => "exit_code_unknown",
        ExitCodeZero => "exit_code_zero",
        FilesystemPermissionDenied => "filesystem_permission_denied",
        FrameBudgetExceeded => "frame_budget_exceeded",
        GcPauseStorm => "gc_pause_storm",
        GraphicsDriverCrash => "graphics_driver_crash",
        HeapPressureCritical => "heap_pressure_critical",
        IncompleteInstall => "incomplete_install",
        InstallDependencyFailed => "install_dependency_failed",
        InstallExecutionFailed => "install_execution_failed",
        InstallProcessorFailed => "install_processor_failed",
        InstalledVersionsDegraded => "installed_versions_degraded",
        JavaMajorMismatch => "java_major_mismatch",
        JavaOverrideEmpty => "java_override_empty",
        JavaOverrideMissing => "java_override_missing",
        JavaOverrideUndefinedSentinel => "java_override_undefined_sentinel",
        JavaProbeFailed => "java_probe_failed",
        JavaUpdateTooOld => "java_update_too_old",
        JvmArgAgentOverride => "jvm_arg_agent_override",
        JvmArgExperimentalUnlockMissing => "jvm_arg_experimental_unlock_missing",
        JvmArgMemoryConflict => "jvm_arg_memory_conflict",
        JvmArgReservedLauncherFlag => "jvm_arg_reserved_launcher_flag",
        JvmArgUnlockOrderInvalid => "jvm_arg_unlock_order_invalid",
        JvmArgUnsafeClasspathOverride => "jvm_arg_unsafe_classpath_override",
        JvmArgUnsafeNativePathOverride => "jvm_arg_unsafe_native_path_override",
        JvmArgUnsupported => "jvm_arg_unsupported",
        JvmArgUnsupportedGc => "jvm_arg_unsupported_gc",
        JvmArgsEmpty => "jvm_args_empty",
        JvmArgsParseFailed => "jvm_args_parse_failed",
        JvmPresetCompatibilityAdjusted => "jvm_preset_compatibility_adjusted",
        LaunchFailureClassified => "launch_failure_classified",
        LaunchJvmPresetDowngradeAvailable => "launch_jvm_preset_downgrade_available",
        LaunchJvmStripAvailable => "launch_jvm_strip_available",
        LaunchMemoryAllocationLow => "launch_memory_allocation_low",
        LaunchMemoryMinClamped => "launch_memory_min_clamped",
        LaunchResourceCpuPressure => "launch_resource_cpu_pressure",
        LaunchResourceDiskPressure => "launch_resource_disk_pressure",
        LaunchResourceInstallPressure => "launch_resource_install_pressure",
        LaunchResourceMemoryPressure => "launch_resource_memory_pressure",
        LaunchRuntimeFallbackAvailable => "launch_runtime_fallback_available",
        LauncherManagedArtifactSignatureCorruption => "launcher_managed_artifact_signature_corruption",
        LauncherStopRequested => "launcher_stop_requested",
        LibrariesMissing => "libraries_missing",
        LoaderBootstrapFailure => "loader_bootstrap_failure",
        ManagedRuntimeCorrupt => "managed_runtime_corrupt",
        ManagedRuntimeMissing => "managed_runtime_missing",
        ManagedRuntimeReadyMarkerMissing => "managed_runtime_ready_marker_missing",
        ManagedRuntimeRepairApplied => "managed_runtime_repair_applied",
        ManagedRuntimeRosettaRequired => "managed_runtime_rosetta_required",
        ManagedRuntimeUnavailableForPlatform => "managed_runtime_unavailable_for_platform",
        MissingDependency => "missing_dependency",
        ModAttributedCrash => "mod_attributed_crash",
        ModTransformationFailure => "mod_transformation_failure",
        }
        phase NoStructuredFact(OperationPhase) {
        Startup => "no_structured_fact_startup",
        Planning => "no_structured_fact_planning",
        Validating => "no_structured_fact_validating",
        Downloading => "no_structured_fact_downloading",
        Installing => "no_structured_fact_installing",
        Preparing => "no_structured_fact_preparing",
        Launching => "no_structured_fact_launching",
        Running => "no_structured_fact_running",
        Repairing => "no_structured_fact_repairing",
        RollingBack => "no_structured_fact_rolling_back",
        Completed => "no_structured_fact_completed",
        Failed => "no_structured_fact_failed",
        }
        after {
        OutOfMemory => "out_of_memory",
        ParentVersionMissing => "parent_version_missing",
        PerformanceFallbackSelected => "performance_fallback_selected",
        PerformanceHealthInvalid => "performance_health_invalid",
        PerformanceRulesInvalid => "performance_rules_invalid",
        PerformanceUserOwnedConflict => "performance_user_owned_conflict",
        PersistedStateRepairAvailable => "persisted_state_repair_available",
        PersistedStateSchemaInvalid => "persisted_state_schema_invalid",
        PrimitiveRefused => "primitive_refused",
        ProcessExited => "process_exited",
        ProcessExitedAfterBoot => "process_exited_after_boot",
        ProcessExitedBeforeBoot => "process_exited_before_boot",
        ProcessKilled => "process_killed",
        ProcessSpawned => "process_spawned",
        ProviderDataInvalid => "provider_data_invalid",
        RecentRepairFailed => "recent_repair_failed",
        RecentStartupFailure => "recent_startup_failure",
        RegisteredArtifactRepairAvailable => "registered_artifact_repair_available",
        RegisteredComponentRebuildFailed => "registered_component_rebuild_failed",
        RepairSuppressedUntil => "repair_suppressed_until",
        StartupWindowExpired => "startup_window_expired",
        TempFileWriteFailed => "temp_file_write_failed",
        UnknownLaunchFailure => "unknown_launch_failure",
        UserModSetDrift => "user_mod_set_drift",
        VersionJsonMissing => "version_json_missing",
        WatchdogActionObserved => "watchdog_action_observed",
        WatchdogKilledProcess => "watchdog_killed_process",
        }
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

pub const MAX_OPERATION_EVIDENCE_FACTS: usize = 64;
pub const MAX_OPERATION_EVIDENCE_FIELDS_PER_FACT: usize = 8;
pub const MAX_OPERATION_EVIDENCE_FIELD_KEY_BYTES: usize = 32;
pub const MAX_OPERATION_EVIDENCE_VALUE_BYTES: usize = 96;
pub const MAX_OPERATION_EVIDENCE_TARGET_BYTES: usize = 96;
pub const MAX_OPERATION_EVIDENCE_SERIALIZED_BYTES: usize = 131_072;

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub enum EvidenceScope {
    Operation(OperationId),
    Unscoped,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OperationEvidenceBatchRejection {
    TooManyFacts,
    MissingOperation {
        fact_index: usize,
    },
    ForeignOperation {
        fact_index: usize,
    },
    UnexpectedOperation {
        fact_index: usize,
    },
    TooManyFields {
        fact_index: usize,
    },
    FieldKeyTooLong {
        fact_index: usize,
        field_index: usize,
    },
    FieldValueTooLong {
        fact_index: usize,
        field_index: usize,
    },
    TargetTooLong {
        fact_index: usize,
    },
    SerializedTooLarge,
    SerializationFailed,
}

impl std::fmt::Display for OperationEvidenceBatchRejection {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::TooManyFacts => "operation evidence exceeds its fact bound",
            Self::MissingOperation { .. } => {
                "operation-scoped evidence contains a fact without operation provenance"
            }
            Self::ForeignOperation { .. } => {
                "operation-scoped evidence contains foreign operation provenance"
            }
            Self::UnexpectedOperation { .. } => "unscoped evidence contains operation provenance",
            Self::TooManyFields { .. } => "operation evidence exceeds its per-fact field bound",
            Self::FieldKeyTooLong { .. } => "operation evidence contains an oversized field key",
            Self::FieldValueTooLong { .. } => {
                "operation evidence contains an oversized field value"
            }
            Self::TargetTooLong { .. } => "operation evidence contains an oversized target",
            Self::SerializedTooLarge => "operation evidence exceeds its serialized byte bound",
            Self::SerializationFailed => "operation evidence could not be serialized",
        })
    }
}

impl std::error::Error for OperationEvidenceBatchRejection {}

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

stable_phase_id_registry! {
    "unknown Guardian diagnosis id";
    76;
    pub enum DiagnosisId {
        before {
        ArtifactOwnershipUnsafe => "artifact_ownership_unsafe",
        AtomicPromotionFailed => "atomic_promotion_failed",
        DownloadUnavailable => "download_unavailable",
        FilesystemPermissionDenied => "filesystem_permission_denied",
        InstallArtifactMetadataInvalid => "install_artifact_metadata_invalid",
        InstallDependencyFailed => "install_dependency_failed",
        InstallExecutionFailed => "install_execution_failed",
        InstallProcessorFailed => "install_processor_failed",
        JavaOverrideUnavailable => "java_override_unavailable",
        JavaProbeFailed => "java_probe_failed",
        JavaRuntimeMajorMismatch => "java_runtime_major_mismatch",
        JavaRuntimeUpdateTooOld => "java_runtime_update_too_old",
        JvmArgUnsafeOverride => "jvm_arg_unsafe_override",
        JvmArgUnsupported => "jvm_arg_unsupported",
        JvmArgsEmpty => "jvm_args_empty",
        JvmArgsMalformed => "jvm_args_malformed",
        LauncherManagedArtifactCorrupt => "launcher_managed_artifact_corrupt",
        LauncherManagedArtifactSignatureCorrupt => "launcher_managed_artifact_signature_corrupt",
        ManagedRuntimeCorrupt => "managed_runtime_corrupt",
        ManagedRuntimeMissing => "managed_runtime_missing",
        ManagedRuntimeRosettaRequired => "managed_runtime_rosetta_required",
        ManagedRuntimeUnavailableForPlatform => "managed_runtime_unavailable_for_platform",
        PerformanceFallbackSelected => "performance_fallback_selected",
        PerformanceRulesInvalid => "performance_rules_invalid",
        PerformanceUserOwnedConflict => "performance_user_owned_conflict",
        PersistedStateSchemaInvalid => "persisted_state_schema_invalid",
        ProcessLifecycleObserved => "process_lifecycle_observed",
        TempFileWriteFailed => "temp_file_write_failed",
        InstalledVersionMetadataMissing => "installed_version_metadata_missing",
        ParentVersionMetadataMissing => "parent_version_metadata_missing",
        InstallIncomplete => "install_incomplete",
        ClientJarMissing => "client_jar_missing",
        LibrariesMissing => "libraries_missing",
        AssetIndexMissing => "asset_index_missing",
        LaunchMemoryMinClamped => "launch_memory_min_clamped",
        LaunchMemoryAllocationLow => "launch_memory_allocation_low",
        LaunchResourceMemoryPressure => "launch_resource_memory_pressure",
        LaunchResourceCpuPressure => "launch_resource_cpu_pressure",
        LaunchResourceInstallPressure => "launch_resource_install_pressure",
        LaunchResourceDiskPressure => "launch_resource_disk_pressure",
        CustomJavaOverridePresent => "custom_java_override_present",
        CustomJvmPresetPresent => "custom_jvm_preset_present",
        CustomJvmArgsPresent => "custom_jvm_args_present",
        PerformanceHealthInvalid => "performance_health_invalid",
        JvmPresetAdjusted => "jvm_preset_adjusted",
        LaunchPrepareFailed => "launch_prepare_failed",
        StartupStalled => "startup_stalled",
        OutOfMemory => "out_of_memory",
        GraphicsDriverCrash => "graphics_driver_crash",
        MissingDependency => "missing_dependency",
        ModTransformationFailure => "mod_transformation_failure",
        ModAttributedCrash => "mod_attributed_crash",
        ClasspathModuleConflict => "classpath_module_conflict",
        AuthModeIncompatible => "auth_mode_incompatible",
        LoaderBootstrapFailure => "loader_bootstrap_failure",
        StartupFailedUnknown => "startup_failed_unknown",
        JavaRuntimeRecovery => "java_runtime_recovery",
        JvmPresetRecovery => "jvm_preset_recovery",
        LaunchFailureUnknown => "unknown",
        JvmUnsupportedOption => "jvm_unsupported_option",
        JvmExperimentalUnlock => "jvm_experimental_unlock",
        JvmOptionOrdering => "jvm_option_ordering",
        JavaRuntimeMismatch => "java_runtime_mismatch",
        LauncherManagedArtifactSignature => "launcher_managed_artifact_signature",
        }
        phase UnknownFailure(OperationPhase) {
        Startup => "unknown_failure_startup",
        Planning => "unknown_failure_planning",
        Validating => "unknown_failure_validating",
        Downloading => "unknown_failure_downloading",
        Installing => "unknown_failure_installing",
        Preparing => "unknown_failure_preparing",
        Launching => "unknown_failure_launching",
        Running => "unknown_failure_running",
        Repairing => "unknown_failure_repairing",
        RollingBack => "unknown_failure_rolling_back",
        Completed => "unknown_failure_completed",
        Failed => "unknown_failure_failed",
        }
        after {

        }
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

impl GuardianDomain {
    pub const fn failure_memory_id(self) -> &'static str {
        match self {
            Self::Config => "Config",
            Self::Library => "Library",
            Self::Runtime => "Runtime",
            Self::Jvm => "Jvm",
            Self::Install => "Install",
            Self::Download => "Download",
            Self::Performance => "Performance",
            Self::Launch => "Launch",
            Self::Startup => "Startup",
            Self::Session => "Session",
            Self::Filesystem => "Filesystem",
            Self::Network => "Network",
            Self::Auth => "Auth",
            Self::State => "State",
            Self::Unknown => "Unknown",
        }
    }
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
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

impl GuardianActionKind {
    pub const ALL: &'static [Self] = &[
        Self::Allow,
        Self::Warn,
        Self::Repair,
        Self::Retry,
        Self::Strip,
        Self::Downgrade,
        Self::Fallback,
        Self::Quarantine,
        Self::AskUser,
        Self::Block,
        Self::RecordOnly,
    ];

    pub const fn as_wire(self) -> &'static str {
        match self {
            Self::Allow => "Allow",
            Self::Warn => "Warn",
            Self::Repair => "Repair",
            Self::Retry => "Retry",
            Self::Strip => "Strip",
            Self::Downgrade => "Downgrade",
            Self::Fallback => "Fallback",
            Self::Quarantine => "Quarantine",
            Self::AskUser => "AskUser",
            Self::Block => "Block",
            Self::RecordOnly => "RecordOnly",
        }
    }

    fn from_wire(value: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|candidate| candidate.as_wire() == value)
    }
}

impl Serialize for GuardianActionKind {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_wire())
    }
}

impl<'de> Deserialize<'de> for GuardianActionKind {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::from_wire(&value).ok_or_else(|| D::Error::custom("unknown Guardian action kind"))
    }
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
    use super::{GuardianActionKind, GuardianMode};

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

    #[test]
    fn durable_guardian_enum_bytes_are_explicit_and_strict() {
        assert_eq!(GuardianMode::Managed.failure_memory_id(), "Managed");
        assert_eq!(GuardianMode::Custom.failure_memory_id(), "Custom");
        assert_eq!(GuardianMode::Disabled.failure_memory_id(), "Disabled");

        for action in GuardianActionKind::ALL {
            let encoded = serde_json::to_string(action).expect("serialize Guardian action");
            assert_eq!(encoded, format!("\"{}\"", action.as_wire()));
            assert_eq!(
                serde_json::from_str::<GuardianActionKind>(&encoded)
                    .expect("deserialize Guardian action"),
                *action
            );
        }
        assert!(serde_json::from_str::<GuardianActionKind>("\"record_only\"").is_err());
    }
}
