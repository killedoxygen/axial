use super::{
    INSTALL_FAILURE_MESSAGE, InstallJournalReconciliation, InstallProgressStepViewModel,
    InstallProgressViewModel, reconcile_install_journal_transition,
};
use crate::guardian::{
    DiagnosisId, GuardianActionKind, GuardianDomain, GuardianInstallArtifactFailureEvidence,
    GuardianInstallArtifactFailureKind, GuardianInstallAssessment,
    GuardianInstallOutcomeFactGroupParse, GuardianInstallOutcomeMemoryPersistence, GuardianMode,
    GuardianPolicyContext, assess_install_artifact_failure_with_context, diagnose,
    guardian_install_outcome_fact_group, guardian_install_outcome_from_persisted_group,
    guardian_install_outcome_persistence_facts,
    install_artifact_failure_from_minecraft_download_fact, install_artifact_failure_guardian_fact,
    install_artifact_failure_safety_case,
};
use crate::observability::{
    RedactionAudience, sanitize_evidence_token, sanitize_public_diagnostic_text,
};
use crate::state::contracts::{
    CommandKind, JournalId, OperationId, OperationJournalEntry, OperationJournalStep,
    OperationOutcome, OperationPhase, OperationStatus, OperationStepResult, OwnershipClass,
    RollbackState, StabilizationSystem, TargetDescriptor, TargetKind,
};
use crate::state::failure_memory::{
    FailureMemoryActionOutcome, FailureMemoryKey, FailureMemoryStoreError,
    GuardianFailureMemoryEntry,
};
use crate::state::{
    GuardianFailureMemoryStore, InstallProgressRecord, OperationJournalStore,
    OperationJournalStoreError, ProducerLease, operation_journal_completed_step_is_visible,
};
use axial_minecraft::LoaderInstallFailureKind;
use axial_minecraft::download::{ExecutionDownloadFact, ExecutionDownloadFactKind};
use axial_minecraft::loaders::LoaderActiveInstallFailure;
use axial_minecraft::{DownloadError, DownloadProgress, RuntimeSourceFailureKind};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::io;
use std::sync::Arc;
use std::time::Duration;
use tracing::warn;

const PROVIDER_FAILURE_SUPPRESSION_COOLDOWN_MINUTES: i64 = 5;
const PROVIDER_FAILURE_MEMORY_SOURCE: &str = "install_provider";
const INSTALL_GUARDIAN_MEMORY_RETRY_DELAY: Duration = Duration::from_millis(100);
const INSTALL_GUARDIAN_MEMORY_RETRY_ATTEMPTS: usize = 3;
const COALESCED_PROGRESS_EVENT_INTERVAL: usize = 25;
const ROSETTA_INSTALL_COMMAND: &str = "softwareupdate --install-rosetta --agree-to-license";
const ROSETTA_REQUIRED_INSTALL_GUIDANCE: &str = "Install Rosetta 2 by running `softwareupdate --install-rosetta --agree-to-license` in Terminal, then retry.";
const RUNTIME_UNAVAILABLE_INSTALL_FAILURE_MESSAGE_PREFIX: &str =
    "This Minecraft version needs a Java runtime that is not available for this device.";

#[derive(Clone, Debug, Eq, PartialEq)]
struct ProviderFailureObservationWindow {
    observed_at: String,
    suppression_until: String,
}

impl ProviderFailureObservationWindow {
    fn from_observed_at(observed_at: &str) -> Option<Self> {
        let observed_at = chrono::DateTime::parse_from_rfc3339(observed_at)
            .ok()?
            .with_timezone(&chrono::Utc);
        let suppression_until = observed_at.checked_add_signed(chrono::Duration::minutes(
            PROVIDER_FAILURE_SUPPRESSION_COOLDOWN_MINUTES,
        ))?;
        Some(Self {
            observed_at: observed_at.to_rfc3339(),
            suppression_until: suppression_until.to_rfc3339(),
        })
    }
}
const RUNTIME_ROSETTA_REQUIRED_INSTALL_FAILURE_MESSAGE_PREFIX: &str =
    "This Minecraft version needs Rosetta 2 on Apple Silicon Macs.";

#[derive(Default)]
pub(crate) struct ContentDownloadFactAccumulator {
    facts: Vec<ExecutionDownloadFact>,
    counts: [usize; 13],
}

#[derive(Debug, Default)]
pub struct InstallProgressJournalTracker {
    recorded_nonterminal_phases: BTreeSet<String>,
}

impl InstallProgressJournalTracker {
    fn contains(&self, phase: &str) -> bool {
        self.recorded_nonterminal_phases.contains(phase)
    }

    fn record(&mut self, phase: String) {
        self.recorded_nonterminal_phases.insert(phase);
    }
}

impl ContentDownloadFactAccumulator {
    pub(crate) fn record(&mut self, fact: ExecutionDownloadFact) {
        self.counts[execution_download_fact_kind_index(fact.kind)] =
            self.counts[execution_download_fact_kind_index(fact.kind)].saturating_add(1);
        self.facts.push(fact);
    }

    pub(crate) fn facts(&self) -> Vec<ExecutionDownloadFact> {
        self.facts.clone()
    }

    pub(crate) fn journal_facts(&self) -> Vec<String> {
        EXECUTION_DOWNLOAD_FACT_KINDS
            .iter()
            .zip(self.counts)
            .filter(|(_, count)| *count > 0)
            .map(|(kind, count)| {
                format!(
                    "execution_download_fact:{}:{count}",
                    execution_download_fact_kind_label(*kind)
                )
            })
            .collect()
    }
}

const EXECUTION_DOWNLOAD_FACT_KINDS: [ExecutionDownloadFactKind; 13] = [
    ExecutionDownloadFactKind::ChecksumMismatch,
    ExecutionDownloadFactKind::MetadataInvalid,
    ExecutionDownloadFactKind::MetadataMissing,
    ExecutionDownloadFactKind::Interrupted,
    ExecutionDownloadFactKind::NetworkFailure,
    ExecutionDownloadFactKind::PermissionFailure,
    ExecutionDownloadFactKind::PromoteFailed,
    ExecutionDownloadFactKind::ProviderFailure,
    ExecutionDownloadFactKind::SizeMismatch,
    ExecutionDownloadFactKind::TempDiscarded,
    ExecutionDownloadFactKind::TempWriteFailed,
    ExecutionDownloadFactKind::WrittenToTemp,
    ExecutionDownloadFactKind::Promoted,
];

fn execution_download_fact_kind_index(kind: ExecutionDownloadFactKind) -> usize {
    EXECUTION_DOWNLOAD_FACT_KINDS
        .iter()
        .position(|candidate| *candidate == kind)
        .expect("every execution download fact kind has a bounded journal slot")
}

const fn execution_download_fact_kind_label(kind: ExecutionDownloadFactKind) -> &'static str {
    match kind {
        ExecutionDownloadFactKind::ChecksumMismatch => "checksum_mismatch",
        ExecutionDownloadFactKind::MetadataInvalid => "metadata_invalid",
        ExecutionDownloadFactKind::MetadataMissing => "metadata_missing",
        ExecutionDownloadFactKind::Interrupted => "interrupted",
        ExecutionDownloadFactKind::NetworkFailure => "network_failure",
        ExecutionDownloadFactKind::PermissionFailure => "permission_failure",
        ExecutionDownloadFactKind::PromoteFailed => "promote_failed",
        ExecutionDownloadFactKind::ProviderFailure => "provider_failure",
        ExecutionDownloadFactKind::SizeMismatch => "size_mismatch",
        ExecutionDownloadFactKind::TempDiscarded => "temp_discarded",
        ExecutionDownloadFactKind::TempWriteFailed => "temp_write_failed",
        ExecutionDownloadFactKind::WrittenToTemp => "written_to_temp",
        ExecutionDownloadFactKind::Promoted => "promoted",
    }
}

async fn reconcile_install_journal_error(
    journals: &OperationJournalStore,
    operation_id: &OperationId,
    error: OperationJournalStoreError,
    expected: impl Fn(&OperationJournalEntry) -> bool,
) -> Result<InstallJournalReconciliation, OperationJournalStoreError> {
    reconcile_install_journal_transition(journals, operation_id, error, expected).await
}

pub(crate) async fn begin_content_operation_journal_for_session(
    journals: &OperationJournalStore,
    operation_id: &OperationId,
    install_id: &str,
    instance_id: &str,
) -> Result<(), OperationJournalStoreError> {
    let expected = planned_content_journal_for_session(operation_id, install_id, instance_id);
    create_fresh_install_journal(journals, expected).await
}

pub(crate) async fn begin_install_operation_journal_for_session(
    journals: &OperationJournalStore,
    operation_id: &OperationId,
    install_id: &str,
    version_id: &str,
) -> Result<(), OperationJournalStoreError> {
    let expected = planned_install_journal_for_session(operation_id, install_id, version_id);
    create_fresh_install_journal(journals, expected).await
}

async fn create_fresh_install_journal(
    journals: &OperationJournalStore,
    expected: OperationJournalEntry,
) -> Result<(), OperationJournalStoreError> {
    loop {
        match journals.create_fresh(expected.clone()).await {
            Err(OperationJournalStoreError::RetryRequired) => {
                if journals.retry().await.is_err() {
                    return Err(OperationJournalStoreError::AlreadyExists);
                }
            }
            result => return result,
        }
    }
}

pub(super) fn planned_content_journal_for_session(
    operation_id: &OperationId,
    install_id: &str,
    instance_id: &str,
) -> OperationJournalEntry {
    let mut entry = OperationJournalEntry::new(
        JournalId::new(format!("journal-{operation_id}")),
        operation_id.clone(),
        CommandKind::ModifyInstanceContent,
        StabilizationSystem::Application,
        OwnershipClass::LauncherManaged,
        RollbackState::NotApplicable,
    );
    entry.targets.push(install_session_target(install_id));
    entry.targets.push(content_instance_target(instance_id));
    entry.planned_steps.push(install_journal_step(
        "modify_instance_content",
        OperationPhase::Planning,
        OperationStepResult::Planned,
        None,
    ));
    entry
}

pub(super) fn planned_install_journal_for_session(
    operation_id: &OperationId,
    install_id: &str,
    version_id: &str,
) -> OperationJournalEntry {
    let mut entry = OperationJournalEntry::new(
        JournalId::new(format!("journal-{operation_id}")),
        operation_id.clone(),
        CommandKind::InstallVersion,
        StabilizationSystem::Application,
        OwnershipClass::LauncherManaged,
        RollbackState::NotApplicable,
    );
    entry.targets.push(install_session_target(install_id));
    entry.targets.push(install_version_target(version_id));
    entry.planned_steps.push(install_journal_step(
        "install_version",
        OperationPhase::Planning,
        OperationStepResult::Planned,
        None,
    ));
    entry
}

pub(crate) fn install_operation_journal_for_session(
    journals: &OperationJournalStore,
    install_id: &str,
) -> Option<OperationJournalEntry> {
    let session_target = install_session_target(install_id);
    journals
        .list()
        .into_iter()
        .filter(|entry| {
            matches!(
                entry.command,
                CommandKind::InstallVersion | CommandKind::ModifyInstanceContent
            ) && entry.targets.contains(&session_target)
        })
        .max_by_key(|entry| entry.sequence)
}

#[cfg(test)]
pub(crate) fn test_operation_id(seed: impl AsRef<[u8]>) -> OperationId {
    OperationId::deterministic_test(seed)
}

#[cfg(test)]
pub(crate) async fn begin_install_operation_journal(
    journals: &OperationJournalStore,
    operation_id: &OperationId,
    version_id: &str,
) -> Result<(), OperationJournalStoreError> {
    let install_id = operation_id.to_string();
    begin_install_operation_journal_for_session(journals, operation_id, &install_id, version_id)
        .await
}

