use super::{
    DiagnosisId, FactReliability, GuardianActionKind, GuardianCopyRequest, GuardianDomain,
    GuardianFact, GuardianFactId, GuardianMode, GuardianPolicyContext, GuardianUserOutcome,
    OperationEvidenceBatch, author_guardian_copy, build_safety_case, decide_guardian_policy,
};
use crate::observability::{EvidenceField, EvidenceSensitivity};
use crate::state::contracts::OperationPhase;
use crate::state::{PersistedStateLoadEvidence, persisted_state_load_target};

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

    let mut fact = persisted_state_schema_invalid_fact();
    let temporal = evidence.temporal_load_issues();
    if temporal.future_observation() != 0 {
        fact.fields.push(EvidenceField::new(
            "temporal_future_observation_count",
            temporal.future_observation().to_string(),
            EvidenceSensitivity::Public,
        ));
    }
    if temporal.out_of_bounds_window() != 0 {
        fact.fields.push(EvidenceField::new(
            "temporal_out_of_bounds_window_count",
            temporal.out_of_bounds_window().to_string(),
            EvidenceSensitivity::Public,
        ));
    }
    let evidence = OperationEvidenceBatch::try_from_guardian_unscoped(std::slice::from_ref(&fact))
        .expect("persisted-state load evidence is bounded and unscoped");
    let safety_case = build_safety_case(GuardianMode::Managed, OperationPhase::Startup, &evidence);
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

pub(super) fn persisted_state_schema_invalid_fact() -> GuardianFact {
    persisted_state_fact(GuardianFactId::PersistedStateSchemaInvalid)
}

pub(super) fn persisted_state_repair_available_fact() -> GuardianFact {
    persisted_state_fact(GuardianFactId::PersistedStateRepairAvailable)
}

fn persisted_state_fact(id: GuardianFactId) -> GuardianFact {
    let target = persisted_state_load_target();
    GuardianFact {
        operation_id: None,
        id,
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
