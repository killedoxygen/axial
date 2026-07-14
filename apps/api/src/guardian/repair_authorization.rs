//! Guardian repair authorization.
//!
//! This module is the only authority that can discharge repair policy gates.
//! Executors consume the resulting kind-typed capability by value.

use super::{
    DiagnosisId, GuardianAction, GuardianActionKind, GuardianActionPlan, GuardianConfidence,
    GuardianDecision, GuardianMinecraftArtifactRepairDescriptor, GuardianMode,
};
use crate::state::contracts::{OwnershipClass, StabilizationSystem, TargetDescriptor, TargetKind};

const ARTIFACT_REPAIR_MAX_ATTEMPTS: u32 = 1;
const RUNTIME_REPAIR_MAX_ATTEMPTS: u32 = 1;

#[cfg(test)]
pub(super) fn repair_hand_coverage() -> [(&'static str, DiagnosisId, u32); 3] {
    [
        (
            QuarantineRedownload::ID,
            DiagnosisId::LauncherManagedArtifactCorrupt,
            ARTIFACT_REPAIR_MAX_ATTEMPTS,
        ),
        (
            MissingDownload::ID,
            DiagnosisId::LauncherManagedArtifactCorrupt,
            ARTIFACT_REPAIR_MAX_ATTEMPTS,
        ),
        (
            ReadyMarker::ID,
            DiagnosisId::ManagedRuntimeCorrupt,
            RUNTIME_REPAIR_MAX_ATTEMPTS,
        ),
    ]
}

pub(crate) struct QuarantineRedownload {
    descriptor: GuardianMinecraftArtifactRepairDescriptor,
}

pub(crate) struct MissingDownload {
    descriptor: GuardianMinecraftArtifactRepairDescriptor,
}

pub(crate) struct ReadyMarker {
    _private: (),
}

impl QuarantineRedownload {
    #[cfg(test)]
    const ID: &'static str = "quarantine_redownload";
}

impl MissingDownload {
    #[cfg(test)]
    const ID: &'static str = "missing_download";
}

impl ReadyMarker {
    #[cfg(test)]
    const ID: &'static str = "ready_marker";
}

mod sealed {
    pub trait ArtifactRepairKind {}
}

pub(crate) trait ArtifactRepairKind: sealed::ArtifactRepairKind {
    const QUARANTINES_EXISTING: bool;
    fn descriptor(&self) -> &GuardianMinecraftArtifactRepairDescriptor;
    fn into_descriptor(self) -> GuardianMinecraftArtifactRepairDescriptor;
}

impl sealed::ArtifactRepairKind for QuarantineRedownload {}
impl ArtifactRepairKind for QuarantineRedownload {
    const QUARANTINES_EXISTING: bool = true;

    fn descriptor(&self) -> &GuardianMinecraftArtifactRepairDescriptor {
        &self.descriptor
    }

    fn into_descriptor(self) -> GuardianMinecraftArtifactRepairDescriptor {
        self.descriptor
    }
}

impl sealed::ArtifactRepairKind for MissingDownload {}
impl ArtifactRepairKind for MissingDownload {
    const QUARANTINES_EXISTING: bool = false;

    fn descriptor(&self) -> &GuardianMinecraftArtifactRepairDescriptor {
        &self.descriptor
    }

    fn into_descriptor(self) -> GuardianMinecraftArtifactRepairDescriptor {
        self.descriptor
    }
}

pub(crate) struct RepairAuthorization<K> {
    diagnosis_id: DiagnosisId,
    target: TargetDescriptor,
    ownership: OwnershipClass,
    mode: GuardianMode,
    action: GuardianActionKind,
    max_attempts: u32,
    kind: K,
}

pub(crate) struct RepairAuthorizationParts<K> {
    pub diagnosis_id: DiagnosisId,
    pub target: TargetDescriptor,
    pub ownership: OwnershipClass,
    pub mode: GuardianMode,
    pub action: GuardianActionKind,
    pub max_attempts: u32,
    pub kind: K,
}

impl<K> RepairAuthorization<K> {
    pub(super) fn into_parts(self) -> RepairAuthorizationParts<K> {
        RepairAuthorizationParts {
            diagnosis_id: self.diagnosis_id,
            target: self.target,
            ownership: self.ownership,
            mode: self.mode,
            action: self.action,
            max_attempts: self.max_attempts,
            kind: self.kind,
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
    DescriptorTargetMismatch,
}

pub(crate) fn authorize_launcher_managed_artifact_repair(
    decision: &GuardianDecision,
    descriptor: GuardianMinecraftArtifactRepairDescriptor,
) -> Result<RepairAuthorization<QuarantineRedownload>, RepairAuthorizationRejection> {
    authorize_artifact_repair(decision, QuarantineRedownload { descriptor })
}

pub(crate) fn authorize_launcher_managed_missing_artifact_repair(
    decision: &GuardianDecision,
    descriptor: GuardianMinecraftArtifactRepairDescriptor,
) -> Result<RepairAuthorization<MissingDownload>, RepairAuthorizationRejection> {
    authorize_artifact_repair(decision, MissingDownload { descriptor })
}

pub(crate) fn authorize_managed_runtime_ready_marker_repair(
    decision: &GuardianDecision,
) -> Result<RepairAuthorization<ReadyMarker>, RepairAuthorizationRejection> {
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

    Ok(RepairAuthorization {
        diagnosis_id,
        ownership: target.ownership,
        mode: decision.mode(),
        action: action.kind,
        max_attempts: RUNTIME_REPAIR_MAX_ATTEMPTS,
        target,
        kind: ReadyMarker { _private: () },
    })
}

fn authorize_artifact_repair<K>(
    decision: &GuardianDecision,
    kind: K,
) -> Result<RepairAuthorization<K>, RepairAuthorizationRejection>
where
    K: ArtifactRepairKind,
{
    if decision.kind() != GuardianActionKind::Repair || decision.mode() != GuardianMode::Managed {
        return Err(RepairAuthorizationRejection::NonRepairDecision);
    }

    let plan = decision
        .action_plan()
        .ok_or(RepairAuthorizationRejection::MissingActionPlan)?;
    let diagnosis_id = supported_artifact_diagnosis(decision, plan)?;
    let action = repair_action(plan)?;
    let target = action
        .target
        .clone()
        .or_else(|| plan.prerequisite.affected_targets.first().cloned())
        .ok_or(RepairAuthorizationRejection::MissingTarget)?;
    let target = public_safe_target(&target)?;

    if target.ownership != OwnershipClass::LauncherManaged {
        return Err(RepairAuthorizationRejection::UnsafeOwnership);
    }
    if !matches!(target.kind, TargetKind::Artifact | TargetKind::Version) {
        return Err(RepairAuthorizationRejection::UnsupportedDiagnosis);
    }
    if kind.descriptor().target() != &target {
        return Err(RepairAuthorizationRejection::DescriptorTargetMismatch);
    }

    Ok(RepairAuthorization {
        diagnosis_id,
        ownership: target.ownership,
        mode: decision.mode(),
        action: action.kind,
        max_attempts: ARTIFACT_REPAIR_MAX_ATTEMPTS,
        target,
        kind,
    })
}

fn supported_artifact_diagnosis(
    decision: &GuardianDecision,
    plan: &GuardianActionPlan,
) -> Result<DiagnosisId, RepairAuthorizationRejection> {
    if plan.prerequisite.diagnosis_id != DiagnosisId::LauncherManagedArtifactCorrupt
        || !decision
            .diagnoses()
            .contains(&DiagnosisId::LauncherManagedArtifactCorrupt)
        || !plan
            .prerequisite
            .candidate_actions
            .contains(&GuardianActionKind::Repair)
    {
        return Err(RepairAuthorizationRejection::UnsupportedDiagnosis);
    }
    Ok(plan.prerequisite.diagnosis_id)
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
    use super::{
        RepairAuthorizationRejection, authorize_launcher_managed_artifact_repair,
        authorize_launcher_managed_missing_artifact_repair,
        authorize_managed_runtime_ready_marker_repair,
    };
    use crate::guardian::{
        ActionPlanPrerequisite, DiagnosisId, GuardianAction, GuardianActionKind,
        GuardianActionPlan, GuardianConfidence, GuardianDecision,
        GuardianMinecraftArtifactRepairDescriptor, GuardianMode,
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
    fn artifact_authorizations_are_bounded_and_kind_typed() {
        let decision = repair_decision(OwnershipClass::LauncherManaged);
        let quarantine = authorize_launcher_managed_artifact_repair(
            &decision,
            artifact_descriptor("minecraft_client_1_21_5"),
        )
        .expect("quarantine-redownload authorization");
        let missing = authorize_launcher_managed_missing_artifact_repair(
            &decision,
            artifact_descriptor("minecraft_client_1_21_5"),
        )
        .expect("missing-download authorization");

        for authorization in [quarantine.parts(), missing.parts()] {
            assert_eq!(
                authorization.0,
                &DiagnosisId::LauncherManagedArtifactCorrupt
            );
            assert_eq!(authorization.1.ownership, OwnershipClass::LauncherManaged);
            assert_eq!(authorization.1.system, StabilizationSystem::Execution);
            assert_eq!(authorization.2, 1);
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
    fn custom_mode_repair_decisions_never_mint_executor_capabilities() {
        let artifact = repair_decision(OwnershipClass::LauncherManaged);
        let artifact = GuardianDecision::for_test(
            artifact.operation_id().cloned(),
            GuardianMode::Custom,
            artifact.kind(),
            artifact.diagnoses().to_vec(),
            artifact.action_plan().cloned(),
        );
        assert_eq!(
            authorize_launcher_managed_artifact_repair(
                &artifact,
                artifact_descriptor("minecraft_client_1_21_5"),
            )
            .expect_error_without_debug("Custom artifact repair must remain offer-only"),
            RepairAuthorizationRejection::NonRepairDecision,
        );

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
    fn authorization_rejects_non_repair_missing_plan_diagnosis_action_and_target() {
        let decision = repair_decision(OwnershipClass::LauncherManaged);
        let non_repair = GuardianDecision::for_test(
            decision.operation_id().cloned(),
            decision.mode(),
            GuardianActionKind::Block,
            decision.diagnoses().to_vec(),
            decision.action_plan().cloned(),
        );
        assert_eq!(
            authorize_launcher_managed_artifact_repair(
                &non_repair,
                artifact_descriptor("minecraft_client_1_21_5"),
            )
            .expect_error_without_debug("non-repair decision"),
            RepairAuthorizationRejection::NonRepairDecision
        );

        let decision = repair_decision(OwnershipClass::LauncherManaged);
        let missing_plan = GuardianDecision::for_test(
            decision.operation_id().cloned(),
            decision.mode(),
            decision.kind(),
            decision.diagnoses().to_vec(),
            None,
        );
        assert_eq!(
            authorize_launcher_managed_artifact_repair(
                &missing_plan,
                artifact_descriptor("minecraft_client_1_21_5"),
            )
            .expect_error_without_debug("missing action plan"),
            RepairAuthorizationRejection::MissingActionPlan
        );

        let decision = repair_decision(OwnershipClass::LauncherManaged);
        let unsupported = GuardianDecision::for_test(
            decision.operation_id().cloned(),
            decision.mode(),
            decision.kind(),
            vec![DiagnosisId::ManagedRuntimeCorrupt],
            decision.action_plan().cloned(),
        );
        assert_eq!(
            authorize_launcher_managed_artifact_repair(
                &unsupported,
                artifact_descriptor("minecraft_client_1_21_5"),
            )
            .expect_error_without_debug("unsupported diagnosis"),
            RepairAuthorizationRejection::UnsupportedDiagnosis
        );

        let decision = repair_decision(OwnershipClass::LauncherManaged);
        let mut action_plan = decision.action_plan().cloned().expect("plan");
        action_plan.actions.clear();
        let missing_action = GuardianDecision::for_test(
            decision.operation_id().cloned(),
            decision.mode(),
            decision.kind(),
            decision.diagnoses().to_vec(),
            Some(action_plan),
        );
        assert_eq!(
            authorize_launcher_managed_artifact_repair(
                &missing_action,
                artifact_descriptor("minecraft_client_1_21_5"),
            )
            .expect_error_without_debug("missing repair action"),
            RepairAuthorizationRejection::MissingTarget
        );

        let decision = repair_decision(OwnershipClass::LauncherManaged);
        let mut action_plan = decision.action_plan().cloned().expect("plan");
        action_plan.actions[0].target = None;
        action_plan.prerequisite.affected_targets.clear();
        let missing_target = GuardianDecision::for_test(
            decision.operation_id().cloned(),
            decision.mode(),
            decision.kind(),
            decision.diagnoses().to_vec(),
            Some(action_plan),
        );
        assert_eq!(
            authorize_launcher_managed_artifact_repair(
                &missing_target,
                artifact_descriptor("minecraft_client_1_21_5"),
            )
            .expect_error_without_debug("missing target"),
            RepairAuthorizationRejection::MissingTarget
        );
    }

    #[test]
    fn authorization_rejects_unsafe_ownership_target_shape_and_target_text() {
        for ownership in [OwnershipClass::UserOwned, OwnershipClass::Unknown] {
            assert_eq!(
                authorize_launcher_managed_artifact_repair(
                    &repair_decision(ownership),
                    artifact_descriptor("minecraft_client_1_21_5"),
                )
                .expect_error_without_debug("unsafe ownership"),
                RepairAuthorizationRejection::UnsafeOwnership
            );
        }

        let unsafe_target = repair_decision_for_target(TargetDescriptor {
            system: StabilizationSystem::Execution,
            kind: TargetKind::Artifact,
            id: r"C:\Users\Alice\.minecraft\libraries\bad.jar token=secret -Xmx8192M".to_string(),
            ownership: OwnershipClass::LauncherManaged,
        });
        assert_eq!(
            authorize_launcher_managed_artifact_repair(
                &unsafe_target,
                artifact_descriptor("minecraft_client_1_21_5"),
            )
            .expect_error_without_debug("unsafe target text"),
            RepairAuthorizationRejection::UnsafePublicBoundary
        );
    }

    #[test]
    fn authorization_rejects_a_descriptor_for_another_effect_target() {
        let decision = repair_decision(OwnershipClass::LauncherManaged);
        let error = authorize_launcher_managed_artifact_repair(
            &decision,
            artifact_descriptor("minecraft_client_other"),
        )
        .expect_error_without_debug("descriptor target mismatch");

        assert_eq!(
            error,
            RepairAuthorizationRejection::DescriptorTargetMismatch
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
                authorize_managed_runtime_ready_marker_repair(&runtime_repair_decision(ownership),)
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

    fn repair_decision(ownership: OwnershipClass) -> GuardianDecision {
        repair_decision_for_target(TargetDescriptor::new(
            StabilizationSystem::Execution,
            TargetKind::Artifact,
            "minecraft_client_1_21_5",
            ownership,
        ))
    }

    fn repair_decision_for_target(target: TargetDescriptor) -> GuardianDecision {
        let ownership = target.ownership;
        GuardianDecision::for_test(
            Some(OperationId::new("operation-install-repair")),
            GuardianMode::Managed,
            GuardianActionKind::Repair,
            vec![DiagnosisId::LauncherManagedArtifactCorrupt],
            Some(GuardianActionPlan::new(
                StabilizationSystem::Guardian,
                ActionPlanPrerequisite {
                    diagnosis_id: DiagnosisId::LauncherManagedArtifactCorrupt,
                    ownership,
                    confidence: GuardianConfidence::Confirmed,
                    affected_targets: vec![target.clone()],
                    candidate_actions: vec![
                        GuardianActionKind::Quarantine,
                        GuardianActionKind::Repair,
                        GuardianActionKind::Block,
                    ],
                },
                vec![GuardianAction {
                    kind: GuardianActionKind::Repair,
                    target: Some(target),
                    reason: DiagnosisId::LauncherManagedArtifactCorrupt,
                }],
            )),
        )
    }

    fn artifact_descriptor(target_id: &str) -> GuardianMinecraftArtifactRepairDescriptor {
        GuardianMinecraftArtifactRepairDescriptor::for_test(
            TargetDescriptor::new(
                StabilizationSystem::Execution,
                TargetKind::Artifact,
                target_id,
                OwnershipClass::LauncherManaged,
            ),
            std::path::Path::new("/tmp/axial-test-artifact.jar"),
            "https://example.invalid/artifact.jar",
            "sha1",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            Some(128),
            1024,
        )
        .expect("valid test descriptor")
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
            Some(OperationId::new(format!("operation-runtime-{ownership:?}"))),
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