#[cfg(test)]
pub(crate) async fn begin_content_operation_journal(
    journals: &OperationJournalStore,
    operation_id: &OperationId,
    instance_id: &str,
) -> Result<(), OperationJournalStoreError> {
    let install_id = operation_id.to_string();
    begin_content_operation_journal_for_session(journals, operation_id, &install_id, instance_id)
        .await
}

#[cfg(test)]
pub(super) fn planned_install_journal(
    operation_id: &OperationId,
    version_id: &str,
) -> OperationJournalEntry {
    planned_install_journal_for_session(operation_id, &operation_id.to_string(), version_id)
}

#[cfg(test)]
pub(super) fn planned_content_journal(
    operation_id: &OperationId,
    instance_id: &str,
) -> OperationJournalEntry {
    planned_content_journal_for_session(operation_id, &operation_id.to_string(), instance_id)
}

pub async fn record_install_operation_progress(
    journals: &OperationJournalStore,
    operation_id: &OperationId,
    progress: &DownloadProgress,
    progress_journal: &mut InstallProgressJournalTracker,
) -> Result<(), OperationJournalStoreError> {
    record_operation_progress(
        journals,
        operation_id,
        CommandKind::InstallVersion,
        "install",
        progress,
        &[],
        progress_journal,
    )
    .await
}

pub(crate) async fn record_content_operation_progress(
    journals: &OperationJournalStore,
    operation_id: &OperationId,
    progress: &DownloadProgress,
    download_facts: &[String],
    progress_journal: &mut InstallProgressJournalTracker,
) -> Result<(), OperationJournalStoreError> {
    record_operation_progress(
        journals,
        operation_id,
        CommandKind::ModifyInstanceContent,
        "content",
        progress,
        download_facts,
        progress_journal,
    )
    .await
}

async fn record_operation_progress(
    journals: &OperationJournalStore,
    operation_id: &OperationId,
    command: CommandKind,
    step_namespace: &str,
    progress: &DownloadProgress,
    terminal_facts: &[String],
    progress_journal: &mut InstallProgressJournalTracker,
) -> Result<(), OperationJournalStoreError> {
    let phase = safe_progress_phase(&progress.phase);
    let terminal = progress.done;
    if !terminal && progress_journal.contains(&phase) {
        return Ok(());
    }

    loop {
        let step_result = if terminal && progress.error.is_some() {
            OperationStepResult::Failed
        } else {
            OperationStepResult::Completed
        };
        let mut step = install_progress_step(step_namespace, &phase, step_result, progress);
        if terminal {
            step.generated_facts.extend_from_slice(terminal_facts);
        }
        let failure_point = terminal
            .then(|| {
                progress
                    .error
                    .as_ref()
                    .map(|_| format!("{step_namespace}_progress_{phase}"))
            })
            .flatten();
        let result = if terminal && progress.error.is_some() {
            journals
                .record_failure(
                    operation_id,
                    step.clone(),
                    failure_point
                        .as_deref()
                        .expect("failed progress has failure point"),
                    OperationOutcome::Failed,
                )
                .await
        } else if terminal {
            journals
                .record_success(operation_id, step.clone(), OperationOutcome::Succeeded)
                .await
        } else {
            journals.record_progress(operation_id, step.clone()).await
        };

        match result {
            Ok(()) => {
                if !terminal {
                    progress_journal.record(phase);
                }
                return Ok(());
            }
            Err(error) => {
                match reconcile_install_journal_error(journals, operation_id, error, |entry| {
                    install_progress_transition_matches(
                        entry,
                        operation_id,
                        command,
                        &step,
                        terminal,
                        failure_point.as_deref(),
                    )
                })
                .await?
                {
                    InstallJournalReconciliation::MutationCommitted => {
                        if !terminal {
                            progress_journal.record(phase);
                        }
                        return Ok(());
                    }
                    InstallJournalReconciliation::RetryMutation => {}
                }
            }
        }
    }
}

pub async fn record_install_operation_interrupted(
    journals: &OperationJournalStore,
    operation_id: &OperationId,
    progress: &DownloadProgress,
) -> Result<(), OperationJournalStoreError> {
    let phase = safe_progress_phase(&progress.phase);
    let evidence = GuardianInstallArtifactFailureEvidence::launcher_managed(
        Some(operation_id.clone()),
        "install_worker_interrupted",
        GuardianInstallArtifactFailureKind::NetworkFailure,
    )
    .with_field("phase", phase);
    record_operation_interrupted(
        journals,
        operation_id,
        CommandKind::InstallVersion,
        "install",
        progress,
        &[],
        &[evidence],
    )
    .await
}

pub(crate) async fn record_content_operation_interrupted(
    journals: &OperationJournalStore,
    operation_id: &OperationId,
    progress: &DownloadProgress,
    download_facts: &[String],
    execution_facts: &[ExecutionDownloadFact],
) -> Result<(), OperationJournalStoreError> {
    let evidence = install_failure_evidence_from_download_facts(operation_id, execution_facts);
    record_operation_interrupted(
        journals,
        operation_id,
        CommandKind::ModifyInstanceContent,
        "content",
        progress,
        download_facts,
        &evidence,
    )
    .await
}

async fn record_operation_interrupted(
    journals: &OperationJournalStore,
    operation_id: &OperationId,
    command: CommandKind,
    step_namespace: &str,
    progress: &DownloadProgress,
    terminal_facts: &[String],
    evidence: &[GuardianInstallArtifactFailureEvidence],
) -> Result<(), OperationJournalStoreError> {
    let memory_window =
        ProviderFailureObservationWindow::from_observed_at(&chrono::Utc::now().to_rfc3339())
            .ok_or(OperationJournalStoreError::InvalidGuardianOutcome)?;
    let (fact_ids, diagnosis_ids) = assess_install_guardian_failure(
        None,
        operation_id,
        evidence,
        OperationPhase::Downloading,
        &memory_window.observed_at,
    )
    .as_ref()
    .and_then(|assessment| {
        install_guardian_terminal_update(
            assessment,
            operation_id,
            evidence,
            OperationPhase::Downloading,
            &memory_window,
        )
    })
    .unwrap_or_default();
    let mut step = install_progress_step(
        step_namespace,
        &safe_progress_phase(&progress.phase),
        OperationStepResult::Failed,
        progress,
    );
    step.generated_facts.extend_from_slice(terminal_facts);
    let failure_point = format!("{step_namespace}_worker_interrupted");
    loop {
        if journals.get(operation_id).as_ref().is_some_and(|entry| {
            install_failure_with_evidence_matches(
                entry,
                operation_id,
                command,
                &step,
                &failure_point,
                &fact_ids,
                &diagnosis_ids,
            )
        }) {
            return Ok(());
        }
        match journals
            .record_failure_with_guardian_evidence(
                operation_id,
                step.clone(),
                failure_point.clone(),
                OperationOutcome::Failed,
                fact_ids.clone(),
                diagnosis_ids.clone(),
            )
            .await
        {
            Ok(()) => return Ok(()),
            Err(error) => {
                match reconcile_install_journal_error(journals, operation_id, error, |entry| {
                    install_failure_with_evidence_matches(
                        entry,
                        operation_id,
                        command,
                        &step,
                        &failure_point,
                        &fact_ids,
                        &diagnosis_ids,
                    )
                })
                .await?
                {
                    InstallJournalReconciliation::MutationCommitted => return Ok(()),
                    InstallJournalReconciliation::RetryMutation => {}
                }
            }
        }
    }
}

pub(crate) async fn record_content_operation_initialization_cancelled(
    journals: &OperationJournalStore,
    operation_id: &OperationId,
) -> Result<(), OperationJournalStoreError> {
    let progress = DownloadProgress {
        phase: "initializing".to_string(),
        current: 0,
        total: 1,
        file: None,
        error: Some("content operation stopped before initialization completed".to_string()),
        done: true,
        bytes_done: None,
        bytes_total: None,
    };
    let step = install_progress_step(
        "content",
        "initializing",
        OperationStepResult::Failed,
        &progress,
    );
    loop {
        if journals.get(operation_id).as_ref().is_some_and(|entry| {
            install_failure_with_evidence_matches(
                entry,
                operation_id,
                CommandKind::ModifyInstanceContent,
                &step,
                "content_initialization_cancelled",
                &[],
                &[],
            )
        }) {
            return Ok(());
        }
        match journals
            .record_failure(
                operation_id,
                step.clone(),
                "content_initialization_cancelled",
                OperationOutcome::Failed,
            )
            .await
        {
            Ok(()) => return Ok(()),
            Err(error) => {
                match reconcile_install_journal_error(journals, operation_id, error, |entry| {
                    install_failure_with_evidence_matches(
                        entry,
                        operation_id,
                        CommandKind::ModifyInstanceContent,
                        &step,
                        "content_initialization_cancelled",
                        &[],
                        &[],
                    )
                })
                .await?
                {
                    InstallJournalReconciliation::MutationCommitted => return Ok(()),
                    InstallJournalReconciliation::RetryMutation => {}
                }
            }
        }
    }
}

pub(crate) fn content_terminal_progress_is_visible(
    entry: &OperationJournalEntry,
    operation_id: &OperationId,
    progress: &DownloadProgress,
    terminal_facts: &[String],
) -> bool {
    let phase = safe_progress_phase(&progress.phase);
    let mut step = install_progress_step(
        "content",
        &phase,
        if progress.error.is_some() {
            OperationStepResult::Failed
        } else {
            OperationStepResult::Completed
        },
        progress,
    );
    step.generated_facts.extend_from_slice(terminal_facts);
    let failure_point = progress
        .error
        .as_ref()
        .map(|_| format!("content_progress_{phase}"));
    install_progress_transition_matches(
        entry,
        operation_id,
        CommandKind::ModifyInstanceContent,
        &step,
        true,
        failure_point.as_deref(),
    )
}

pub(super) async fn record_install_operation_initialization_cancelled(
    journals: &OperationJournalStore,
    operation_id: &OperationId,
) -> Result<(), OperationJournalStoreError> {
    let progress = interrupted_install_progress();
    let evidence = GuardianInstallArtifactFailureEvidence::launcher_managed(
        Some(operation_id.clone()),
        "install_initialization_cancelled",
        GuardianInstallArtifactFailureKind::NetworkFailure,
    )
    .with_field("phase", "initializing");
    let memory_window =
        ProviderFailureObservationWindow::from_observed_at(&chrono::Utc::now().to_rfc3339())
            .ok_or(OperationJournalStoreError::InvalidGuardianOutcome)?;
    let (fact_ids, diagnosis_ids) = assess_install_guardian_failure(
        None,
        operation_id,
        std::slice::from_ref(&evidence),
        OperationPhase::Downloading,
        &memory_window.observed_at,
    )
    .as_ref()
    .and_then(|assessment| {
        install_guardian_terminal_update(
            assessment,
            operation_id,
            std::slice::from_ref(&evidence),
            OperationPhase::Downloading,
            &memory_window,
        )
    })
    .unwrap_or_default();
    let step = install_progress_step(
        "install",
        "initializing",
        OperationStepResult::Failed,
        &progress,
    );
    loop {
        match journals
            .record_failure_with_guardian_evidence(
                operation_id,
                step.clone(),
                "install_initialization_cancelled",
                OperationOutcome::Failed,
                fact_ids.clone(),
                diagnosis_ids.clone(),
            )
            .await
        {
            Ok(()) => return Ok(()),
            Err(error) => {
                match reconcile_install_journal_error(journals, operation_id, error, |entry| {
                    install_failure_with_evidence_matches(
                        entry,
                        operation_id,
                        CommandKind::InstallVersion,
                        &step,
                        "install_initialization_cancelled",
                        &fact_ids,
                        &diagnosis_ids,
                    )
                })
                .await?
                {
                    InstallJournalReconciliation::MutationCommitted => return Ok(()),
                    InstallJournalReconciliation::RetryMutation => {}
                }
            }
        }
    }
}

