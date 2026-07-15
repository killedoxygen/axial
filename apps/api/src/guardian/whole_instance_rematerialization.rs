use crate::guardian::{
    DiagnosisId, FactReliability, GuardianActionKind, GuardianDecision, GuardianFact,
    GuardianFactId, GuardianMode, GuardianPolicyContext, build_safety_case, decide_guardian_policy,
};
use crate::state::contracts::{
    OperationPhase, OwnershipClass, StabilizationSystem, TargetDescriptor,
};
use crate::state::{
    RegisteredWholeInstanceDurableOutcome, RegisteredWholeInstancePreparation,
    RegisteredWholeInstanceRematerializationAdmission,
    RegisteredWholeInstanceRematerializationAuthorization,
    RegisteredWholeInstanceRematerializationEligibility,
};
use axial_minecraft::{ManagedWholeInstanceCommitReceipt, ManagedWholeInstanceRebuildError};
use std::future::Future;
use std::path::PathBuf;

#[must_use]
pub(crate) enum GuardianWholeInstanceRematerializationDisposition {
    Offered(GuardianWholeInstanceRematerializationOffer),
    WitnessOnly { decision: GuardianDecision },
}

#[must_use]
pub(crate) struct GuardianWholeInstanceRematerializationOffer {
    authorization: RegisteredWholeInstanceRematerializationAuthorization,
}

