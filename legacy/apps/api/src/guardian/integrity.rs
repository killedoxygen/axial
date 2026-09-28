use super::{
    FactReliability, GuardianDecision, GuardianFact, GuardianFactId, GuardianMode,
    GuardianPolicyContext, OperationEvidenceBatch, OperationEvidenceBatchRejection,
    assess_operation_evidence,
};
use crate::execution::{ExecutionFact, ExecutionFactKind};
use crate::state::RegisteredArtifactRepairCandidate;
use crate::state::contracts::DurableGuardianEvidence;
use crate::state::contracts::{OperationId, OperationPhase, OwnershipClass};

pub(crate) struct Tier2IntegrityGuardianEvidence {
    durable: Option<DurableGuardianEvidence>,
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
        Self { durable: None }
    }

    pub(crate) fn durable(&self) -> Option<&DurableGuardianEvidence> {
        self.durable.as_ref()
    }
}

pub(crate) fn try_tier2_integrity_guardian_evidence(
    operation_id: &OperationId,
    execution_facts: &[ExecutionFact],
) -> Result<Tier2IntegrityGuardianEvidence, OperationEvidenceBatchRejection> {
    let evidence = OperationEvidenceBatch::try_from_execution_operation(
        operation_id,
        OperationPhase::Validating,
        execution_facts,
    )?;
    if evidence.facts().is_empty() {
        return Ok(Tier2IntegrityGuardianEvidence::empty());
    }
    let assessment = assess_operation_evidence(
        GuardianMode::Managed,
        OperationPhase::Validating,
        &evidence,
        GuardianPolicyContext::current_operation(),
    );
    let durable = assessment
        .durable_evidence(&evidence, None)
        .map_err(|_| OperationEvidenceBatchRejection::SerializationFailed)?;
    Ok(Tier2IntegrityGuardianEvidence {
        durable: Some(durable),
    })
}