async fn record_operation_guardian_evidence(
    journals: &OperationJournalStore,
    operation_id: &OperationId,
    command: CommandKind,
    evidence: &[GuardianInstallArtifactFailureEvidence],
    phase: OperationPhase,
) -> Result<(), OperationJournalStoreError> {
    let guardian_facts = evidence
        .iter()
        .map(|evidence| install_artifact_failure_guardian_fact(evidence, phase))
        .collect::<Vec<_>>();
    if guardian_facts.is_empty() {
        return Ok(());
    }

    let fact_ids = guardian_facts
        .iter()
        .map(|fact| format!("guardian_fact:{}", fact.id.as_str()))
        .collect::<Vec<_>>();
    let diagnosis_ids = diagnose(&guardian_facts, phase)
        .into_iter()
        .map(|diagnosis| diagnosis.id())
        .collect::<Vec<_>>();
    record_guardian_evidence_with_reconciliation(
        journals,
        operation_id,
        command,
        fact_ids,
        diagnosis_ids,
    )
    .await
}

pub(super) async fn record_loader_install_operation_guardian_failure_outcome(
    producer: &ProducerLease,
    journals: Arc<OperationJournalStore>,
    failure_memory: Arc<GuardianFailureMemoryStore>,
    operation_id: &OperationId,
    target_id: &str,
    failure: &LoaderActiveInstallFailure,
    observed_at: &str,
) -> Result<(), OperationJournalStoreError> {
    let failure_kind = failure.kind();
    let (kind, ownership, phase) = loader_install_guardian_evidence_kind(failure_kind);
    let evidence = loader_error_guardian_failure_evidence(
        operation_id,
        target_id,
        failure,
        failure_kind,
        kind,
        ownership,
    );
    record_install_guardian_failure_outcome(
        producer,
        journals,
        failure_memory,
        operation_id,
        &[evidence],
        phase,
        observed_at,
    )
    .await
}

pub(super) async fn record_loader_base_install_dependency_guardian_failure_outcome(
    journals: &OperationJournalStore,
    operation_id: &OperationId,
    target_id: &str,
    base_version_id: &str,
) -> Result<(), OperationJournalStoreError> {
    let evidence = GuardianInstallArtifactFailureEvidence::launcher_managed(
        Some(operation_id.clone()),
        target_id,
        GuardianInstallArtifactFailureKind::DependencyFailed,
    )
    .with_field("dependency", "base_version")
    .with_field("base_version", base_version_id);
    record_install_guardian_failure_outcome_without_memory(
        journals,
        operation_id,
        &[evidence],
        OperationPhase::Downloading,
    )
    .await
}

pub fn install_guardian_outcome_summary_from_journal(
    entry: &OperationJournalEntry,
) -> Option<crate::guardian::GuardianInstallOutcomeSummary> {
    match persisted_install_guardian_outcome(entry) {
        PersistedInstallGuardianOutcome::Valid { summary, .. } => Some(summary),
        PersistedInstallGuardianOutcome::Absent | PersistedInstallGuardianOutcome::Invalid => None,
    }
}

enum PersistedInstallGuardianOutcome {
    Absent,
    Valid {
        summary: crate::guardian::GuardianInstallOutcomeSummary,
        memory: Option<Box<GuardianInstallOutcomeMemoryPersistence>>,
    },
    Invalid,
}

pub(super) async fn settle_startup_install_guardian_failure_memory(
    journals: &OperationJournalStore,
    failure_memory: &GuardianFailureMemoryStore,
    observed_at: &str,
) -> Result<(), OperationJournalStoreError> {
    let observed = chrono::DateTime::parse_from_rfc3339(observed_at)
        .map_err(|_| OperationJournalStoreError::InvalidGuardianOutcome)?
        .with_timezone(&chrono::Utc);
    if observed.to_rfc3339() != observed_at {
        return Err(OperationJournalStoreError::InvalidGuardianOutcome);
    }

    let _settlement = failure_memory.lock_install_guardian_settlement().await;
    let mut pending_retries = 0;
    settle_startup_install_guardian_pending(failure_memory, &mut pending_retries).await?;

    let mut active_keys = BTreeSet::new();
    let mut candidates = Vec::new();
    for entry in journals.list() {
        if !matches!(
            entry.command,
            CommandKind::InstallVersion | CommandKind::ModifyInstanceContent
        ) {
            continue;
        }
        let (summary, memory) = match persisted_install_guardian_outcome(&entry) {
            PersistedInstallGuardianOutcome::Absent => continue,
            PersistedInstallGuardianOutcome::Invalid => {
                return Err(OperationJournalStoreError::InvalidGuardianOutcome);
            }
            PersistedInstallGuardianOutcome::Valid { summary, memory } => (summary, memory),
        };
        if !summary.decision_is(GuardianActionKind::Retry) {
            continue;
        }
        if !install_journal_identity_matches(&entry, &entry.operation_id, entry.command)
            || !install_retry_carrier_journal_state_is_valid(&entry)
            || summary.diagnosis_id() != DiagnosisId::DownloadUnavailable
        {
            return Err(OperationJournalStoreError::InvalidGuardianOutcome);
        }
        let Some(memory) = memory else {
            return Err(OperationJournalStoreError::InvalidGuardianOutcome);
        };
        if !suppression_deadline_active(memory.suppression_until(), observed_at) {
            continue;
        }
        if !install_provider_retry_target_is_valid(memory.target()) {
            return Err(OperationJournalStoreError::InvalidGuardianOutcome);
        }
        let candidate = GuardianFailureMemoryEntry::observed(
            DiagnosisId::DownloadUnavailable,
            GuardianDomain::Download,
            memory.target().clone(),
            GuardianMode::Managed,
            Some(PROVIDER_FAILURE_MEMORY_SOURCE),
            memory.observed_at().to_string(),
        )
        .with_action(
            GuardianActionKind::Retry,
            FailureMemoryActionOutcome::Retried,
        )
        .with_suppression_until(memory.suppression_until().to_string());
        if !memory.binding_matches_target(&candidate.key)
            || !active_keys.insert(candidate.key.as_str().to_string())
        {
            return Err(OperationJournalStoreError::InvalidGuardianOutcome);
        }
        candidates.push(candidate);
    }

    settle_startup_install_guardian_retry_batch(failure_memory, candidates).await
}

fn install_retry_carrier_journal_state_is_valid(entry: &OperationJournalEntry) -> bool {
    match (entry.status, entry.outcome) {
        (OperationStatus::Planned | OperationStatus::Running, None) => {
            entry.failure_point.is_none()
        }
        (OperationStatus::Failed, Some(OperationOutcome::Failed)) => entry.failure_point.is_some(),
        _ => false,
    }
}

fn install_provider_retry_target_is_valid(target: &TargetDescriptor) -> bool {
    target.system == StabilizationSystem::Execution
        && target.kind == TargetKind::Artifact
        && matches!(
            target.ownership,
            OwnershipClass::LauncherManaged | OwnershipClass::ExternalProviderDerived
        )
}

async fn settle_startup_install_guardian_pending(
    failure_memory: &GuardianFailureMemoryStore,
    retries: &mut usize,
) -> Result<(), OperationJournalStoreError> {
    loop {
        match failure_memory.settle_install_guardian_pending().await {
            Ok(()) => return Ok(()),
            Err(error)
                if retry_install_guardian_memory_persistence(
                    &error,
                    retries,
                    "startup_pending",
                )
                .await => {}
            Err(_) => {
                return Err(OperationJournalStoreError::GuardianFailureMemoryUnavailable);
            }
        }
    }
}

async fn settle_startup_install_guardian_retry_batch(
    failure_memory: &GuardianFailureMemoryStore,
    entries: Vec<GuardianFailureMemoryEntry>,
) -> Result<(), OperationJournalStoreError> {
    let mut retries = 0;
    loop {
        match failure_memory
            .reconcile_install_guardian_retry_batch(entries.clone())
            .await
        {
            Ok(()) => return Ok(()),
            Err(error @ FailureMemoryStoreError::Validation(_)) => {
                warn!(
                    failure_memory_error = error.class(),
                    "Guardian startup Retry carrier conflicts with failure memory"
                );
                return Err(OperationJournalStoreError::InvalidGuardianOutcome);
            }
            Err(error)
                if retry_install_guardian_memory_persistence(
                    &error,
                    &mut retries,
                    "startup_publication",
                )
                .await =>
            {
                settle_startup_install_guardian_pending(failure_memory, &mut retries).await?;
            }
            Err(_) => {
                return Err(OperationJournalStoreError::GuardianFailureMemoryUnavailable);
            }
        }
    }
}

fn persisted_install_guardian_outcome(
    entry: &OperationJournalEntry,
) -> PersistedInstallGuardianOutcome {
    let mut marker_facts = None;
    for step in &entry.completed_steps {
        match guardian_install_outcome_fact_group(step.generated_facts.iter().map(String::as_str)) {
            GuardianInstallOutcomeFactGroupParse::Absent => {}
            GuardianInstallOutcomeFactGroupParse::Invalid => {
                return PersistedInstallGuardianOutcome::Invalid;
            }
            GuardianInstallOutcomeFactGroupParse::Valid(_) if marker_facts.is_some() => {
                return PersistedInstallGuardianOutcome::Invalid;
            }
            GuardianInstallOutcomeFactGroupParse::Valid(_) => {
                marker_facts = Some(&step.generated_facts);
            }
        }
    }
    let Some(marker_facts) = marker_facts else {
        return PersistedInstallGuardianOutcome::Absent;
    };
    let memory = match guardian_install_outcome_fact_group(marker_facts.iter().map(String::as_str))
    {
        GuardianInstallOutcomeFactGroupParse::Valid(group) => group.memory(),
        GuardianInstallOutcomeFactGroupParse::Absent
        | GuardianInstallOutcomeFactGroupParse::Invalid => {
            return PersistedInstallGuardianOutcome::Invalid;
        }
    };
    let mut outcomes = entry
        .guardian_diagnosis_ids
        .iter()
        .filter_map(|diagnosis_id| {
            let GuardianInstallOutcomeFactGroupParse::Valid(group) =
                guardian_install_outcome_fact_group(marker_facts.iter().map(String::as_str))
            else {
                return None;
            };
            guardian_install_outcome_from_persisted_group(*diagnosis_id, group)
        });
    let Some(summary) = outcomes.next() else {
        return PersistedInstallGuardianOutcome::Invalid;
    };
    if outcomes.next().is_some() {
        return PersistedInstallGuardianOutcome::Invalid;
    }
    PersistedInstallGuardianOutcome::Valid {
        summary,
        memory: memory.map(Box::new),
    }
}

pub fn sanitize_install_progress(mut progress: DownloadProgress) -> DownloadProgress {
    progress.phase = sanitize_evidence_token(&progress.phase, RedactionAudience::UserVisible, 48)
        .unwrap_or_else(|| "install".to_string());
    progress.file = progress
        .file
        .take()
        .and_then(|file| sanitize_evidence_token(&file, RedactionAudience::UserVisible, 96));
    progress.error = progress.error.take().map(|error| {
        if progress.done {
            if is_specific_terminal_install_failure_message(&error) {
                return error;
            }
            return INSTALL_FAILURE_MESSAGE.to_string();
        }
        sanitize_public_diagnostic_text(
            &error,
            RedactionAudience::UserVisible,
            160,
            INSTALL_FAILURE_MESSAGE,
        )
    });
    progress
}

