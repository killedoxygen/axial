use super::operation::latest_generated_fact_value;
use super::{
    InstallGuardianRepairSummary, InstallJournalReconciliation,
    reconcile_install_journal_transition,
};
use crate::guardian::{
    DiagnosisId, GuardianActionKind, GuardianArtifactRepairOutcome, GuardianArtifactRepairStatus,
    GuardianCopyRequest, GuardianInstallArtifactFailureEvidence,
    GuardianInstallArtifactFailureKind, GuardianInstallAssessment,
    GuardianMinecraftArtifactRepairDescriptor, author_guardian_copy,
    execute_guardian_missing_download, execute_guardian_quarantine_redownload,
};
use crate::observability::{RedactionAudience, sanitize_evidence_token};
use crate::state::contracts::{
    CommandKind, OperationId, OperationJournalEntry, OwnershipClass, StabilizationSystem,
};
use crate::state::{GuardianFailureMemoryStore, OperationJournalStore, OperationJournalStoreError};
use axial_minecraft::download::SelectedDownloadArtifactDescriptor;
use reqwest::Client;

const REPAIR_OPERATION_FACT_PREFIX: &str = "guardian_repair_operation:";
const REPAIR_STATUS_FACT_PREFIX: &str = "guardian_repair_status:";
const REPAIR_SUMMARY_FACT_PREFIX: &str = "guardian_repair_summary:";

#[derive(Default)]
pub(super) struct InstallRepairResume {
    resumed: bool,
}

impl InstallRepairResume {
    pub(super) fn resume_after(&mut self, outcome: Option<&GuardianArtifactRepairOutcome>) -> bool {
        if self.resumed
            || !outcome
                .is_some_and(|outcome| outcome.status == GuardianArtifactRepairStatus::Repaired)
        {
            return false;
        }
        self.resumed = true;
        true
    }
}

pub async fn record_install_operation_guardian_repair_outcome(
    journals: &OperationJournalStore,
    operation_id: &OperationId,
    outcome: &GuardianArtifactRepairOutcome,
) -> Result<(), OperationJournalStoreError> {
    let repair_operation_id = sanitize_evidence_token(
        outcome.operation_id.as_str(),
        RedactionAudience::UserVisible,
        96,
    )
    .unwrap_or_else(|| "guardian-repair".to_string());
    let diagnosis_id = outcome.diagnosis_id;
    let summary = sanitize_evidence_token(&outcome.summary, RedactionAudience::UserVisible, 96)
        .unwrap_or_else(|| "guardian_artifact_repair".to_string());

    let facts = vec![
        format!("{REPAIR_OPERATION_FACT_PREFIX}{repair_operation_id}"),
        format!(
            "{REPAIR_STATUS_FACT_PREFIX}{}",
            outcome.status.as_persisted_id()
        ),
        format!("{REPAIR_SUMMARY_FACT_PREFIX}{summary}"),
    ];
    loop {
        match journals
            .record_guardian_evidence(operation_id, facts.clone(), vec![diagnosis_id])
            .await
        {
            Ok(()) => return Ok(()),
            Err(error) => {
                let reconciliation =
                    reconcile_install_journal_transition(journals, operation_id, error, |entry| {
                        entry.operation_id == *operation_id
                            && entry.command == CommandKind::InstallVersion
                            && entry.owner == StabilizationSystem::Application
                            && entry.ownership == OwnershipClass::LauncherManaged
                            && entry.completed_steps.last().is_some_and(|step| {
                                facts.iter().all(|fact| {
                                    step.generated_facts.iter().any(|existing| existing == fact)
                                })
                            })
                            && entry.guardian_diagnosis_ids.contains(&diagnosis_id)
                    })
                    .await?;
                if matches!(
                    reconciliation,
                    InstallJournalReconciliation::MutationCommitted
                ) {
                    return Ok(());
                }
            }
        }
    }
}

pub fn install_guardian_repair_summary_from_journal(
    entry: &OperationJournalEntry,
) -> Option<InstallGuardianRepairSummary> {
    let repair_operation_id = latest_generated_fact_value(entry, REPAIR_OPERATION_FACT_PREFIX)?;
    let status_id = latest_generated_fact_value(entry, REPAIR_STATUS_FACT_PREFIX)?;
    let status = GuardianArtifactRepairStatus::from_persisted_id(&status_id)?;
    let diagnosis_id = entry
        .guardian_diagnosis_ids
        .iter()
        .copied()
        .rev()
        .find(|diagnosis_id| *diagnosis_id == DiagnosisId::LauncherManagedArtifactCorrupt)?;
    let outcome = author_guardian_copy(GuardianCopyRequest::artifact_repair(diagnosis_id, status))?;

    Some(InstallGuardianRepairSummary {
        repair_operation_id: OperationId::new(repair_operation_id),
        diagnosis_id,
        status: status.as_persisted_id().to_string(),
        label: outcome.summary().to_string(),
        detail: outcome.details().first().cloned(),
    })
}

