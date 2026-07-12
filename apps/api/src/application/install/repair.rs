use super::operation::latest_generated_fact_value;
use super::{
    InstallGuardianRepairSummary, InstallJournalReconciliation,
    reconcile_install_journal_transition,
};
use crate::guardian::{
    DiagnosisId, GuardianArtifactRepairMutation, GuardianArtifactRepairOutcome,
    GuardianArtifactRepairRequest, GuardianArtifactRepairStatus, GuardianCopyRequest,
    GuardianInstallArtifactFailureEvidence, GuardianInstallArtifactRepairPlanKind,
    GuardianMinecraftArtifactRepairDescriptor, GuardianMode, author_guardian_copy,
    execute_guardian_artifact_repair, install_artifact_failure_from_minecraft_download_fact,
    plan_install_artifact_failure_repair,
};
use crate::observability::{RedactionAudience, sanitize_evidence_token};
use crate::state::contracts::{
    CommandKind, OperationId, OperationJournalEntry, OperationPhase, OwnershipClass,
    StabilizationSystem,
};
use crate::state::{GuardianFailureMemoryStore, OperationJournalStore, OperationJournalStoreError};
use axial_minecraft::download::{
    ExecutionDownloadFact, ExecutionDownloadFactKind, SelectedDownloadArtifactDescriptor,
};
use reqwest::Client;

const REPAIR_OPERATION_FACT_PREFIX: &str = "guardian_repair_operation:";
const REPAIR_STATUS_FACT_PREFIX: &str = "guardian_repair_status:";
const REPAIR_SUMMARY_FACT_PREFIX: &str = "guardian_repair_summary:";

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
        label: outcome.summary,
        detail: outcome.details.first().cloned(),
    })
}

pub async fn repair_install_artifact_corruption_with_guardian(
    journals: &OperationJournalStore,
    failure_memory: &GuardianFailureMemoryStore,
    client: &Client,
    operation_id: &OperationId,
    facts: &[ExecutionDownloadFact],
    descriptors: &[SelectedDownloadArtifactDescriptor],
    observed_at: &str,
) -> Result<Option<GuardianArtifactRepairOutcome>, OperationJournalStoreError> {
    let Some(repair) = first_repairable_install_artifact(facts, descriptors, operation_id) else {
        return Ok(None);
    };
    let destination_missing = repair
        .descriptor
        .destination()
        .try_exists()
        .is_ok_and(|exists| !exists);
    let plan_kind = if repair.evidence.kind
        == crate::guardian::GuardianInstallArtifactFailureKind::ArtifactMissing
        || destination_missing
    {
        GuardianInstallArtifactRepairPlanKind::MissingArtifact
    } else {
        GuardianInstallArtifactRepairPlanKind::ExistingArtifact
    };
    let Ok(plan) = plan_install_artifact_failure_repair(
        Some(operation_id.clone()),
        GuardianMode::Managed,
        OperationPhase::Downloading,
        std::slice::from_ref(&repair.evidence),
        plan_kind,
    ) else {
        return Ok(None);
    };

    let request = GuardianArtifactRepairRequest {
        operation_id: None,
        plan: &plan,
        destination: repair.descriptor.destination(),
        source: repair.descriptor.repair_source(),
        client,
        journals,
        failure_memory,
        mode: GuardianMode::Managed,
        observed_at,
        mutation: if destination_missing {
            GuardianArtifactRepairMutation::DownloadMissing
        } else {
            GuardianArtifactRepairMutation::QuarantineExisting
        },
    };

    execute_guardian_artifact_repair(request).await.map(Some)
}

struct RepairableInstallArtifact {
    descriptor: GuardianMinecraftArtifactRepairDescriptor,
    evidence: GuardianInstallArtifactFailureEvidence,
}

fn first_repairable_install_artifact(
    facts: &[ExecutionDownloadFact],
    descriptors: &[SelectedDownloadArtifactDescriptor],
    operation_id: &OperationId,
) -> Option<RepairableInstallArtifact> {
    facts
        .iter()
        .filter(|fact| repairable_install_artifact_fact_kind(fact.kind))
        .filter(|fact| !artifact_missing_shadowed_by_terminal_failure(fact, facts))
        .filter_map(|fact| {
            let descriptor = descriptors
                .iter()
                .find(|descriptor| descriptor.target == fact.target)?;
            let descriptor =
                GuardianMinecraftArtifactRepairDescriptor::from_core_selected_descriptor(
                    descriptor,
                )
                .ok()?;
            let evidence = install_artifact_failure_from_minecraft_download_fact(
                Some(operation_id.clone()),
                OwnershipClass::LauncherManaged,
                fact,
            )?;
            Some(RepairableInstallArtifact {
                descriptor,
                evidence,
            })
        })
        .next()
}

fn repairable_install_artifact_fact_kind(kind: ExecutionDownloadFactKind) -> bool {
    matches!(
        kind,
        ExecutionDownloadFactKind::ArtifactMissing
            | ExecutionDownloadFactKind::ChecksumMismatch
            | ExecutionDownloadFactKind::SizeMismatch
    )
}

fn artifact_missing_shadowed_by_terminal_failure(
    fact: &ExecutionDownloadFact,
    facts: &[ExecutionDownloadFact],
) -> bool {
    fact.kind == ExecutionDownloadFactKind::ArtifactMissing
        && facts.iter().any(|candidate| {
            candidate.kind != ExecutionDownloadFactKind::ArtifactMissing
                && terminal_download_failure_fact_kind(candidate.kind)
        })
}

fn terminal_download_failure_fact_kind(kind: ExecutionDownloadFactKind) -> bool {
    matches!(
        kind,
        ExecutionDownloadFactKind::ChecksumMismatch
            | ExecutionDownloadFactKind::MetadataInvalid
            | ExecutionDownloadFactKind::MetadataMissing
            | ExecutionDownloadFactKind::Interrupted
            | ExecutionDownloadFactKind::NetworkFailure
            | ExecutionDownloadFactKind::OwnershipRefused
            | ExecutionDownloadFactKind::PermissionFailure
            | ExecutionDownloadFactKind::PromoteFailed
            | ExecutionDownloadFactKind::ProviderFailure
            | ExecutionDownloadFactKind::SizeMismatch
            | ExecutionDownloadFactKind::TempWriteFailed
    )
}

#[cfg(test)]
mod tests {
    use super::install_guardian_repair_summary_from_journal;
    use crate::guardian::DiagnosisId;
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
}