pub(crate) fn install_progress_with_terminal_error(
    mut progress: DownloadProgress,
    error: &DownloadError,
) -> DownloadProgress {
    if progress.done
        && progress.error.is_some()
        && let Some(message) = specific_terminal_install_failure_message(error)
    {
        progress.error = Some(message);
    }
    progress
}

fn specific_terminal_install_failure_message(error: &DownloadError) -> Option<String> {
    match error {
        DownloadError::RuntimeUnavailableForPlatform {
            component,
            platform,
        } => Some(runtime_unavailable_install_failure_message(
            component, platform,
        )),
        DownloadError::RuntimeRosettaRequired { component } => {
            Some(runtime_rosetta_required_install_failure_message(component))
        }
        _ => None,
    }
}

fn runtime_unavailable_install_failure_message(component: &str, platform: &str) -> String {
    let component = sanitize_evidence_token(component, RedactionAudience::UserVisible, 64)
        .unwrap_or_else(|| "required-runtime".to_string());
    let platform = sanitize_evidence_token(platform, RedactionAudience::UserVisible, 64)
        .unwrap_or_else(|| "this-device".to_string());
    format!(
        "{RUNTIME_UNAVAILABLE_INSTALL_FAILURE_MESSAGE_PREFIX} Required runtime: {component} on {platform}."
    )
}

fn runtime_rosetta_required_install_failure_message(component: &str) -> String {
    let component = sanitize_evidence_token(component, RedactionAudience::UserVisible, 64)
        .unwrap_or_else(|| "required-runtime".to_string());
    format!(
        "{RUNTIME_ROSETTA_REQUIRED_INSTALL_FAILURE_MESSAGE_PREFIX} Required runtime: {component}. Install Rosetta 2 by running `{ROSETTA_INSTALL_COMMAND}` in Terminal, then retry."
    )
}

fn is_specific_terminal_install_failure_message(error: &str) -> bool {
    if error.starts_with(RUNTIME_ROSETTA_REQUIRED_INSTALL_FAILURE_MESSAGE_PREFIX) {
        return is_rosetta_required_terminal_install_failure_message(error);
    }

    error.starts_with(RUNTIME_UNAVAILABLE_INSTALL_FAILURE_MESSAGE_PREFIX)
        && sanitize_public_diagnostic_text(
            error,
            RedactionAudience::UserVisible,
            220,
            INSTALL_FAILURE_MESSAGE,
        )
        .as_str()
            == error
}

fn is_rosetta_required_terminal_install_failure_message(error: &str) -> bool {
    let prefix =
        format!("{RUNTIME_ROSETTA_REQUIRED_INSTALL_FAILURE_MESSAGE_PREFIX} Required runtime: ");
    let suffix = format!(". {ROSETTA_REQUIRED_INSTALL_GUIDANCE}");
    let Some(component) = error
        .strip_prefix(&prefix)
        .and_then(|rest| rest.strip_suffix(&suffix))
    else {
        return false;
    };

    sanitize_evidence_token(component, RedactionAudience::UserVisible, 64)
        .is_some_and(|sanitized| sanitized == component)
}

pub(crate) fn vanilla_install_progress_view_model(
    progress: &DownloadProgress,
) -> InstallProgressViewModel {
    install_progress_view_model(progress, InstallProgressKind::Vanilla)
}

#[cfg(test)]
pub(crate) fn loader_install_progress_view_model(
    progress: &DownloadProgress,
) -> InstallProgressViewModel {
    install_progress_view_model(progress, InstallProgressKind::Loader)
}

pub fn public_vanilla_install_progress_record_json(record: &InstallProgressRecord) -> Value {
    public_install_progress_record_json(record, InstallProgressKind::Vanilla)
}

pub fn public_loader_install_progress_record_json(record: &InstallProgressRecord) -> Value {
    public_install_progress_record_json(record, InstallProgressKind::Loader)
}

pub(crate) fn vanilla_install_progress_record_view_model(
    record: &InstallProgressRecord,
) -> InstallProgressViewModel {
    install_progress_record_view_model(record, InstallProgressKind::Vanilla)
}

pub(crate) fn loader_install_progress_record_view_model(
    record: &InstallProgressRecord,
) -> InstallProgressViewModel {
    install_progress_record_view_model(record, InstallProgressKind::Loader)
}

#[derive(Default)]
pub(crate) struct InstallProgressPresenter {
    high_watermarks: [u8; 2],
    complete_denominator_seen: bool,
}

impl InstallProgressPresenter {
    pub(crate) fn record(&mut self, progress: DownloadProgress) -> InstallProgressRecord {
        self.complete_denominator_seen |= progress.bytes_total.is_some_and(|total| total > 0);
        let vanilla = self.public_json(&progress, InstallProgressKind::Vanilla);
        let loader = self.public_json(&progress, InstallProgressKind::Loader);
        let vanilla_event_json =
            serde_json::to_string(&vanilla).unwrap_or_else(|_| "{}".to_string());
        let loader_event_json = serde_json::to_string(&loader).unwrap_or_else(|_| "{}".to_string());
        InstallProgressRecord::with_event_json(progress, vanilla_event_json, loader_event_json)
    }

    fn public_json(&mut self, progress: &DownloadProgress, kind: InstallProgressKind) -> Value {
        let mut view_model = install_progress_view_model(progress, kind);
        if !view_model.terminal {
            if !self.complete_denominator_seen {
                view_model.progress_pct = view_model.progress_pct.min(kind.pre_transfer_ceiling());
            }
            let high_watermark = &mut self.high_watermarks[kind.index()];
            view_model.progress_pct = view_model.progress_pct.max(*high_watermark);
            *high_watermark = view_model.progress_pct;
        }
        public_install_progress_json_with_view_model(progress, view_model)
    }
}

#[derive(Default)]
pub(crate) struct InstallProgressCoalescer {
    last_emitted: Option<DownloadProgress>,
    pending: Option<DownloadProgress>,
    pending_count: usize,
}

impl InstallProgressCoalescer {
    pub(crate) fn push(&mut self, progress: DownloadProgress) -> Vec<DownloadProgress> {
        if should_passthrough_progress(&progress) || self.is_phase_transition(&progress) {
            let mut emitted = self.flush_vec();
            emitted.push(self.mark_emitted(progress));
            return emitted;
        }

        if self.should_emit_coalesced_now(&progress) {
            self.pending = None;
            self.pending_count = 0;
            return vec![self.mark_emitted(progress)];
        }

        self.pending = Some(progress);
        self.pending_count = self.pending_count.saturating_add(1);
        if self.pending_count >= COALESCED_PROGRESS_EVENT_INTERVAL {
            return self.flush_vec();
        }

        Vec::new()
    }

    pub(crate) fn flush(&mut self) -> Option<DownloadProgress> {
        let progress = self.pending.take()?;
        self.pending_count = 0;
        Some(self.mark_emitted(progress))
    }

    fn flush_vec(&mut self) -> Vec<DownloadProgress> {
        self.flush().into_iter().collect()
    }

    fn mark_emitted(&mut self, progress: DownloadProgress) -> DownloadProgress {
        self.last_emitted = Some(progress.clone());
        progress
    }

    fn is_phase_transition(&self, progress: &DownloadProgress) -> bool {
        self.pending
            .as_ref()
            .or(self.last_emitted.as_ref())
            .is_some_and(|previous| previous.phase != progress.phase)
    }

    fn should_emit_coalesced_now(&self, progress: &DownloadProgress) -> bool {
        let Some(last) = self.last_emitted.as_ref() else {
            return true;
        };
        progress.bytes_total != last.bytes_total
            || progress.total != last.total
            || byte_progress_bucket(progress) != byte_progress_bucket(last)
            || (progress.total > 0 && progress.current >= progress.total)
    }
}

fn should_passthrough_progress(progress: &DownloadProgress) -> bool {
    progress.done
        || progress.error.is_some()
        || matches!(
            progress.phase.as_str(),
            "done" | "error" | "java_runtime_ready"
        )
        || !is_coalesced_progress_phase(progress.phase.as_str())
}

fn is_coalesced_progress_phase(phase: &str) -> bool {
    matches!(phase, "libraries" | "loader_libraries" | "java_runtime")
}

