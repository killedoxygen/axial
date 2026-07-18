use crate::guardian::{
    DiagnosisId, FactReliability, GuardianActionKind, GuardianDecision, GuardianFact,
    GuardianFactId, GuardianMode, GuardianPolicyContext, build_safety_case, decide_guardian_policy,
};
use crate::state::contracts::{
    OperationPhase, OwnershipClass, StabilizationSystem, TargetDescriptor,
};
use crate::state::{
    ProducerLease, RegisteredUserConfigRestoreEligibility, RegisteredWholeInstanceDurableOutcome,
    RegisteredWholeInstancePreparation, RegisteredWholeInstanceRematerializationAdmission,
    RegisteredWholeInstanceRematerializationAuthorization,
    RegisteredWholeInstanceRematerializationEligibility,
};
use axial_minecraft::{ManagedWholeInstanceCommitReceipt, ManagedWholeInstanceRebuildError};
use std::future::Future;
use std::path::PathBuf;

#[must_use]
pub(crate) enum GuardianWholeInstanceRematerializationDisposition {
    Offered(
        #[cfg_attr(
            not(test),
            expect(
                dead_code,
                reason = "Phase 4 backend contract; Phase 6 transport deferred"
            )
        )]
        GuardianWholeInstanceRematerializationOffer,
    ),
    WitnessOnly {
        #[cfg_attr(
            not(test),
            expect(
                dead_code,
                reason = "Phase 4 backend contract; Phase 6 transport deferred"
            )
        )]
        decision: GuardianDecision,
    },
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

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "Phase 4 backend contract; Phase 6 transport deferred"
    )
)]
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

pub(crate) struct GuardianWholeInstanceRematerializationOutcome {
    status: GuardianWholeInstanceRematerializationStatus,
    restore_offer: Option<GuardianUserConfigRestoreOffer>,
}

#[must_use]
pub(crate) struct GuardianUserConfigRestoreOffer {
    _eligibility: RegisteredUserConfigRestoreEligibility,
}

