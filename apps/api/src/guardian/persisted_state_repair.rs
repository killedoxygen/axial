#[cfg(test)]
use super::FactReliability;
use super::state_evidence::{
    persisted_state_repair_available_fact, persisted_state_schema_invalid_fact,
};
use super::{
    DiagnosisId, GuardianActionKind, GuardianConfidence, GuardianDecision, GuardianDomain,
    GuardianFact, GuardianFactId, GuardianMode, GuardianPolicyContext, GuardianSeverity,
    SafetyCase, build_safety_case, decide_guardian_policy,
};
use crate::state::contracts::{OperationPhase, OwnershipClass, StabilizationSystem};
use crate::state::{
    PersistedStateRejectedRecordEligibility, PersistedStateRejectedRecordQuarantineAuthorization,
    authorize_persisted_state_rejected_record_quarantine, persisted_state_load_target,
};

pub(crate) const PERSISTED_STATE_REPAIR_CANDIDATES: [GuardianActionKind; 3] = [
    GuardianActionKind::Quarantine,
    GuardianActionKind::AskUser,
    GuardianActionKind::RecordOnly,
];

pub(crate) struct PersistedStateRepairAssessmentProof {
    assessed_mode: GuardianMode,
}

pub(crate) enum PersistedStateRepairDisposition {
    Managed(PersistedStateRepairManagedAuthorization),
    Custom(PersistedStateRepairCustomOffer),
    RecordOnly(PersistedStateRepairRecordOnlyWitness),
}

pub(crate) struct PersistedStateRepairManagedAuthorization {
    authorization: PersistedStateRejectedRecordQuarantineAuthorization,
}

pub(crate) struct PersistedStateRepairCustomOffer {
    eligibility: PersistedStateRejectedRecordEligibility,
    decision: GuardianDecision,
}

pub(crate) struct PersistedStateRepairRecordOnlyWitness {
    assessed_mode: GuardianMode,
    observed_decision: GuardianActionKind,
}

impl PersistedStateRepairCustomOffer {
    pub(crate) fn discard(self) {
        let Self {
            eligibility,
            decision,
        } = self;
        drop((eligibility, decision));
    }
}

impl PersistedStateRepairRecordOnlyWitness {
    pub(crate) fn discard(self) {
        let Self {
            assessed_mode,
            observed_decision,
        } = self;
        let _ = (assessed_mode, observed_decision);
    }
}

impl PersistedStateRepairAssessmentProof {
    pub(crate) fn assessed_mode(&self) -> GuardianMode {
        self.assessed_mode
    }
}

impl PersistedStateRepairManagedAuthorization {
    pub(crate) fn into_authorization(self) -> PersistedStateRejectedRecordQuarantineAuthorization {
        self.authorization
    }
}

pub(crate) fn assess_persisted_state_repair(
    mode: GuardianMode,
    eligibility: PersistedStateRejectedRecordEligibility,
) -> PersistedStateRepairDisposition {
    let Some((proof, decision)) = evaluate_persisted_state_repair_policy(mode) else {
        drop(eligibility);
        return PersistedStateRepairDisposition::RecordOnly(record_only_witness(
            mode,
            GuardianActionKind::RecordOnly,
        ));
    };
    disposition_from_decision(eligibility, proof, decision)
}

fn evaluate_persisted_state_repair_policy(
    mode: GuardianMode,
) -> Option<(PersistedStateRepairAssessmentProof, GuardianDecision)> {
    let facts = persisted_state_repair_facts();
    let safety_case = build_safety_case(None, mode, OperationPhase::Startup, &facts);
    let proof = seal_exact_assessment(&facts, &safety_case)?;
    let decision = decide_guardian_policy(&safety_case, GuardianPolicyContext::current_operation());
    Some((proof, decision))
}

#[cfg(test)]
pub(super) fn persisted_state_startup_policy_cells() -> [(GuardianMode, GuardianActionKind); 3] {
    std::array::from_fn(|index| {
        let mode = GuardianMode::ALL[index];
        let (_, decision) =
            evaluate_persisted_state_repair_policy(mode).expect("persisted-state assessment shape");
        (mode, decision.kind())
    })
}