fn byte_progress_bucket(progress: &DownloadProgress) -> Option<u8> {
    let (done, total) = (progress.bytes_done?, progress.bytes_total?);
    if total == 0 {
        return None;
    }
    Some((((u128::from(done.min(total)) * 100) / u128::from(total)).min(100)) as u8)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InstallProgressKind {
    Vanilla,
    Loader,
}

impl InstallProgressKind {
    const fn index(self) -> usize {
        match self {
            Self::Vanilla => 0,
            Self::Loader => 1,
        }
    }

    const fn pre_transfer_ceiling(self) -> u8 {
        match self {
            Self::Vanilla => 2,
            Self::Loader => 8,
        }
    }
}

fn public_install_progress_json(progress: &DownloadProgress, kind: InstallProgressKind) -> Value {
    let view_model = install_progress_view_model(progress, kind);
    public_install_progress_json_with_view_model(progress, view_model)
}

fn public_install_progress_record_json(
    record: &InstallProgressRecord,
    kind: InstallProgressKind,
) -> Value {
    record
        .event_json(kind == InstallProgressKind::Loader)
        .and_then(|payload| serde_json::from_str(payload).ok())
        .unwrap_or_else(|| public_install_progress_json(&record.progress, kind))
}

fn install_progress_record_view_model(
    record: &InstallProgressRecord,
    kind: InstallProgressKind,
) -> InstallProgressViewModel {
    let payload = public_install_progress_record_json(record, kind);
    serde_json::from_value(payload["view_model"].clone())
        .unwrap_or_else(|_| install_progress_view_model(&record.progress, kind))
}

fn public_install_progress_json_with_view_model(
    progress: &DownloadProgress,
    view_model: InstallProgressViewModel,
) -> Value {
    let progress = sanitize_install_progress(progress.clone());
    let mut payload = serde_json::to_value(&progress).unwrap_or_else(|_| json!({}));
    payload["view_model"] = json!(view_model);
    payload
}

fn install_progress_view_model(
    progress: &DownloadProgress,
    kind: InstallProgressKind,
) -> InstallProgressViewModel {
    let progress = sanitize_install_progress(progress.clone());
    let phase = progress.phase.trim();
    let label = install_progress_label(&progress, kind);
    let failed = phase == "error" || progress.error.is_some();
    let terminal = progress.done || failed;
    InstallProgressViewModel {
        phase_id: if phase.is_empty() {
            "install".to_string()
        } else {
            phase.to_string()
        },
        progress_pct: install_progress_pct(&progress, kind),
        active_step: install_active_step_view_model(&progress, &label),
        label,
        terminal,
        failed,
    }
}

fn install_progress_label(progress: &DownloadProgress, kind: InstallProgressKind) -> String {
    match progress.phase.as_str() {
        "profile" => progress
            .file
            .clone()
            .unwrap_or_else(|| "Preparing loader profile".to_string()),
        "artifacts" => progress
            .file
            .clone()
            .unwrap_or_else(|| "Downloading loader artifacts".to_string()),
        "loader_libraries" => count_label("Loader libraries", progress),
        "processors" => progress
            .file
            .clone()
            .unwrap_or_else(|| count_label("Running processors", progress)),
        "loader_overlay" => "Applying loader archive".to_string(),
        "loader_publish" => "Publishing loader version".to_string(),
        "version_json" => {
            if progress.current >= progress.total && progress.total > 0 {
                "Version info ready".to_string()
            } else {
                "Resolving version info".to_string()
            }
        }
        "client_jar" => "Downloading game JAR".to_string(),
        "libraries" => count_label("Libraries", progress),
        "asset_index" => {
            if progress.current >= progress.total && progress.total > 0 {
                "Asset index ready".to_string()
            } else {
                "Fetching asset index".to_string()
            }
        }
        "assets" => count_label("Assets", progress),
        "log_config" => "Downloading log config".to_string(),
        "java_runtime" => java_runtime_label(progress),
        "java_runtime_ready" => "Java runtime ready".to_string(),
        "planning" => "Checking content".to_string(),
        "download" => count_label("Downloading content", progress),
        "overrides" => "Applying pack configuration".to_string(),
        "commit" => "Finishing content changes".to_string(),
        "removing" => "Removing content".to_string(),
        "done" => "Complete".to_string(),
        "error" | "error_instance_removed" => progress
            .error
            .clone()
            .unwrap_or_else(|| INSTALL_FAILURE_MESSAGE.to_string()),
        phase => progress.file.clone().unwrap_or_else(|| match kind {
            InstallProgressKind::Loader => {
                if phase.is_empty() {
                    "Working on loader install".to_string()
                } else {
                    format!("Working on {phase}")
                }
            }
            InstallProgressKind::Vanilla => {
                if phase.is_empty() {
                    "Working on install".to_string()
                } else {
                    format!("Working on {phase}")
                }
            }
        }),
    }
}

fn install_progress_pct(progress: &DownloadProgress, kind: InstallProgressKind) -> u8 {
    if let Some(pct) = byte_weighted_install_pct(progress, kind) {
        return pct;
    }
    // Fallback for events without transfer-plan facts: pre-plan phases,
    // loader-specific work, and journal-replayed history.
    let pct = match (kind, progress.phase.as_str()) {
        (_, "done") => 100,
        (_, "error" | "error_instance_removed") => 100,
        (_, "planning") => 3,
        (_, "download") => 5 + (progress_fraction(progress) * 85.0).round() as i32,
        (_, "overrides") => 92,
        (_, "commit") => 96,
        (_, "removing") => 50,
        (InstallProgressKind::Vanilla, "version_json") => 2,
        (InstallProgressKind::Vanilla, "client_jar") => 7,
        (InstallProgressKind::Vanilla, "libraries") => {
            7 + (progress_fraction(progress) * 13.0).round() as i32
        }
        (InstallProgressKind::Vanilla, "asset_index") => 21,
        (InstallProgressKind::Vanilla, "assets") => {
            21 + (progress_fraction(progress) * 72.0).round() as i32
        }
        (InstallProgressKind::Vanilla, "log_config") => 94,
        (InstallProgressKind::Loader, "artifacts") => 5,
        (InstallProgressKind::Loader, "profile") => 82,
        (InstallProgressKind::Loader, "loader_libraries") => {
            82 + (progress_fraction(progress) * 8.0).round() as i32
        }
        (InstallProgressKind::Loader, "processors") => {
            90 + (progress_fraction(progress) * 9.0).round() as i32
        }
        (InstallProgressKind::Loader, "loader_overlay") => {
            82 + (progress_fraction(progress) * 15.0).round() as i32
        }
        (InstallProgressKind::Loader, "loader_publish") => 99,
        (InstallProgressKind::Loader, "version_json") => 8,
        (InstallProgressKind::Loader, "client_jar") => 12,
        (InstallProgressKind::Loader, "libraries") => {
            12 + (progress_fraction(progress) * 12.0).round() as i32
        }
        (InstallProgressKind::Loader, "asset_index") => 25,
        (InstallProgressKind::Loader, "assets") => {
            25 + (progress_fraction(progress) * 50.0).round() as i32
        }
        (InstallProgressKind::Loader, "log_config") => 76,
        _ => 0,
    };
    pct.clamp(0, 100) as u8
}

/// Overall progress from the installer's transfer-plan facts: bytes of
/// planned work completed across every concurrent phase (client jar,
/// libraries, assets, managed Java runtime). Capped below 100 so only the
/// terminal `done` event completes the bar. Loader installs reserve an early
/// provider span and a post-base span for phases that carry no byte facts.
fn byte_weighted_install_pct(progress: &DownloadProgress, kind: InstallProgressKind) -> Option<u8> {
    if matches!(progress.phase.as_str(), "done" | "error") {
        return None;
    }
    let (done, total) = (progress.bytes_done?, progress.bytes_total?);
    if total == 0 {
        return None;
    }
    let fraction = (done.min(total) as f64) / (total as f64);
    let (base, span) = match kind {
        InstallProgressKind::Vanilla => (0.0, 99.0),
        InstallProgressKind::Loader => (8.0, 72.0),
    };
    Some((base + fraction * span).round().clamp(0.0, 99.0) as u8)
}

fn install_active_step_view_model(
    progress: &DownloadProgress,
    label: &str,
) -> Option<InstallProgressStepViewModel> {
    if !matches!(
        progress.phase.as_str(),
        "java_runtime" | "processors" | "download"
    ) {
        return None;
    }
    if progress.total <= 0 {
        return None;
    }

    Some(InstallProgressStepViewModel {
        phase_id: progress.phase.clone(),
        label: label.to_string(),
        progress_pct: (progress_fraction(progress) * 100.0)
            .round()
            .clamp(0.0, 100.0) as u8,
        current: progress.current.max(0),
        total: progress.total,
    })
}

fn count_label(base: &str, progress: &DownloadProgress) -> String {
    if progress.total > 0 {
        format!("{} ({}/{})", base, progress.current.max(0), progress.total)
    } else {
        base.to_string()
    }
}

fn java_runtime_label(progress: &DownloadProgress) -> String {
    if progress.total > 0 {
        count_label("Java runtime files", progress)
    } else {
        "Preparing Java runtime".to_string()
    }
}

fn progress_fraction(progress: &DownloadProgress) -> f32 {
    if progress.total <= 0 {
        return 0.0;
    }
    (progress.current.max(0) as f32 / progress.total as f32).clamp(0.0, 1.0)
}

pub(super) fn install_failure_evidence_from_download_facts(
    operation_id: &OperationId,
    facts: &[ExecutionDownloadFact],
) -> Vec<GuardianInstallArtifactFailureEvidence> {
    facts
        .iter()
        .filter_map(|fact| {
            install_artifact_failure_from_minecraft_download_fact(
                Some(operation_id.clone()),
                OwnershipClass::LauncherManaged,
                fact,
            )
        })
        .collect()
}

pub(crate) struct ContentFailureOutcomeRequest<'a> {
    pub(crate) operation_id: &'a OperationId,
    pub(crate) download_facts: &'a [ExecutionDownloadFact],
    pub(crate) additional_evidence: Option<GuardianInstallArtifactFailureEvidence>,
    pub(crate) phase: OperationPhase,
    pub(crate) observed_at: &'a str,
}

pub(crate) async fn record_content_failure_outcome(
    producer: &ProducerLease,
    journals: Arc<OperationJournalStore>,
    failure_memory: Arc<GuardianFailureMemoryStore>,
    request: ContentFailureOutcomeRequest<'_>,
) -> Result<(), OperationJournalStoreError> {
    let ContentFailureOutcomeRequest {
        operation_id,
        download_facts,
        additional_evidence,
        phase,
        observed_at,
    } = request;
    let mut evidence = install_failure_evidence_from_download_facts(operation_id, download_facts);
    if let Some(additional_evidence) = additional_evidence {
        evidence.push(additional_evidence);
    }
    if evidence.is_empty() {
        return Ok(());
    }
    record_operation_guardian_failure_outcome(
        producer,
        journals,
        failure_memory,
        OperationGuardianFailureRequest {
            operation_id,
            command: CommandKind::ModifyInstanceContent,
            evidence: &evidence,
            phase,
            observed_at,
        },
    )
    .await
}

pub(super) fn install_failure_evidence_from_download_error_or_facts(
    operation_id: &OperationId,
    error: &DownloadError,
    facts: &[ExecutionDownloadFact],
) -> Vec<GuardianInstallArtifactFailureEvidence> {
    if let Some(evidence) = typed_runtime_failure_evidence(operation_id, error) {
        return vec![evidence];
    }

    let terminal_facts = terminal_download_failure_facts_for_error(error, facts);
    let terminal_fact_evidence =
        install_failure_evidence_from_download_facts(operation_id, &terminal_facts);
    if should_prefer_terminal_download_facts(error) && !terminal_fact_evidence.is_empty() {
        return terminal_fact_evidence;
    }

    if let Some(evidence) = install_failure_evidence_from_download_error(operation_id, error) {
        return vec![evidence];
    }

    if !terminal_fact_evidence.is_empty() {
        return terminal_fact_evidence;
    }

    install_failure_evidence_from_download_facts(operation_id, facts)
}

pub(super) fn typed_runtime_failure_evidence(
    operation_id: &OperationId,
    error: &DownloadError,
) -> Option<GuardianInstallArtifactFailureEvidence> {
    match error {
        DownloadError::RuntimeUnavailableForPlatform {
            component,
            platform,
        } => Some(
            GuardianInstallArtifactFailureEvidence::launcher_managed(
                Some(operation_id.clone()),
                format!("java_runtime_{component}_{platform}"),
                GuardianInstallArtifactFailureKind::RuntimeUnavailableForPlatform,
            )
            .with_field("component", component.as_str())
            .with_field("platform", platform.as_str()),
        ),
        DownloadError::RuntimeRosettaRequired { component } => Some(
            GuardianInstallArtifactFailureEvidence::launcher_managed(
                Some(operation_id.clone()),
                format!("java_runtime_{component}_rosetta"),
                GuardianInstallArtifactFailureKind::RuntimeRosettaRequired,
            )
            .with_field("component", component.as_str()),
        ),
        DownloadError::RuntimeSource(failure) => {
            let component = failure.component().as_str();
            let kind = match failure.kind() {
                RuntimeSourceFailureKind::Unavailable => {
                    GuardianInstallArtifactFailureKind::ProviderFailure
                }
                RuntimeSourceFailureKind::MetadataInvalid
                | RuntimeSourceFailureKind::IntegrityMismatch
                | RuntimeSourceFailureKind::PolicyRejected => {
                    GuardianInstallArtifactFailureKind::MetadataInvalid
                }
            };
            Some(
                GuardianInstallArtifactFailureEvidence::launcher_managed(
                    Some(operation_id.clone()),
                    format!("java_runtime_source_{component}"),
                    kind,
                )
                .with_ownership(OwnershipClass::ExternalProviderDerived)
                .with_field("component", component)
                .with_field("source_failure_kind", failure.kind().as_str()),
            )
        }
        DownloadError::PrepareRuntime(_) => {
            Some(GuardianInstallArtifactFailureEvidence::launcher_managed(
                Some(operation_id.clone()),
                "java_runtime",
                GuardianInstallArtifactFailureKind::ExecutionFailed,
            ))
        }
        _ => None,
    }
}

fn install_failure_evidence_from_download_error(
    operation_id: &OperationId,
    error: &DownloadError,
) -> Option<GuardianInstallArtifactFailureEvidence> {
    let (target_id, kind) = install_failure_target_and_kind_from_download_error(error)?;

    Some(GuardianInstallArtifactFailureEvidence::launcher_managed(
        Some(operation_id.clone()),
        target_id,
        kind,
    ))
}

