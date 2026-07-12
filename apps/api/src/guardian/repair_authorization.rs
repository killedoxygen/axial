//! Guardian repair authorization.
//!
//! This module is the only authority that can discharge repair policy gates.
//! Executors consume the resulting kind-typed capability by value.

use super::{
    DiagnosisId, GuardianAction, GuardianActionKind, GuardianActionPlan, GuardianConfidence,
    GuardianDecision, GuardianMinecraftArtifactRepairDescriptor, GuardianMode,
};
use crate::state::contracts::{OwnershipClass, StabilizationSystem, TargetDescriptor, TargetKind};
use crate::state::failure_memory::FailureMemoryKey;

const ARTIFACT_REPAIR_MAX_ATTEMPTS: u32 = 1;
const RUNTIME_REPAIR_MAX_ATTEMPTS: u32 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RepairAuthorizationContext {
    journal_available: bool,
    executor_available: bool,
    suppression_active: bool,
    public_redaction_ready: bool,
}

impl RepairAuthorizationContext {
    pub fn current_operation() -> Self {
        Self {
            journal_available: true,
            executor_available: true,
            suppression_active: false,
            public_redaction_ready: true,
        }
    }

    #[cfg(test)]
    fn with_missing_journal(mut self) -> Self {
        self.journal_available = false;
        self
    }

    #[cfg(test)]
    fn with_missing_executor(mut self) -> Self {
        self.executor_available = false;
        self
    }

    #[cfg(test)]
    fn with_suppression(mut self) -> Self {
        self.suppression_active = true;
        self
    }

    #[cfg(test)]
    fn with_unredacted_public_boundary(mut self) -> Self {
        self.public_redaction_ready = false;
        self
    }
}

impl Default for RepairAuthorizationContext {
    fn default() -> Self {
        Self::current_operation()
    }
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
    suppression_key: FailureMemoryKey,
    kind: K,
}

pub(crate) struct RepairAuthorizationParts<K> {
    pub diagnosis_id: DiagnosisId,
    pub target: TargetDescriptor,
    pub ownership: OwnershipClass,
    pub mode: GuardianMode,
    pub action: GuardianActionKind,
    pub max_attempts: u32,
    pub suppression_key: FailureMemoryKey,
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
            suppression_key: self.suppression_key,
            kind: self.kind,
        }
    }

    #[cfg(test)]
    fn parts(&self) -> (&DiagnosisId, &TargetDescriptor, u32, &FailureMemoryKey) {
        (
            &self.diagnosis_id,
            &self.target,
            self.max_attempts,
            &self.suppression_key,
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RepairAuthorizationRejection {
    NonRepairDecision,
    MissingActionPlan,
    UnsupportedDiagnosis,
    MissingTarget,
    UnsafeOwnership,
    MissingJournal,
    MissingExecutorCapability,
    Suppressed,
    UnsafePublicBoundary,
    DescriptorTargetMismatch,
}

pub(crate) fn authorize_launcher_managed_artifact_repair(
    decision: &GuardianDecision,
    context: RepairAuthorizationContext,
    descriptor: GuardianMinecraftArtifactRepairDescriptor,
) -> Result<RepairAuthorization<QuarantineRedownload>, RepairAuthorizationRejection> {
    authorize_artifact_repair(decision, context, QuarantineRedownload { descriptor })
}

pub(crate) fn authorize_launcher_managed_missing_artifact_repair(
    decision: &GuardianDecision,
    context: RepairAuthorizationContext,
    descriptor: GuardianMinecraftArtifactRepairDescriptor,
) -> Result<RepairAuthorization<MissingDownload>, RepairAuthorizationRejection> {
    authorize_artifact_repair(decision, context, MissingDownload { descriptor })
}

pub(crate) fn authorize_managed_runtime_ready_marker_repair(
    decision: &GuardianDecision,
    context: RepairAuthorizationContext,
) -> Result<RepairAuthorization<ReadyMarker>, RepairAuthorizationRejection> {
    authorize_context(context)?;
    if decision.kind != GuardianActionKind::Repair || decision.mode == GuardianMode::Disabled {
        return Err(RepairAuthorizationRejection::NonRepairDecision);
    }

    let plan = decision
        .action_plan
        .as_ref()
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
        mode: decision.mode,
        action: action.kind,
        max_attempts: RUNTIME_REPAIR_MAX_ATTEMPTS,
        suppression_key: FailureMemoryKey::for_observation(
            super::GuardianDomain::Runtime,
            &diagnosis_id,
            &target,
            decision.mode,
            None,
        ),
        target,
        kind: ReadyMarker { _private: () },
    })
}

