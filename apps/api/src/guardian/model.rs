use crate::observability::EvidenceField;
use crate::state::contracts::{
    OperationId, OperationPhase, OwnershipClass, StabilizationSystem, TargetDescriptor,
};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum GuardianMode {
    Managed,
    Custom,
    Disabled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum GuardianFactId {
    AgentHookFailed,
    AgentUnavailable,
    ArtifactChecksumMismatch,
    ArtifactMissing,
    ArtifactQuarantined,
    ArtifactSizeMismatch,
    ArtifactVerified,
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
    DownloadTempDiscarded,
    DownloadWrittenToTemp,
    ExitCodeNonzero,
    ExitCodeUnknown,
    ExitCodeZero,
    FileWrittenToTemp,
    FilesystemLocked,
    FilesystemPermissionDenied,
    FrameBudgetExceeded,
    GcPauseStorm,
    GraphicsDriverCrash,
    HeapPressureCritical,
    IncompleteInstall,
    InstallDependencyFailed,
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
    LaunchCommandInvalid,
    LaunchCommandPrepared,
    LaunchMemoryAllocationLow,
    LaunchMemoryMinClamped,
    LaunchResourceCpuPressure,
    LaunchResourceDiskPressure,
    LaunchResourceInstallPressure,
    LaunchResourceMemoryPressure,
    LauncherManagedArtifactSignatureCorruption,
    LauncherStopRequested,
    LibrariesMissing,
    LoaderBootstrapFailure,
    ManagedFileCorrupt,
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
    OwnershipUnknown,
    ParentVersionMissing,
    PerformanceFallbackSelected,
    PerformanceHealthDegraded,
    PerformanceHealthFallback,
    PerformanceHealthInvalid,
    PerformanceRepeatedFailureMemory,
    PerformanceRulesInvalid,
    PerformanceUserOwnedConflict,
    PersistedStateSchemaInvalid,
    PrimitiveRefused,
    ProcessExited,
    ProcessExitedAfterBoot,
    ProcessExitedBeforeBoot,
    ProcessSpawned,
    ProviderDataInvalid,
    RecentRepairFailed,
    RecentStartupFailure,
    RepairSuppressedUntil,
    RollbackAvailable,
    RollbackUnavailable,
    StartupWindowExpired,
    TempFileLeftover,
    UnknownLaunchFailure,
    VersionJsonMissing,
    WatchdogKilledProcess,
}

impl GuardianFactId {
    pub const ALL: [Self; 115] = [
        Self::AgentHookFailed,
        Self::AgentUnavailable,
        Self::ArtifactChecksumMismatch,
        Self::ArtifactMissing,
        Self::ArtifactQuarantined,
        Self::ArtifactSizeMismatch,
        Self::ArtifactVerified,
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
        Self::DownloadTempDiscarded,
        Self::DownloadWrittenToTemp,
        Self::ExitCodeNonzero,
        Self::ExitCodeUnknown,
        Self::ExitCodeZero,
        Self::FileWrittenToTemp,
        Self::FilesystemLocked,
        Self::FilesystemPermissionDenied,
        Self::FrameBudgetExceeded,
        Self::GcPauseStorm,
        Self::GraphicsDriverCrash,
        Self::HeapPressureCritical,
        Self::IncompleteInstall,
        Self::InstallDependencyFailed,
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
        Self::LaunchCommandInvalid,
        Self::LaunchCommandPrepared,
        Self::LaunchMemoryAllocationLow,
        Self::LaunchMemoryMinClamped,
        Self::LaunchResourceCpuPressure,
        Self::LaunchResourceDiskPressure,
        Self::LaunchResourceInstallPressure,
        Self::LaunchResourceMemoryPressure,
        Self::LauncherManagedArtifactSignatureCorruption,
        Self::LauncherStopRequested,
        Self::LibrariesMissing,
        Self::LoaderBootstrapFailure,
        Self::ManagedFileCorrupt,
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
        Self::OwnershipUnknown,
        Self::ParentVersionMissing,
        Self::PerformanceFallbackSelected,
        Self::PerformanceHealthDegraded,
        Self::PerformanceHealthFallback,
        Self::PerformanceHealthInvalid,
        Self::PerformanceRepeatedFailureMemory,
        Self::PerformanceRulesInvalid,
        Self::PerformanceUserOwnedConflict,
        Self::PersistedStateSchemaInvalid,
        Self::PrimitiveRefused,
        Self::ProcessExited,
        Self::ProcessExitedAfterBoot,
        Self::ProcessExitedBeforeBoot,
        Self::ProcessSpawned,
        Self::ProviderDataInvalid,
        Self::RecentRepairFailed,
        Self::RecentStartupFailure,
        Self::RepairSuppressedUntil,
        Self::RollbackAvailable,
        Self::RollbackUnavailable,
        Self::StartupWindowExpired,
        Self::TempFileLeftover,
        Self::UnknownLaunchFailure,
        Self::VersionJsonMissing,
        Self::WatchdogKilledProcess,
    ];

    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::AgentHookFailed => "agent_hook_failed",
            Self::AgentUnavailable => "agent_unavailable",
            Self::ArtifactChecksumMismatch => "artifact_checksum_mismatch",
            Self::ArtifactMissing => "artifact_missing",
            Self::ArtifactQuarantined => "artifact_quarantined",
            Self::ArtifactSizeMismatch => "artifact_size_mismatch",
            Self::ArtifactVerified => "artifact_verified",
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
            Self::DownloadTempDiscarded => "download_temp_discarded",
            Self::DownloadWrittenToTemp => "download_written_to_temp",
            Self::ExitCodeNonzero => "exit_code_nonzero",
            Self::ExitCodeUnknown => "exit_code_unknown",
            Self::ExitCodeZero => "exit_code_zero",
            Self::FileWrittenToTemp => "file_written_to_temp",
            Self::FilesystemLocked => "filesystem_locked",
            Self::FilesystemPermissionDenied => "filesystem_permission_denied",
            Self::FrameBudgetExceeded => "frame_budget_exceeded",
            Self::GcPauseStorm => "gc_pause_storm",
            Self::GraphicsDriverCrash => "graphics_driver_crash",
            Self::HeapPressureCritical => "heap_pressure_critical",
            Self::IncompleteInstall => "incomplete_install",
            Self::InstallDependencyFailed => "install_dependency_failed",
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
            Self::LaunchCommandInvalid => "launch_command_invalid",
            Self::LaunchCommandPrepared => "launch_command_prepared",
            Self::LaunchMemoryAllocationLow => "launch_memory_allocation_low",
            Self::LaunchMemoryMinClamped => "launch_memory_min_clamped",
            Self::LaunchResourceCpuPressure => "launch_resource_cpu_pressure",
            Self::LaunchResourceDiskPressure => "launch_resource_disk_pressure",
            Self::LaunchResourceInstallPressure => "launch_resource_install_pressure",
            Self::LaunchResourceMemoryPressure => "launch_resource_memory_pressure",
            Self::LauncherManagedArtifactSignatureCorruption => {
                "launcher_managed_artifact_signature_corruption"
            }
            Self::LauncherStopRequested => "launcher_stop_requested",
            Self::LibrariesMissing => "libraries_missing",
            Self::LoaderBootstrapFailure => "loader_bootstrap_failure",
            Self::ManagedFileCorrupt => "managed_file_corrupt",
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
            Self::OwnershipUnknown => "ownership_unknown",
            Self::ParentVersionMissing => "parent_version_missing",
            Self::PerformanceFallbackSelected => "performance_fallback_selected",
            Self::PerformanceHealthDegraded => "performance_health_degraded",
            Self::PerformanceHealthFallback => "performance_health_fallback",
            Self::PerformanceHealthInvalid => "performance_health_invalid",
            Self::PerformanceRepeatedFailureMemory => "performance_repeated_failure_memory",
            Self::PerformanceRulesInvalid => "performance_rules_invalid",
            Self::PerformanceUserOwnedConflict => "performance_user_owned_conflict",
            Self::PersistedStateSchemaInvalid => "persisted_state_schema_invalid",
            Self::PrimitiveRefused => "primitive_refused",
            Self::ProcessExited => "process_exited",
            Self::ProcessExitedAfterBoot => "process_exited_after_boot",
            Self::ProcessExitedBeforeBoot => "process_exited_before_boot",
            Self::ProcessSpawned => "process_spawned",
            Self::ProviderDataInvalid => "provider_data_invalid",
            Self::RecentRepairFailed => "recent_repair_failed",
            Self::RecentStartupFailure => "recent_startup_failure",
            Self::RepairSuppressedUntil => "repair_suppressed_until",
            Self::RollbackAvailable => "rollback_available",
            Self::RollbackUnavailable => "rollback_unavailable",
            Self::StartupWindowExpired => "startup_window_expired",
            Self::TempFileLeftover => "temp_file_leftover",
            Self::UnknownLaunchFailure => "unknown_launch_failure",
            Self::VersionJsonMissing => "version_json_missing",
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

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DiagnosisId(pub String);

impl DiagnosisId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Diagnosis {
    pub id: DiagnosisId,
    pub domain: GuardianDomain,
    pub severity: GuardianSeverity,
    pub confidence: GuardianConfidence,
    pub ownership: OwnershipClass,
    pub phase: OperationPhase,
    pub fact_ids: Vec<GuardianFactId>,
    pub affected_targets: Vec<TargetDescriptor>,
    pub impact: GuardianImpactVector,
    pub candidate_actions: Vec<GuardianActionKind>,
    pub public_reason_template: String,
}

impl Diagnosis {
    pub fn action_prerequisite(&self) -> Result<ActionPlanPrerequisite, GuardianCoreError> {
        if self.affected_targets.is_empty() {
            return Err(GuardianCoreError::MissingAffectedTarget);
        }
        if self.candidate_actions.is_empty() {
            return Err(GuardianCoreError::MissingCandidateAction);
        }
        Ok(ActionPlanPrerequisite {
            diagnosis_id: self.id.clone(),
            ownership: self.ownership,
            confidence: self.confidence,
            affected_targets: self.affected_targets.clone(),
            candidate_actions: self.candidate_actions.clone(),
        })
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct GuardianImpactVector {
    pub privacy_risk: f32,
    pub data_loss_risk: f32,
    pub launchability_impact: f32,
    pub state_corruption_impact: f32,
    pub user_intent_impact: f32,
    pub performance_impact: f32,
    pub host_stability_impact: f32,
}

impl GuardianImpactVector {
    pub fn scalar_severity(self) -> f32 {
        self.privacy_risk
            .max(self.data_loss_risk)
            .max(self.launchability_impact * 0.90)
            .max(self.state_corruption_impact * 0.85)
            .max(self.user_intent_impact * 0.70)
            .max(self.host_stability_impact * 0.65)
            .max(self.performance_impact * 0.45)
    }

    pub(crate) fn launch_blocking() -> Self {
        Self {
            launchability_impact: 0.95,
            ..Self::default()
        }
    }

    pub(crate) fn repairable_corruption() -> Self {
        Self {
            launchability_impact: 0.80,
            state_corruption_impact: 0.85,
            ..Self::default()
        }
    }

    pub(crate) fn record_only() -> Self {
        Self {
            launchability_impact: 0.15,
            ..Self::default()
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
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
pub struct GuardianDecision {
    pub operation_id: Option<OperationId>,
    pub mode: GuardianMode,
    pub kind: GuardianActionKind,
    pub diagnoses: Vec<DiagnosisId>,
    pub action_plan: Option<GuardianActionPlan>,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GuardianCoreError {
    MissingAffectedTarget,
    MissingCandidateAction,
}