fn install_failure_target_and_kind_from_download_error(
    error: &DownloadError,
) -> Option<(&'static str, GuardianInstallArtifactFailureKind)> {
    let evidence = match error {
        DownloadError::FileOperation(_) => (
            "install_filesystem",
            GuardianInstallArtifactFailureKind::PermissionDenied,
        ),
        DownloadError::ResolveManifest(_) => (
            "version_manifest",
            GuardianInstallArtifactFailureKind::ProviderFailure,
        ),
        DownloadError::Request(_) => (
            "minecraft_download",
            GuardianInstallArtifactFailureKind::NetworkFailure,
        ),
        DownloadError::ParseVersion(_) => (
            "version_json",
            GuardianInstallArtifactFailureKind::MetadataInvalid,
        ),
        DownloadError::LibraryPlan(_) => (
            "library_metadata",
            GuardianInstallArtifactFailureKind::MetadataInvalid,
        ),
        DownloadError::PrepareRuntime(_)
        | DownloadError::RuntimeSource(_)
        | DownloadError::RuntimeRosettaRequired { .. }
        | DownloadError::RuntimeUnavailableForPlatform { .. } => return None,
        DownloadError::Integrity(_) => return None,
    };

    Some(evidence)
}

fn terminal_download_failure_facts_for_error(
    error: &DownloadError,
    facts: &[ExecutionDownloadFact],
) -> Vec<ExecutionDownloadFact> {
    facts
        .iter()
        .filter(|fact| terminal_download_failure_fact_kind_for_error(error, fact.kind))
        .cloned()
        .collect()
}

fn should_prefer_terminal_download_facts(error: &DownloadError) -> bool {
    matches!(
        error,
        DownloadError::FileOperation(_) | DownloadError::Request(_) | DownloadError::Integrity(_)
    )
}

fn terminal_download_failure_fact_kind_for_error(
    error: &DownloadError,
    kind: ExecutionDownloadFactKind,
) -> bool {
    if matches!(error, DownloadError::Request(_)) {
        return request_terminal_download_failure_fact_kind(kind);
    }

    terminal_download_failure_fact_kind(kind)
}

fn request_terminal_download_failure_fact_kind(kind: ExecutionDownloadFactKind) -> bool {
    matches!(
        kind,
        ExecutionDownloadFactKind::Interrupted
            | ExecutionDownloadFactKind::NetworkFailure
            | ExecutionDownloadFactKind::ProviderFailure
    )
}

fn terminal_download_failure_fact_kind(kind: ExecutionDownloadFactKind) -> bool {
    matches!(
        kind,
        ExecutionDownloadFactKind::ChecksumMismatch
            | ExecutionDownloadFactKind::MetadataInvalid
            | ExecutionDownloadFactKind::MetadataMissing
            | ExecutionDownloadFactKind::Interrupted
            | ExecutionDownloadFactKind::NetworkFailure
            | ExecutionDownloadFactKind::PermissionFailure
            | ExecutionDownloadFactKind::PromoteFailed
            | ExecutionDownloadFactKind::ProviderFailure
            | ExecutionDownloadFactKind::SizeMismatch
            | ExecutionDownloadFactKind::TempWriteFailed
    )
}

async fn record_install_guardian_failure_outcome_without_memory(
    journals: &OperationJournalStore,
    operation_id: &OperationId,
    evidence: &[GuardianInstallArtifactFailureEvidence],
    phase: OperationPhase,
) -> Result<(), OperationJournalStoreError> {
    record_operation_guardian_evidence(
        journals,
        operation_id,
        CommandKind::InstallVersion,
        evidence,
        phase,
    )
    .await?;
    let memory_window =
        ProviderFailureObservationWindow::from_observed_at(&chrono::Utc::now().to_rfc3339())
            .ok_or(OperationJournalStoreError::InvalidGuardianOutcome)?;
    settle_operation_guardian_failure(
        journals,
        None,
        operation_id,
        CommandKind::InstallVersion,
        evidence,
        phase,
        &memory_window,
    )
    .await
    .map_err(|error| match error {
        InstallGuardianSettlementError::Journal(error) => error,
        InstallGuardianSettlementError::Memory(_) => {
            OperationJournalStoreError::GuardianFailureMemoryUnavailable
        }
    })
}

pub(super) async fn record_install_guardian_failure_outcome(
    producer: &ProducerLease,
    journals: Arc<OperationJournalStore>,
    failure_memory: Arc<GuardianFailureMemoryStore>,
    operation_id: &OperationId,
    evidence: &[GuardianInstallArtifactFailureEvidence],
    phase: OperationPhase,
    observed_at: &str,
) -> Result<(), OperationJournalStoreError> {
    record_operation_guardian_failure_outcome(
        producer,
        journals,
        failure_memory,
        OperationGuardianFailureRequest {
            operation_id,
            command: CommandKind::InstallVersion,
            evidence,
            phase,
            observed_at,
        },
    )
    .await
}

struct OperationGuardianFailureRequest<'a> {
    operation_id: &'a OperationId,
    command: CommandKind,
    evidence: &'a [GuardianInstallArtifactFailureEvidence],
    phase: OperationPhase,
    observed_at: &'a str,
}

fn assess_install_guardian_failure(
    failure_memory: Option<&GuardianFailureMemoryStore>,
    operation_id: &OperationId,
    evidence: &[GuardianInstallArtifactFailureEvidence],
    phase: OperationPhase,
    observed_at: &str,
) -> Option<GuardianInstallAssessment> {
    let mode = GuardianMode::Managed;
    let context = failure_memory_suppression_context(
        failure_memory,
        Some(operation_id.clone()),
        mode,
        phase,
        evidence,
        observed_at,
    );
    assess_install_artifact_failure_with_context(
        Some(operation_id.clone()),
        mode,
        phase,
        evidence,
        context,
    )
}

async fn record_operation_guardian_failure_outcome(
    producer: &ProducerLease,
    journals: Arc<OperationJournalStore>,
    failure_memory: Arc<GuardianFailureMemoryStore>,
    request: OperationGuardianFailureRequest<'_>,
) -> Result<(), OperationJournalStoreError> {
    record_operation_guardian_evidence(
        &journals,
        request.operation_id,
        request.command,
        request.evidence,
        request.phase,
    )
    .await?;
    let operation_id = request.operation_id.clone();
    let logged_operation_id = operation_id.clone();
    let evidence = request.evidence.to_vec();
    let memory_window = ProviderFailureObservationWindow::from_observed_at(request.observed_at)
        .ok_or(OperationJournalStoreError::InvalidGuardianOutcome)?;
    let command = request.command;
    let phase = request.phase;
    #[cfg(test)]
    let policy_evaluation_count = crate::guardian::guardian_policy_evaluation_count_scope();
    let settlement = producer.claim_child().spawn_joinable(async move {
        let settlement = settle_owned_operation_guardian_failure(
            journals,
            failure_memory,
            operation_id,
            command,
            evidence,
            phase,
            memory_window,
        );
        #[cfg(test)]
        return crate::guardian::with_guardian_policy_evaluation_count_scope(
            policy_evaluation_count,
            settlement,
        )
        .await;
        #[cfg(not(test))]
        settlement.await
    });
    match settlement.await {
        Ok(result) => result,
        Err(error) => {
            let join_failure_kind = if error.is_panic() {
                "panic"
            } else if error.is_cancelled() {
                "cancelled"
            } else {
                "unknown"
            };
            warn!(
                operation_id = %logged_operation_id,
                join_failure_kind,
                "producer-owned Guardian install settlement stopped unexpectedly"
            );
            Err(OperationJournalStoreError::GuardianFailureMemoryUnavailable)
        }
    }
}

async fn settle_owned_operation_guardian_failure(
    journals: Arc<OperationJournalStore>,
    failure_memory: Arc<GuardianFailureMemoryStore>,
    operation_id: OperationId,
    command: CommandKind,
    evidence: Vec<GuardianInstallArtifactFailureEvidence>,
    phase: OperationPhase,
    memory_window: ProviderFailureObservationWindow,
) -> Result<(), OperationJournalStoreError> {
    let _settlement = failure_memory.lock_install_guardian_settlement().await;
    let mut persistence_retries = 0;
    loop {
        match failure_memory.settle_install_guardian_pending().await {
            Ok(()) => {}
            Err(error)
                if retry_install_guardian_memory_persistence(
                    &error,
                    &mut persistence_retries,
                    "pending",
                )
                .await =>
            {
                continue;
            }
            Err(_) => return Err(OperationJournalStoreError::GuardianFailureMemoryUnavailable),
        }
        match settle_operation_guardian_failure(
            &journals,
            Some(&failure_memory),
            &operation_id,
            command,
            &evidence,
            phase,
            &memory_window,
        )
        .await
        {
            Ok(()) => return Ok(()),
            Err(InstallGuardianSettlementError::Journal(error)) => return Err(error),
            Err(InstallGuardianSettlementError::Memory(error))
                if retry_install_guardian_memory_persistence(
                    &error,
                    &mut persistence_retries,
                    "publication",
                )
                .await => {}
            Err(InstallGuardianSettlementError::Memory(_)) => {
                return Err(OperationJournalStoreError::GuardianFailureMemoryUnavailable);
            }
        }
    }
}

async fn retry_install_guardian_memory_persistence(
    error: &FailureMemoryStoreError,
    retries: &mut usize,
    stage: &'static str,
) -> bool {
    let FailureMemoryStoreError::Persistence(error) = error else {
        return false;
    };
    if *retries >= INSTALL_GUARDIAN_MEMORY_RETRY_ATTEMPTS
        || !install_guardian_memory_persistence_is_retryable(error.kind())
    {
        return false;
    }
    *retries += 1;
    warn!(
        persistence_kind = ?error.kind(),
        retry_attempt = *retries,
        retry_limit = INSTALL_GUARDIAN_MEMORY_RETRY_ATTEMPTS,
        stage,
        "retrying Guardian install failure-memory persistence"
    );
    tokio::time::sleep(INSTALL_GUARDIAN_MEMORY_RETRY_DELAY).await;
    true
}

fn install_guardian_memory_persistence_is_retryable(kind: io::ErrorKind) -> bool {
    matches!(
        kind,
        io::ErrorKind::NotFound
            | io::ErrorKind::ConnectionRefused
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::HostUnreachable
            | io::ErrorKind::NetworkUnreachable
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::NotConnected
            | io::ErrorKind::NetworkDown
            | io::ErrorKind::BrokenPipe
            | io::ErrorKind::WouldBlock
            | io::ErrorKind::TimedOut
            | io::ErrorKind::WriteZero
            | io::ErrorKind::StaleNetworkFileHandle
            | io::ErrorKind::ResourceBusy
            | io::ErrorKind::ExecutableFileBusy
            | io::ErrorKind::Deadlock
            | io::ErrorKind::Interrupted
            | io::ErrorKind::UnexpectedEof
            | io::ErrorKind::Other
    )
}

enum InstallGuardianSettlementError {
    Journal(OperationJournalStoreError),
    Memory(FailureMemoryStoreError),
}

impl From<OperationJournalStoreError> for InstallGuardianSettlementError {
    fn from(error: OperationJournalStoreError) -> Self {
        Self::Journal(error)
    }
}

impl From<FailureMemoryStoreError> for InstallGuardianSettlementError {
    fn from(error: FailureMemoryStoreError) -> Self {
        Self::Memory(error)
    }
}