fn authorize_artifact_repair<K>(
    decision: &GuardianDecision,
    context: RepairAuthorizationContext,
    kind: K,
) -> Result<RepairAuthorization<K>, RepairAuthorizationRejection>
where
    K: ArtifactRepairKind,
{
    authorize_context(context)?;
    if decision.kind != GuardianActionKind::Repair {
        return Err(RepairAuthorizationRejection::NonRepairDecision);
    }

    let plan = decision
        .action_plan
        .as_ref()
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
        mode: decision.mode,
        action: action.kind,
        max_attempts: ARTIFACT_REPAIR_MAX_ATTEMPTS,
        suppression_key: FailureMemoryKey::for_observation(
            super::GuardianDomain::Install,
            &diagnosis_id,
            &target,
            decision.mode,
            None,
        ),
        target,
        kind,
    })
}

fn authorize_context(
    context: RepairAuthorizationContext,
) -> Result<(), RepairAuthorizationRejection> {
    if !context.public_redaction_ready {
        return Err(RepairAuthorizationRejection::UnsafePublicBoundary);
    }
    if !context.journal_available {
        return Err(RepairAuthorizationRejection::MissingJournal);
    }
    if context.suppression_active {
        return Err(RepairAuthorizationRejection::Suppressed);
    }
    if !context.executor_available {
        return Err(RepairAuthorizationRejection::MissingExecutorCapability);
    }
    Ok(())
}

