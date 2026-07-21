//! Guardian runtime repair authorization.
//!
//! This module is the only authority that can discharge ready-marker repair
//! policy gates. Executors consume the resulting capability by value.

use super::{
    DiagnosisId, GuardianAction, GuardianActionKind, GuardianActionPlan, GuardianConfidence,
    GuardianDecision, GuardianMode,
};
use crate::state::contracts::{OwnershipClass, StabilizationSystem, TargetDescriptor, TargetKind};

#[cfg(test)]
const REGISTERED_ARTIFACT_REPAIR_MAX_ATTEMPTS: u32 = 1;
const RUNTIME_REPAIR_MAX_ATTEMPTS: u32 = 1;

#[cfg(test)]
pub(super) fn repair_hand_coverage() -> [(&'static str, DiagnosisId, u32); 2] {
    [
        (
            "registered_artifact",
            DiagnosisId::LauncherManagedArtifactCorrupt,
            REGISTERED_ARTIFACT_REPAIR_MAX_ATTEMPTS,
        ),
        (
            "ready_marker",
            DiagnosisId::ManagedRuntimeCorrupt,
            RUNTIME_REPAIR_MAX_ATTEMPTS,
        ),
    ]
}

pub(crate) struct ReadyMarkerRepairAuthorization {
    diagnosis_id: DiagnosisId,
    target: TargetDescriptor,
    ownership: OwnershipClass,
    action: GuardianActionKind,
    max_attempts: u32,
}

pub(crate) struct ReadyMarkerRepairAuthorizationParts {
    pub diagnosis_id: DiagnosisId,
    pub target: TargetDescriptor,
    pub ownership: OwnershipClass,
    pub action: GuardianActionKind,
    pub max_attempts: u32,
}

impl ReadyMarkerRepairAuthorization {
    pub(super) fn into_parts(self) -> ReadyMarkerRepairAuthorizationParts {
        ReadyMarkerRepairAuthorizationParts {
            diagnosis_id: self.diagnosis_id,
            target: self.target,
            ownership: self.ownership,
            action: self.action,
            max_attempts: self.max_attempts,
        }
    }

    #[cfg(test)]
    fn parts(&self) -> (&DiagnosisId, &TargetDescriptor, u32) {
        (&self.diagnosis_id, &self.target, self.max_attempts)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RepairAuthorizationRejection {
    NonRepairDecision,
    MissingActionPlan,
    UnsupportedDiagnosis,
    MissingTarget,
    UnsafeOwnership,
    UnsafePublicBoundary,
}

pub(crate) fn authorize_managed_runtime_ready_marker_repair(
    decision: &GuardianDecision,
) -> Result<ReadyMarkerRepairAuthorization, RepairAuthorizationRejection> {
    if decision.kind() != GuardianActionKind::Repair || decision.mode() != GuardianMode::Managed {
        return Err(RepairAuthorizationRejection::NonRepairDecision);
    }

    let plan = decision
        .action_plan()
        .ok_or(RepairAuthorizationRejection::MissingActionPlan)?;
    let (diagnosis_id, action) = supported_runtime_ready_marker_repair(decision, plan)?;
    let target = action
        .target
        .clone()
        .or_else(|| plan.prerequisite.affected_targets.first().cloned())
        .ok_or(RepairAuthorizationRejection::MissingTarget)?;
    let target = public_safe_target(&target)?;

    if target.ownership != OwnershipClass::LauncherManaged {
        return Err(RepairAuthorizationRejection::UnsafeOwnership);
    }
    if target.system != StabilizationSystem::Execution || target.kind != TargetKind::Runtime {
        return Err(RepairAuthorizationRejection::UnsupportedDiagnosis);
    }

    Ok(ReadyMarkerRepairAuthorization {
        diagnosis_id,
        ownership: target.ownership,
        action: action.kind,
        max_attempts: RUNTIME_REPAIR_MAX_ATTEMPTS,
        target,
    })
}

fn supported_runtime_ready_marker_repair<'a>(
    decision: &GuardianDecision,
    plan: &'a GuardianActionPlan,
) -> Result<(DiagnosisId, &'a GuardianAction), RepairAuthorizationRejection> {
    if plan.owner != StabilizationSystem::Guardian
        || plan.prerequisite.diagnosis_id != DiagnosisId::ManagedRuntimeCorrupt
        || !decision
            .diagnoses()
            .contains(&DiagnosisId::ManagedRuntimeCorrupt)
        || !plan
            .prerequisite
            .candidate_actions
            .contains(&GuardianActionKind::Repair)
        || !repair_confidence_is_sufficient(plan.prerequisite.confidence)
    {
        return Err(RepairAuthorizationRejection::UnsupportedDiagnosis);
    }
    let action = repair_action(plan)?;
    if action.reason != plan.prerequisite.diagnosis_id {
        return Err(RepairAuthorizationRejection::UnsupportedDiagnosis);
    }
    Ok((plan.prerequisite.diagnosis_id, action))
}

fn repair_confidence_is_sufficient(confidence: GuardianConfidence) -> bool {
    matches!(
        confidence,
        GuardianConfidence::Confirmed | GuardianConfidence::Certain
    )
}

fn repair_action(
    plan: &GuardianActionPlan,
) -> Result<&GuardianAction, RepairAuthorizationRejection> {
    plan.actions
        .iter()
        .find(|action| action.kind == GuardianActionKind::Repair)
        .ok_or(RepairAuthorizationRejection::MissingTarget)
}

fn public_safe_target(
    target: &TargetDescriptor,
) -> Result<TargetDescriptor, RepairAuthorizationRejection> {
    let sanitized = TargetDescriptor::new(target.system, target.kind, &target.id, target.ownership);
    if sanitized.id != target.id {
        return Err(RepairAuthorizationRejection::UnsafePublicBoundary);
    }
    Ok(sanitized)
}

#[cfg(test)]
mod tests {
    use super::{RepairAuthorizationRejection, authorize_managed_runtime_ready_marker_repair};
    use crate::guardian::{
        ActionPlanPrerequisite, DiagnosisId, GuardianAction, GuardianActionKind,
        GuardianActionPlan, GuardianConfidence, GuardianDecision, GuardianMode,
    };
    use crate::state::contracts::{
        OperationId, OwnershipClass, StabilizationSystem, TargetDescriptor, TargetKind,
    };

    trait ExpectErrorWithoutDebug<E> {
        fn expect_error_without_debug(self, message: &str) -> E;
    }

    impl<T, E> ExpectErrorWithoutDebug<E> for Result<T, E> {
        fn expect_error_without_debug(self, message: &str) -> E {
            match self {
                Ok(_) => panic!("{message}"),
                Err(error) => error,
            }
        }
    }

    #[test]
    fn ready_marker_authorization_carries_exact_bounded_capability() {
        let authorization = authorize_managed_runtime_ready_marker_repair(
            &runtime_repair_decision(OwnershipClass::LauncherManaged),
        )
        .expect("ready-marker authorization");
        let (diagnosis_id, target, max_attempts) = authorization.parts();

        assert_eq!(diagnosis_id, &DiagnosisId::ManagedRuntimeCorrupt);
        assert_eq!(target.kind, TargetKind::Runtime);
        assert_eq!(target.system, StabilizationSystem::Execution);
        assert_eq!(max_attempts, 1);
    }

    #[test]
    fn custom_mode_never_mints_ready_marker_capability() {
        let runtime = runtime_repair_decision(OwnershipClass::LauncherManaged);
        let runtime = GuardianDecision::for_test(
            runtime.operation_id().cloned(),
            GuardianMode::Custom,
            runtime.kind(),
            runtime.diagnoses().to_vec(),
            runtime.action_plan().cloned(),
        );
        assert_eq!(
            authorize_managed_runtime_ready_marker_repair(&runtime)
                .expect_error_without_debug("Custom runtime repair must remain offer-only"),
            RepairAuthorizationRejection::NonRepairDecision,
        );
    }

    #[test]
    fn ready_marker_authorization_rejects_mode_confidence_reason_owner_and_target_shape() {
        let decision = runtime_repair_decision(OwnershipClass::LauncherManaged);
        let disabled = GuardianDecision::for_test(
            decision.operation_id().cloned(),
            GuardianMode::Disabled,
            decision.kind(),
            decision.diagnoses().to_vec(),
            decision.action_plan().cloned(),
        );
        assert_eq!(
            authorize_managed_runtime_ready_marker_repair(&disabled)
                .expect_error_without_debug("disabled mode"),
            RepairAuthorizationRejection::NonRepairDecision
        );

        let decision = runtime_repair_decision(OwnershipClass::LauncherManaged);
        let mut action_plan = decision.action_plan().cloned().expect("plan");
        action_plan.prerequisite.confidence = GuardianConfidence::Low;
        let low_confidence = GuardianDecision::for_test(
            decision.operation_id().cloned(),
            decision.mode(),
            decision.kind(),
            decision.diagnoses().to_vec(),
            Some(action_plan),
        );
        assert_eq!(
            authorize_managed_runtime_ready_marker_repair(&low_confidence)
                .expect_error_without_debug("low confidence"),
            RepairAuthorizationRejection::UnsupportedDiagnosis
        );

        let decision = runtime_repair_decision(OwnershipClass::LauncherManaged);
        let mut action_plan = decision.action_plan().cloned().expect("plan");
        action_plan.actions[0].reason = DiagnosisId::ManagedRuntimeMissing;
        let wrong_reason = GuardianDecision::for_test(
            decision.operation_id().cloned(),
            decision.mode(),
            decision.kind(),
            decision.diagnoses().to_vec(),
            Some(action_plan),
        );
        assert_eq!(
            authorize_managed_runtime_ready_marker_repair(&wrong_reason)
                .expect_error_without_debug("wrong action reason"),
            RepairAuthorizationRejection::UnsupportedDiagnosis
        );

        let decision = runtime_repair_decision(OwnershipClass::LauncherManaged);
        let mut action_plan = decision.action_plan().cloned().expect("plan");
        action_plan.owner = StabilizationSystem::Application;
        let wrong_owner = GuardianDecision::for_test(
            decision.operation_id().cloned(),
            decision.mode(),
            decision.kind(),
            decision.diagnoses().to_vec(),
            Some(action_plan),
        );
        assert_eq!(
            authorize_managed_runtime_ready_marker_repair(&wrong_owner)
                .expect_error_without_debug("wrong action-plan owner"),
            RepairAuthorizationRejection::UnsupportedDiagnosis
        );

        for ownership in [OwnershipClass::UserOwned, OwnershipClass::Unknown] {
            assert_eq!(
                authorize_managed_runtime_ready_marker_repair(&runtime_repair_decision(ownership))
                    .expect_error_without_debug("unsafe runtime ownership"),
                RepairAuthorizationRejection::UnsafeOwnership
            );
        }

        let unsupported = runtime_repair_decision_for_target(TargetDescriptor::new(
            StabilizationSystem::Guardian,
            TargetKind::Runtime,
            "java_runtime_delta",
            OwnershipClass::LauncherManaged,
        ));
        assert_eq!(
            authorize_managed_runtime_ready_marker_repair(&unsupported)
                .expect_error_without_debug("unsupported runtime target"),
            RepairAuthorizationRejection::UnsupportedDiagnosis
        );
    }

    fn runtime_repair_decision(ownership: OwnershipClass) -> GuardianDecision {
        runtime_repair_decision_for_target(TargetDescriptor::new(
            StabilizationSystem::Execution,
            TargetKind::Runtime,
            "java_runtime_delta",
            ownership,
        ))
    }

    fn runtime_repair_decision_for_target(target: TargetDescriptor) -> GuardianDecision {
        let ownership = target.ownership;
        let diagnosis_id = DiagnosisId::ManagedRuntimeCorrupt;
        GuardianDecision::for_test(
            Some(OperationId::deterministic_test(format!("operation-runtime-{ownership:?}"))),
            GuardianMode::Managed,
            GuardianActionKind::Repair,
            vec![diagnosis_id],
            Some(GuardianActionPlan::new(
                StabilizationSystem::Guardian,
                ActionPlanPrerequisite {
                    diagnosis_id,
                    ownership,
                    confidence: GuardianConfidence::Confirmed,
                    affected_targets: vec![target.clone()],
                    candidate_actions: vec![GuardianActionKind::Repair, GuardianActionKind::Block],
                },
                vec![GuardianAction {
                    kind: GuardianActionKind::Repair,
                    target: Some(target),
                    reason: diagnosis_id,
                }],
            )),
        )
    }
}