impl GuardianWholeInstanceRematerializationOffer {
    pub(crate) fn into_authorization(
        self,
    ) -> RegisteredWholeInstanceRematerializationAuthorization {
        self.authorization
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub(crate) enum GuardianWholeInstanceRematerializationAssessmentError {
    #[error("whole-instance rematerialization decision was malformed")]
    InvalidDecision,
    #[error("whole-instance rematerialization authorization was rejected")]
    AuthorizationRejected,
}

pub(crate) fn assess_whole_instance_rematerialization(
    eligibility: RegisteredWholeInstanceRematerializationEligibility,
    current_mode: GuardianMode,
) -> Result<
    GuardianWholeInstanceRematerializationDisposition,
    GuardianWholeInstanceRematerializationAssessmentError,
> {
    let operation_id = eligibility.operation_id().clone();
    let target = eligibility.target();
    let fact = GuardianFact {
        operation_id: Some(operation_id.clone()),
        id: GuardianFactId::RegisteredComponentRebuildFailed,
        domain: eligibility.domain(),
        phase: OperationPhase::Repairing,
        reliability: FactReliability::DirectStructured,
        severity: None,
        confidence: None,
        ownership: OwnershipClass::LauncherManaged,
        target: Some(target.clone()),
        fields: Vec::new(),
    };
    let safety_case = build_safety_case(
        Some(operation_id.clone()),
        current_mode,
        OperationPhase::Repairing,
        &[fact],
    );
    let decision = decide_guardian_policy(&safety_case, GuardianPolicyContext::current_operation());
    match decision.kind() {
        GuardianActionKind::AskUser => {
            let authorization = eligibility.authorize_decision(&decision).map_err(|_| {
                GuardianWholeInstanceRematerializationAssessmentError::AuthorizationRejected
            })?;
            Ok(GuardianWholeInstanceRematerializationDisposition::Offered(
                GuardianWholeInstanceRematerializationOffer { authorization },
            ))
        }
        GuardianActionKind::RecordOnly
            if witness_decision_is_exact(&decision, &operation_id, &target) =>
        {
            Ok(GuardianWholeInstanceRematerializationDisposition::WitnessOnly { decision })
        }
        _ => Err(GuardianWholeInstanceRematerializationAssessmentError::InvalidDecision),
    }
}

fn witness_decision_is_exact(
    decision: &GuardianDecision,
    operation_id: &crate::state::contracts::OperationId,
    target: &TargetDescriptor,
) -> bool {
    decision.operation_id() == Some(operation_id)
        && decision.mode() == GuardianMode::Disabled
        && decision.diagnoses() == [DiagnosisId::LauncherManagedArtifactCorrupt]
        && decision.action_plan().is_some_and(|plan| {
            plan.owner == StabilizationSystem::Guardian
                && plan.prerequisite.diagnosis_id == DiagnosisId::LauncherManagedArtifactCorrupt
                && plan.prerequisite.ownership == OwnershipClass::LauncherManaged
                && plan.prerequisite.affected_targets == [target.clone()]
                && plan.prerequisite.candidate_actions
                    == [GuardianActionKind::AskUser, GuardianActionKind::Block]
                && matches!(
                    plan.actions.as_slice(),
                    [action]
                        if action.kind == GuardianActionKind::RecordOnly
                            && action.reason == DiagnosisId::LauncherManagedArtifactCorrupt
                            && action.target.as_ref() == Some(target)
                )
        })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GuardianWholeInstanceRematerializationStatus {
    Rematerialized,
    Failed,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct GuardianWholeInstanceRematerializationOutcome {
    status: GuardianWholeInstanceRematerializationStatus,
}

impl GuardianWholeInstanceRematerializationOutcome {
    pub(crate) fn status(&self) -> GuardianWholeInstanceRematerializationStatus {
        self.status
    }
}

#[derive(Debug, thiserror::Error)]
#[error("whole-instance rematerialization State settlement failed: {class}")]
pub(crate) struct GuardianWholeInstanceRematerializationError {
    class: &'static str,
}

impl GuardianWholeInstanceRematerializationError {
    pub(crate) fn class(&self) -> &'static str {
        self.class
    }
}

pub(crate) async fn execute_whole_instance_rematerialization(
    admission: RegisteredWholeInstanceRematerializationAdmission,
) -> Result<
    GuardianWholeInstanceRematerializationOutcome,
    GuardianWholeInstanceRematerializationError,
> {
    execute_whole_instance_rematerialization_with(
        admission,
        |root, runtime_cache, version_id| async move {
            axial_minecraft::rematerialize_managed_instance(root, &runtime_cache, &version_id).await
        },
    )
    .await
}

pub(crate) async fn execute_whole_instance_rematerialization_with<Driver, DriverFuture>(
    admission: RegisteredWholeInstanceRematerializationAdmission,
    driver: Driver,
) -> Result<
    GuardianWholeInstanceRematerializationOutcome,
    GuardianWholeInstanceRematerializationError,
>
where
    Driver: FnOnce(PathBuf, axial_minecraft::runtime::ManagedRuntimeCache, String) -> DriverFuture
        + Send,
    DriverFuture: Future<Output = Result<ManagedWholeInstanceCommitReceipt, ManagedWholeInstanceRebuildError>>
        + Send,
{
    let preparation = admission.into_effect().await.map_err(|error| {
        GuardianWholeInstanceRematerializationError {
            class: error.class(),
        }
    })?;
    let (request, completion) = match preparation {
        RegisteredWholeInstancePreparation::Admitted {
            request,
            completion,
        } => (request, completion),
        RegisteredWholeInstancePreparation::Closed(outcome) => {
            return Ok(guardian_outcome(outcome));
        }
    };
    let (root, runtime_cache, version_id) = {
        let (root, runtime_cache, version_id) = request.core_request();
        (
            root.to_path_buf(),
            runtime_cache.clone(),
            version_id.to_string(),
        )
    };
    let settlement = match driver(root, runtime_cache, version_id).await {
        Ok(receipt) => completion.settle_commit(receipt).await,
        Err(ManagedWholeInstanceRebuildError::RolledBack(receipt)) => {
            completion.settle_rollback(receipt).await
        }
        Err(
            ManagedWholeInstanceRebuildError::Reconstruction(_)
            | ManagedWholeInstanceRebuildError::Preparation
            | ManagedWholeInstanceRebuildError::RuntimePreparation,
        ) => completion.into_failed_settlement(),
    };
    let outcome =
        settlement
            .settle()
            .await
            .map_err(|error| GuardianWholeInstanceRematerializationError {
                class: error.class(),
            })?;
    Ok(guardian_outcome(outcome))
}

fn guardian_outcome(
    outcome: RegisteredWholeInstanceDurableOutcome,
) -> GuardianWholeInstanceRematerializationOutcome {
    GuardianWholeInstanceRematerializationOutcome {
        status: if outcome.succeeded() {
            GuardianWholeInstanceRematerializationStatus::Rematerialized
        } else {
            GuardianWholeInstanceRematerializationStatus::Failed
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    trait AmbiguousIfClone<Marker> {
        fn assert_not_clone() {}
    }

    struct CloneMarker;

    impl<T: ?Sized> AmbiguousIfClone<()> for T {}
    impl<T: Clone> AmbiguousIfClone<CloneMarker> for T {}

    const _: fn() = || {
        let _ =
            <GuardianWholeInstanceRematerializationOffer as AmbiguousIfClone<_>>::assert_not_clone;
    };

    #[test]
    fn production_executor_has_one_core_rematerialization_call_site() {
        let source = include_str!("whole_instance_rematerialization.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("production source precedes tests");
        assert_eq!(
            source
                .matches("axial_minecraft::rematerialize_managed_instance(")
                .count(),
            1
        );
        assert!(!source.contains("OperationJournal"));
        assert!(!source.contains("ReconciliationQuarantineCheckpoint"));
    }
}
