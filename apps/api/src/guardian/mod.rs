//! Guardian system boundary.
//!
//! Guardian owns safety facts, diagnosis, policy decisions, action planning,
//! supervised recovery, user outcomes, and bounded failure memory across
//! launch, install, runtime, and performance workflows.

pub mod artifact_descriptor;
pub mod artifact_repair;
mod copy;
mod directive;
pub mod healing;
pub mod install_evidence;
pub mod jvm_preset;
pub mod launch_decision;
pub mod launch_failure_memory;
pub mod launch_recovery;
pub mod performance;
pub mod policy;
pub mod preflight;
pub mod repair_plan;
pub mod state_evidence;

#[cfg(test)]
mod decision_snapshot;
mod diagnosis;
mod facts;
#[cfg(test)]
mod launch_copy_snapshot;
#[cfg(test)]
mod launch_decision_snapshot;
mod model;
#[cfg(test)]
mod outcome_snapshot;
#[cfg(test)]
mod preflight_copy_snapshot;
#[cfg(test)]
mod preflight_decision_snapshot;
#[cfg(test)]
mod preset_stage_copy_snapshot;
#[cfg(test)]
mod projection_copy_snapshot;
mod repair_terminal;
mod rules;

#[cfg(test)]
mod tests;

pub use artifact_descriptor::GuardianMinecraftArtifactRepairDescriptor;
pub use artifact_repair::{
    GuardianArtifactRepairMutation, GuardianArtifactRepairOutcome, GuardianArtifactRepairRequest,
    GuardianArtifactRepairSource, GuardianArtifactRepairStatus, execute_guardian_artifact_repair,
};
pub(crate) use copy::{
    GuardianCopyRequest, GuardianLaunchAdmission, GuardianRuntimeRepairCopy, author_guardian_copy,
    guardian_directive_description, guardian_failed_launch_recovery_log,
    guardian_install_outcome_fact_group, guardian_install_outcome_from_persisted_group,
    guardian_install_outcome_persistence_facts, guardian_launch_stage_evidence,
    guardian_proof_evidence, guardian_summary_from_admission,
    guardian_summary_with_blocked_outcome, guardian_summary_with_intervention,
    guardian_summary_with_observed_outcome, guardian_summary_with_suppressed_outcome,
};
pub use copy::{
    GuardianInstallOutcomeSummary, GuardianJvmPresetNotice, GuardianJvmPresetOption,
    GuardianUserOutcome, guardian_jvm_preset_notice, guardian_jvm_preset_options,
};
#[cfg(test)]
pub(crate) use copy::{guardian_launch_stage_evidence_for_test, guardian_user_outcome_for_test};
pub use diagnosis::{Diagnosis, build_safety_case, diagnose};
pub use directive::{
    GuardianDirective, GuardianManagedJavaReason, GuardianPresetDowngradeReason,
    GuardianPresetValue, GuardianStripJvmArgsReason,
};
pub(crate) use directive::{GuardianRecoveryIntentAxis, GuardianRecoveryMetadata};
pub use facts::guardian_fact_from_execution;
pub use healing::{
    GuardianManagedRuntimeRepairRequest, GuardianRepairOutcome, GuardianRepairStatus,
    execute_managed_runtime_ready_marker_repair,
};
pub use install_evidence::{
    GuardianInstallArtifactFailureEvidence, GuardianInstallArtifactFailureKind,
    GuardianInstallArtifactRepairPlanKind, GuardianInstallArtifactRepairPlanRejection,
    GuardianInstallFailureOutcome, install_artifact_failure_from_minecraft_download_fact,
    install_artifact_failure_guardian_fact, install_artifact_failure_guardian_outcome,
    install_artifact_failure_guardian_outcome_with_context, install_artifact_failure_safety_case,
    plan_install_artifact_failure_repair,
};
pub use jvm_preset::{
    GuardianJvmPresetId, GuardianJvmPresetResolution, normalize_create_jvm_preset,
};
pub use launch_decision::{
    GuardianLaunchFailureOutcome, GuardianObservedLaunchFailurePhase,
    GuardianPrepareFailureRequest, GuardianPresetAdjustmentRequest,
    GuardianStartupFailureObservation, GuardianStartupFailureRequest,
    conservative_launch_recovery_preset, guardian_prelaunch_preset_adjustment_directive,
    guardian_prepare_failure_outcome, guardian_startup_failure_outcome,
    is_guardian_launch_crash_class,
};
pub use launch_failure_memory::{
    GuardianLaunchFailureMemoryIntakeRequest, launch_failure_memory_guardian_facts,
    record_launch_failure_observation,
};
pub use launch_recovery::{
    GuardianLaunchRecoveryCurrentIntent, GuardianLaunchRecoveryJournalTransition,
    GuardianLaunchRecoveryOutcome, GuardianLaunchRecoveryPlan, GuardianLaunchRecoveryPlanRejection,
    GuardianLaunchRecoveryPlanRequest, GuardianLaunchRecoveryRecordRequest,
    GuardianLaunchRecoveryStatus, launch_recovery_journal_transition_conflicts,
    launch_recovery_journal_transition_matches, launch_recovery_user_intent_fingerprint,
    plan_launch_recovery_directive, record_launch_recovery_attempt, record_launch_recovery_failure,
    record_launch_recovery_success,
};
pub use model::{
    ActionPlanPrerequisite, DiagnosisId, FactReliability, GuardianAction, GuardianActionKind,
    GuardianActionPlan, GuardianConfidence, GuardianDecision, GuardianDomain, GuardianFact,
    GuardianFactId, GuardianMode, GuardianSeverity, SafetyCase, SafetyOutcome,
};
pub use performance::{
    GuardianPerformanceOperationKind, GuardianPerformanceSupervisionPlan,
    GuardianPerformanceSupervisionRejection, GuardianPerformanceSupervisionRequest,
    performance_failure_memory_guardian_fact, performance_health_guardian_facts,
    performance_plan_guardian_facts, performance_rules_guardian_facts,
    performance_state_error_guardian_fact, plan_performance_supervision,
};
pub(super) use policy::PreflightAdmission;
pub use policy::{GuardianPolicyContext, decide_guardian_policy};
pub use preflight::{
    GuardianPreflightOutcome, GuardianPreflightOutcomeRequest, GuardianPreflightOverrideSignals,
    GuardianPreflightReadiness, GuardianPreflightResourceSignals, guardian_preflight_outcome,
};
pub use repair_plan::{
    GuardianRepairExecutor, GuardianRepairMutation, GuardianRepairPlan,
    GuardianRepairPlanRejection, GuardianRepairPlanningContext, GuardianRepairReversibility,
    GuardianRepairTask, GuardianRepairTaskKind, plan_launcher_managed_artifact_repair,
    plan_launcher_managed_missing_artifact_repair, plan_managed_runtime_ready_marker_repair,
};
pub use state_evidence::{GuardianStateLoadOutcome, persisted_state_load_guardian_outcome};