fn persisted_state_repair_facts() -> [GuardianFact; 2] {
    [
        persisted_state_schema_invalid_fact(),
        persisted_state_repair_available_fact(),
    ]
}

fn seal_exact_assessment(
    facts: &[GuardianFact],
    safety_case: &SafetyCase,
) -> Option<PersistedStateRepairAssessmentProof> {
    if facts != persisted_state_repair_facts()
        || safety_case.operation_id.is_some()
        || safety_case.phase != OperationPhase::Startup
        || safety_case.diagnoses.len() != 1
    {
        return None;
    }
    let diagnosis = &safety_case.diagnoses[0];
    if diagnosis.id() != DiagnosisId::PersistedStateSchemaInvalid
        || diagnosis.domain() != GuardianDomain::State
        || diagnosis.severity() != GuardianSeverity::Warning
        || diagnosis.confidence() != GuardianConfidence::Confirmed
        || diagnosis.ownership() != OwnershipClass::LauncherManaged
        || diagnosis.phase() != OperationPhase::Startup
        || diagnosis.fact_ids() != [GuardianFactId::PersistedStateSchemaInvalid]
        || diagnosis.affected_targets() != [persisted_state_load_target()]
        || diagnosis.candidate_actions() != PERSISTED_STATE_REPAIR_CANDIDATES
        || diagnosis.public_reason_template() != "persisted_state_schema_invalid"
    {
        return None;
    }

    Some(PersistedStateRepairAssessmentProof {
        assessed_mode: safety_case.mode,
    })
}

fn disposition_from_decision(
    eligibility: PersistedStateRejectedRecordEligibility,
    proof: PersistedStateRepairAssessmentProof,
    decision: GuardianDecision,
) -> PersistedStateRepairDisposition {
    let assessed_mode = proof.assessed_mode();
    let observed_decision = decision.kind();
    match assessed_mode {
        GuardianMode::Managed => {
            match authorize_persisted_state_rejected_record_quarantine(
                eligibility,
                proof,
                &decision,
            ) {
                Ok(authorization) => PersistedStateRepairDisposition::Managed(
                    PersistedStateRepairManagedAuthorization { authorization },
                ),
                Err(_) => PersistedStateRepairDisposition::RecordOnly(record_only_witness(
                    assessed_mode,
                    observed_decision,
                )),
            }
        }
        GuardianMode::Custom
            if exact_decision(&decision, GuardianMode::Custom, GuardianActionKind::AskUser) =>
        {
            PersistedStateRepairDisposition::Custom(PersistedStateRepairCustomOffer {
                eligibility,
                decision,
            })
        }
        GuardianMode::Disabled
            if exact_decision(
                &decision,
                GuardianMode::Disabled,
                GuardianActionKind::RecordOnly,
            ) =>
        {
            drop(eligibility);
            PersistedStateRepairDisposition::RecordOnly(record_only_witness(
                assessed_mode,
                observed_decision,
            ))
        }
        GuardianMode::Custom | GuardianMode::Disabled => {
            drop(eligibility);
            PersistedStateRepairDisposition::RecordOnly(record_only_witness(
                assessed_mode,
                observed_decision,
            ))
        }
    }
}

fn exact_decision(
    decision: &GuardianDecision,
    expected_mode: GuardianMode,
    expected_action: GuardianActionKind,
) -> bool {
    if decision.operation_id().is_some()
        || decision.mode() != expected_mode
        || decision.kind() != expected_action
        || decision.diagnoses() != [DiagnosisId::PersistedStateSchemaInvalid]
    {
        return false;
    }
    let Some(plan) = decision.action_plan() else {
        return false;
    };
    let target = persisted_state_load_target();
    plan.owner == StabilizationSystem::Guardian
        && plan.prerequisite.diagnosis_id == DiagnosisId::PersistedStateSchemaInvalid
        && plan.prerequisite.ownership == OwnershipClass::LauncherManaged
        && plan.prerequisite.confidence == GuardianConfidence::Confirmed
        && plan.prerequisite.affected_targets == [target.clone()]
        && plan.prerequisite.candidate_actions == PERSISTED_STATE_REPAIR_CANDIDATES
        && plan.actions.len() == 1
        && plan.actions[0].kind == expected_action
        && plan.actions[0].target.as_ref() == Some(&target)
        && plan.actions[0].reason == DiagnosisId::PersistedStateSchemaInvalid
}