fn supported_artifact_diagnosis(
    decision: &GuardianDecision,
    plan: &GuardianActionPlan,
) -> Result<DiagnosisId, RepairAuthorizationRejection> {
    if plan.prerequisite.diagnosis_id != DiagnosisId::LauncherManagedArtifactCorrupt
        || !decision
            .diagnoses
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
            .diagnoses
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
        RepairAuthorizationContext, RepairAuthorizationRejection,
        authorize_launcher_managed_artifact_repair,
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
            RepairAuthorizationContext::current_operation(),
            artifact_descriptor("minecraft_client_1_21_5"),
        )
        .expect("quarantine-redownload authorization");
        let missing = authorize_launcher_managed_missing_artifact_repair(
            &decision,
            RepairAuthorizationContext::current_operation(),
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
            assert_eq!(
                authorization.3.as_str(),
                "Install:launcher_managed_artifact_corrupt:Execution.Artifact.minecraft_client_1_21_5:Managed:no_intent"
            );
        }
    }

    #[test]
    fn ready_marker_authorization_carries_exact_bounded_capability() {
        let authorization = authorize_managed_runtime_ready_marker_repair(
            &runtime_repair_decision(OwnershipClass::LauncherManaged),
            RepairAuthorizationContext::current_operation(),
        )
        .expect("ready-marker authorization");
        let (diagnosis_id, target, max_attempts, suppression_key) = authorization.parts();

        assert_eq!(diagnosis_id, &DiagnosisId::ManagedRuntimeCorrupt);
        assert_eq!(target.kind, TargetKind::Runtime);
        assert_eq!(target.system, StabilizationSystem::Execution);
        assert_eq!(max_attempts, 1);
        assert_eq!(
            suppression_key.as_str(),
            "Runtime:managed_runtime_corrupt:Execution.Runtime.java_runtime_delta:Managed:no_intent"
        );
    }

    #[test]
    fn authorization_rejects_missing_journal_executor_suppression_and_public_boundary() {
        let decision = repair_decision(OwnershipClass::LauncherManaged);
        for (context, expected) in [
            (
                RepairAuthorizationContext::current_operation().with_missing_journal(),
                RepairAuthorizationRejection::MissingJournal,
            ),
            (
                RepairAuthorizationContext::current_operation().with_missing_executor(),
                RepairAuthorizationRejection::MissingExecutorCapability,
            ),
            (
                RepairAuthorizationContext::current_operation().with_suppression(),
                RepairAuthorizationRejection::Suppressed,
            ),
            (
                RepairAuthorizationContext::current_operation().with_unredacted_public_boundary(),
                RepairAuthorizationRejection::UnsafePublicBoundary,
            ),
        ] {
            assert_eq!(
                authorize_launcher_managed_artifact_repair(
                    &decision,
                    context,
                    artifact_descriptor("minecraft_client_1_21_5"),
                )
                .expect_error_without_debug("context gate must reject"),
                expected
            );
        }
    }

    #[test]
    fn authorization_rejects_non_repair_missing_plan_diagnosis_action_and_target() {
        let context = RepairAuthorizationContext::current_operation();
        let mut non_repair = repair_decision(OwnershipClass::LauncherManaged);
        non_repair.kind = GuardianActionKind::Block;
        assert_eq!(
            authorize_launcher_managed_artifact_repair(
                &non_repair,
                context,
                artifact_descriptor("minecraft_client_1_21_5"),
            )
            .expect_error_without_debug("non-repair decision"),
            RepairAuthorizationRejection::NonRepairDecision
        );

        let mut missing_plan = repair_decision(OwnershipClass::LauncherManaged);
        missing_plan.action_plan = None;
        assert_eq!(
            authorize_launcher_managed_artifact_repair(
                &missing_plan,
                context,
                artifact_descriptor("minecraft_client_1_21_5"),
            )
            .expect_error_without_debug("missing action plan"),
            RepairAuthorizationRejection::MissingActionPlan
        );

        let mut unsupported = repair_decision(OwnershipClass::LauncherManaged);
        unsupported.diagnoses = vec![DiagnosisId::ManagedRuntimeCorrupt];
        assert_eq!(
            authorize_launcher_managed_artifact_repair(
                &unsupported,
                context,
                artifact_descriptor("minecraft_client_1_21_5"),
            )
            .expect_error_without_debug("unsupported diagnosis"),
            RepairAuthorizationRejection::UnsupportedDiagnosis
        );

        let mut missing_action = repair_decision(OwnershipClass::LauncherManaged);
        missing_action
            .action_plan
            .as_mut()
            .expect("plan")
            .actions
            .clear();
        assert_eq!(
            authorize_launcher_managed_artifact_repair(
                &missing_action,
                context,
                artifact_descriptor("minecraft_client_1_21_5"),
            )
            .expect_error_without_debug("missing repair action"),
            RepairAuthorizationRejection::MissingTarget
        );

        let mut missing_target = repair_decision(OwnershipClass::LauncherManaged);
        let plan = missing_target.action_plan.as_mut().expect("plan");
        plan.actions[0].target = None;
        plan.prerequisite.affected_targets.clear();
        assert_eq!(
            authorize_launcher_managed_artifact_repair(
                &missing_target,
                context,
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
                    RepairAuthorizationContext::current_operation(),
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
                RepairAuthorizationContext::current_operation(),
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
            RepairAuthorizationContext::current_operation(),
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
        let context = RepairAuthorizationContext::current_operation();
        let mut disabled = runtime_repair_decision(OwnershipClass::LauncherManaged);
        disabled.mode = GuardianMode::Disabled;
        assert_eq!(
            authorize_managed_runtime_ready_marker_repair(&disabled, context)
                .expect_error_without_debug("disabled mode"),
            RepairAuthorizationRejection::NonRepairDecision
        );

        let mut low_confidence = runtime_repair_decision(OwnershipClass::LauncherManaged);
        low_confidence
            .action_plan
            .as_mut()
            .expect("plan")
            .prerequisite
            .confidence = GuardianConfidence::Low;
        assert_eq!(
            authorize_managed_runtime_ready_marker_repair(&low_confidence, context)
                .expect_error_without_debug("low confidence"),
            RepairAuthorizationRejection::UnsupportedDiagnosis
        );

        let mut wrong_reason = runtime_repair_decision(OwnershipClass::LauncherManaged);
        wrong_reason.action_plan.as_mut().expect("plan").actions[0].reason =
            DiagnosisId::ManagedRuntimeMissing;
        assert_eq!(
            authorize_managed_runtime_ready_marker_repair(&wrong_reason, context)
                .expect_error_without_debug("wrong action reason"),
            RepairAuthorizationRejection::UnsupportedDiagnosis
        );

        let mut wrong_owner = runtime_repair_decision(OwnershipClass::LauncherManaged);
        wrong_owner.action_plan.as_mut().expect("plan").owner = StabilizationSystem::Application;
        assert_eq!(
            authorize_managed_runtime_ready_marker_repair(&wrong_owner, context)
                .expect_error_without_debug("wrong action-plan owner"),
            RepairAuthorizationRejection::UnsupportedDiagnosis
        );

        for ownership in [OwnershipClass::UserOwned, OwnershipClass::Unknown] {
            assert_eq!(
                authorize_managed_runtime_ready_marker_repair(
                    &runtime_repair_decision(ownership),
                    context,
                )
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
            authorize_managed_runtime_ready_marker_repair(&unsupported, context)
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
        GuardianDecision {
            operation_id: Some(OperationId::new("operation-install-repair")),
            mode: GuardianMode::Managed,
            kind: GuardianActionKind::Repair,
            diagnoses: vec![DiagnosisId::LauncherManagedArtifactCorrupt],
            action_plan: Some(GuardianActionPlan::new(
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
        }
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
        GuardianDecision {
            operation_id: Some(OperationId::new(format!("operation-runtime-{ownership:?}"))),
            mode: GuardianMode::Managed,
            kind: GuardianActionKind::Repair,
            diagnoses: vec![diagnosis_id],
            action_plan: Some(GuardianActionPlan::new(
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
        }
    }
}
