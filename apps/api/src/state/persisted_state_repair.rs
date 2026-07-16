use super::persisted_state_load::PersistedStateRejectedRecordEligibility;
use crate::guardian::persisted_state_repair::{
    PERSISTED_STATE_REPAIR_CANDIDATES, PersistedStateRepairAssessmentProof,
};
use crate::guardian::{DiagnosisId, GuardianActionKind, GuardianDecision, GuardianMode};
use crate::state::contracts::{OwnershipClass, StabilizationSystem};

pub(crate) struct PersistedStateRejectedRecordQuarantineAuthorization {
    eligibility: PersistedStateRejectedRecordEligibility,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PersistedStateRepairAuthorizationRejection {
    InvalidAssessment,
    RecordIdentityChanged,
}

pub(crate) fn authorize_persisted_state_rejected_record_quarantine(
    eligibility: PersistedStateRejectedRecordEligibility,
    proof: PersistedStateRepairAssessmentProof,
    decision: &GuardianDecision,
) -> Result<
    PersistedStateRejectedRecordQuarantineAuthorization,
    PersistedStateRepairAuthorizationRejection,
> {
    if proof.assessed_mode() != GuardianMode::Managed || !exact_managed_decision(decision) {
        return Err(PersistedStateRepairAuthorizationRejection::InvalidAssessment);
    }
    if !eligibility.still_current() {
        return Err(PersistedStateRepairAuthorizationRejection::RecordIdentityChanged);
    }

    Ok(PersistedStateRejectedRecordQuarantineAuthorization { eligibility })
}

fn exact_managed_decision(decision: &GuardianDecision) -> bool {
    if decision.operation_id().is_some()
        || decision.mode() != GuardianMode::Managed
        || decision.kind() != GuardianActionKind::Quarantine
        || decision.diagnoses() != [DiagnosisId::PersistedStateSchemaInvalid]
    {
        return false;
    }
    let Some(plan) = decision.action_plan() else {
        return false;
    };
    if plan.owner != StabilizationSystem::Guardian
        || plan.prerequisite.diagnosis_id != DiagnosisId::PersistedStateSchemaInvalid
        || plan.prerequisite.ownership != OwnershipClass::LauncherManaged
        || plan.prerequisite.confidence != crate::guardian::GuardianConfidence::Confirmed
        || plan.prerequisite.candidate_actions != PERSISTED_STATE_REPAIR_CANDIDATES
        || plan.prerequisite.affected_targets.len() != 1
        || plan.actions.len() != 1
    {
        return false;
    }
    let target = &plan.prerequisite.affected_targets[0];
    let action = &plan.actions[0];
    exact_target(target)
        && action.kind == GuardianActionKind::Quarantine
        && action.reason == DiagnosisId::PersistedStateSchemaInvalid
        && action.target.as_ref() == Some(target)
}

fn exact_target(target: &crate::state::contracts::TargetDescriptor) -> bool {
    target == &super::persisted_state_load_target()
}

#[cfg(test)]
impl PersistedStateRejectedRecordQuarantineAuthorization {
    pub(crate) fn record_id(&self) -> &str {
        self.eligibility.record_id()
    }
}
