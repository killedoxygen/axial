//! Guardian system boundary.
//!
//! Guardian owns safety facts, diagnosis, policy decisions, action planning,
//! supervised recovery, user outcomes, and bounded failure memory across
//! launch, install, runtime, and performance workflows.

mod artifact_repair;
mod component_rebuild;
mod copy;
mod directive;
mod healing;
mod integrity;
pub mod jvm_preset;
pub mod launch_decision;
pub mod launch_failure_memory;
pub mod launch_recovery;
pub mod performance;
pub(crate) mod persisted_state_repair;
pub mod policy;
pub mod preflight;
mod repair_authorization;
mod state_evidence;

#[cfg(test)]
mod decision_snapshot;
mod diagnosis;
mod facts;
#[cfg(test)]
mod invariant_coverage;
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
mod reconciliation_journal;
mod rules;

#[cfg(test)]
mod tests;

pub use artifact_repair::GuardianArtifactRepairStatus;
pub(crate) use artifact_repair::{
    GuardianArtifactRepairSettlement, execute_registered_guardian_artifact_repair,
};
pub(crate) use component_rebuild::{
    GuardianComponentRebuildOutcome, GuardianComponentRebuildStatus,
    execute_managed_assets_component_rebuild, execute_managed_libraries_component_rebuild,
    execute_managed_runtime_component_rebuild, execute_managed_version_bundle_component_rebuild,
};
#[cfg(test)]
pub(crate) use component_rebuild::{
    execute_failed_managed_assets_component_rebuild_for_test,
    execute_managed_assets_component_rebuild_fixture_for_test,
};
pub(crate) use copy::GuardianSummaryDecision;
pub(crate) use copy::{
    GuardianCopyRequest, GuardianInstallAssessment, GuardianInstallOutcomeMemoryPersistence,
    GuardianLaunchAdmission, GuardianRuntimeRepairCopy, assess_install_failure,
    author_guardian_copy, guardian_directive_description, guardian_failed_launch_recovery_log,
    guardian_install_outcome_from_terminal, guardian_launch_stage_evidence,
    guardian_proof_evidence, guardian_summary_from_admission,
    guardian_summary_from_persisted_export_value, guardian_summary_with_artifact_repair_outcome,
    guardian_summary_with_blocked_outcome, guardian_summary_with_intervention,
    guardian_summary_with_observed_outcome, guardian_summary_with_suppressed_outcome,
    launch_notice, launch_notice_from_values, launch_session_outcome,
};
pub use copy::{
    GuardianInstallOutcomeSummary, GuardianJvmPresetNotice, GuardianJvmPresetOption,
    GuardianSummary, GuardianUserOutcome, guardian_jvm_preset_notice, guardian_jvm_preset_options,
};
#[cfg(test)]
pub(crate) use copy::{
    guardian_launch_stage_evidence_for_test, guardian_summary_for_test,
    guardian_user_outcome_for_test,
};
pub use diagnosis::{
    Diagnosis, GuardianEvidenceAssessment, assess_operation_evidence, build_safety_case, diagnose,
};
pub use directive::{
    GuardianDirective, GuardianManagedJavaReason, GuardianPresetDowngradeReason,
    GuardianPresetValue, GuardianStripJvmArgsReason,
};
pub(crate) use directive::{GuardianRecoveryIntentAxis, GuardianRecoveryMetadata};
pub use facts::OperationEvidenceBatch;
#[cfg(test)]
pub(crate) fn guardian_fact_from_execution(
    fact: &crate::execution::ExecutionFact,
    phase: crate::state::contracts::OperationPhase,
) -> GuardianFact {
    let evidence = match fact.operation_id.as_ref() {
        Some(operation_id) => OperationEvidenceBatch::try_from_execution_operation(
            operation_id,
            phase,
            std::slice::from_ref(fact),
        ),
        None => {
            OperationEvidenceBatch::try_from_execution_unscoped(phase, std::slice::from_ref(fact))
        }
    }
    .expect("valid one-fact execution evidence batch");
    evidence.facts()[0].clone()
}
pub(crate) use healing::execute_managed_runtime_ready_marker_repair;
pub use healing::{GuardianRepairOutcome, GuardianRepairStatus};
pub(crate) use integrity::{
    Tier2IntegrityGuardianEvidence, Tier2RegisteredArtifactAssessment,
    assess_tier2_registered_artifact_repair, try_tier2_integrity_guardian_evidence,
};
pub use jvm_preset::{
    GuardianJvmPresetId, GuardianJvmPresetResolution, normalize_create_jvm_preset,
};
pub(crate) use launch_decision::user_mod_set_drift_fact;
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
pub(crate) use launch_recovery::resume_launch_recovery_attempt;
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
    ActionPlanPrerequisite, DiagnosisId, EvidenceScope, FactReliability, GuardianAction,
    GuardianActionKind, GuardianActionPlan, GuardianConfidence, GuardianDomain, GuardianFact,
    GuardianFactId, GuardianMode, GuardianSeverity, MAX_OPERATION_EVIDENCE_FACTS,
    OperationEvidenceBatchRejection, SafetyCase, SafetyOutcome,
};
pub use performance::{
    GuardianPerformanceOperationKind, GuardianPerformanceSupervisionPlan,
    GuardianPerformanceSupervisionRejection, GuardianPerformanceSupervisionRequest,
    performance_health_guardian_facts, performance_plan_guardian_facts,
    performance_rules_guardian_facts, performance_state_error_guardian_fact,
    plan_performance_supervision,
};
pub(super) use policy::PreflightAdmission;
#[cfg(test)]
pub(crate) use policy::with_guardian_policy_evaluation_count;
pub use policy::{GuardianDecision, GuardianPolicyContext, decide_guardian_policy};
#[cfg(test)]
pub(crate) use policy::{
    guardian_policy_evaluation_count_scope, with_guardian_policy_evaluation_count_scope,
};
#[cfg(test)]
pub use preflight::guardian_preflight_outcome;
pub use preflight::{
    GuardianPreflightOutcome, GuardianPreflightOutcomeRequest, GuardianPreflightOverrideSignals,
    GuardianPreflightReadiness, GuardianPreflightResourceSignals, try_guardian_preflight_outcome,
};
#[cfg(test)]
pub(crate) use repair_authorization::RepairAuthorizationRejection;
pub(crate) use repair_authorization::{
    ReadyMarkerRepairAuthorization, authorize_managed_runtime_ready_marker_repair,
};
pub(crate) use state_evidence::persisted_state_load_guardian_outcome;

#[cfg(test)]
pub(crate) fn unscoped_evidence_for_test(facts: &[GuardianFact]) -> OperationEvidenceBatch {
    OperationEvidenceBatch::try_from_guardian_unscoped(facts)
        .expect("test Guardian evidence must be bounded and unscoped")
}
