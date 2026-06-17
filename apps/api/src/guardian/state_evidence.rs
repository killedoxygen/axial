use super::{
    DiagnosisId, GuardianDecisionKind, GuardianMode, GuardianObservation, GuardianPolicyContext,
    GuardianUserOutcome, build_safety_case, decide_guardian_policy, guardian_fact_from_observation,
    persisted_state_load_user_outcome,
};
use crate::state::contracts::{
    OperationPhase, OwnershipClass, StabilizationSystem, TargetDescriptor, TargetKind,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GuardianStateLoadOutcome {
    pub decision: GuardianDecisionKind,
    pub diagnosis_id: DiagnosisId,
    pub user_outcome: GuardianUserOutcome,
}

pub fn persisted_state_load_guardian_outcome(
    load_issue_count: usize,
) -> Option<GuardianStateLoadOutcome> {
    if load_issue_count == 0 {
        return None;
    }

    let target = TargetDescriptor::new(
        StabilizationSystem::State,
        TargetKind::Config,
        "persisted-state-load",
        OwnershipClass::LauncherManaged,
    );
    let fact = guardian_fact_from_observation(
        GuardianObservation::PersistedStateSchemaInvalid,
        OperationPhase::Startup,
        Some(target),
    );
    let safety_case = build_safety_case(
        None,
        GuardianMode::Managed,
        OperationPhase::Startup,
        std::slice::from_ref(&fact),
    );
    let decision = decide_guardian_policy(&safety_case, GuardianPolicyContext::current_operation());
    let diagnosis_id = safety_case.diagnoses.first()?.id.clone();

    Some(GuardianStateLoadOutcome {
        decision: decision.kind,
        diagnosis_id: diagnosis_id.clone(),
        user_outcome: persisted_state_load_user_outcome(decision.kind, diagnosis_id.as_str()),
    })
}

#[cfg(test)]
mod tests {
    use super::persisted_state_load_guardian_outcome;
    use crate::guardian::GuardianDecisionKind;
    use crate::state::contracts::OperationPhase;

    #[test]
    fn no_state_load_issues_produce_no_guardian_outcome() {
        assert_eq!(persisted_state_load_guardian_outcome(0), None);
    }

    #[test]
    fn state_load_issues_flow_through_guardian_policy() {
        let outcome = persisted_state_load_guardian_outcome(2).expect("guardian outcome");

        assert_eq!(outcome.decision, GuardianDecisionKind::Warn);
        assert_eq!(
            outcome.diagnosis_id.as_str(),
            "persisted_state_schema_invalid"
        );
        assert_eq!(outcome.user_outcome.phase, OperationPhase::Startup);
        assert_eq!(
            outcome.user_outcome.summary,
            "Guardian kept Croopor running after persisted operation state could not be trusted."
        );
    }
}