impl GuardianWholeInstanceRematerializationOutcome {
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "Phase 4 backend contract; Phase 6 transport deferred"
        )
    )]
    pub(crate) fn status(&self) -> GuardianWholeInstanceRematerializationStatus {
        self.status
    }

    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "Phase 4 backend contract; Phase 6 transport deferred"
        )
    )]
    pub(crate) fn into_restore_offer(
        self,
    ) -> Option<crate::guardian::GuardianUserConfigRestoreOffer> {
        self.restore_offer
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

async fn await_whole_instance_owner<Owner>(
    producer: ProducerLease,
    owner: Owner,
) -> Result<
    GuardianWholeInstanceRematerializationOutcome,
    GuardianWholeInstanceRematerializationError,
>
where
    Owner: Future<
            Output = Result<
                GuardianWholeInstanceRematerializationOutcome,
                GuardianWholeInstanceRematerializationError,
            >,
        > + Send
        + 'static,
{
    producer.spawn_joinable(owner).await.map_err(|_| {
        GuardianWholeInstanceRematerializationError {
            class: "invalid_guardian_outcome",
        }
    })?
}

pub(crate) async fn execute_whole_instance_rematerialization(
    producer: ProducerLease,
    admission: RegisteredWholeInstanceRematerializationAdmission,
) -> Result<
    GuardianWholeInstanceRematerializationOutcome,
    GuardianWholeInstanceRematerializationError,
> {
    execute_whole_instance_rematerialization_with(
        producer,
        admission,
        |root, runtime_cache, version_id| async move {
            axial_minecraft::rematerialize_managed_instance(root, &runtime_cache, &version_id).await
        },
    )
    .await
}

pub(crate) async fn execute_whole_instance_rematerialization_with<Driver, DriverFuture>(
    producer: ProducerLease,
    admission: RegisteredWholeInstanceRematerializationAdmission,
    driver: Driver,
) -> Result<
    GuardianWholeInstanceRematerializationOutcome,
    GuardianWholeInstanceRematerializationError,
>
where
    Driver: FnOnce(PathBuf, axial_minecraft::runtime::ManagedRuntimeCache, String) -> DriverFuture
        + Send
        + 'static,
    DriverFuture: Future<Output = Result<ManagedWholeInstanceCommitReceipt, ManagedWholeInstanceRebuildError>>
        + Send
        + 'static,
{
    await_whole_instance_owner(producer, async move {
        let preparation = admission.into_effect().await.map_err(|error| {
            GuardianWholeInstanceRematerializationError {
                class: error.class(),
            }
        })?;
        let (request, completion) = match preparation {
            RegisteredWholeInstancePreparation::Admitted {
                request,
                completion,
            } => (request, *completion),
            RegisteredWholeInstancePreparation::Closed(outcome) => {
                return Ok(guardian_outcome(*outcome));
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
                | ManagedWholeInstanceRebuildError::RuntimePreparation,
            ) => completion.into_failed_settlement(),
        };
        let outcome = settlement.settle().await.map_err(|error| {
            GuardianWholeInstanceRematerializationError {
                class: error.class(),
            }
        })?;
        Ok(guardian_outcome(outcome))
    })
    .await
}

fn guardian_outcome(
    outcome: RegisteredWholeInstanceDurableOutcome,
) -> GuardianWholeInstanceRematerializationOutcome {
    let status = if outcome.succeeded() {
        GuardianWholeInstanceRematerializationStatus::Rematerialized
    } else {
        GuardianWholeInstanceRematerializationStatus::Failed
    };
    GuardianWholeInstanceRematerializationOutcome {
        status,
        restore_offer: outcome.into_restore_eligibility().map(|eligibility| {
            GuardianUserConfigRestoreOffer {
                _eligibility: eligibility,
            }
        }),
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
    static_assertions::assert_not_impl_any!(
        GuardianUserConfigRestoreOffer: Clone, std::fmt::Debug, serde::Serialize, serde::de::DeserializeOwned
    );
    static_assertions::assert_not_impl_any!(
        GuardianWholeInstanceRematerializationOutcome: Clone, std::fmt::Debug, serde::Serialize, serde::de::DeserializeOwned
    );

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

    #[test]
    fn user_config_restore_offer_has_no_writer_route_dto_conversion_or_scheduler() {
        let guardian = include_str!("whole_instance_rematerialization.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("Guardian production source");
        let state = include_str!("../state/reconciliation.rs");
        let store = include_str!("../state/user_config_snapshots.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("snapshot store production source");
        let capture = include_str!("../execution/user_owned_state.rs")
            .split("#[cfg(test)]\nmod tests")
            .next()
            .expect("user config capture production source");
        let public_surface = concat!(
            include_str!("../routes/mod.rs"),
            include_str!("../routes/instances.rs"),
            include_str!("../dto/mod.rs"),
            include_str!("../application/commands.rs"),
        );

        assert!(!guardian.contains("impl GuardianUserConfigRestoreOffer"));
        assert!(!state.contains("impl RegisteredUserConfigRestoreEligibility"));
        assert!(!guardian.contains("tokio::time::interval"));
        assert!(!store.contains("std::fs::write"));
        assert!(!store.contains("File::create"));
        assert!(!store.contains("OpenOptions"));
        assert!(!store.contains("AnchoredRecordDirectory"));
        for forbidden in [
            "write_file_atomically",
            "std::fs::write",
            "File::create",
            "OpenOptions",
            "restore_user_config",
        ] {
            assert!(!capture.contains(forbidden));
        }
        for forbidden in [
            "GuardianUserConfigRestoreOffer",
            "RegisteredUserConfigRestoreEligibility",
            "guardian-user-config-snapshots",
            "restore_user_config",
            "user_config_snapshot",
        ] {
            assert!(!public_surface.contains(forbidden));
        }
    }
}
