use super::{
    DiagnosisId, FactReliability, GuardianDecision, GuardianFact, GuardianFactId, GuardianMode,
    GuardianPolicyContext, build_safety_case, decide_guardian_policy, diagnose,
    guardian_fact_from_execution,
};
use crate::execution::{ExecutionFact, ExecutionFactKind};
use crate::state::contracts::{OperationId, OperationPhase, OwnershipClass};
use crate::state::{
    MAX_OPERATION_JOURNAL_DIAGNOSES, MAX_OPERATION_JOURNAL_STEP_FACTS,
    RegisteredArtifactRepairCandidate,
};
use std::collections::HashSet;

pub(crate) const TIER2_INTEGRITY_COUNTER_TOKEN_COUNT: usize = 9;
const MAX_TIER2_INTEGRITY_FACT_IDS: usize =
    MAX_OPERATION_JOURNAL_STEP_FACTS - TIER2_INTEGRITY_COUNTER_TOKEN_COUNT;

pub(crate) struct Tier2IntegrityGuardianEvidence {
    fact_ids: Vec<String>,
    diagnosis_ids: Vec<DiagnosisId>,
}

pub(crate) struct Tier2RegisteredArtifactAssessment {
    decision: GuardianDecision,
}

impl Tier2RegisteredArtifactAssessment {
    pub(crate) const fn decision(&self) -> &GuardianDecision {
        &self.decision
    }
}

impl Tier2IntegrityGuardianEvidence {
    pub(crate) fn empty() -> Self {
        Self {
            fact_ids: Vec::new(),
            diagnosis_ids: Vec::new(),
        }
    }

    pub(crate) fn fact_ids(&self) -> &[String] {
        &self.fact_ids
    }

    pub(crate) fn diagnosis_ids(&self) -> &[DiagnosisId] {
        &self.diagnosis_ids
    }
}

pub(crate) fn tier2_integrity_guardian_evidence(
    operation_id: &OperationId,
    execution_facts: &[ExecutionFact],
) -> Tier2IntegrityGuardianEvidence {
    let mut seen_fact_ids = HashSet::new();
    let facts = execution_facts
        .iter()
        .filter_map(|fact| {
            let fact = tier2_guardian_fact(operation_id, fact);
            seen_fact_ids.insert(fact.id).then_some(fact)
        })
        .take(MAX_TIER2_INTEGRITY_FACT_IDS)
        .collect::<Vec<_>>();
    let fact_ids = facts
        .iter()
        .map(|fact| format!("guardian_fact:{}", fact.id.as_str()))
        .collect();
    let diagnosis_ids = if facts.is_empty() {
        Vec::new()
    } else {
        let mut seen_diagnosis_ids = HashSet::new();
        diagnose(&facts, OperationPhase::Validating)
            .into_iter()
            .map(|diagnosis| diagnosis.id())
            .filter(|diagnosis_id| seen_diagnosis_ids.insert(*diagnosis_id))
            .take(MAX_OPERATION_JOURNAL_DIAGNOSES)
            .collect()
    };
    Tier2IntegrityGuardianEvidence {
        fact_ids,
        diagnosis_ids,
    }
}

pub(crate) fn assess_tier2_registered_artifact_repair(
    operation_id: OperationId,
    mode: GuardianMode,
    execution_fact: &ExecutionFact,
    candidate: RegisteredArtifactRepairCandidate<'_>,
) -> Option<Tier2RegisteredArtifactAssessment> {
    if !matches!(
        execution_fact.kind,
        ExecutionFactKind::ArtifactHashMismatch
            | ExecutionFactKind::ArtifactMissing
            | ExecutionFactKind::ArtifactSizeDrift
    ) || execution_fact.target.as_ref() != Some(candidate.target())
        || candidate.target().ownership != OwnershipClass::LauncherManaged
    {
        return None;
    }

    let phase = OperationPhase::Validating;
    let mut finding = tier2_guardian_fact(&operation_id, execution_fact);
    finding.domain = candidate.domain();
    let available = GuardianFact {
        operation_id: Some(operation_id.clone()),
        id: GuardianFactId::RegisteredArtifactRepairAvailable,
        domain: candidate.domain(),
        phase,
        reliability: FactReliability::DirectStructured,
        severity: None,
        confidence: None,
        ownership: OwnershipClass::LauncherManaged,
        target: Some(candidate.target().clone()),
        fields: Vec::new(),
    };
    let safety_case = build_safety_case(Some(operation_id), mode, phase, &[finding, available]);
    Some(Tier2RegisteredArtifactAssessment {
        decision: decide_guardian_policy(&safety_case, GuardianPolicyContext::current_operation()),
    })
}

