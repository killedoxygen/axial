//! Guardian system boundary.
//!
//! Guardian owns safety facts, diagnosis, policy decisions, action planning,
//! outcomes, and failure memory. This module defines the Phase 1 reasoning
//! core without changing current launch behavior.

pub mod artifact_descriptor;
pub mod artifact_repair;
pub mod healing;
pub mod install_evidence;
pub mod launch_recovery;
pub mod outcome;
pub mod performance;
pub mod policy;
pub mod repair_plan;

use crate::execution::{ExecutionFact, ExecutionFactKind};
use crate::observability::{EvidenceField, RedactionAudience, sanitize_evidence_token};
use crate::state::contracts::{
    OperationId, OperationPhase, OwnershipClass, StabilizationSystem, TargetDescriptor, TargetKind,
};
use serde::{Deserialize, Serialize};

pub use artifact_descriptor::{
    GuardianArtifactDescriptorError, GuardianMinecraftArtifactKind,
    GuardianMinecraftArtifactRepairDescriptor, GuardianMinecraftArtifactRepairMetadata,
    MAX_MINECRAFT_REPAIR_ARTIFACT_BYTES,
};
pub use artifact_repair::{
    GuardianArtifactRepairOutcome, GuardianArtifactRepairRequest, GuardianArtifactRepairSource,
    GuardianArtifactRepairStatus, execute_guardian_artifact_repair,
    execute_guardian_missing_artifact_repair,
};
pub use healing::{
    GuardianManagedRuntimeRepairRequest, GuardianRepairOutcome, GuardianRepairStatus,
    execute_managed_runtime_ready_marker_repair,
};
pub use install_evidence::{
    GuardianInstallArtifactFailureEvidence, GuardianInstallArtifactFailureKind,
    install_artifact_failure_from_minecraft_download_fact, install_artifact_failure_guardian_fact,
    install_artifact_failure_safety_case,
};
pub use launch_recovery::{
    GuardianLaunchRecoveryKind, GuardianLaunchRecoveryOutcome, GuardianLaunchRecoveryRequest,
    GuardianLaunchRecoveryStatus, record_launch_recovery_attempt, record_launch_recovery_failure,
};
pub use outcome::{GuardianUserOutcome, startup_failure_guardian_outcome};
pub use performance::{
    performance_failure_memory_guardian_fact, performance_health_guardian_facts,
    performance_plan_guardian_facts, performance_rules_guardian_facts,
    performance_state_error_guardian_fact,
};
pub use policy::{
    GuardianPolicyContext, action_safety_score, decide_guardian_policy, decision_pressure_score,
    launch_summary_decision_kind, launch_summary_safety_outcome,
};
pub use repair_plan::{
    GuardianRepairExecutor, GuardianRepairMutation, GuardianRepairPlan,
    GuardianRepairPlanRejection, GuardianRepairPlanningContext, GuardianRepairReversibility,
    GuardianRepairTask, GuardianRepairTaskKind, plan_launcher_managed_artifact_repair,
    plan_launcher_managed_missing_artifact_repair,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum GuardianMode {
    Managed,
    Custom,
    Disabled,
}

#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct GuardianFactId(pub String);

impl GuardianFactId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
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
pub enum GuardianObservation {
    JavaOverrideEmpty,
    JavaOverrideUndefinedSentinel,
    JavaOverrideMissing,
    JavaProbeFailed,
    JavaMajorMismatch,
    JvmArgsParseFailed,
    JvmArgReservedLauncherFlag,
    JvmArgMemoryConflict,
    JvmArgUnsupportedGc,
    JvmArgUnlockOrderInvalid,
    JvmArgUnsafeClasspathOverride,
    JvmArgUnsafeNativePathOverride,
    JvmArgAgentOverride,
    RawJvmArgsPresent,
    ProcessExitedBeforeBoot,
    ProcessExitedAfterBoot,
    BootMarkerObserved,
    LauncherStopRequested,
    Unknown(String),
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
    pub fact_ids: Vec<String>,
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

    fn launch_blocking() -> Self {
        Self {
            launchability_impact: 0.95,
            ..Self::default()
        }
    }

    fn repairable_corruption() -> Self {
        Self {
            launchability_impact: 0.80,
            state_corruption_impact: 0.85,
            ..Self::default()
        }
    }

    fn record_only() -> Self {
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
    pub hard_constraints: Vec<GuardianHardConstraint>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum GuardianHardConstraint {
    OwnershipRequired,
    RedactionRequired,
    JournalRequiredForMutation,
    UserOwnedDestructiveMutationForbidden,
    UnknownOwnedDestructiveMutationForbidden,
    RetryLoopForbidden,
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
    pub kind: GuardianDecisionKind,
    pub diagnoses: Vec<DiagnosisId>,
    pub action_plan: Option<GuardianActionPlan>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum GuardianDecisionKind {
    Allow,
    Warn,
    Repair,
    Retry,
    Replace,
    Strip,
    Downgrade,
    Degrade,
    Fallback,
    Quarantine,
    Rollback,
    Block,
    AskUser,
    RecordOnly,
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
    Replace,
    Strip,
    Downgrade,
    Degrade,
    Fallback,
    Quarantine,
    Rollback,
    AskUser,
    Block,
    RecordOnly,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SafetyOutcome {
    pub decision: GuardianDecisionKind,
    pub summary: String,
    pub detail: Option<String>,
    pub diagnoses: Vec<DiagnosisId>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GuardianCoreError {
    MissingAffectedTarget,
    MissingCandidateAction,
}

pub fn guardian_fact_from_execution(fact: &ExecutionFact, phase: OperationPhase) -> GuardianFact {
    let (id, domain, reliability) = execution_fact_shape(fact);
    let target = fact.target.as_ref().map(public_safe_target);
    let ownership = target
        .as_ref()
        .map(|target| target.ownership)
        .unwrap_or(OwnershipClass::Unknown);
    GuardianFact {
        operation_id: fact.operation_id.clone(),
        id,
        domain,
        phase,
        reliability,
        severity: None,
        confidence: None,
        ownership,
        target,
        fields: public_safe_fields(&fact.fields),
    }
}

pub fn guardian_fact_from_observation(
    observation: GuardianObservation,
    phase: OperationPhase,
    target: Option<TargetDescriptor>,
) -> GuardianFact {
    let (id, domain, reliability) = observation_fact_shape(&observation);
    let target = target.as_ref().map(public_safe_target);
    let ownership = target
        .as_ref()
        .map(|target| target.ownership)
        .unwrap_or(OwnershipClass::Unknown);
    GuardianFact {
        operation_id: None,
        id,
        domain,
        phase,
        reliability,
        severity: None,
        confidence: None,
        ownership,
        target,
        fields: Vec::new(),
    }
}

pub fn diagnose_facts(facts: &[GuardianFact], phase: OperationPhase) -> Vec<Diagnosis> {
    let mut diagnoses = facts
        .iter()
        .filter_map(|fact| diagnosis_for_fact(fact, phase))
        .collect::<Vec<_>>();
    if diagnoses.is_empty() {
        diagnoses.push(unknown_diagnosis(facts, phase));
    }
    diagnoses
}

pub fn build_safety_case(
    operation_id: Option<OperationId>,
    mode: GuardianMode,
    phase: OperationPhase,
    facts: &[GuardianFact],
) -> SafetyCase {
    SafetyCase {
        operation_id,
        mode,
        phase,
        diagnoses: diagnose_facts(facts, phase),
        hard_constraints: vec![
            GuardianHardConstraint::OwnershipRequired,
            GuardianHardConstraint::RedactionRequired,
            GuardianHardConstraint::RetryLoopForbidden,
        ],
    }
}

fn execution_fact_shape(fact: &ExecutionFact) -> (GuardianFactId, GuardianDomain, FactReliability) {
    let id = match fact.kind {
        ExecutionFactKind::ArtifactMissing | ExecutionFactKind::FileMissing => "artifact_missing",
        ExecutionFactKind::ArtifactVerified => "artifact_verified",
        ExecutionFactKind::ChecksumMismatch | ExecutionFactKind::DownloadChecksumMismatch => {
            "artifact_checksum_mismatch"
        }
        ExecutionFactKind::SizeMismatch | ExecutionFactKind::DownloadSizeMismatch => {
            "artifact_size_mismatch"
        }
        ExecutionFactKind::DownloadProviderFailure => "download_provider_unavailable",
        ExecutionFactKind::DownloadNetworkFailure | ExecutionFactKind::DownloadInterrupted => {
            "download_interrupted"
        }
        ExecutionFactKind::DownloadTempDiscarded => "download_temp_discarded",
        ExecutionFactKind::DownloadTempWriteFailed => "temp_file_leftover",
        ExecutionFactKind::DownloadWrittenToTemp => "download_written_to_temp",
        ExecutionFactKind::DownloadPromoted | ExecutionFactKind::FilePromoted => {
            "atomic_promotion_completed"
        }
        ExecutionFactKind::FileCorrupt => "managed_file_corrupt",
        ExecutionFactKind::FileLocked => "filesystem_locked",
        ExecutionFactKind::FileOwnershipUnknown => "ownership_unknown",
        ExecutionFactKind::FilePermissionDenied => "filesystem_permission_denied",
        ExecutionFactKind::FileQuarantined => "artifact_quarantined",
        ExecutionFactKind::FileTempLeftover => "temp_file_leftover",
        ExecutionFactKind::FileWrittenToTemp => "file_written_to_temp",
        ExecutionFactKind::RuntimeCorrupt => "managed_runtime_corrupt",
        ExecutionFactKind::RuntimeJavaOverrideEmpty => "java_override_empty",
        ExecutionFactKind::RuntimeJavaOverrideUndefinedSentinel => {
            "java_override_undefined_sentinel"
        }
        ExecutionFactKind::RuntimeMissingExecutable => {
            if fact
                .target
                .as_ref()
                .is_some_and(|target| target.ownership == OwnershipClass::UserOwned)
            {
                "java_override_missing"
            } else {
                "managed_runtime_missing"
            }
        }
        ExecutionFactKind::RuntimeProbeFailed => "java_probe_failed",
        ExecutionFactKind::RuntimeReadyMarkerMissing => "managed_runtime_ready_marker_missing",
        ExecutionFactKind::RuntimeRepairApplied => "managed_runtime_repair_applied",
        ExecutionFactKind::RuntimeWrongMajor => "java_major_mismatch",
        ExecutionFactKind::RuntimeWrongUpdate => "java_update_too_old",
        ExecutionFactKind::JvmArgsEmpty => "jvm_args_empty",
        ExecutionFactKind::JvmArgsParseFailed => "jvm_args_parse_failed",
        ExecutionFactKind::JvmArgReservedLauncherFlag => "jvm_arg_reserved_launcher_flag",
        ExecutionFactKind::JvmArgMemoryConflict => "jvm_arg_memory_conflict",
        ExecutionFactKind::JvmArgUnsupportedGc => "jvm_arg_unsupported_gc",
        ExecutionFactKind::JvmArgUnlockOrderInvalid => "jvm_arg_unlock_order_invalid",
        ExecutionFactKind::JvmArgUnsafeClasspathOverride => "jvm_arg_unsafe_classpath_override",
        ExecutionFactKind::JvmArgUnsafeNativePathOverride => "jvm_arg_unsafe_native_path_override",
        ExecutionFactKind::JvmArgAgentOverride => "jvm_arg_agent_override",
        ExecutionFactKind::LaunchCommandInvalid => "launch_command_invalid",
        ExecutionFactKind::LaunchCommandPrepared => "launch_command_prepared",
        ExecutionFactKind::ProcessSpawned => "process_spawned",
        ExecutionFactKind::ProcessStopIntent => "launcher_stop_requested",
        ExecutionFactKind::ProcessKilled => "watchdog_killed_process",
        ExecutionFactKind::ProcessExitCode => exit_code_fact_id(fact),
        ExecutionFactKind::ProcessBootEvidence => "boot_marker_observed",
        ExecutionFactKind::ProcessWatchdogAction => "watchdog_killed_process",
        ExecutionFactKind::ProcessExited => "process_exited",
        ExecutionFactKind::PrimitiveRefused => "primitive_refused",
        ExecutionFactKind::ProviderDataInvalid => "provider_data_invalid",
        ExecutionFactKind::RollbackAvailable => "rollback_available",
        ExecutionFactKind::RollbackUnavailable => "rollback_unavailable",
    };
    (
        GuardianFactId::new(id),
        domain_for_fact_id(id),
        reliability_for_execution_fact(fact.kind),
    )
}

fn observation_fact_shape(
    observation: &GuardianObservation,
) -> (GuardianFactId, GuardianDomain, FactReliability) {
    let id = match observation {
        GuardianObservation::JavaOverrideEmpty => "java_override_empty",
        GuardianObservation::JavaOverrideUndefinedSentinel => "java_override_undefined_sentinel",
        GuardianObservation::JavaOverrideMissing => "java_override_missing",
        GuardianObservation::JavaProbeFailed => "java_probe_failed",
        GuardianObservation::JavaMajorMismatch => "java_major_mismatch",
        GuardianObservation::JvmArgsParseFailed => "jvm_args_parse_failed",
        GuardianObservation::JvmArgReservedLauncherFlag => "jvm_arg_reserved_launcher_flag",
        GuardianObservation::JvmArgMemoryConflict => "jvm_arg_memory_conflict",
        GuardianObservation::JvmArgUnsupportedGc => "jvm_arg_unsupported_gc",
        GuardianObservation::JvmArgUnlockOrderInvalid => "jvm_arg_unlock_order_invalid",
        GuardianObservation::JvmArgUnsafeClasspathOverride => "jvm_arg_unsafe_classpath_override",
        GuardianObservation::JvmArgUnsafeNativePathOverride => {
            "jvm_arg_unsafe_native_path_override"
        }
        GuardianObservation::JvmArgAgentOverride => "jvm_arg_agent_override",
        GuardianObservation::RawJvmArgsPresent => "raw_jvm_args_present",
        GuardianObservation::ProcessExitedBeforeBoot => "process_exited_before_boot",
        GuardianObservation::ProcessExitedAfterBoot => "process_exited_after_boot",
        GuardianObservation::BootMarkerObserved => "boot_marker_observed",
        GuardianObservation::LauncherStopRequested => "launcher_stop_requested",
        GuardianObservation::Unknown(value) => value.as_str(),
    };
    (
        GuardianFactId::new(sanitize_fact_id(id)),
        domain_for_fact_id(id),
        reliability_for_observation(observation),
    )
}

fn diagnosis_for_fact(fact: &GuardianFact, phase: OperationPhase) -> Option<Diagnosis> {
    let target = affected_targets_for_fact(fact, phase);
    match fact.id.as_str() {
        "java_override_empty" | "java_override_missing" | "java_override_undefined_sentinel" => {
            Some(Diagnosis {
                id: DiagnosisId::new("java_override_unavailable"),
                domain: GuardianDomain::Runtime,
                severity: GuardianSeverity::Blocking,
                confidence: GuardianConfidence::Confirmed,
                ownership: fact.ownership,
                phase,
                fact_ids: vec![fact.id.as_str().to_string()],
                affected_targets: target,
                impact: GuardianImpactVector {
                    launchability_impact: 0.95,
                    user_intent_impact: 0.65,
                    ..GuardianImpactVector::default()
                },
                candidate_actions: vec![GuardianActionKind::Fallback, GuardianActionKind::Block],
                public_reason_template: "selected_java_runtime_unavailable".to_string(),
            })
        }
        "java_probe_failed" => Some(Diagnosis {
            id: DiagnosisId::new("java_probe_failed"),
            domain: GuardianDomain::Runtime,
            severity: GuardianSeverity::Blocking,
            confidence: GuardianConfidence::High,
            ownership: fact.ownership,
            phase,
            fact_ids: vec![fact.id.as_str().to_string()],
            affected_targets: target,
            impact: GuardianImpactVector::launch_blocking(),
            candidate_actions: vec![GuardianActionKind::Fallback, GuardianActionKind::Block],
            public_reason_template: "java_runtime_probe_failed".to_string(),
        }),
        "java_major_mismatch" => Some(Diagnosis {
            id: DiagnosisId::new("java_runtime_major_mismatch"),
            domain: GuardianDomain::Runtime,
            severity: GuardianSeverity::Blocking,
            confidence: GuardianConfidence::Confirmed,
            ownership: fact.ownership,
            phase,
            fact_ids: vec![fact.id.as_str().to_string()],
            affected_targets: target,
            impact: GuardianImpactVector::launch_blocking(),
            candidate_actions: vec![GuardianActionKind::Fallback, GuardianActionKind::Block],
            public_reason_template: "java_runtime_major_mismatch".to_string(),
        }),
        "java_update_too_old" => Some(Diagnosis {
            id: DiagnosisId::new("java_runtime_update_too_old"),
            domain: GuardianDomain::Runtime,
            severity: GuardianSeverity::Blocking,
            confidence: GuardianConfidence::Confirmed,
            ownership: fact.ownership,
            phase,
            fact_ids: vec![fact.id.as_str().to_string()],
            affected_targets: target,
            impact: GuardianImpactVector::launch_blocking(),
            candidate_actions: vec![GuardianActionKind::Fallback, GuardianActionKind::Block],
            public_reason_template: "java_update_too_old".to_string(),
        }),
        "managed_runtime_missing" => Some(Diagnosis {
            id: DiagnosisId::new("managed_runtime_missing"),
            domain: GuardianDomain::Runtime,
            severity: fact.severity.unwrap_or(GuardianSeverity::Recoverable),
            confidence: fact.confidence.unwrap_or(GuardianConfidence::Confirmed),
            ownership: fact.ownership,
            phase,
            fact_ids: vec![fact.id.as_str().to_string()],
            affected_targets: target,
            impact: GuardianImpactVector {
                launchability_impact: 0.35,
                state_corruption_impact: 0.10,
                ..GuardianImpactVector::default()
            },
            candidate_actions: vec![GuardianActionKind::RecordOnly],
            public_reason_template: "managed_runtime_missing".to_string(),
        }),
        "managed_runtime_ready_marker_missing" | "managed_runtime_corrupt" => Some(Diagnosis {
            id: DiagnosisId::new("managed_runtime_corrupt"),
            domain: GuardianDomain::Runtime,
            severity: GuardianSeverity::Repairable,
            confidence: GuardianConfidence::Confirmed,
            ownership: fact.ownership,
            phase,
            fact_ids: vec![fact.id.as_str().to_string()],
            affected_targets: target,
            impact: GuardianImpactVector::repairable_corruption(),
            candidate_actions: vec![GuardianActionKind::Repair, GuardianActionKind::Block],
            public_reason_template: "managed_runtime_needs_repair".to_string(),
        }),
        "version_json_missing"
        | "parent_version_missing"
        | "incomplete_install"
        | "client_jar_missing"
        | "libraries_missing"
        | "asset_index_missing" => Some(Diagnosis {
            id: DiagnosisId::new(readiness_diagnosis_id(fact.id.as_str())),
            domain: GuardianDomain::Install,
            severity: fact.severity.unwrap_or(GuardianSeverity::Blocking),
            confidence: fact.confidence.unwrap_or(GuardianConfidence::Confirmed),
            ownership: fact.ownership,
            phase,
            fact_ids: vec![fact.id.as_str().to_string()],
            affected_targets: target,
            impact: GuardianImpactVector {
                launchability_impact: 0.95,
                state_corruption_impact: readiness_state_corruption_impact(fact.id.as_str()),
                ..GuardianImpactVector::default()
            },
            candidate_actions: vec![GuardianActionKind::Block],
            public_reason_template: fact.id.as_str().to_string(),
        }),
        "launch_command_invalid" => Some(Diagnosis {
            id: DiagnosisId::new("launch_command_invalid"),
            domain: GuardianDomain::Launch,
            severity: GuardianSeverity::Blocking,
            confidence: GuardianConfidence::Confirmed,
            ownership: fact.ownership,
            phase,
            fact_ids: vec![fact.id.as_str().to_string()],
            affected_targets: target,
            impact: GuardianImpactVector::launch_blocking(),
            candidate_actions: vec![GuardianActionKind::Block],
            public_reason_template: "launch_command_invalid".to_string(),
        }),
        "launch_command_prepared" => Some(Diagnosis {
            id: DiagnosisId::new("launch_command_prepared"),
            domain: GuardianDomain::Launch,
            severity: GuardianSeverity::Info,
            confidence: GuardianConfidence::Confirmed,
            ownership: fact.ownership,
            phase,
            fact_ids: vec![fact.id.as_str().to_string()],
            affected_targets: target,
            impact: GuardianImpactVector::record_only(),
            candidate_actions: vec![GuardianActionKind::RecordOnly],
            public_reason_template: "launch_command_prepared".to_string(),
        }),
        "jvm_args_empty" => Some(Diagnosis {
            id: DiagnosisId::new("jvm_args_empty"),
            domain: GuardianDomain::Jvm,
            severity: GuardianSeverity::Info,
            confidence: GuardianConfidence::Confirmed,
            ownership: fact.ownership,
            phase,
            fact_ids: vec![fact.id.as_str().to_string()],
            affected_targets: target,
            impact: GuardianImpactVector::record_only(),
            candidate_actions: vec![GuardianActionKind::RecordOnly],
            public_reason_template: "jvm_args_empty".to_string(),
        }),
        "jvm_args_parse_failed" => Some(Diagnosis {
            id: DiagnosisId::new("jvm_args_malformed"),
            domain: GuardianDomain::Jvm,
            severity: GuardianSeverity::Blocking,
            confidence: GuardianConfidence::Confirmed,
            ownership: fact.ownership,
            phase,
            fact_ids: vec![fact.id.as_str().to_string()],
            affected_targets: target,
            impact: GuardianImpactVector {
                launchability_impact: 0.90,
                user_intent_impact: 0.70,
                ..GuardianImpactVector::default()
            },
            candidate_actions: vec![
                GuardianActionKind::Strip,
                GuardianActionKind::AskUser,
                GuardianActionKind::Block,
            ],
            public_reason_template: "jvm_args_malformed".to_string(),
        }),
        "jvm_arg_unsupported_gc" | "jvm_arg_unlock_order_invalid" => Some(Diagnosis {
            id: DiagnosisId::new("jvm_arg_unsupported"),
            domain: GuardianDomain::Jvm,
            severity: GuardianSeverity::Blocking,
            confidence: GuardianConfidence::Confirmed,
            ownership: fact.ownership,
            phase,
            fact_ids: vec![fact.id.as_str().to_string()],
            affected_targets: target,
            impact: GuardianImpactVector::launch_blocking(),
            candidate_actions: vec![
                GuardianActionKind::Strip,
                GuardianActionKind::AskUser,
                GuardianActionKind::Block,
            ],
            public_reason_template: "jvm_arg_unsupported".to_string(),
        }),
        "jvm_arg_reserved_launcher_flag"
        | "jvm_arg_memory_conflict"
        | "jvm_arg_unsafe_classpath_override"
        | "jvm_arg_unsafe_native_path_override"
        | "jvm_arg_agent_override" => Some(Diagnosis {
            id: DiagnosisId::new("jvm_arg_unsafe_override"),
            domain: GuardianDomain::Jvm,
            severity: GuardianSeverity::Blocking,
            confidence: GuardianConfidence::Confirmed,
            ownership: fact.ownership,
            phase,
            fact_ids: vec![fact.id.as_str().to_string()],
            affected_targets: target,
            impact: GuardianImpactVector {
                launchability_impact: 0.90,
                user_intent_impact: 0.75,
                host_stability_impact: 0.60,
                ..GuardianImpactVector::default()
            },
            candidate_actions: vec![
                GuardianActionKind::Strip,
                GuardianActionKind::AskUser,
                GuardianActionKind::Block,
            ],
            public_reason_template: "jvm_arg_unsafe_override".to_string(),
        }),
        "artifact_checksum_mismatch" | "artifact_size_mismatch" | "managed_file_corrupt" => {
            Some(Diagnosis {
                id: DiagnosisId::new("launcher_managed_artifact_corrupt"),
                domain: fact.domain,
                severity: GuardianSeverity::Repairable,
                confidence: GuardianConfidence::Confirmed,
                ownership: fact.ownership,
                phase,
                fact_ids: vec![fact.id.as_str().to_string()],
                affected_targets: target,
                impact: GuardianImpactVector::repairable_corruption(),
                candidate_actions: vec![
                    GuardianActionKind::Quarantine,
                    GuardianActionKind::Repair,
                    GuardianActionKind::Block,
                ],
                public_reason_template: "managed_artifact_corrupt".to_string(),
            })
        }
        "artifact_missing" => Some(Diagnosis {
            id: DiagnosisId::new("launcher_managed_artifact_corrupt"),
            domain: fact.domain,
            severity: GuardianSeverity::Repairable,
            confidence: GuardianConfidence::High,
            ownership: fact.ownership,
            phase,
            fact_ids: vec![fact.id.as_str().to_string()],
            affected_targets: target,
            impact: GuardianImpactVector::repairable_corruption(),
            candidate_actions: vec![
                GuardianActionKind::Quarantine,
                GuardianActionKind::Repair,
                GuardianActionKind::Block,
            ],
            public_reason_template: "managed_artifact_corrupt".to_string(),
        }),
        "provider_data_invalid" => Some(Diagnosis {
            id: DiagnosisId::new("install_artifact_metadata_invalid"),
            domain: GuardianDomain::Install,
            severity: GuardianSeverity::Blocking,
            confidence: GuardianConfidence::Confirmed,
            ownership: fact.ownership,
            phase,
            fact_ids: vec![fact.id.as_str().to_string()],
            affected_targets: target,
            impact: GuardianImpactVector::launch_blocking(),
            candidate_actions: vec![
                GuardianActionKind::Retry,
                GuardianActionKind::AskUser,
                GuardianActionKind::Block,
            ],
            public_reason_template: "install_artifact_metadata_invalid".to_string(),
        }),
        "download_provider_unavailable" | "download_interrupted" => Some(Diagnosis {
            id: DiagnosisId::new("download_unavailable"),
            domain: GuardianDomain::Download,
            severity: GuardianSeverity::Blocking,
            confidence: GuardianConfidence::Medium,
            ownership: fact.ownership,
            phase,
            fact_ids: vec![fact.id.as_str().to_string()],
            affected_targets: target,
            impact: GuardianImpactVector::launch_blocking(),
            candidate_actions: vec![
                GuardianActionKind::Retry,
                GuardianActionKind::AskUser,
                GuardianActionKind::Block,
            ],
            public_reason_template: "download_unavailable".to_string(),
        }),
        "filesystem_permission_denied" => Some(Diagnosis {
            id: DiagnosisId::new("filesystem_permission_denied"),
            domain: GuardianDomain::Filesystem,
            severity: GuardianSeverity::Blocking,
            confidence: GuardianConfidence::Confirmed,
            ownership: fact.ownership,
            phase,
            fact_ids: vec![fact.id.as_str().to_string()],
            affected_targets: target,
            impact: GuardianImpactVector::launch_blocking(),
            candidate_actions: vec![GuardianActionKind::AskUser, GuardianActionKind::Block],
            public_reason_template: "filesystem_permission_denied".to_string(),
        }),
        "ownership_unknown" | "primitive_refused" => Some(Diagnosis {
            id: DiagnosisId::new("artifact_ownership_unsafe"),
            domain: GuardianDomain::Filesystem,
            severity: GuardianSeverity::Blocking,
            confidence: GuardianConfidence::Confirmed,
            ownership: fact.ownership,
            phase,
            fact_ids: vec![fact.id.as_str().to_string()],
            affected_targets: target,
            impact: GuardianImpactVector {
                data_loss_risk: 0.95,
                user_intent_impact: 0.80,
                launchability_impact: 0.70,
                ..GuardianImpactVector::default()
            },
            candidate_actions: vec![GuardianActionKind::AskUser, GuardianActionKind::Block],
            public_reason_template: "artifact_ownership_unsafe".to_string(),
        }),
        "performance_rules_invalid" => Some(Diagnosis {
            id: DiagnosisId::new("performance_rules_invalid"),
            domain: GuardianDomain::Performance,
            severity: fact.severity.unwrap_or(GuardianSeverity::Degraded),
            confidence: fact.confidence.unwrap_or(GuardianConfidence::High),
            ownership: fact.ownership,
            phase,
            fact_ids: vec![fact.id.as_str().to_string()],
            affected_targets: target,
            impact: GuardianImpactVector {
                launchability_impact: 0.25,
                performance_impact: 0.80,
                ..GuardianImpactVector::default()
            },
            candidate_actions: vec![GuardianActionKind::RecordOnly, GuardianActionKind::Warn],
            public_reason_template: "performance_rules_invalid".to_string(),
        }),
        "performance_health_degraded" | "performance_health_invalid" => Some(Diagnosis {
            id: DiagnosisId::new(fact.id.as_str()),
            domain: GuardianDomain::Performance,
            severity: fact.severity.unwrap_or(GuardianSeverity::Degraded),
            confidence: fact.confidence.unwrap_or(GuardianConfidence::High),
            ownership: fact.ownership,
            phase,
            fact_ids: vec![fact.id.as_str().to_string()],
            affected_targets: target,
            impact: GuardianImpactVector {
                launchability_impact: 0.35,
                state_corruption_impact: if fact.id.as_str() == "performance_health_invalid" {
                    0.75
                } else {
                    0.35
                },
                performance_impact: 0.80,
                ..GuardianImpactVector::default()
            },
            candidate_actions: vec![GuardianActionKind::RecordOnly, GuardianActionKind::Warn],
            public_reason_template: fact.id.as_str().to_string(),
        }),
        "performance_fallback_selected" | "performance_health_fallback" => Some(Diagnosis {
            id: DiagnosisId::new("performance_fallback_selected"),
            domain: GuardianDomain::Performance,
            severity: fact.severity.unwrap_or(GuardianSeverity::Warning),
            confidence: fact.confidence.unwrap_or(GuardianConfidence::High),
            ownership: fact.ownership,
            phase,
            fact_ids: vec![fact.id.as_str().to_string()],
            affected_targets: target,
            impact: GuardianImpactVector {
                launchability_impact: 0.15,
                performance_impact: 0.60,
                ..GuardianImpactVector::default()
            },
            candidate_actions: vec![GuardianActionKind::RecordOnly, GuardianActionKind::Warn],
            public_reason_template: "performance_fallback_selected".to_string(),
        }),
        "performance_repeated_failure_memory" => Some(Diagnosis {
            id: DiagnosisId::new("performance_repeated_failure_memory"),
            domain: GuardianDomain::Performance,
            severity: fact.severity.unwrap_or(GuardianSeverity::Degraded),
            confidence: fact.confidence.unwrap_or(GuardianConfidence::High),
            ownership: fact.ownership,
            phase,
            fact_ids: vec![fact.id.as_str().to_string()],
            affected_targets: target,
            impact: GuardianImpactVector {
                launchability_impact: 0.30,
                performance_impact: 0.75,
                ..GuardianImpactVector::default()
            },
            candidate_actions: vec![GuardianActionKind::RecordOnly, GuardianActionKind::Warn],
            public_reason_template: "performance_repeated_failure_memory".to_string(),
        }),
        "performance_user_owned_conflict" => Some(Diagnosis {
            id: DiagnosisId::new("performance_user_owned_conflict"),
            domain: GuardianDomain::Performance,
            severity: fact.severity.unwrap_or(GuardianSeverity::Blocking),
            confidence: fact.confidence.unwrap_or(GuardianConfidence::Confirmed),
            ownership: fact.ownership,
            phase,
            fact_ids: vec![fact.id.as_str().to_string()],
            affected_targets: target,
            impact: GuardianImpactVector {
                data_loss_risk: 0.75,
                user_intent_impact: 0.85,
                performance_impact: 0.45,
                ..GuardianImpactVector::default()
            },
            candidate_actions: vec![
                GuardianActionKind::RecordOnly,
                GuardianActionKind::Warn,
                GuardianActionKind::AskUser,
                GuardianActionKind::Block,
            ],
            public_reason_template: "performance_user_owned_conflict".to_string(),
        }),
        _ if is_process_fact(fact.id.as_str()) => Some(Diagnosis {
            id: DiagnosisId::new("process_lifecycle_observed"),
            domain: GuardianDomain::Session,
            severity: GuardianSeverity::Info,
            confidence: GuardianConfidence::High,
            ownership: fact.ownership,
            phase,
            fact_ids: vec![fact.id.as_str().to_string()],
            affected_targets: target,
            impact: GuardianImpactVector::record_only(),
            candidate_actions: vec![GuardianActionKind::RecordOnly],
            public_reason_template: "process_lifecycle_observed".to_string(),
        }),
        _ => None,
    }
}

fn readiness_diagnosis_id(fact_id: &str) -> &'static str {
    match fact_id {
        "version_json_missing" => "installed_version_metadata_missing",
        "parent_version_missing" => "parent_version_metadata_missing",
        "incomplete_install" => "install_incomplete",
        "client_jar_missing" => "client_jar_missing",
        "libraries_missing" => "libraries_missing",
        "asset_index_missing" => "asset_index_missing",
        _ => "launch_readiness_blocking",
    }
}

fn readiness_state_corruption_impact(fact_id: &str) -> f32 {
    match fact_id {
        "incomplete_install" => 0.75,
        "version_json_missing" | "parent_version_missing" => 0.65,
        "client_jar_missing" | "libraries_missing" | "asset_index_missing" => 0.55,
        _ => 0.50,
    }
}

fn unknown_diagnosis(facts: &[GuardianFact], phase: OperationPhase) -> Diagnosis {
    let ownership = facts
        .first()
        .map(|fact| fact.ownership)
        .unwrap_or(OwnershipClass::Unknown);
    let mut affected_targets = facts
        .iter()
        .filter_map(|fact| fact.target.clone())
        .collect::<Vec<_>>();
    if affected_targets.is_empty() {
        affected_targets.push(fallback_target(GuardianDomain::Unknown, ownership, phase));
    }
    Diagnosis {
        id: DiagnosisId::new(format!("unknown_failure_{}", phase_name(phase))),
        domain: GuardianDomain::Unknown,
        severity: GuardianSeverity::Warning,
        confidence: GuardianConfidence::Low,
        ownership,
        phase,
        fact_ids: supporting_fact_ids(facts, phase),
        affected_targets,
        impact: GuardianImpactVector::record_only(),
        candidate_actions: vec![
            GuardianActionKind::RecordOnly,
            GuardianActionKind::Warn,
            GuardianActionKind::AskUser,
        ],
        public_reason_template: "unknown_failure".to_string(),
    }
}

fn affected_targets_for_fact(fact: &GuardianFact, phase: OperationPhase) -> Vec<TargetDescriptor> {
    fact.target
        .clone()
        .into_iter()
        .chain(
            fact.target
                .is_none()
                .then(|| fallback_target(fact.domain, fact.ownership, phase)),
        )
        .collect()
}

fn supporting_fact_ids(facts: &[GuardianFact], phase: OperationPhase) -> Vec<String> {
    let mut fact_ids = facts
        .iter()
        .map(|fact| fact.id.as_str().to_string())
        .collect::<Vec<_>>();
    if fact_ids.is_empty() {
        fact_ids.push(format!("no_structured_fact_{}", phase_name(phase)));
    }
    fact_ids
}

fn fallback_target(
    domain: GuardianDomain,
    ownership: OwnershipClass,
    phase: OperationPhase,
) -> TargetDescriptor {
    TargetDescriptor::new(
        StabilizationSystem::Guardian,
        target_kind_for_domain(domain),
        format!("guardian-{}-{}", domain_name(domain), phase_name(phase)),
        ownership,
    )
}

fn target_kind_for_domain(domain: GuardianDomain) -> TargetKind {
    match domain {
        GuardianDomain::Runtime => TargetKind::Runtime,
        GuardianDomain::Download | GuardianDomain::Network => TargetKind::NetworkResource,
        GuardianDomain::Library | GuardianDomain::Filesystem | GuardianDomain::Install => {
            TargetKind::Artifact
        }
        GuardianDomain::Launch | GuardianDomain::Startup | GuardianDomain::Session => {
            TargetKind::Session
        }
        GuardianDomain::Auth => TargetKind::Account,
        GuardianDomain::Performance => TargetKind::PerformanceComposition,
        GuardianDomain::Config
        | GuardianDomain::Jvm
        | GuardianDomain::State
        | GuardianDomain::Unknown => TargetKind::Config,
    }
}

fn public_safe_target(target: &TargetDescriptor) -> TargetDescriptor {
    TargetDescriptor::new(
        target.system,
        target.kind,
        target.id.as_str(),
        target.ownership,
    )
}

fn public_safe_fields(fields: &[EvidenceField]) -> Vec<EvidenceField> {
    fields
        .iter()
        .filter_map(|field| {
            field
                .value_for(RedactionAudience::UserVisible)
                .and_then(|value| {
                    sanitize_evidence_token(value, RedactionAudience::UserVisible, 96)
                })
                .map(|value| EvidenceField::new(field.key.clone(), value, field.sensitivity))
        })
        .collect()
}

fn exit_code_fact_id(fact: &ExecutionFact) -> &'static str {
    let exit_code = fact
        .fields
        .iter()
        .find(|field| field.key == "exit_code")
        .and_then(|field| field.value.parse::<i32>().ok());
    match exit_code {
        Some(0) => "exit_code_zero",
        Some(_) => "exit_code_nonzero",
        None => "exit_code_unknown",
    }
}

fn reliability_for_execution_fact(kind: ExecutionFactKind) -> FactReliability {
    match kind {
        ExecutionFactKind::RuntimeProbeFailed
        | ExecutionFactKind::RuntimeWrongMajor
        | ExecutionFactKind::RuntimeWrongUpdate
        | ExecutionFactKind::DownloadChecksumMismatch
        | ExecutionFactKind::DownloadSizeMismatch
        | ExecutionFactKind::ChecksumMismatch
        | ExecutionFactKind::SizeMismatch => FactReliability::ValidatedProbe,
        ExecutionFactKind::RuntimeJavaOverrideEmpty
        | ExecutionFactKind::RuntimeJavaOverrideUndefinedSentinel => {
            FactReliability::ExactClassifier
        }
        ExecutionFactKind::JvmArgsParseFailed
        | ExecutionFactKind::JvmArgReservedLauncherFlag
        | ExecutionFactKind::JvmArgMemoryConflict
        | ExecutionFactKind::JvmArgUnsupportedGc
        | ExecutionFactKind::JvmArgUnlockOrderInvalid
        | ExecutionFactKind::JvmArgUnsafeClasspathOverride
        | ExecutionFactKind::JvmArgUnsafeNativePathOverride
        | ExecutionFactKind::JvmArgAgentOverride => FactReliability::ExactClassifier,
        ExecutionFactKind::ProcessSpawned
        | ExecutionFactKind::ProcessStopIntent
        | ExecutionFactKind::ProcessKilled
        | ExecutionFactKind::ProcessExitCode
        | ExecutionFactKind::ProcessBootEvidence
        | ExecutionFactKind::ProcessWatchdogAction
        | ExecutionFactKind::ProcessExited => FactReliability::ProcessLifecycle,
        ExecutionFactKind::RuntimeReadyMarkerMissing => FactReliability::ExpectedMarkerAbsence,
        _ => FactReliability::DirectStructured,
    }
}

fn reliability_for_observation(observation: &GuardianObservation) -> FactReliability {
    match observation {
        GuardianObservation::BootMarkerObserved
        | GuardianObservation::LauncherStopRequested
        | GuardianObservation::ProcessExitedBeforeBoot
        | GuardianObservation::ProcessExitedAfterBoot => FactReliability::ProcessLifecycle,
        GuardianObservation::JavaProbeFailed | GuardianObservation::JavaMajorMismatch => {
            FactReliability::ValidatedProbe
        }
        GuardianObservation::JvmArgsParseFailed
        | GuardianObservation::JvmArgReservedLauncherFlag
        | GuardianObservation::JvmArgMemoryConflict
        | GuardianObservation::JvmArgUnsupportedGc
        | GuardianObservation::JvmArgUnlockOrderInvalid
        | GuardianObservation::JvmArgUnsafeClasspathOverride
        | GuardianObservation::JvmArgUnsafeNativePathOverride
        | GuardianObservation::JvmArgAgentOverride => FactReliability::ExactClassifier,
        GuardianObservation::RawJvmArgsPresent | GuardianObservation::Unknown(_) => {
            FactReliability::HeuristicClassifier
        }
        _ => FactReliability::DirectStructured,
    }
}

fn domain_for_fact_id(id: &str) -> GuardianDomain {
    if id.starts_with("java_") || id.starts_with("managed_runtime") {
        GuardianDomain::Runtime
    } else if id.starts_with("jvm_") || id == "raw_jvm_args_present" {
        GuardianDomain::Jvm
    } else if id.starts_with("launch_command") {
        GuardianDomain::Launch
    } else if matches!(
        id,
        "version_json_missing"
            | "parent_version_missing"
            | "incomplete_install"
            | "client_jar_missing"
            | "libraries_missing"
            | "asset_index_missing"
    ) {
        GuardianDomain::Install
    } else if id.starts_with("download_") {
        GuardianDomain::Download
    } else if id.starts_with("process_")
        || id.starts_with("exit_code")
        || id == "boot_marker_observed"
        || id == "launcher_stop_requested"
        || id == "watchdog_killed_process"
    {
        GuardianDomain::Session
    } else if id.contains("artifact") || id.starts_with("file_") {
        GuardianDomain::Library
    } else if id.starts_with("filesystem_") || id == "temp_file_leftover" {
        GuardianDomain::Filesystem
    } else if id.starts_with("provider") {
        GuardianDomain::Network
    } else if id.starts_with("performance_") {
        GuardianDomain::Performance
    } else {
        GuardianDomain::Unknown
    }
}

fn is_process_fact(id: &str) -> bool {
    matches!(
        id,
        "process_spawned"
            | "launcher_stop_requested"
            | "watchdog_killed_process"
            | "exit_code_zero"
            | "exit_code_nonzero"
            | "exit_code_unknown"
            | "boot_marker_observed"
            | "process_exited"
            | "process_exited_before_boot"
            | "process_exited_after_boot"
    )
}

fn sanitize_fact_id(id: &str) -> String {
    sanitize_evidence_token(id, RedactionAudience::UserVisible, 96)
        .unwrap_or_else(|| "unknown_fact".to_string())
}

fn phase_name(phase: OperationPhase) -> &'static str {
    match phase {
        OperationPhase::Startup => "startup",
        OperationPhase::Planning => "planning",
        OperationPhase::Validating => "validating",
        OperationPhase::Downloading => "downloading",
        OperationPhase::Installing => "installing",
        OperationPhase::Preparing => "preparing",
        OperationPhase::Launching => "launching",
        OperationPhase::Running => "running",
        OperationPhase::Repairing => "repairing",
        OperationPhase::RollingBack => "rolling_back",
        OperationPhase::Completed => "completed",
        OperationPhase::Failed => "failed",
    }
}

fn domain_name(domain: GuardianDomain) -> &'static str {
    match domain {
        GuardianDomain::Config => "config",
        GuardianDomain::Library => "library",
        GuardianDomain::Runtime => "runtime",
        GuardianDomain::Jvm => "jvm",
        GuardianDomain::Install => "install",
        GuardianDomain::Download => "download",
        GuardianDomain::Performance => "performance",
        GuardianDomain::Launch => "launch",
        GuardianDomain::Startup => "startup",
        GuardianDomain::Session => "session",
        GuardianDomain::Filesystem => "filesystem",
        GuardianDomain::Network => "network",
        GuardianDomain::Auth => "auth",
        GuardianDomain::State => "state",
        GuardianDomain::Unknown => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ActionPlanPrerequisite, Diagnosis, FactReliability, GuardianAction, GuardianActionKind,
        GuardianActionPlan, GuardianConfidence, GuardianDomain, GuardianFact, GuardianFactId,
        GuardianMode, GuardianObservation, GuardianSeverity, GuardianSeverity::Repairable,
        build_safety_case, diagnose_facts, guardian_fact_from_execution,
        guardian_fact_from_observation,
    };
    use crate::execution::{ExecutionFact, ExecutionFactKind};
    use crate::observability::{EvidenceField, EvidenceSensitivity};
    use crate::state::contracts::{
        OperationPhase, OwnershipClass, StabilizationSystem, TargetDescriptor, TargetKind,
    };

    #[test]
    fn execution_runtime_fact_maps_to_confirmed_runtime_diagnosis() {
        let target = target(
            "runtime",
            TargetKind::Runtime,
            OwnershipClass::LauncherManaged,
        );
        let execution_fact = ExecutionFact {
            operation_id: None,
            kind: ExecutionFactKind::RuntimeReadyMarkerMissing,
            target: Some(target.clone()),
            fields: Vec::new(),
        };

        let fact = guardian_fact_from_execution(&execution_fact, OperationPhase::Preparing);
        let diagnoses = diagnose_facts(&[fact], OperationPhase::Preparing);

        assert_eq!(diagnoses.len(), 1);
        let diagnosis = &diagnoses[0];
        assert_eq!(diagnosis.id.as_str(), "managed_runtime_corrupt");
        assert_eq!(diagnosis.domain, GuardianDomain::Runtime);
        assert_eq!(diagnosis.severity, Repairable);
        assert_eq!(diagnosis.confidence, GuardianConfidence::Confirmed);
        assert_eq!(diagnosis.ownership, OwnershipClass::LauncherManaged);
        assert!(
            diagnosis
                .candidate_actions
                .contains(&GuardianActionKind::Repair)
        );
        let prerequisite = diagnosis
            .action_prerequisite()
            .expect("action prerequisite");
        assert_eq!(prerequisite.ownership, OwnershipClass::LauncherManaged);
        assert_eq!(prerequisite.confidence, GuardianConfidence::Confirmed);
    }

    #[test]
    fn execution_java_override_sentinel_maps_to_unavailable_diagnosis() {
        let target = target(
            "instance_java_override",
            TargetKind::Config,
            OwnershipClass::UserOwned,
        );
        let execution_fact = ExecutionFact {
            operation_id: None,
            kind: ExecutionFactKind::RuntimeJavaOverrideUndefinedSentinel,
            target: Some(target),
            fields: vec![EvidenceField::new(
                "sentinel",
                "undefined",
                EvidenceSensitivity::Public,
            )],
        };

        let fact = guardian_fact_from_execution(&execution_fact, OperationPhase::Validating);
        let diagnoses = diagnose_facts(&[fact.clone()], OperationPhase::Validating);

        assert_eq!(fact.id.as_str(), "java_override_undefined_sentinel");
        assert_eq!(fact.domain, GuardianDomain::Runtime);
        assert_eq!(fact.reliability, FactReliability::ExactClassifier);
        assert_eq!(diagnoses.len(), 1);
        assert_eq!(diagnoses[0].id.as_str(), "java_override_unavailable");
        assert_eq!(diagnoses[0].severity, GuardianSeverity::Blocking);
        assert_eq!(diagnoses[0].ownership, OwnershipClass::UserOwned);
        assert!(
            diagnoses[0]
                .candidate_actions
                .contains(&GuardianActionKind::Fallback)
        );
    }

    #[test]
    fn execution_java_update_fact_maps_to_update_diagnosis() {
        let target = target(
            "manual_java",
            TargetKind::Runtime,
            OwnershipClass::UserOwned,
        );
        let execution_fact = ExecutionFact {
            operation_id: None,
            kind: ExecutionFactKind::RuntimeWrongUpdate,
            target: Some(target),
            fields: vec![
                EvidenceField::new("required_min_update", "312", EvidenceSensitivity::Public),
                EvidenceField::new("actual_update", "311", EvidenceSensitivity::Public),
            ],
        };

        let fact = guardian_fact_from_execution(&execution_fact, OperationPhase::Validating);
        let diagnoses = diagnose_facts(&[fact.clone()], OperationPhase::Validating);

        assert_eq!(fact.id.as_str(), "java_update_too_old");
        assert_eq!(fact.domain, GuardianDomain::Runtime);
        assert_eq!(fact.reliability, FactReliability::ValidatedProbe);
        assert_eq!(diagnoses.len(), 1);
        assert_eq!(diagnoses[0].id.as_str(), "java_runtime_update_too_old");
        assert_eq!(diagnoses[0].severity, GuardianSeverity::Blocking);
        assert!(
            diagnoses[0]
                .candidate_actions
                .contains(&GuardianActionKind::Fallback)
        );
    }

    #[test]
    fn execution_launch_command_fact_maps_to_launch_domain() {
        let target = target(
            "session-1",
            TargetKind::Session,
            OwnershipClass::LauncherManaged,
        );
        let execution_fact = ExecutionFact {
            operation_id: None,
            kind: ExecutionFactKind::LaunchCommandPrepared,
            target: Some(target),
            fields: vec![EvidenceField::new(
                "program",
                "launch_program",
                EvidenceSensitivity::Public,
            )],
        };

        let fact = guardian_fact_from_execution(&execution_fact, OperationPhase::Preparing);
        let diagnoses = diagnose_facts(&[fact.clone()], OperationPhase::Preparing);

        assert_eq!(fact.id.as_str(), "launch_command_prepared");
        assert_eq!(fact.domain, GuardianDomain::Launch);
        assert_eq!(diagnoses.len(), 1);
        assert_eq!(diagnoses[0].id.as_str(), "launch_command_prepared");
        assert_eq!(diagnoses[0].severity, GuardianSeverity::Info);
    }

    #[test]
    fn execution_launch_command_invalid_fact_maps_to_blocking_diagnosis() {
        let target = target(
            "session-1",
            TargetKind::Session,
            OwnershipClass::LauncherManaged,
        );
        let execution_fact = ExecutionFact {
            operation_id: None,
            kind: ExecutionFactKind::LaunchCommandInvalid,
            target: Some(target),
            fields: vec![EvidenceField::new(
                "arg_count",
                "1",
                EvidenceSensitivity::Public,
            )],
        };

        let fact = guardian_fact_from_execution(&execution_fact, OperationPhase::Preparing);
        let diagnoses = diagnose_facts(&[fact], OperationPhase::Preparing);

        assert_eq!(diagnoses.len(), 1);
        assert_eq!(diagnoses[0].id.as_str(), "launch_command_invalid");
        assert_eq!(diagnoses[0].severity, GuardianSeverity::Blocking);
        assert!(
            diagnoses[0]
                .candidate_actions
                .contains(&GuardianActionKind::Block)
        );
    }

    #[test]
    fn launch_readiness_fact_maps_to_blocking_install_diagnosis() {
        let fact = GuardianFact {
            operation_id: None,
            id: GuardianFactId::new("incomplete_install"),
            domain: GuardianDomain::Install,
            phase: OperationPhase::Validating,
            reliability: FactReliability::DirectStructured,
            severity: Some(GuardianSeverity::Blocking),
            confidence: Some(GuardianConfidence::Confirmed),
            ownership: OwnershipClass::LauncherManaged,
            target: Some(target(
                "incomplete_install",
                TargetKind::Version,
                OwnershipClass::LauncherManaged,
            )),
            fields: Vec::new(),
        };

        let diagnoses = diagnose_facts(&[fact], OperationPhase::Validating);

        assert_eq!(diagnoses.len(), 1);
        assert_eq!(diagnoses[0].id.as_str(), "install_incomplete");
        assert_eq!(diagnoses[0].domain, GuardianDomain::Install);
        assert_eq!(diagnoses[0].severity, GuardianSeverity::Blocking);
        assert_eq!(diagnoses[0].confidence, GuardianConfidence::Confirmed);
        assert_eq!(
            diagnoses[0].candidate_actions,
            vec![GuardianActionKind::Block]
        );
        assert_eq!(diagnoses[0].affected_targets[0].kind, TargetKind::Version);
    }

    #[test]
    fn managed_runtime_readiness_fact_maps_to_recoverable_diagnosis() {
        let fact = GuardianFact {
            operation_id: None,
            id: GuardianFactId::new("managed_runtime_missing"),
            domain: GuardianDomain::Runtime,
            phase: OperationPhase::Validating,
            reliability: FactReliability::ExpectedMarkerAbsence,
            severity: Some(GuardianSeverity::Recoverable),
            confidence: Some(GuardianConfidence::Confirmed),
            ownership: OwnershipClass::LauncherManaged,
            target: Some(target(
                "managed_runtime",
                TargetKind::Runtime,
                OwnershipClass::LauncherManaged,
            )),
            fields: Vec::new(),
        };

        let diagnoses = diagnose_facts(&[fact], OperationPhase::Validating);

        assert_eq!(diagnoses.len(), 1);
        assert_eq!(diagnoses[0].id.as_str(), "managed_runtime_missing");
        assert_eq!(diagnoses[0].domain, GuardianDomain::Runtime);
        assert_eq!(diagnoses[0].severity, GuardianSeverity::Recoverable);
        assert_eq!(
            diagnoses[0].candidate_actions,
            vec![GuardianActionKind::RecordOnly]
        );
        assert_eq!(diagnoses[0].affected_targets[0].kind, TargetKind::Runtime);
    }

    #[test]
    fn execution_jvm_parse_fact_maps_to_malformed_diagnosis() {
        let target = target(
            "explicit_jvm_args",
            TargetKind::Config,
            OwnershipClass::UserOwned,
        );
        let execution_fact = ExecutionFact {
            operation_id: None,
            kind: ExecutionFactKind::JvmArgsParseFailed,
            target: Some(target),
            fields: vec![EvidenceField::new(
                "raw",
                r#""unterminated -Xmx8G C:\Users\Alice"#,
                EvidenceSensitivity::Internal,
            )],
        };

        let fact = guardian_fact_from_execution(&execution_fact, OperationPhase::Validating);
        let diagnoses = diagnose_facts(&[fact.clone()], OperationPhase::Validating);

        assert_eq!(fact.id.as_str(), "jvm_args_parse_failed");
        assert_eq!(fact.domain, GuardianDomain::Jvm);
        assert_eq!(fact.reliability, FactReliability::ExactClassifier);
        assert!(fact.fields.is_empty());
        assert_eq!(diagnoses.len(), 1);
        assert_eq!(diagnoses[0].id.as_str(), "jvm_args_malformed");
        assert_eq!(diagnoses[0].severity, GuardianSeverity::Blocking);
        assert_eq!(diagnoses[0].confidence, GuardianConfidence::Confirmed);
        assert!(
            diagnoses[0]
                .candidate_actions
                .contains(&GuardianActionKind::Strip)
        );
    }

    #[test]
    fn execution_jvm_unsafe_fact_maps_to_unsafe_override_diagnosis() {
        let target = target(
            "explicit_jvm_args",
            TargetKind::Config,
            OwnershipClass::UserOwned,
        );
        let execution_fact = ExecutionFact {
            operation_id: None,
            kind: ExecutionFactKind::JvmArgAgentOverride,
            target: Some(target),
            fields: vec![EvidenceField::new(
                "arg_family",
                "agent",
                EvidenceSensitivity::Public,
            )],
        };

        let fact = guardian_fact_from_execution(&execution_fact, OperationPhase::Validating);
        let diagnoses = diagnose_facts(&[fact], OperationPhase::Validating);

        assert_eq!(diagnoses.len(), 1);
        assert_eq!(diagnoses[0].id.as_str(), "jvm_arg_unsafe_override");
        assert_eq!(diagnoses[0].domain, GuardianDomain::Jvm);
        assert_eq!(diagnoses[0].ownership, OwnershipClass::UserOwned);
        assert!(
            diagnoses[0]
                .candidate_actions
                .contains(&GuardianActionKind::AskUser)
        );
    }

    #[test]
    fn execution_download_and_process_facts_map_to_guardian_fact_ids() {
        let target = target(
            "session",
            TargetKind::Session,
            OwnershipClass::LauncherManaged,
        );
        let cases = [
            (
                ExecutionFactKind::DownloadProviderFailure,
                "download_provider_unavailable",
            ),
            (
                ExecutionFactKind::DownloadInterrupted,
                "download_interrupted",
            ),
            (
                ExecutionFactKind::DownloadChecksumMismatch,
                "artifact_checksum_mismatch",
            ),
            (
                ExecutionFactKind::DownloadSizeMismatch,
                "artifact_size_mismatch",
            ),
            (
                ExecutionFactKind::ProcessStopIntent,
                "launcher_stop_requested",
            ),
            (
                ExecutionFactKind::ProcessWatchdogAction,
                "watchdog_killed_process",
            ),
            (
                ExecutionFactKind::ProcessBootEvidence,
                "boot_marker_observed",
            ),
        ];

        for (kind, expected) in cases {
            let fact = guardian_fact_from_execution(
                &ExecutionFact {
                    operation_id: None,
                    kind,
                    target: Some(target.clone()),
                    fields: Vec::new(),
                },
                OperationPhase::Running,
            );
            assert_eq!(fact.id.as_str(), expected);
        }
    }

    #[test]
    fn exit_code_fact_maps_zero_and_nonzero_without_exit_classification() {
        let target = target(
            "session",
            TargetKind::Session,
            OwnershipClass::LauncherManaged,
        );
        for (exit_code, expected) in [(0, "exit_code_zero"), (1, "exit_code_nonzero")] {
            let fact = guardian_fact_from_execution(
                &ExecutionFact {
                    operation_id: None,
                    kind: ExecutionFactKind::ProcessExitCode,
                    target: Some(target.clone()),
                    fields: vec![EvidenceField::new(
                        "exit_code",
                        exit_code.to_string(),
                        EvidenceSensitivity::Public,
                    )],
                },
                OperationPhase::Running,
            );
            assert_eq!(fact.id.as_str(), expected);
            let diagnoses = diagnose_facts(&[fact], OperationPhase::Running);
            assert_eq!(diagnoses[0].id.as_str(), "process_lifecycle_observed");
            assert_eq!(
                diagnoses[0].candidate_actions,
                vec![GuardianActionKind::RecordOnly]
            );
        }
    }

    #[test]
    fn unknown_facts_produce_low_confidence_unknown_diagnosis() {
        let fact = guardian_fact_from_observation(
            GuardianObservation::Unknown("unexpected_signal".to_string()),
            OperationPhase::Launching,
            Some(target(
                "unknown",
                TargetKind::Session,
                OwnershipClass::Unknown,
            )),
        );

        let diagnoses = diagnose_facts(&[fact], OperationPhase::Launching);

        assert_eq!(diagnoses.len(), 1);
        assert_eq!(diagnoses[0].id.as_str(), "unknown_failure_launching");
        assert_eq!(diagnoses[0].domain, GuardianDomain::Unknown);
        assert_eq!(diagnoses[0].confidence, GuardianConfidence::Low);
        assert!(
            diagnoses[0]
                .candidate_actions
                .contains(&GuardianActionKind::RecordOnly)
        );
    }

    #[test]
    fn action_prerequisite_requires_target_and_candidate_action() {
        let mut diagnosis = Diagnosis {
            id: super::DiagnosisId::new("incomplete"),
            domain: GuardianDomain::Unknown,
            severity: GuardianSeverity::Warning,
            confidence: GuardianConfidence::Low,
            ownership: OwnershipClass::Unknown,
            phase: OperationPhase::Launching,
            fact_ids: vec!["fact".to_string()],
            affected_targets: Vec::new(),
            impact: Default::default(),
            candidate_actions: vec![GuardianActionKind::RecordOnly],
            public_reason_template: "unknown".to_string(),
        };
        assert!(diagnosis.action_prerequisite().is_err());

        diagnosis.affected_targets.push(target(
            "target",
            TargetKind::Session,
            OwnershipClass::LauncherManaged,
        ));
        diagnosis.candidate_actions.clear();
        assert!(diagnosis.action_prerequisite().is_err());

        diagnosis
            .candidate_actions
            .push(GuardianActionKind::RecordOnly);
        let prerequisite: ActionPlanPrerequisite = diagnosis
            .action_prerequisite()
            .expect("complete prerequisite");
        assert_eq!(prerequisite.confidence, GuardianConfidence::Low);
        assert_eq!(prerequisite.ownership, OwnershipClass::Unknown);
    }

    #[test]
    fn action_plan_representation_carries_prerequisite_metadata() {
        let target = target(
            "runtime",
            TargetKind::Runtime,
            OwnershipClass::LauncherManaged,
        );
        let diagnosis = Diagnosis {
            id: super::DiagnosisId::new("managed_runtime_corrupt"),
            domain: GuardianDomain::Runtime,
            severity: GuardianSeverity::Repairable,
            confidence: GuardianConfidence::Confirmed,
            ownership: OwnershipClass::LauncherManaged,
            phase: OperationPhase::Preparing,
            fact_ids: vec!["managed_runtime_corrupt".to_string()],
            affected_targets: vec![target.clone()],
            impact: Default::default(),
            candidate_actions: vec![GuardianActionKind::Repair],
            public_reason_template: "managed_runtime_needs_repair".to_string(),
        };
        let prerequisite = diagnosis
            .action_prerequisite()
            .expect("complete prerequisite");
        let plan = GuardianActionPlan::new(
            StabilizationSystem::Guardian,
            prerequisite,
            vec![GuardianAction {
                kind: GuardianActionKind::Repair,
                target: Some(target),
                reason: diagnosis.id.clone(),
            }],
        );

        assert_eq!(plan.prerequisite.confidence, GuardianConfidence::Confirmed);
        assert_eq!(plan.prerequisite.ownership, OwnershipClass::LauncherManaged);
        let encoded = serde_json::to_string(&plan).expect("plan json");
        assert!(encoded.contains("prerequisite"));
        assert!(encoded.contains("Confirmed"));
        assert!(encoded.contains("LauncherManaged"));
    }

    #[test]
    fn targetless_fact_receives_guardian_fallback_target() {
        let fact = guardian_fact_from_execution(
            &ExecutionFact {
                operation_id: None,
                kind: ExecutionFactKind::RuntimeProbeFailed,
                target: None,
                fields: Vec::new(),
            },
            OperationPhase::Preparing,
        );

        let diagnoses = diagnose_facts(&[fact], OperationPhase::Preparing);

        assert_eq!(diagnoses.len(), 1);
        assert_eq!(diagnoses[0].id.as_str(), "java_probe_failed");
        assert_eq!(
            diagnoses[0].affected_targets[0],
            TargetDescriptor::new(
                StabilizationSystem::Guardian,
                TargetKind::Runtime,
                "guardian-runtime-preparing",
                OwnershipClass::Unknown,
            )
        );
        diagnoses[0]
            .action_prerequisite()
            .expect("fallback target makes prerequisite representable");
    }

    #[test]
    fn empty_fact_set_unknown_diagnosis_has_fallback_target() {
        let diagnoses = diagnose_facts(&[], OperationPhase::Launching);

        assert_eq!(diagnoses.len(), 1);
        assert_eq!(diagnoses[0].id.as_str(), "unknown_failure_launching");
        assert_eq!(
            diagnoses[0].fact_ids,
            vec!["no_structured_fact_launching".to_string()]
        );
        assert_eq!(
            diagnoses[0].affected_targets[0],
            TargetDescriptor::new(
                StabilizationSystem::Guardian,
                TargetKind::Config,
                "guardian-unknown-launching",
                OwnershipClass::Unknown,
            )
        );
    }

    #[test]
    fn guardian_fact_redaction_drops_raw_paths_jvm_args_and_tokens() {
        let target = TargetDescriptor {
            system: StabilizationSystem::Execution,
            kind: TargetKind::Runtime,
            id: r"C:\Users\Alice\java.exe --accessToken abc".to_string(),
            ownership: OwnershipClass::UserOwned,
        };
        let fact = guardian_fact_from_execution(
            &ExecutionFact {
                operation_id: None,
                kind: ExecutionFactKind::RuntimeProbeFailed,
                target: Some(target),
                fields: vec![
                    EvidenceField::new(
                        "raw",
                        "/home/alice/.jdks/java -Xmx8192M --accessToken secret",
                        EvidenceSensitivity::Public,
                    ),
                    EvidenceField::new("safe", "probe_failed", EvidenceSensitivity::Public),
                ],
            },
            OperationPhase::Preparing,
        );

        let encoded = serde_json::to_string(&fact).expect("fact json");
        let lower = encoded.to_ascii_lowercase();
        assert!(lower.contains("probe_failed"));
        assert!(!lower.contains("/home/"));
        assert!(!lower.contains("users\\\\alice"));
        assert!(!lower.contains("java.exe"));
        assert!(!lower.contains("-xmx"));
        assert!(!lower.contains("--accesstoken"));
        assert!(!lower.contains("secret"));
    }

    #[test]
    fn safety_case_carries_diagnosis_and_hard_constraints() {
        let fact = guardian_fact_from_observation(
            GuardianObservation::JavaMajorMismatch,
            OperationPhase::Preparing,
            Some(target(
                "runtime",
                TargetKind::Runtime,
                OwnershipClass::LauncherManaged,
            )),
        );

        let safety_case = build_safety_case(
            None,
            GuardianMode::Managed,
            OperationPhase::Preparing,
            &[fact],
        );

        assert_eq!(safety_case.diagnoses.len(), 1);
        assert_eq!(
            safety_case.diagnoses[0].id.as_str(),
            "java_runtime_major_mismatch"
        );
        assert!(!safety_case.hard_constraints.is_empty());
    }

    #[test]
    fn impact_vector_uses_priority_weighting() {
        let vector = super::GuardianImpactVector {
            privacy_risk: 0.0,
            data_loss_risk: 0.0,
            launchability_impact: 0.8,
            state_corruption_impact: 0.4,
            user_intent_impact: 0.2,
            performance_impact: 1.0,
            host_stability_impact: 0.3,
        };

        assert!((vector.scalar_severity() - 0.72).abs() < f32::EPSILON);
    }

    fn target(id: &str, kind: TargetKind, ownership: OwnershipClass) -> TargetDescriptor {
        TargetDescriptor::new(StabilizationSystem::Guardian, kind, id, ownership)
    }

    fn _assert_fact_is_send_sync(_: &GuardianFact) {}
}
