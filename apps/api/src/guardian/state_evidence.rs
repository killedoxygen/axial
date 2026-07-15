use super::{
    DiagnosisId, FactReliability, GuardianActionKind, GuardianCopyRequest, GuardianDomain,
    GuardianFact, GuardianFactId, GuardianMode, GuardianPolicyContext, GuardianUserOutcome,
    author_guardian_copy, build_safety_case, decide_guardian_policy,
};
use crate::state::PersistedStateLoadEvidence;
use crate::state::contracts::{
    OperationPhase, OwnershipClass, StabilizationSystem, TargetDescriptor, TargetKind,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct GuardianStateLoadOutcome {
    pub(crate) decision: GuardianActionKind,
    pub(crate) diagnosis_id: DiagnosisId,
    pub(crate) user_outcome: GuardianUserOutcome,
}

pub(crate) fn persisted_state_load_guardian_outcome(
    evidence: &PersistedStateLoadEvidence,
) -> Option<GuardianStateLoadOutcome> {
    if evidence.issue_count() == 0 {
        return None;
    }

    let target = TargetDescriptor::new(
        StabilizationSystem::State,
        TargetKind::Config,
        "persisted-state-load",
        OwnershipClass::LauncherManaged,
    );
    let fact = persisted_state_load_fact(target);
    let safety_case = build_safety_case(
        None,
        GuardianMode::Managed,
        OperationPhase::Startup,
        std::slice::from_ref(&fact),
    );
    let decision = decide_guardian_policy(&safety_case, GuardianPolicyContext::current_operation());
    let diagnosis_id = safety_case.diagnoses.first()?.id();

    let user_outcome = author_guardian_copy(GuardianCopyRequest::persisted_state_load(
        diagnosis_id,
        decision.kind(),
    ))?;
    Some(GuardianStateLoadOutcome {
        decision: decision.kind(),
        diagnosis_id,
        user_outcome,
    })
}

fn persisted_state_load_fact(target: TargetDescriptor) -> GuardianFact {
    GuardianFact {
        operation_id: None,
        id: GuardianFactId::PersistedStateSchemaInvalid,
        domain: GuardianDomain::State,
        phase: OperationPhase::Startup,
        reliability: FactReliability::DirectStructured,
        severity: None,
        confidence: None,
        ownership: target.ownership,
        target: Some(target),
        fields: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::persisted_state_load_guardian_outcome;
    use crate::guardian::GuardianActionKind;
    use crate::state::PersistedStateLoadEvidence;
    use crate::state::contracts::OperationPhase;

    #[test]
    fn no_state_load_issues_produce_no_guardian_outcome() {
        assert_eq!(
            persisted_state_load_guardian_outcome(&PersistedStateLoadEvidence::for_test(0)),
            None
        );
    }

    #[test]
    fn state_load_issues_flow_through_guardian_policy() {
        let outcome =
            persisted_state_load_guardian_outcome(&PersistedStateLoadEvidence::for_test(2))
                .expect("guardian outcome");

        assert_eq!(outcome.decision, GuardianActionKind::Warn);
        assert_eq!(outcome.user_outcome.decision(), outcome.decision);
        assert_eq!(
            outcome.diagnosis_id.as_str(),
            "persisted_state_schema_invalid"
        );
        assert_eq!(outcome.user_outcome.phase(), OperationPhase::Startup);
        assert_eq!(
            outcome.user_outcome.summary(),
            "Guardian kept Axial running after persisted operation state could not be trusted."
        );
    }
}