pub(super) async fn repair_install_artifact_corruption_with_guardian(
    journals: &OperationJournalStore,
    failure_memory: &GuardianFailureMemoryStore,
    client: &Client,
    assessment: GuardianInstallAssessment,
    evidence: &[GuardianInstallArtifactFailureEvidence],
    descriptors: &[SelectedDownloadArtifactDescriptor],
    observed_at: &str,
) -> Result<Option<GuardianArtifactRepairOutcome>, OperationJournalStoreError> {
    if assessment.decision_kind() != GuardianActionKind::Repair {
        return Ok(None);
    }
    let Some(repair) = first_repairable_install_artifact(&assessment, evidence, descriptors) else {
        return Ok(None);
    };
    let destination_missing = repair
        .descriptor
        .destination()
        .try_exists()
        .is_ok_and(|exists| !exists);
    let missing_download =
        repair.kind == GuardianInstallArtifactFailureKind::ArtifactMissing || destination_missing;
    let outcome = if missing_download {
        let Ok(authorization) = assessment.into_missing_repair_authorization(repair.descriptor)
        else {
            return Ok(None);
        };
        execute_guardian_missing_download(
            authorization,
            None,
            client,
            journals,
            failure_memory,
            observed_at,
        )
        .await
    } else {
        let Ok(authorization) = assessment.into_existing_repair_authorization(repair.descriptor)
        else {
            return Ok(None);
        };
        execute_guardian_quarantine_redownload(
            authorization,
            None,
            client,
            journals,
            failure_memory,
            observed_at,
        )
        .await
    };
    outcome.map(Some)
}

struct RepairableInstallArtifact {
    descriptor: GuardianMinecraftArtifactRepairDescriptor,
    kind: GuardianInstallArtifactFailureKind,
}

fn first_repairable_install_artifact(
    assessment: &GuardianInstallAssessment,
    evidence: &[GuardianInstallArtifactFailureEvidence],
    descriptors: &[SelectedDownloadArtifactDescriptor],
) -> Option<RepairableInstallArtifact> {
    let repair_target = assessment.repair_target()?;
    evidence
        .iter()
        .filter(|evidence| repairable_install_artifact_kind(evidence.kind))
        .filter(|candidate| {
            candidate.kind != GuardianInstallArtifactFailureKind::ArtifactMissing
                || !evidence
                    .iter()
                    .any(|other| other.kind != GuardianInstallArtifactFailureKind::ArtifactMissing)
        })
        .filter_map(|evidence| {
            let descriptor = descriptors
                .iter()
                .find(|descriptor| descriptor.target == evidence.target_id)?;
            let descriptor =
                GuardianMinecraftArtifactRepairDescriptor::from_core_selected_descriptor(
                    descriptor,
                )
                .ok()?;
            if descriptor.target() != repair_target {
                return None;
            }
            Some(RepairableInstallArtifact {
                descriptor,
                kind: evidence.kind,
            })
        })
        .next()
}

fn repairable_install_artifact_kind(kind: GuardianInstallArtifactFailureKind) -> bool {
    matches!(
        kind,
        GuardianInstallArtifactFailureKind::ArtifactMissing
            | GuardianInstallArtifactFailureKind::ChecksumMismatch
            | GuardianInstallArtifactFailureKind::SizeMismatch
    )
}

#[cfg(test)]
mod tests {
    use super::{InstallRepairResume, install_guardian_repair_summary_from_journal};
    use crate::guardian::{
        DiagnosisId, GuardianActionKind, GuardianArtifactRepairOutcome,
        GuardianArtifactRepairStatus,
    };
    use crate::state::contracts::{
        CommandKind, JournalId, OperationId, OperationJournalEntry, OperationJournalStep,
        OperationPhase, OperationStepResult, OwnershipClass, RollbackState, StabilizationSystem,
    };

    #[test]
    fn malformed_persisted_artifact_repair_status_has_no_public_summary() {
        let operation_id = OperationId::new("install-operation");
        let mut entry = OperationJournalEntry::new(
            JournalId::new("install-journal"),
            operation_id,
            CommandKind::InstallVersion,
            StabilizationSystem::Application,
            OwnershipClass::LauncherManaged,
            RollbackState::NotApplicable,
        );
        let mut step = OperationJournalStep::new("guardian-repair", OperationPhase::Repairing);
        step.result = OperationStepResult::Completed;
        step.generated_facts = vec![
            "guardian_repair_operation:repair-operation".to_string(),
            "guardian_repair_status:legacy_repaired".to_string(),
            "guardian_repair_summary:repair-summary".to_string(),
        ];
        entry.completed_steps.push(step);
        entry
            .guardian_diagnosis_ids
            .push(DiagnosisId::LauncherManagedArtifactCorrupt);

        assert!(install_guardian_repair_summary_from_journal(&entry).is_none());
    }

    #[test]
    fn install_repair_resume_is_spent_only_by_the_first_successful_repair() {
        for status in [
            GuardianArtifactRepairStatus::Blocked,
            GuardianArtifactRepairStatus::Failed,
            GuardianArtifactRepairStatus::Suppressed,
        ] {
            let mut resume = InstallRepairResume::default();
            assert!(!resume.resume_after(None));
            assert!(!resume.resume_after(Some(&repair_outcome(status))));
            assert!(resume.resume_after(Some(&repair_outcome(
                GuardianArtifactRepairStatus::Repaired
            ))));
            assert!(!resume.resume_after(Some(&repair_outcome(
                GuardianArtifactRepairStatus::Repaired
            ))));
        }
    }

    #[test]
    fn install_repair_resume_allows_exactly_one_rerun_after_repeated_repaired_outcomes() {
        let mut resume = InstallRepairResume::default();
        let repaired = repair_outcome(GuardianArtifactRepairStatus::Repaired);

        assert!(resume.resume_after(Some(&repaired)));
        assert!(!resume.resume_after(Some(&repaired)));
        assert!(!resume.resume_after(None));
    }

    fn repair_outcome(status: GuardianArtifactRepairStatus) -> GuardianArtifactRepairOutcome {
        GuardianArtifactRepairOutcome {
            operation_id: OperationId::new("guardian-repair"),
            diagnosis_id: DiagnosisId::LauncherManagedArtifactCorrupt,
            action: GuardianActionKind::Repair,
            status,
            facts: Vec::new(),
            summary: "guardian_artifact_repair".to_string(),
        }
    }
}
