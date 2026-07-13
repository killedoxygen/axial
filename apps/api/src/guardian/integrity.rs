use super::{DiagnosisId, GuardianFact, diagnose, guardian_fact_from_execution};
use crate::execution::ExecutionFact;
use crate::state::contracts::{OperationId, OperationPhase};
use crate::state::{MAX_OPERATION_JOURNAL_DIAGNOSES, MAX_OPERATION_JOURNAL_STEP_FACTS};
use std::collections::HashSet;

pub(crate) const TIER2_INTEGRITY_COUNTER_TOKEN_COUNT: usize = 9;
const MAX_TIER2_INTEGRITY_FACT_IDS: usize =
    MAX_OPERATION_JOURNAL_STEP_FACTS - TIER2_INTEGRITY_COUNTER_TOKEN_COUNT;

pub(crate) struct Tier2IntegrityGuardianEvidence {
    fact_ids: Vec<String>,
    diagnosis_ids: Vec<DiagnosisId>,
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

fn tier2_guardian_fact(operation_id: &OperationId, fact: &ExecutionFact) -> GuardianFact {
    let mut fact = fact.clone();
    fact.operation_id = Some(operation_id.clone());
    guardian_fact_from_execution(&fact, OperationPhase::Validating)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::{ExecutionFactKind, ExecutionFactSemantics};
    use crate::guardian::GuardianFactId;
    use crate::observability::{EvidenceField, EvidenceSensitivity};
    use crate::state::contracts::{
        OwnershipClass, StabilizationSystem, TargetDescriptor, TargetKind,
    };

    fn execution_fact(kind: ExecutionFactKind) -> ExecutionFact {
        ExecutionFact {
            operation_id: Some(OperationId::new("foreign-operation")),
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
        let operation_id = OperationId::new("integrity-sweep-exact");

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
        let operation_id = OperationId::new("integrity-sweep-dedup");
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
            tier2_integrity_guardian_evidence(&OperationId::new("integrity-sweep-healthy"), &[]);

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
            tier2_integrity_guardian_evidence(&OperationId::new("integrity-sweep-capped"), &facts);

        assert!(evidence.fact_ids().len() <= MAX_TIER2_INTEGRITY_FACT_IDS);
        assert!(evidence.diagnosis_ids().len() <= MAX_OPERATION_JOURNAL_DIAGNOSES);
        assert_eq!(
            evidence.fact_ids().iter().collect::<HashSet<_>>().len(),
            evidence.fact_ids().len()
        );
    }

    #[test]
    fn tier_two_mapping_covers_findings_and_primitive_refusal() {
        let operation_id = OperationId::new("integrity-sweep-mapping");
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
}