fn record_only_witness(
    assessed_mode: GuardianMode,
    observed_decision: GuardianActionKind,
) -> PersistedStateRepairRecordOnlyWitness {
    PersistedStateRepairRecordOnlyWitness {
        assessed_mode,
        observed_decision,
    }
}

#[cfg(test)]
fn exact_fact_shape(fact: &GuardianFact, expected_id: GuardianFactId) -> bool {
    fact.operation_id.is_none()
        && fact.id == expected_id
        && fact.domain == GuardianDomain::State
        && fact.phase == OperationPhase::Startup
        && fact.reliability == FactReliability::DirectStructured
        && fact.severity.is_none()
        && fact.confidence.is_none()
        && fact.ownership == OwnershipClass::LauncherManaged
        && fact.target.as_ref() == Some(&persisted_state_load_target())
        && fact.fields.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guardian::{
        ActionPlanPrerequisite, GuardianAction, GuardianActionPlan,
        with_guardian_policy_evaluation_count,
    };
    use crate::observability::{EvidenceField, EvidenceSensitivity};
    use crate::state::contracts::{OperationId, TargetDescriptor, TargetKind};
    use crate::state::{
        PersistedStateRejectedRecordQuarantineAuthorization,
        persisted_state_rejected_record_eligibility_for_test,
    };
    use static_assertions::assert_not_impl_any;
    use std::ffi::OsStr;
    use std::fs;
    use std::path::{Path, PathBuf};

    assert_not_impl_any!(
        PersistedStateRejectedRecordEligibility:
            Clone, std::fmt::Debug, serde::Serialize, serde::de::DeserializeOwned,
            AsRef<Path>, AsRef<[u8]>
    );
    assert_not_impl_any!(
        PersistedStateRepairAssessmentProof:
            Clone, std::fmt::Debug, serde::Serialize, serde::de::DeserializeOwned,
            AsRef<Path>, AsRef<[u8]>
    );
    assert_not_impl_any!(
        PersistedStateRejectedRecordQuarantineAuthorization:
            Clone, std::fmt::Debug, serde::Serialize, serde::de::DeserializeOwned,
            AsRef<Path>, AsRef<[u8]>
    );
    assert_not_impl_any!(
        PersistedStateRepairManagedAuthorization:
            Clone, std::fmt::Debug, serde::Serialize, serde::de::DeserializeOwned,
            AsRef<Path>, AsRef<[u8]>
    );
    assert_not_impl_any!(
        PersistedStateRepairCustomOffer:
            Clone, std::fmt::Debug, serde::Serialize, serde::de::DeserializeOwned,
            AsRef<Path>, AsRef<[u8]>
    );
    assert_not_impl_any!(
        PersistedStateRepairRecordOnlyWitness:
            Clone, std::fmt::Debug, serde::Serialize, serde::de::DeserializeOwned,
            AsRef<Path>, AsRef<[u8]>
    );

    #[test]
    fn exact_facts_and_safety_case_seal_one_condition_only_policy_lane() {
        let facts = persisted_state_repair_facts();
        assert!(exact_fact_shape(
            &facts[0],
            GuardianFactId::PersistedStateSchemaInvalid
        ));
        assert!(exact_fact_shape(
            &facts[1],
            GuardianFactId::PersistedStateRepairAvailable
        ));
        assert_eq!(facts[0].target, facts[1].target);

        let safety_case =
            build_safety_case(None, GuardianMode::Managed, OperationPhase::Startup, &facts);
        let proof = seal_exact_assessment(&facts, &safety_case).expect("sealed assessment");
        assert_eq!(proof.assessed_mode(), GuardianMode::Managed);
        let diagnosis = &safety_case.diagnoses[0];
        assert_eq!(
            diagnosis.fact_ids(),
            [GuardianFactId::PersistedStateSchemaInvalid]
        );
        assert_eq!(
            diagnosis.candidate_actions(),
            PERSISTED_STATE_REPAIR_CANDIDATES
        );
        assert_eq!(
            diagnosis.affected_targets(),
            [persisted_state_load_target()]
        );
    }

    #[tokio::test]
    async fn startup_policy_is_total_by_mode_and_each_assessment_evaluates_once() {
        for (index, mode) in GuardianMode::ALL.iter().copied().enumerate() {
            let (root, eligibility) = eligibility_fixture(&format!("mode-{index}"));
            let (disposition, evaluations) = with_guardian_policy_evaluation_count(async move {
                assess_persisted_state_repair(mode, eligibility)
            })
            .await;
            assert_eq!(evaluations, 1);

            match (mode, disposition) {
                (GuardianMode::Managed, PersistedStateRepairDisposition::Managed(managed)) => {
                    let authorization = managed.into_authorization();
                    drop(authorization);
                }
                (GuardianMode::Custom, PersistedStateRepairDisposition::Custom(offer)) => {
                    assert_eq!(offer.decision.mode(), GuardianMode::Custom);
                    assert_eq!(offer.decision.kind(), GuardianActionKind::AskUser);
                    assert_eq!(
                        offer.decision.diagnoses(),
                        [DiagnosisId::PersistedStateSchemaInvalid]
                    );
                    assert_eq!(
                        offer.decision.action_plan().expect("Custom plan").actions[0]
                            .target
                            .as_ref(),
                        Some(&persisted_state_load_target())
                    );
                    drop(offer);
                }
                (GuardianMode::Disabled, PersistedStateRepairDisposition::RecordOnly(witness)) => {
                    assert_eq!(witness.assessed_mode, GuardianMode::Disabled);
                    assert_eq!(witness.observed_decision, GuardianActionKind::RecordOnly);
                }
                _ => panic!("unexpected persisted-state mode disposition"),
            }
            fs::remove_dir_all(root).expect("remove eligibility fixture");
        }
    }

    #[test]
    fn malformed_fact_and_safety_case_shapes_cannot_mint_assessment_proof() {
        let mut variants = Vec::new();

        let mut facts = persisted_state_repair_facts();
        facts[0].operation_id = Some(OperationId::new("unexpected-operation"));
        variants.push(facts);
        let mut facts = persisted_state_repair_facts();
        facts[0].domain = GuardianDomain::Config;
        variants.push(facts);
        let mut facts = persisted_state_repair_facts();
        facts[0].phase = OperationPhase::Planning;
        variants.push(facts);
        let mut facts = persisted_state_repair_facts();
        facts[0].reliability = FactReliability::UserReported;
        variants.push(facts);
        let mut facts = persisted_state_repair_facts();
        facts[0].severity = Some(GuardianSeverity::Warning);
        variants.push(facts);
        let mut facts = persisted_state_repair_facts();
        facts[0].confidence = Some(GuardianConfidence::Confirmed);
        variants.push(facts);
        let mut facts = persisted_state_repair_facts();
        facts[0].ownership = OwnershipClass::Unknown;
        variants.push(facts);
        let mut facts = persisted_state_repair_facts();
        facts[0].target = Some(TargetDescriptor::new(
            StabilizationSystem::State,
            TargetKind::Config,
            "different-target",
            OwnershipClass::LauncherManaged,
        ));
        variants.push(facts);
        let mut facts = persisted_state_repair_facts();
        facts[0].fields.push(EvidenceField::new(
            "unexpected",
            "field",
            EvidenceSensitivity::Internal,
        ));
        variants.push(facts);
        let mut facts = persisted_state_repair_facts();
        facts[1].id = GuardianFactId::PersistedStateSchemaInvalid;
        variants.push(facts);

        for facts in variants {
            let safety_case =
                build_safety_case(None, GuardianMode::Managed, OperationPhase::Startup, &facts);
            assert!(seal_exact_assessment(&facts, &safety_case).is_none());
        }

        let facts = persisted_state_repair_facts();
        let mut safety_case =
            build_safety_case(None, GuardianMode::Managed, OperationPhase::Startup, &facts);
        safety_case.phase = OperationPhase::Planning;
        assert!(seal_exact_assessment(&facts, &safety_case).is_none());
    }

    #[test]
    fn every_malformed_managed_decision_shape_fails_closed() {
        for (index, mutation) in DecisionMutation::ALL.iter().copied().enumerate() {
            let (proof, decision) = valid_policy(GuardianMode::Managed);
            let decision = mutation.apply(decision);
            let (root, eligibility) = eligibility_fixture(&format!("decision-{index}"));
            let disposition = disposition_from_decision(eligibility, proof, decision);
            match disposition {
                PersistedStateRepairDisposition::RecordOnly(witness) => {
                    assert_eq!(witness.assessed_mode, GuardianMode::Managed);
                }
                _ => panic!("malformed decision retained executable authority"),
            }
            fs::remove_dir_all(root).expect("remove malformed-decision fixture");
        }
    }

    #[cfg(unix)]
    #[test]
    fn replaced_record_identity_refuses_managed_authorization() {
        let (root, eligibility) = eligibility_fixture("identity-replaced");
        let replacement = root.join("replacement.json");
        fs::write(&replacement, b"replacement").expect("write replacement");
        fs::rename(&replacement, root.join("record.json")).expect("replace record");
        let (proof, decision) = valid_policy(GuardianMode::Managed);

        match disposition_from_decision(eligibility, proof, decision) {
            PersistedStateRepairDisposition::RecordOnly(witness) => {
                assert_eq!(witness.assessed_mode, GuardianMode::Managed);
                assert_eq!(witness.observed_decision, GuardianActionKind::Quarantine);
            }
            _ => panic!("replaced identity retained executable authority"),
        }
        fs::remove_dir_all(root).expect("remove replaced-identity fixture");
    }

    fn valid_policy(mode: GuardianMode) -> (PersistedStateRepairAssessmentProof, GuardianDecision) {
        evaluate_persisted_state_repair_policy(mode).expect("valid persisted-state policy")
    }

    fn eligibility_fixture(label: &str) -> (PathBuf, PersistedStateRejectedRecordEligibility) {
        let root = std::env::temp_dir().join(format!(
            "axial-persisted-state-policy-{label}-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&root).expect("create eligibility fixture");
        fs::write(root.join("record.json"), b"{").expect("write rejected record");
        let eligibility = persisted_state_rejected_record_eligibility_for_test(
            &root,
            OsStr::new("record.json"),
            "rejected-record",
        )
        .expect("anchor rejected record eligibility");
        (root, eligibility)
    }

    #[derive(Clone, Copy)]
    enum DecisionMutation {
        Operation,
        Mode,
        Action,
        Diagnoses,
        MissingPlan,
        PlanOwner,
        PlanDiagnosis,
        PlanOwnership,
        PlanConfidence,
        PlanTarget,
        CandidateOrder,
        ActionKind,
        ActionTarget,
        ActionReason,
        ExtraAction,
    }

    impl DecisionMutation {
        const ALL: [Self; 15] = [
            Self::Operation,
            Self::Mode,
            Self::Action,
            Self::Diagnoses,
            Self::MissingPlan,
            Self::PlanOwner,
            Self::PlanDiagnosis,
            Self::PlanOwnership,
            Self::PlanConfidence,
            Self::PlanTarget,
            Self::CandidateOrder,
            Self::ActionKind,
            Self::ActionTarget,
            Self::ActionReason,
            Self::ExtraAction,
        ];

        fn apply(self, decision: GuardianDecision) -> GuardianDecision {
            let operation_id = decision.operation_id().cloned();
            let mut mode = decision.mode();
            let mut action = decision.kind();
            let mut diagnoses = decision.diagnoses().to_vec();
            let mut plan = decision.action_plan().cloned();
            match self {
                Self::Operation => {}
                Self::Mode => mode = GuardianMode::Custom,
                Self::Action => action = GuardianActionKind::AskUser,
                Self::Diagnoses => diagnoses = vec![DiagnosisId::PerformanceRulesInvalid],
                Self::MissingPlan => plan = None,
                Self::PlanOwner => plan.as_mut().expect("plan").owner = StabilizationSystem::State,
                Self::PlanDiagnosis => {
                    plan.as_mut().expect("plan").prerequisite.diagnosis_id =
                        DiagnosisId::PerformanceRulesInvalid
                }
                Self::PlanOwnership => {
                    plan.as_mut().expect("plan").prerequisite.ownership = OwnershipClass::Unknown
                }
                Self::PlanConfidence => {
                    plan.as_mut().expect("plan").prerequisite.confidence = GuardianConfidence::High
                }
                Self::PlanTarget => {
                    plan.as_mut().expect("plan").prerequisite.affected_targets =
                        vec![TargetDescriptor::new(
                            StabilizationSystem::State,
                            TargetKind::Config,
                            "different-target",
                            OwnershipClass::LauncherManaged,
                        )]
                }
                Self::CandidateOrder => plan
                    .as_mut()
                    .expect("plan")
                    .prerequisite
                    .candidate_actions
                    .reverse(),
                Self::ActionKind => {
                    plan.as_mut().expect("plan").actions[0].kind = GuardianActionKind::AskUser
                }
                Self::ActionTarget => plan.as_mut().expect("plan").actions[0].target = None,
                Self::ActionReason => {
                    plan.as_mut().expect("plan").actions[0].reason =
                        DiagnosisId::PerformanceRulesInvalid
                }
                Self::ExtraAction => {
                    let extra = plan.as_ref().expect("plan").actions[0].clone();
                    plan.as_mut().expect("plan").actions.push(extra);
                }
            }
            GuardianDecision::for_test(
                if matches!(self, Self::Operation) {
                    Some(OperationId::new("unexpected-operation"))
                } else {
                    operation_id
                },
                mode,
                action,
                diagnoses,
                plan,
            )
        }
    }

    #[test]
    fn exact_state_validator_rejects_directly_forged_plan_payloads() {
        let target = persisted_state_load_target();
        let prerequisite = ActionPlanPrerequisite {
            diagnosis_id: DiagnosisId::PersistedStateSchemaInvalid,
            ownership: OwnershipClass::LauncherManaged,
            confidence: GuardianConfidence::Confirmed,
            affected_targets: vec![target.clone()],
            candidate_actions: PERSISTED_STATE_REPAIR_CANDIDATES.to_vec(),
        };
        let plan = GuardianActionPlan::new(
            StabilizationSystem::Guardian,
            prerequisite,
            vec![GuardianAction {
                kind: GuardianActionKind::Quarantine,
                target: Some(target),
                reason: DiagnosisId::PersistedStateSchemaInvalid,
            }],
        );
        let decision = GuardianDecision::for_test(
            None,
            GuardianMode::Managed,
            GuardianActionKind::Quarantine,
            vec![DiagnosisId::PersistedStateSchemaInvalid],
            Some(plan),
        );
        let (proof, _) = valid_policy(GuardianMode::Managed);
        let (root, eligibility) = eligibility_fixture("direct-state-validation");
        let authorization =
            authorize_persisted_state_rejected_record_quarantine(eligibility, proof, &decision)
                .expect("exact sealed proof and decision");
        drop(authorization);
        fs::remove_dir_all(root).expect("remove direct-state-validation fixture");
    }
}