fn tier2_guardian_fact(operation_id: &OperationId, fact: &ExecutionFact) -> GuardianFact {
    let mut fact = fact.clone();
    fact.operation_id = Some(operation_id.clone());
    guardian_fact_from_execution(&fact, OperationPhase::Validating)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::{ExecutionFactKind, ExecutionFactSemantics};
    use crate::guardian::{GuardianActionKind, GuardianDomain};
    use crate::observability::{EvidenceField, EvidenceSensitivity};
    use crate::state::contracts::{
        OwnershipClass, StabilizationSystem, TargetDescriptor, TargetKind,
    };

    fn execution_fact(kind: ExecutionFactKind) -> ExecutionFact {
        ExecutionFact {
            operation_id: Some(OperationId::deterministic_test("foreign-operation")),
            kind,
            target: Some(TargetDescriptor::new(
                StabilizationSystem::Execution,
                TargetKind::Artifact,
                "known_good_artifact",
                OwnershipClass::LauncherManaged,
            )),
            fields: vec![EvidenceField::new(
                "path",
                "/private/library/secret.jar",
                EvidenceSensitivity::Sensitive,
            )],
        }
    }

    #[test]
    fn tier_two_evidence_attaches_exact_operation_and_redacts_fields() {
        let operation_id = OperationId::deterministic_test("integrity-sweep-exact");

        let fact = tier2_guardian_fact(
            &operation_id,
            &execution_fact(ExecutionFactKind::ArtifactHashMismatch),
        );

        assert_eq!(fact.operation_id, Some(operation_id));
        assert_eq!(fact.phase, OperationPhase::Validating);
        assert!(fact.fields.is_empty());
    }

    #[test]
    fn tier_two_evidence_deduplicates_before_diagnosis() {
        let operation_id = OperationId::deterministic_test("integrity-sweep-dedup");
        let facts = [
            execution_fact(ExecutionFactKind::ArtifactHashMismatch),
            execution_fact(ExecutionFactKind::ArtifactHashMismatch),
            execution_fact(ExecutionFactKind::ArtifactMissing),
        ];

        let evidence = tier2_integrity_guardian_evidence(&operation_id, &facts);

        assert_eq!(
            evidence.fact_ids(),
            &[
                format!(
                    "guardian_fact:{}",
                    GuardianFactId::ArtifactHashMismatch.as_str()
                ),
                format!("guardian_fact:{}", GuardianFactId::ArtifactMissing.as_str()),
            ]
        );
        assert_eq!(
            evidence.diagnosis_ids(),
            &[DiagnosisId::LauncherManagedArtifactCorrupt]
        );
    }

    #[test]
    fn empty_tier_two_evidence_has_no_unknown_diagnosis() {
        let evidence =
            tier2_integrity_guardian_evidence(&OperationId::deterministic_test("integrity-sweep-healthy"), &[]);

        assert!(evidence.fact_ids().is_empty());
        assert!(evidence.diagnosis_ids().is_empty());
    }

    #[test]
    fn tier_two_evidence_stays_within_journal_caps() {
        let facts = ExecutionFactKind::ALL
            .iter()
            .copied()
            .filter(|kind| kind.semantics() == ExecutionFactSemantics::Diagnostic)
            .cycle()
            .take(MAX_OPERATION_JOURNAL_STEP_FACTS * 3)
            .map(execution_fact)
            .collect::<Vec<_>>();

        let evidence =
            tier2_integrity_guardian_evidence(&OperationId::deterministic_test("integrity-sweep-capped"), &facts);

        assert!(evidence.fact_ids().len() <= MAX_TIER2_INTEGRITY_FACT_IDS);
        assert!(evidence.diagnosis_ids().len() <= MAX_OPERATION_JOURNAL_DIAGNOSES);
        assert_eq!(
            evidence.fact_ids().iter().collect::<HashSet<_>>().len(),
            evidence.fact_ids().len()
        );
    }

    #[test]
    fn tier_two_mapping_covers_findings_and_primitive_refusal() {
        let operation_id = OperationId::deterministic_test("integrity-sweep-mapping");
        let kinds = [
            (
                ExecutionFactKind::ArtifactMissing,
                GuardianFactId::ArtifactMissing,
            ),
            (
                ExecutionFactKind::ArtifactHashMismatch,
                GuardianFactId::ArtifactHashMismatch,
            ),
            (
                ExecutionFactKind::ArtifactSizeDrift,
                GuardianFactId::ArtifactSizeDrift,
            ),
            (
                ExecutionFactKind::FilePermissionDenied,
                GuardianFactId::FilesystemPermissionDenied,
            ),
            (
                ExecutionFactKind::PrimitiveRefused,
                GuardianFactId::PrimitiveRefused,
            ),
        ];

        for (kind, expected) in kinds {
            assert_eq!(
                tier2_guardian_fact(&operation_id, &execution_fact(kind)).id,
                expected
            );
        }
    }

    #[test]
    fn exact_tier_two_registered_artifact_assessment_respects_guardian_modes() {
        let target = TargetDescriptor::new(
            StabilizationSystem::Execution,
            TargetKind::Artifact,
            "leaf-v2.01234567.89abcdef.01234567.89abcdef.01234567.89abcdef.01234567.89abcdef",
            OwnershipClass::LauncherManaged,
        );
        let fact = ExecutionFact {
            operation_id: None,
            kind: ExecutionFactKind::ArtifactHashMismatch,
            target: Some(target.clone()),
            fields: Vec::new(),
        };

        for (mode, expected) in [
            (GuardianMode::Managed, GuardianActionKind::Repair),
            (GuardianMode::Custom, GuardianActionKind::AskUser),
            (GuardianMode::Disabled, GuardianActionKind::RecordOnly),
        ] {
            let assessment = assess_tier2_registered_artifact_repair(
                OperationId::deterministic_test("tier-two-registered-artifact"),
                mode,
                &fact,
                RegisteredArtifactRepairCandidate::for_test(&target, GuardianDomain::Download),
            )
            .expect("exact registered artifact assessment");

            assert_eq!(assessment.decision().kind(), expected);
            assert_eq!(
                assessment.decision().operation_id(),
                Some(&OperationId::deterministic_test("tier-two-registered-artifact"))
            );
            assert_eq!(
                assessment
                    .decision()
                    .action_plan()
                    .expect("registered artifact plan")
                    .prerequisite
                    .affected_targets,
                vec![target.clone()]
            );
        }
    }

    #[test]
    fn tier_two_registered_artifact_assessment_rejects_a_fabricated_target() {
        let target = TargetDescriptor::new(
            StabilizationSystem::Execution,
            TargetKind::Artifact,
            "leaf-v2.01234567.89abcdef.01234567.89abcdef.01234567.89abcdef.01234567.89abcdef",
            OwnershipClass::LauncherManaged,
        );
        let fabricated = TargetDescriptor::new(
            StabilizationSystem::Execution,
            TargetKind::Artifact,
            "leaf-v2.00000000.00000000.00000000.00000000.00000000.00000000.00000000.00000000",
            OwnershipClass::LauncherManaged,
        );
        let fact = ExecutionFact {
            operation_id: None,
            kind: ExecutionFactKind::ArtifactMissing,
            target: Some(fabricated),
            fields: Vec::new(),
        };

        assert!(
            assess_tier2_registered_artifact_repair(
                OperationId::deterministic_test("tier-two-fabricated-artifact"),
                GuardianMode::Managed,
                &fact,
                RegisteredArtifactRepairCandidate::for_test(&target, GuardianDomain::Download),
            )
            .is_none()
        );
    }
}