async fn settle_operation_guardian_failure(
    journals: &OperationJournalStore,
    failure_memory: Option<&GuardianFailureMemoryStore>,
    operation_id: &OperationId,
    command: CommandKind,
    evidence: &[GuardianInstallArtifactFailureEvidence],
    phase: OperationPhase,
    memory_window: &ProviderFailureObservationWindow,
) -> Result<(), InstallGuardianSettlementError> {
    if let Some(entry) = journals.get(operation_id) {
        if !install_journal_identity_matches(&entry, operation_id, command) {
            return Err(OperationJournalStoreError::InvalidGuardianOutcome.into());
        }
        match persisted_install_guardian_outcome(&entry) {
            PersistedInstallGuardianOutcome::Absent => {}
            PersistedInstallGuardianOutcome::Invalid => {
                return Err(OperationJournalStoreError::InvalidGuardianOutcome.into());
            }
            PersistedInstallGuardianOutcome::Valid { summary, memory } => {
                publish_provider_failure_memory_if_needed(
                    failure_memory,
                    ProviderFailureMemoryPublicationRequest {
                        operation_id: Some(operation_id.clone()),
                        mode: GuardianMode::Managed,
                        phase,
                        evidence,
                        diagnosis_id: summary.diagnosis_id(),
                        retry: summary.decision_is(GuardianActionKind::Retry),
                        observed_at: &memory_window.observed_at,
                        publication: ProviderMemoryPublication::Replay(
                            memory.map(|memory| *memory),
                        ),
                    },
                )
                .await?;
                return Ok(());
            }
        }
    }

    let Some(assessment) = assess_install_guardian_failure(
        failure_memory,
        operation_id,
        evidence,
        phase,
        &memory_window.observed_at,
    ) else {
        return Ok(());
    };
    let Some(outcome) = assessment.terminal_outcome() else {
        return Ok(());
    };
    let memory = if outcome.decision == GuardianActionKind::Retry {
        Some(
            install_failure_memory_persistence(
                Some(operation_id.clone()),
                GuardianMode::Managed,
                phase,
                evidence,
                outcome.diagnosis_id,
                memory_window,
            )
            .ok_or(OperationJournalStoreError::InvalidGuardianOutcome)?,
        )
    } else {
        None
    };
    let facts = guardian_install_outcome_persistence_facts(&outcome.user_outcome, memory.as_ref())
        .ok_or(OperationJournalStoreError::InvalidGuardianOutcome)?;
    record_guardian_evidence_with_reconciliation(
        journals,
        operation_id,
        command,
        facts,
        vec![outcome.diagnosis_id],
    )
    .await?;
    if let Some(memory) = memory {
        publish_provider_failure_memory_if_needed(
            failure_memory,
            ProviderFailureMemoryPublicationRequest {
                operation_id: Some(operation_id.clone()),
                mode: GuardianMode::Managed,
                phase,
                evidence,
                diagnosis_id: outcome.diagnosis_id,
                retry: true,
                observed_at: &memory_window.observed_at,
                publication: ProviderMemoryPublication::Assessed(memory),
            },
        )
        .await?;
    }
    Ok(())
}

fn install_guardian_terminal_update(
    assessment: &GuardianInstallAssessment,
    operation_id: &OperationId,
    evidence: &[GuardianInstallArtifactFailureEvidence],
    phase: OperationPhase,
    memory_window: &ProviderFailureObservationWindow,
) -> Option<(Vec<String>, Vec<DiagnosisId>)> {
    let outcome = assessment.terminal_outcome()?;
    let memory = if outcome.decision == GuardianActionKind::Retry {
        Some(install_failure_memory_persistence(
            Some(operation_id.clone()),
            GuardianMode::Managed,
            phase,
            evidence,
            outcome.diagnosis_id,
            memory_window,
        )?)
    } else {
        None
    };
    Some((
        guardian_install_outcome_persistence_facts(&outcome.user_outcome, memory.as_ref())?,
        vec![outcome.diagnosis_id],
    ))
}

async fn record_guardian_evidence_with_reconciliation(
    journals: &OperationJournalStore,
    operation_id: &OperationId,
    command: CommandKind,
    facts: Vec<String>,
    diagnosis_ids: Vec<DiagnosisId>,
) -> Result<(), OperationJournalStoreError> {
    loop {
        match journals
            .record_guardian_evidence(operation_id, facts.clone(), diagnosis_ids.clone())
            .await
        {
            Ok(()) => return Ok(()),
            Err(error) => {
                match reconcile_install_journal_error(journals, operation_id, error, |entry| {
                    install_journal_identity_matches(entry, operation_id, command)
                        && facts
                            .iter()
                            .all(|fact| install_entry_contains_fact(entry, fact))
                        && diagnosis_ids
                            .iter()
                            .all(|diagnosis_id| entry.guardian_diagnosis_ids.contains(diagnosis_id))
                })
                .await?
                {
                    InstallJournalReconciliation::MutationCommitted => return Ok(()),
                    InstallJournalReconciliation::RetryMutation => {}
                }
            }
        }
    }
}

fn install_journal_identity_matches(
    entry: &OperationJournalEntry,
    operation_id: &OperationId,
    command: CommandKind,
) -> bool {
    &entry.operation_id == operation_id
        && entry.command == command
        && entry.owner == StabilizationSystem::Application
        && entry.ownership == OwnershipClass::LauncherManaged
}

fn install_progress_transition_matches(
    entry: &OperationJournalEntry,
    operation_id: &OperationId,
    command: CommandKind,
    step: &OperationJournalStep,
    terminal: bool,
    failure_point: Option<&str>,
) -> bool {
    if !install_journal_identity_matches(entry, operation_id, command)
        || !operation_journal_completed_step_is_visible(entry, step)
    {
        return false;
    }
    if !terminal {
        return entry.status == OperationStatus::Running
            && entry.outcome.is_none()
            && entry.failure_point.is_none();
    }
    if let Some(failure_point) = failure_point {
        entry.status == OperationStatus::Failed
            && entry.outcome == Some(OperationOutcome::Failed)
            && entry.failure_point.as_deref() == Some(failure_point)
    } else {
        entry.status == OperationStatus::Succeeded
            && entry.outcome == Some(OperationOutcome::Succeeded)
            && entry.failure_point.is_none()
    }
}

fn install_failure_with_evidence_matches(
    entry: &OperationJournalEntry,
    operation_id: &OperationId,
    command: CommandKind,
    step: &OperationJournalStep,
    failure_point: &str,
    fact_ids: &[String],
    diagnosis_ids: &[DiagnosisId],
) -> bool {
    let mut expected_step = step.clone();
    for fact_id in fact_ids {
        if !expected_step.generated_facts.contains(fact_id) {
            expected_step.generated_facts.push(fact_id.clone());
        }
    }
    install_journal_identity_matches(entry, operation_id, command)
        && entry.status == OperationStatus::Failed
        && entry.outcome == Some(OperationOutcome::Failed)
        && entry.failure_point.as_deref() == Some(failure_point)
        && operation_journal_completed_step_is_visible(entry, &expected_step)
        && diagnosis_ids
            .iter()
            .all(|diagnosis_id| entry.guardian_diagnosis_ids.contains(diagnosis_id))
}

fn install_entry_contains_fact(entry: &OperationJournalEntry, fact: &str) -> bool {
    entry
        .completed_steps
        .last()
        .is_some_and(|step| step.generated_facts.iter().any(|existing| existing == fact))
}

pub(crate) const fn loader_install_guardian_evidence_kind(
    failure_kind: LoaderInstallFailureKind,
) -> (
    GuardianInstallArtifactFailureKind,
    OwnershipClass,
    OperationPhase,
) {
    match failure_kind {
        LoaderInstallFailureKind::ProviderHttpFailure
        | LoaderInstallFailureKind::ProviderRateLimited
        | LoaderInstallFailureKind::ArtifactMissing => (
            GuardianInstallArtifactFailureKind::ProviderFailure,
            OwnershipClass::ExternalProviderDerived,
            OperationPhase::Downloading,
        ),
        LoaderInstallFailureKind::ProviderNetworkFailure => (
            GuardianInstallArtifactFailureKind::NetworkFailure,
            OwnershipClass::ExternalProviderDerived,
            OperationPhase::Downloading,
        ),
        LoaderInstallFailureKind::ProviderResponseTooLarge
        | LoaderInstallFailureKind::ProviderSchemaInvalid
        | LoaderInstallFailureKind::InvalidProfile => (
            GuardianInstallArtifactFailureKind::MetadataInvalid,
            OwnershipClass::ExternalProviderDerived,
            OperationPhase::Downloading,
        ),
        LoaderInstallFailureKind::ParseFailed
        | LoaderInstallFailureKind::VerifyFailed
        | LoaderInstallFailureKind::InstallExecutionFailed => (
            GuardianInstallArtifactFailureKind::ExecutionFailed,
            OwnershipClass::LauncherManaged,
            OperationPhase::Installing,
        ),
        LoaderInstallFailureKind::ProcessorFailed => (
            GuardianInstallArtifactFailureKind::ProcessorFailed,
            OwnershipClass::LauncherManaged,
            OperationPhase::Installing,
        ),
    }
}

fn loader_error_guardian_failure_evidence(
    operation_id: &OperationId,
    target_id: &str,
    failure: &LoaderActiveInstallFailure,
    failure_kind: LoaderInstallFailureKind,
    kind: GuardianInstallArtifactFailureKind,
    ownership: OwnershipClass,
) -> GuardianInstallArtifactFailureEvidence {
    let mut evidence = GuardianInstallArtifactFailureEvidence::launcher_managed(
        Some(operation_id.clone()),
        target_id,
        kind,
    )
    .with_ownership(ownership)
    .with_field("failure_kind", failure_kind.as_str());
    if let Some(provider_kind) = failure.source().provider_failure_kind() {
        evidence = evidence.with_field("provider_failure", provider_kind.as_str());
    }
    if let Some(status) = failure.source().provider_status() {
        evidence = evidence.with_field("status", status.to_string());
    }
    evidence
}

fn failure_memory_suppression_context(
    failure_memory: Option<&GuardianFailureMemoryStore>,
    operation_id: Option<OperationId>,
    mode: GuardianMode,
    phase: OperationPhase,
    evidence: &[GuardianInstallArtifactFailureEvidence],
    observed_at: &str,
) -> GuardianPolicyContext {
    let mut context = GuardianPolicyContext::current_operation();
    if provider_failure_memory_entry(
        failure_memory,
        operation_id,
        mode,
        phase,
        evidence,
        observed_at,
    )
    .is_some()
    {
        context = context.with_suppression();
    }
    context
}

fn provider_failure_memory_entry(
    failure_memory: Option<&GuardianFailureMemoryStore>,
    operation_id: Option<OperationId>,
    mode: GuardianMode,
    phase: OperationPhase,
    evidence: &[GuardianInstallArtifactFailureEvidence],
    observed_at: &str,
) -> Option<crate::state::failure_memory::GuardianFailureMemoryEntry> {
    let memory = failure_memory?;
    let key = install_failure_memory_key(
        operation_id,
        mode,
        phase,
        evidence,
        DiagnosisId::DownloadUnavailable,
    )?;
    let entry = memory.get(&key)?;
    if !suppression_active(&entry, observed_at) {
        return None;
    }
    Some(entry)
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ProviderMemoryPublication {
    Assessed(GuardianInstallOutcomeMemoryPersistence),
    Replay(Option<GuardianInstallOutcomeMemoryPersistence>),
}

struct ProviderFailureMemoryPublicationRequest<'a> {
    operation_id: Option<OperationId>,
    mode: GuardianMode,
    phase: OperationPhase,
    evidence: &'a [GuardianInstallArtifactFailureEvidence],
    diagnosis_id: DiagnosisId,
    retry: bool,
    observed_at: &'a str,
    publication: ProviderMemoryPublication,
}