#[cfg(test)]
pub(crate) fn tier2_integrity_guardian_evidence(
    operation_id: &OperationId,
    execution_facts: &[ExecutionFact],
) -> Tier2IntegrityGuardianEvidence {
    try_tier2_integrity_guardian_evidence(operation_id, execution_facts)
        .expect("valid Tier 2 operation evidence")
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
    let mapped = OperationEvidenceBatch::try_from_execution_operation(
        &operation_id,
        phase,
        std::slice::from_ref(execution_fact),
    )
    .ok()?;
    let mut finding = mapped.facts().first()?.clone();
    finding.domain = candidate.domain();
    let available = GuardianFact {
        operation_id: None,
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
    let evidence = OperationEvidenceBatch::try_from_trusted_guardian_operation(
        operation_id,
        vec![finding, available],
    )
    .ok()?;
    let assessment = assess_operation_evidence(
        mode,
        phase,
        &evidence,
        GuardianPolicyContext::current_operation(),
    );
    Some(Tier2RegisteredArtifactAssessment {
        decision: assessment.decision().clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::ExecutionFactKind;
    use crate::guardian::{GuardianActionKind, GuardianDomain};
    use crate::observability::{EvidenceField, EvidenceSensitivity};
    use crate::state::contracts::{
        OwnershipClass, StabilizationSystem, TargetDescriptor, TargetKind,
    };

    fn execution_fact(operation_id: &OperationId, kind: ExecutionFactKind) -> ExecutionFact {
        ExecutionFact {
            operation_id: Some(operation_id.clone()),
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

        let execution_fact = execution_fact(&operation_id, ExecutionFactKind::ArtifactHashMismatch);
        let batch = OperationEvidenceBatch::try_from_execution_operation(
            &operation_id,
            OperationPhase::Validating,
            std::slice::from_ref(&execution_fact),
        )
        .expect("exact operation batch");
        let fact = batch.facts().first().expect("mapped fact");

        assert_eq!(batch.operation_id(), Some(&operation_id));
        assert_eq!(fact.operation_id, None);
        assert_eq!(fact.phase, OperationPhase::Validating);
        assert!(fact.fields.is_empty());
    }

    #[test]
    fn tier_two_evidence_deduplicates_before_diagnosis() {
        let operation_id = OperationId::deterministic_test("integrity-sweep-dedup");
        let facts = [
            execution_fact(&operation_id, ExecutionFactKind::ArtifactHashMismatch),
            execution_fact(&operation_id, ExecutionFactKind::ArtifactHashMismatch),
            execution_fact(&operation_id, ExecutionFactKind::ArtifactMissing),
        ];

        let evidence = tier2_integrity_guardian_evidence(&operation_id, &facts);
        let durable = evidence.durable().expect("typed durable evidence");

        assert_eq!(
            durable.fact_ids(),
            &[
                GuardianFactId::ArtifactHashMismatch,
                GuardianFactId::ArtifactMissing,
            ]
        );
        assert_eq!(
            durable.diagnosis_ids(),
            &[crate::guardian::DiagnosisId::LauncherManagedArtifactCorrupt]
        );
    }

    #[test]
    fn empty_tier_two_evidence_has_no_unknown_diagnosis() {
        let evidence = tier2_integrity_guardian_evidence(
            &OperationId::deterministic_test("integrity-sweep-healthy"),
            &[],
        );

        assert!(evidence.durable().is_none());
    }

    #[test]
    fn tier_two_evidence_rejects_foreign_operation_provenance() {
        let operation_id = OperationId::deterministic_test("integrity-sweep-current");
        let foreign = OperationId::deterministic_test("integrity-sweep-foreign");
        let fact = execution_fact(&foreign, ExecutionFactKind::ArtifactMissing);

        assert!(matches!(
            try_tier2_integrity_guardian_evidence(&operation_id, &[fact]),
            Err(OperationEvidenceBatchRejection::ForeignOperation { fact_index: 0 })
        ));
    }

    #[test]
    fn tier_two_evidence_rejects_over_cap_corpus() {
        let operation_id = OperationId::deterministic_test("integrity-sweep-capped");
        let facts = (0..=crate::guardian::MAX_OPERATION_EVIDENCE_FACTS)
            .map(|_| execution_fact(&operation_id, ExecutionFactKind::ArtifactMissing))
            .collect::<Vec<_>>();

        assert!(matches!(
            try_tier2_integrity_guardian_evidence(&operation_id, &facts),
            Err(OperationEvidenceBatchRejection::TooManyFacts)
        ));
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
            let fact = execution_fact(&operation_id, kind);
            let batch = OperationEvidenceBatch::try_from_execution_operation(
                &operation_id,
                OperationPhase::Validating,
                std::slice::from_ref(&fact),
            )
            .expect("mapped Tier 2 batch");
            assert_eq!(batch.facts().first().expect("mapped fact").id, expected);
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
        let operation_id = OperationId::deterministic_test("tier-two-registered-artifact");
        let fact = ExecutionFact {
            operation_id: Some(operation_id.clone()),
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
                operation_id.clone(),
                mode,
                &fact,
                RegisteredArtifactRepairCandidate::for_test(&target, GuardianDomain::Download),
            )
            .expect("exact registered artifact assessment");

            assert_eq!(assessment.decision().kind(), expected);
            assert_eq!(
                assessment.decision().operation_id(),
                Some(&OperationId::deterministic_test(
                    "tier-two-registered-artifact"
                ))
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
        let operation_id = OperationId::deterministic_test("tier-two-fabricated-artifact");
        let fact = ExecutionFact {
            operation_id: Some(operation_id.clone()),
            kind: ExecutionFactKind::ArtifactMissing,
            target: Some(fabricated),
            fields: Vec::new(),
        };

        assert!(
            assess_tier2_registered_artifact_repair(
                operation_id,
                GuardianMode::Managed,
                &fact,
                RegisteredArtifactRepairCandidate::for_test(&target, GuardianDomain::Download),
            )
            .is_none()
        );
    }
}