async fn publish_provider_failure_memory_if_needed(
    failure_memory: Option<&GuardianFailureMemoryStore>,
    request: ProviderFailureMemoryPublicationRequest<'_>,
) -> Result<(), FailureMemoryStoreError> {
    let ProviderFailureMemoryPublicationRequest {
        operation_id,
        mode,
        phase,
        evidence,
        diagnosis_id,
        retry,
        observed_at,
        publication,
    } = request;
    if diagnosis_id != DiagnosisId::DownloadUnavailable || !retry {
        return Ok(());
    }
    let Some(memory) = failure_memory else {
        return Ok(());
    };
    let safety_case = install_artifact_failure_safety_case(operation_id, mode, phase, evidence);
    let Some(diagnosis) = safety_case
        .diagnoses
        .iter()
        .find(|diagnosis| diagnosis.id() == diagnosis_id)
    else {
        return Ok(());
    };
    let Some(target) = diagnosis.affected_targets().first().cloned() else {
        return Ok(());
    };
    let replay = matches!(&publication, ProviderMemoryPublication::Replay(_));
    let memory_persistence = match publication {
        ProviderMemoryPublication::Assessed(memory) => memory,
        ProviderMemoryPublication::Replay(Some(memory)) => {
            if !suppression_deadline_active(memory.suppression_until(), observed_at) {
                return Ok(());
            }
            memory
        }
        ProviderMemoryPublication::Replay(None) => return Ok(()),
    };
    let entry = GuardianFailureMemoryEntry::observed(
        diagnosis.id(),
        diagnosis.domain(),
        target,
        mode,
        Some(PROVIDER_FAILURE_MEMORY_SOURCE),
        memory_persistence.observed_at().to_string(),
    )
    .with_action(
        GuardianActionKind::Retry,
        FailureMemoryActionOutcome::Retried,
    )
    .with_suppression_until(memory_persistence.suppression_until().to_string());
    if !memory_persistence.matches_failure_memory_key(&entry.key, &entry.target) {
        return Ok(());
    }
    if replay {
        return memory.reconcile_install_guardian_retry(entry).await;
    }
    memory.record_install_guardian_retry(entry).await
}

fn install_failure_memory_persistence(
    operation_id: Option<OperationId>,
    mode: GuardianMode,
    phase: OperationPhase,
    evidence: &[GuardianInstallArtifactFailureEvidence],
    diagnosis_id: DiagnosisId,
    memory_window: &ProviderFailureObservationWindow,
) -> Option<GuardianInstallOutcomeMemoryPersistence> {
    let safety_case = install_artifact_failure_safety_case(operation_id, mode, phase, evidence);
    let diagnosis = safety_case
        .diagnoses
        .iter()
        .find(|diagnosis| diagnosis.id() == diagnosis_id)?;
    let target = diagnosis.affected_targets().first()?.clone();
    let key = FailureMemoryKey::for_observation(
        diagnosis.domain(),
        &diagnosis.id(),
        &target,
        mode,
        Some(PROVIDER_FAILURE_MEMORY_SOURCE),
    );
    GuardianInstallOutcomeMemoryPersistence::for_failure_memory_key(
        &key,
        target,
        memory_window.observed_at.clone(),
        memory_window.suppression_until.clone(),
    )
}

fn install_failure_memory_key(
    operation_id: Option<OperationId>,
    mode: GuardianMode,
    phase: OperationPhase,
    evidence: &[GuardianInstallArtifactFailureEvidence],
    diagnosis_id: DiagnosisId,
) -> Option<FailureMemoryKey> {
    let safety_case = install_artifact_failure_safety_case(operation_id, mode, phase, evidence);
    let diagnosis = safety_case
        .diagnoses
        .iter()
        .find(|diagnosis| diagnosis.id() == diagnosis_id)?;
    let target = diagnosis.affected_targets().first()?;
    Some(FailureMemoryKey::for_observation(
        diagnosis.domain(),
        &diagnosis.id(),
        target,
        mode,
        Some(PROVIDER_FAILURE_MEMORY_SOURCE),
    ))
}

fn suppression_active(entry: &GuardianFailureMemoryEntry, observed_at: &str) -> bool {
    let Some(suppression_until) = &entry.suppression_until else {
        return false;
    };
    suppression_deadline_active(suppression_until, observed_at)
}

fn suppression_deadline_active(suppression_until: &str, observed_at: &str) -> bool {
    let Ok(suppression_until) = chrono::DateTime::parse_from_rfc3339(suppression_until) else {
        return false;
    };
    let Ok(observed_at) = chrono::DateTime::parse_from_rfc3339(observed_at) else {
        return false;
    };
    suppression_until > observed_at
}

pub(super) fn public_install_id(id: &str) -> String {
    sanitize_evidence_token(id, RedactionAudience::UserVisible, 96)
        .unwrap_or_else(|| "install".to_string())
}

pub(crate) fn interrupted_install_progress() -> DownloadProgress {
    observed_install_failure_progress()
}

pub(crate) fn observed_install_failure_progress() -> DownloadProgress {
    DownloadProgress {
        phase: "error".to_string(),
        current: 0,
        total: 0,
        file: None,
        error: Some(INSTALL_FAILURE_MESSAGE.to_string()),
        done: true,
        bytes_done: None,
        bytes_total: None,
    }
}

pub(super) fn install_journal_is_terminal(status: OperationStatus) -> bool {
    matches!(
        status,
        OperationStatus::Succeeded
            | OperationStatus::Failed
            | OperationStatus::Blocked
            | OperationStatus::Cancelled
    )
}

pub(super) fn install_failure_point_from_journal(entry: &OperationJournalEntry) -> Option<String> {
    entry.failure_point.as_deref().and_then(|failure_point| {
        sanitize_evidence_token(failure_point, RedactionAudience::UserVisible, 96)
    })
}

pub(super) fn install_progress_history_from_journal(
    entry: &OperationJournalEntry,
) -> Vec<DownloadProgress> {
    let mut history = entry
        .completed_steps
        .iter()
        .filter_map(progress_from_install_journal_step)
        .collect::<Vec<_>>();

    if install_journal_is_terminal(entry.status) && !history.iter().any(|progress| progress.done) {
        history.push(terminal_progress_for_journal_status(entry.status));
    }

    history
}

fn progress_from_install_journal_step(step: &OperationJournalStep) -> Option<DownloadProgress> {
    let phase = install_phase_fact_value(step)?;
    let done = step
        .generated_facts
        .iter()
        .any(|fact| fact == "install_done:true");
    let failed = step
        .generated_facts
        .iter()
        .any(|fact| fact == "install_error:true")
        || step.result == OperationStepResult::Failed;

    Some(DownloadProgress {
        phase,
        current: if done && !failed { 1 } else { 0 },
        total: if done && !failed { 1 } else { 0 },
        file: None,
        error: (done && failed).then(|| INSTALL_FAILURE_MESSAGE.to_string()),
        done,
        bytes_done: None,
        bytes_total: None,
    })
}

fn install_phase_fact_value(step: &OperationJournalStep) -> Option<String> {
    step.generated_facts.iter().find_map(|fact| {
        fact.strip_prefix("install_phase:")
            .and_then(|phase| sanitize_evidence_token(phase, RedactionAudience::UserVisible, 48))
    })
}

fn terminal_progress_for_journal_status(status: OperationStatus) -> DownloadProgress {
    if status == OperationStatus::Succeeded {
        return DownloadProgress {
            phase: "done".to_string(),
            current: 1,
            total: 1,
            file: None,
            error: None,
            done: true,
            bytes_done: None,
            bytes_total: None,
        };
    }

    observed_install_failure_progress()
}

fn install_progress_step(
    step_namespace: &str,
    phase: &str,
    result: OperationStepResult,
    progress: &DownloadProgress,
) -> OperationJournalStep {
    let mut step = install_journal_step(
        format!("{step_namespace}_progress_{phase}"),
        install_operation_phase(progress),
        result,
        None,
    );
    step.generated_facts.push(format!("install_phase:{phase}"));
    if progress.done {
        step.generated_facts.push("install_done:true".to_string());
    }
    if progress.error.is_some() {
        step.generated_facts.push("install_error:true".to_string());
    }
    step
}

fn install_journal_step(
    step_id: impl AsRef<str>,
    phase: OperationPhase,
    result: OperationStepResult,
    changed_target: Option<TargetDescriptor>,
) -> OperationJournalStep {
    let step_id = sanitize_evidence_token(step_id.as_ref(), RedactionAudience::UserVisible, 96)
        .unwrap_or_else(|| "install_step".to_string());
    let mut step = OperationJournalStep::new(step_id, phase);
    step.result = result;
    step.changed_target = changed_target;
    step.rollback = RollbackState::NotApplicable;
    step
}

fn install_operation_phase(progress: &DownloadProgress) -> OperationPhase {
    if progress.done && progress.error.is_some() {
        return OperationPhase::Failed;
    }
    if progress.done {
        return OperationPhase::Completed;
    }

    match progress.phase.trim() {
        "version_json" | "client_jar" | "libraries" | "asset_index" | "assets" | "log_config"
        | "java_runtime" | "java_runtime_ready" | "artifacts" | "loader_libraries" | "download" => {
            OperationPhase::Downloading
        }
        "profile" | "processors" | "loader_overlay" | "loader_publish" => {
            OperationPhase::Installing
        }
        "planning" => OperationPhase::Planning,
        "overrides" | "commit" | "removing" => OperationPhase::Installing,
        _ => OperationPhase::Running,
    }
}

fn install_version_target(version_id: &str) -> TargetDescriptor {
    TargetDescriptor::new(
        StabilizationSystem::Application,
        TargetKind::Version,
        version_id,
        OwnershipClass::LauncherManaged,
    )
}

fn install_session_target(install_id: &str) -> TargetDescriptor {
    TargetDescriptor::new(
        StabilizationSystem::Application,
        TargetKind::Session,
        install_id,
        OwnershipClass::LauncherManaged,
    )
}

fn content_instance_target(instance_id: &str) -> TargetDescriptor {
    TargetDescriptor::new(
        StabilizationSystem::Application,
        TargetKind::Instance,
        instance_id,
        OwnershipClass::LauncherManaged,
    )
}

fn safe_progress_phase(phase: &str) -> String {
    sanitize_evidence_token(phase, RedactionAudience::UserVisible, 48)
        .unwrap_or_else(|| "install".to_string())
}

#[cfg(test)]
mod operation_id_tests {
    use super::*;

    #[tokio::test]
    async fn install_session_lookup_uses_journal_sequence() {
        let journals = OperationJournalStore::new();
        let first = OperationId::try_from("op-ffffffff-ffff-4fff-8fff-ffffffffffff")
            .expect("valid first operation id");
        let second = OperationId::try_from("op-00000000-0000-4000-8000-000000000000")
            .expect("valid second operation id");

        journals
            .create(planned_install_journal_for_session(
                &first,
                "retained-session",
                "1.20.4",
            ))
            .await
            .expect("create first session journal");
        journals
            .create(planned_install_journal_for_session(
                &second,
                "retained-session",
                "1.20.4",
            ))
            .await
            .expect("create second session journal");

        assert_eq!(
            install_operation_journal_for_session(&journals, "retained-session")
                .expect("latest retained session journal")
                .operation_id,
            second
        );
    }

    #[tokio::test]
    async fn fresh_install_journal_refuses_matching_identity_collision() {
        let journals = OperationJournalStore::new();
        let operation_id = OperationId::try_from("op-ffffffff-ffff-4fff-8fff-ffffffffffff")
            .expect("valid collision id");
        let expected = planned_install_journal_for_session(
            &operation_id,
            "colliding-session",
            "1.20.4",
        );
        journals
            .create(expected)
            .await
            .expect("seed matching collision");

        assert!(matches!(
            begin_install_operation_journal_for_session(
                &journals,
                &operation_id,
                "colliding-session",
                "1.20.4",
            )
            .await,
            Err(OperationJournalStoreError::AlreadyExists)
        ));
    }
}
