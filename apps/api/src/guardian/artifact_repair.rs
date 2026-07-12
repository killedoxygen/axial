//! Guardian artifact repair execution.
//!
//! Exact executors consume kind-typed Guardian repair authorizations that bind
//! validated provider metadata. They do not discover providers or decide policy.

use super::{
    ArtifactRepairKind, DiagnosisId, GuardianActionKind, GuardianDomain,
    GuardianMinecraftArtifactRepairDescriptor, GuardianMode, MissingDownload, QuarantineRedownload,
    RepairAuthorization,
};
use crate::execution::ExecutionFact;
use crate::execution::download::{
    DownloadChecksum, DownloadChecksumAlgorithm, DownloadToTempRequest, download_url_to_temp,
};
use crate::execution::file::{QuarantineFileRequest, quarantine_launcher_managed_file};
use crate::observability::{RedactionAudience, sanitize_evidence_token};
use crate::state::contracts::{
    CommandKind, JournalId, OperationId, OperationJournalEntry, OperationJournalStep,
    OperationOutcome, OperationPhase, OperationStatus, OperationStepResult, RollbackState,
    StabilizationSystem, TargetDescriptor,
};
use crate::state::failure_memory::{
    FailureMemoryActionOutcome, GuardianFailureMemoryEntry, GuardianFailureMemoryStore,
};
use crate::state::{
    OperationJournalReconciliation, OperationJournalStore, OperationJournalStoreError,
    operation_journal_completed_step_is_visible, operation_journal_plan_is_visible,
    operation_journal_terminal_is_visible,
};
use chrono::{DateTime, Duration};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::time::Duration as StdDuration;
use tracing::warn;

const DEFAULT_ARTIFACT_REPAIR_SUPPRESSION_MINUTES: i64 = 15;
const ARTIFACT_JOURNAL_RETRY_INITIAL_DELAY: StdDuration = StdDuration::from_millis(20);
const ARTIFACT_JOURNAL_RETRY_MAX_DELAY: StdDuration = StdDuration::from_secs(1);

enum ArtifactJournalReconciliation {
    MutationCommitted,
    AcceptedFailure(OperationJournalStoreError),
    RetryMutation,
}

#[derive(Clone, Debug)]
pub(super) struct GuardianArtifactRepairSource<'a> {
    pub(super) url: &'a str,
    pub(super) checksum_algorithm: &'a str,
    pub(super) expected_checksum: &'a str,
    pub(super) expected_size: Option<u64>,
    pub(super) max_bytes: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct GuardianArtifactRepairOutcome {
    pub operation_id: OperationId,
    pub diagnosis_id: DiagnosisId,
    pub action: GuardianActionKind,
    pub status: GuardianArtifactRepairStatus,
    pub facts: Vec<String>,
    pub summary: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum GuardianArtifactRepairStatus {
    Repaired,
    Blocked,
    Failed,
    Suppressed,
}

impl GuardianArtifactRepairStatus {
    pub const fn as_persisted_id(self) -> &'static str {
        match self {
            Self::Repaired => "repaired",
            Self::Blocked => "blocked",
            Self::Failed => "failed",
            Self::Suppressed => "suppressed",
        }
    }

    pub fn from_persisted_id(value: &str) -> Option<Self> {
        match value {
            "repaired" => Some(Self::Repaired),
            "blocked" => Some(Self::Blocked),
            "failed" => Some(Self::Failed),
            "suppressed" => Some(Self::Suppressed),
            _ => None,
        }
    }
}

enum ArtifactTerminal {
    Blocked(&'static str),
    Suppressed(String),
    Repaired {
        step_id: &'static str,
        facts: Vec<String>,
        quarantined_target: Option<TargetDescriptor>,
    },
    Failed {
        step_id: &'static str,
        rollback: RollbackState,
        facts: Vec<String>,
        summary: &'static str,
    },
}

enum ArtifactTerminalJournal {
    Create(OperationOutcome),
    Record(&'static str, RollbackState, Option<&'static str>),
}

struct ArtifactRepairContext<'a> {
    authorization: ArtifactAuthorization,
    descriptor: GuardianMinecraftArtifactRepairDescriptor,
    client: &'a Client,
    journals: &'a OperationJournalStore,
    failure_memory: &'a GuardianFailureMemoryStore,
    observed_at: &'a str,
    quarantines_existing: bool,
}

struct ArtifactAuthorization {
    diagnosis_id: DiagnosisId,
    target: TargetDescriptor,
    ownership: crate::state::contracts::OwnershipClass,
    mode: GuardianMode,
    action: GuardianActionKind,
    max_attempts: u32,
    suppression_key: crate::state::failure_memory::FailureMemoryKey,
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn execute_guardian_quarantine_redownload(
    authorization: RepairAuthorization<QuarantineRedownload>,
    operation_id: Option<OperationId>,
    client: &Client,
    journals: &OperationJournalStore,
    failure_memory: &GuardianFailureMemoryStore,
    observed_at: &str,
) -> Result<GuardianArtifactRepairOutcome, OperationJournalStoreError> {
    execute_artifact_repair_kernel(
        authorization,
        operation_id,
        client,
        journals,
        failure_memory,
        observed_at,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn execute_guardian_missing_download(
    authorization: RepairAuthorization<MissingDownload>,
    operation_id: Option<OperationId>,
    client: &Client,
    journals: &OperationJournalStore,
    failure_memory: &GuardianFailureMemoryStore,
    observed_at: &str,
) -> Result<GuardianArtifactRepairOutcome, OperationJournalStoreError> {
    execute_artifact_repair_kernel(
        authorization,
        operation_id,
        client,
        journals,
        failure_memory,
        observed_at,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn execute_artifact_repair_kernel<K: ArtifactRepairKind>(
    authorization: RepairAuthorization<K>,
    operation_id: Option<OperationId>,
    client: &Client,
    journals: &OperationJournalStore,
    failure_memory: &GuardianFailureMemoryStore,
    observed_at: &str,
) -> Result<GuardianArtifactRepairOutcome, OperationJournalStoreError> {
    let operation_id = operation_id.unwrap_or_else(new_repair_operation_id);
    let authorization = authorization.into_parts();
    let descriptor = authorization.kind.into_descriptor();
    let context = ArtifactRepairContext {
        authorization: ArtifactAuthorization {
            diagnosis_id: authorization.diagnosis_id,
            target: authorization.target,
            ownership: authorization.ownership,
            mode: authorization.mode,
            action: authorization.action,
            max_attempts: authorization.max_attempts,
            suppression_key: authorization.suppression_key,
        },
        descriptor,
        client,
        journals,
        failure_memory,
        observed_at,
        quarantines_existing: K::QUARANTINES_EXISTING,
    };
    let target = context.authorization.target.clone();

    let checksum = match validate_artifact_repair_input(&context) {
        Ok(checksum) => checksum,
        Err(block_reason) => {
            return finish_artifact_repair(
                &context,
                operation_id,
                ArtifactTerminal::Blocked(block_reason),
            )
            .await;
        }
    };

    if context.authorization.max_attempts == 0 {
        return finish_artifact_repair(
            &context,
            operation_id,
            ArtifactTerminal::Blocked("guardian_artifact_repair_blocked_by_policy"),
        )
        .await;
    }
    if let Some(suppression_until) = context
        .failure_memory
        .get(&context.authorization.suppression_key)
        .and_then(|entry| {
            super::repair_terminal::active_repair_suppression_until(&entry, context.observed_at)
        })
    {
        return finish_artifact_repair(
            &context,
            operation_id,
            ArtifactTerminal::Suppressed(suppression_until),
        )
        .await;
    }

    if let Some(error) =
        create_planned_journal_reconciled(context.journals, &operation_id, &context).await?
    {
        terminalize_recovered_artifact_journal(
            context.journals,
            &operation_id,
            &target,
            RollbackState::Unavailable,
            "guardian_artifact_repair_initialization_failed",
        )
        .await?;
        return Err(error);
    }

    let quarantined_target = if context.quarantines_existing {
        let quarantine_report = match quarantine_launcher_managed_file(QuarantineFileRequest {
            operation_id: Some(operation_id.clone()),
            target: target.clone(),
            source: context.descriptor.destination(),
        }) {
            Ok(report) => report,
            Err(error) => {
                let fact_ids = fact_ids(&error.facts);
                return finish_artifact_repair(
                    &context,
                    operation_id,
                    ArtifactTerminal::Failed {
                        step_id: "quarantine_launcher_managed_target",
                        rollback: RollbackState::Unavailable,
                        facts: fact_ids,
                        summary: "guardian_artifact_quarantine_failed",
                    },
                )
                .await;
            }
        };
        let quarantine_facts = fact_ids(&quarantine_report.facts);
        let quarantine_checkpoint = repair_step(
            "quarantine_launcher_managed_target",
            OperationStepResult::Completed,
            Some(target.clone()),
            quarantine_facts,
            RollbackState::Available,
        );
        loop {
            let result = context
                .journals
                .record_checkpoint(&operation_id, quarantine_checkpoint.clone())
                .await;
            match result {
                Ok(()) => break,
                Err(error) => match reconcile_artifact_journal_error(
                    context.journals,
                    &operation_id,
                    error,
                    |entry| {
                        artifact_journal_identity_matches(entry, &operation_id)
                            && entry.status == OperationStatus::Running
                            && quarantine_checkpoint
                                .changed_target
                                .as_ref()
                                .is_some_and(|target| {
                                    entry.targets.contains(target)
                                        && entry.ownership == target.ownership
                                })
                            && operation_journal_completed_step_is_visible(
                                entry,
                                &quarantine_checkpoint,
                            )
                    },
                )
                .await?
                {
                    ArtifactJournalReconciliation::MutationCommitted => break,
                    ArtifactJournalReconciliation::AcceptedFailure(error) => {
                        terminalize_recovered_artifact_journal(
                            context.journals,
                            &operation_id,
                            &target,
                            RollbackState::Available,
                            "guardian_artifact_quarantine_checkpoint_failed",
                        )
                        .await?;
                        return Err(error);
                    }
                    ArtifactJournalReconciliation::RetryMutation => {}
                },
            }
        }
        Some(TargetDescriptor::new(
            StabilizationSystem::Execution,
            target.kind,
            format!("quarantine-{}", target.id),
            target.ownership,
        ))
    } else {
        None
    };

    let source = context.descriptor.repair_source();
    let mut download_request =
        DownloadToTempRequest::new(target.clone(), context.descriptor.destination(), source.url)
            .with_expected_checksum(checksum);
    if let Some(max_bytes) = source.max_bytes {
        download_request = download_request.with_max_bytes(max_bytes);
    }
    if let Some(expected_size) = source.expected_size {
        download_request = download_request.with_expected_size(expected_size);
    }
    download_request.operation_id = Some(operation_id.clone());

    match download_url_to_temp(download_request, context.client).await {
        Ok(report) => {
            let fact_ids = fact_ids(&report.facts);
            finish_artifact_repair(
                &context,
                operation_id,
                ArtifactTerminal::Repaired {
                    step_id: "promote_verified_artifact",
                    facts: fact_ids,
                    quarantined_target,
                },
            )
            .await
        }
        Err(error) => {
            let fact_ids = fact_ids(&error.facts);
            finish_artifact_repair(
                &context,
                operation_id,
                ArtifactTerminal::Failed {
                    step_id: "download_artifact_to_temp",
                    rollback: if context.quarantines_existing {
                        RollbackState::Available
                    } else {
                        RollbackState::Unavailable
                    },
                    facts: fact_ids,
                    summary: "guardian_artifact_redownload_failed",
                },
            )
            .await
        }
    }
}

async fn finish_artifact_repair(
    context: &ArtifactRepairContext<'_>,
    operation_id: OperationId,
    terminal: ArtifactTerminal,
) -> Result<GuardianArtifactRepairOutcome, OperationJournalStoreError> {
    let (journal, memory_outcome, status, facts, summary, suppression_until, quarantined) =
        match terminal {
            ArtifactTerminal::Blocked(summary) => (
                ArtifactTerminalJournal::Create(OperationOutcome::Blocked),
                None,
                GuardianArtifactRepairStatus::Blocked,
                Vec::new(),
                summary,
                None,
                None,
            ),
            ArtifactTerminal::Suppressed(suppression_until) => (
                ArtifactTerminalJournal::Create(OperationOutcome::Suppressed),
                None,
                GuardianArtifactRepairStatus::Suppressed,
                Vec::new(),
                "guardian_artifact_repair_suppressed",
                Some(suppression_until),
                None,
            ),
            ArtifactTerminal::Repaired {
                step_id,
                facts,
                quarantined_target,
            } => (
                ArtifactTerminalJournal::Record(step_id, RollbackState::Available, None),
                Some(FailureMemoryActionOutcome::Repaired),
                GuardianArtifactRepairStatus::Repaired,
                facts,
                "guardian_artifact_repaired",
                default_suppression_until(context.observed_at),
                quarantined_target,
            ),
            ArtifactTerminal::Failed {
                step_id,
                rollback,
                facts,
                summary,
            } => (
                ArtifactTerminalJournal::Record(step_id, rollback, Some(step_id)),
                Some(FailureMemoryActionOutcome::Failed),
                GuardianArtifactRepairStatus::Failed,
                facts,
                summary,
                default_suppression_until(context.observed_at),
                None,
            ),
        };
    let journal_operation_id = operation_id.clone();
    let journal_facts = facts.clone();
    let complete_journal = async move {
        match journal {
            ArtifactTerminalJournal::Create(outcome) => {
                create_terminal_journal_reconciled(
                    context.journals,
                    &journal_operation_id,
                    context,
                    OperationStatus::Blocked,
                    outcome,
                    OperationStepResult::Skipped,
                    journal_facts,
                )
                .await
            }
            ArtifactTerminalJournal::Record(step_id, rollback, failure_point) => {
                let step_result = if failure_point.is_some() {
                    OperationStepResult::Failed
                } else {
                    OperationStepResult::Completed
                };
                record_artifact_terminal_reconciled(
                    context.journals,
                    &journal_operation_id,
                    repair_step(
                        step_id,
                        step_result,
                        Some(context.authorization.target.clone()),
                        journal_facts,
                        rollback,
                    ),
                    failure_point,
                )
                .await
            }
        }
    };
    super::repair_terminal::complete_repair_terminal(
        complete_journal,
        || {
            if let Some(memory_outcome) = memory_outcome {
                record_artifact_repair_memory(
                    context.failure_memory,
                    &context.authorization.diagnosis_id,
                    context.authorization.mode,
                    &context.authorization.target,
                    memory_outcome,
                    context.observed_at,
                    suppression_until.as_deref(),
                    context.authorization.max_attempts > 0,
                    quarantined,
                );
            }
        },
        || {
            artifact_repair_outcome(
                operation_id,
                context.authorization.diagnosis_id,
                context.authorization.action,
                status,
                facts,
                summary,
            )
        },
    )
    .await
}

fn validate_artifact_repair_input<'a>(
    context: &'a ArtifactRepairContext<'a>,
) -> Result<DownloadChecksum<'a>, &'static str> {
    if !context.quarantines_existing {
        match context.descriptor.destination().try_exists() {
            Ok(false) => {}
            Ok(true) => return Err("guardian_missing_artifact_repair_blocked_target_exists"),
            Err(_) => return Err("guardian_missing_artifact_repair_blocked_target_unreadable"),
        }
    }
    let source = context.descriptor.repair_source();
    source_download_checksum(&source).ok_or("guardian_artifact_repair_blocked_invalid_checksum")
}

fn source_download_checksum<'a>(
    source: &GuardianArtifactRepairSource<'a>,
) -> Option<DownloadChecksum<'a>> {
    DownloadChecksumAlgorithm::parse(source.checksum_algorithm)
        .map(|algorithm| DownloadChecksum::new(algorithm, source.expected_checksum))
}

fn planned_artifact_journal(
    operation_id: &OperationId,
    context: &ArtifactRepairContext<'_>,
) -> OperationJournalEntry {
    let mut entry = OperationJournalEntry::new(
        JournalId::new(format!("journal-{}", operation_id.as_str())),
        operation_id.clone(),
        CommandKind::RepairInstance,
        StabilizationSystem::Guardian,
        context.authorization.ownership,
        RollbackState::Available,
    );
    entry.targets.push(context.authorization.target.clone());
    entry.planned_steps = artifact_repair_steps(context.quarantines_existing)
        .iter()
        .map(|(step_id, rollback)| {
            repair_step(
                step_id,
                OperationStepResult::Planned,
                Some(context.authorization.target.clone()),
                Vec::new(),
                *rollback,
            )
        })
        .collect();
    entry
        .guardian_diagnosis_ids
        .push(context.authorization.diagnosis_id);
    entry
}

async fn reconcile_artifact_journal_error(
    journals: &OperationJournalStore,
    operation_id: &OperationId,
    error: OperationJournalStoreError,
    expected: impl Fn(&OperationJournalEntry) -> bool,
) -> Result<ArtifactJournalReconciliation, OperationJournalStoreError> {
    match journals
        .reconcile_transition(
            operation_id,
            error,
            ARTIFACT_JOURNAL_RETRY_INITIAL_DELAY,
            ARTIFACT_JOURNAL_RETRY_MAX_DELAY,
            expected,
        )
        .await?
    {
        OperationJournalReconciliation::CommittedAfterPersistenceFailure(error) => {
            Ok(ArtifactJournalReconciliation::AcceptedFailure(error))
        }
        OperationJournalReconciliation::RequestedTransitionAlreadyCommitted => {
            Ok(ArtifactJournalReconciliation::MutationCommitted)
        }
        OperationJournalReconciliation::RetryRequestedTransition => {
            Ok(ArtifactJournalReconciliation::RetryMutation)
        }
    }
}

async fn create_planned_journal_reconciled(
    journals: &OperationJournalStore,
    operation_id: &OperationId,
    context: &ArtifactRepairContext<'_>,
) -> Result<Option<OperationJournalStoreError>, OperationJournalStoreError> {
    let expected = planned_artifact_journal(operation_id, context);
    loop {
        match journals.create(expected.clone()).await {
            Ok(()) => return Ok(None),
            Err(OperationJournalStoreError::AlreadyExists)
                if journals
                    .get(operation_id)
                    .is_some_and(|entry| operation_journal_plan_is_visible(&entry, &expected)) =>
            {
                return Ok(None);
            }
            Err(error) => {
                match reconcile_artifact_journal_error(journals, operation_id, error, |entry| {
                    operation_journal_plan_is_visible(entry, &expected)
                })
                .await?
                {
                    ArtifactJournalReconciliation::MutationCommitted => return Ok(None),
                    ArtifactJournalReconciliation::AcceptedFailure(error) => {
                        return Ok(Some(error));
                    }
                    ArtifactJournalReconciliation::RetryMutation => {}
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn create_terminal_journal_reconciled(
    journals: &OperationJournalStore,
    operation_id: &OperationId,
    context: &ArtifactRepairContext<'_>,
    status: OperationStatus,
    outcome: OperationOutcome,
    step_result: OperationStepResult,
    facts: Vec<String>,
) -> Result<Option<OperationJournalStoreError>, OperationJournalStoreError> {
    let expected =
        terminal_artifact_journal(operation_id, context, status, outcome, step_result, facts);
    loop {
        match journals.create(expected.clone()).await {
            Ok(()) => return Ok(None),
            Err(OperationJournalStoreError::AlreadyExists)
                if journals.get(operation_id).is_some_and(|entry| {
                    operation_journal_terminal_is_visible(&entry, &expected)
                }) =>
            {
                return Ok(None);
            }
            Err(error) => {
                match reconcile_artifact_journal_error(journals, operation_id, error, |entry| {
                    operation_journal_terminal_is_visible(entry, &expected)
                })
                .await?
                {
                    ArtifactJournalReconciliation::MutationCommitted => return Ok(None),
                    ArtifactJournalReconciliation::AcceptedFailure(error) => {
                        return Ok(Some(error));
                    }
                    ArtifactJournalReconciliation::RetryMutation => {}
                }
            }
        }
    }
}

async fn record_artifact_terminal_reconciled(
    journals: &OperationJournalStore,
    operation_id: &OperationId,
    step: OperationJournalStep,
    failure_point: Option<&str>,
) -> Result<Option<OperationJournalStoreError>, OperationJournalStoreError> {
    loop {
        let result = if let Some(failure_point) = failure_point {
            journals
                .record_failure(
                    operation_id,
                    step.clone(),
                    failure_point,
                    OperationOutcome::Failed,
                )
                .await
        } else {
            journals
                .record_success(operation_id, step.clone(), OperationOutcome::Succeeded)
                .await
        };
        match result {
            Ok(()) => return Ok(None),
            Err(OperationJournalStoreError::AlreadyTerminal)
                if journals.get(operation_id).is_some_and(|entry| {
                    artifact_terminal_transition_matches(&entry, operation_id, failure_point, &step)
                }) =>
            {
                return Ok(None);
            }
            Err(error) => {
                match reconcile_artifact_journal_error(journals, operation_id, error, |entry| {
                    artifact_terminal_transition_matches(entry, operation_id, failure_point, &step)
                })
                .await?
                {
                    ArtifactJournalReconciliation::MutationCommitted => return Ok(None),
                    ArtifactJournalReconciliation::AcceptedFailure(error) => {
                        return Ok(Some(error));
                    }
                    ArtifactJournalReconciliation::RetryMutation => {}
                }
            }
        }
    }
}

async fn terminalize_recovered_artifact_journal(
    journals: &OperationJournalStore,
    operation_id: &OperationId,
    target: &TargetDescriptor,
    rollback: RollbackState,
    failure_point: &str,
) -> Result<(), OperationJournalStoreError> {
    let step = repair_step(
        failure_point,
        OperationStepResult::Failed,
        Some(target.clone()),
        Vec::new(),
        rollback,
    );
    loop {
        let result = journals
            .record_failure(
                operation_id,
                step.clone(),
                failure_point,
                OperationOutcome::Failed,
            )
            .await;
        match result {
            Ok(()) => return Ok(()),
            Err(OperationJournalStoreError::AlreadyTerminal)
                if journals.get(operation_id).is_some_and(|entry| {
                    artifact_terminal_transition_matches(
                        &entry,
                        operation_id,
                        Some(failure_point),
                        &step,
                    )
                }) =>
            {
                return Ok(());
            }
            Err(error) => {
                match reconcile_artifact_journal_error(journals, operation_id, error, |entry| {
                    artifact_terminal_transition_matches(
                        entry,
                        operation_id,
                        Some(failure_point),
                        &step,
                    )
                })
                .await?
                {
                    ArtifactJournalReconciliation::MutationCommitted => return Ok(()),
                    ArtifactJournalReconciliation::AcceptedFailure(_) => return Ok(()),
                    ArtifactJournalReconciliation::RetryMutation => {}
                }
            }
        }
    }
}

fn terminal_artifact_journal(
    operation_id: &OperationId,
    context: &ArtifactRepairContext<'_>,
    status: OperationStatus,
    outcome: OperationOutcome,
    step_result: OperationStepResult,
    facts: Vec<String>,
) -> OperationJournalEntry {
    let mut entry = OperationJournalEntry::new(
        JournalId::new(format!("journal-{}", operation_id.as_str())),
        operation_id.clone(),
        CommandKind::RepairInstance,
        StabilizationSystem::Guardian,
        context.authorization.ownership,
        RollbackState::Available,
    );
    entry.status = status;
    entry.targets.push(context.authorization.target.clone());
    entry.planned_steps = artifact_repair_steps(context.quarantines_existing)
        .iter()
        .map(|(step_id, rollback)| {
            repair_step(
                step_id,
                OperationStepResult::Planned,
                Some(context.authorization.target.clone()),
                Vec::new(),
                *rollback,
            )
        })
        .collect();
    entry.completed_steps.push(repair_step(
        "guardian_artifact_repair_blocked",
        step_result,
        Some(context.authorization.target.clone()),
        facts,
        RollbackState::Available,
    ));
    entry
        .guardian_diagnosis_ids
        .push(context.authorization.diagnosis_id);
    entry.outcome = Some(outcome);
    entry
}

fn artifact_terminal_transition_matches(
    entry: &OperationJournalEntry,
    operation_id: &OperationId,
    failure_point: Option<&str>,
    step: &OperationJournalStep,
) -> bool {
    let (status, outcome) = if failure_point.is_some() {
        (OperationStatus::Failed, OperationOutcome::Failed)
    } else {
        (OperationStatus::Succeeded, OperationOutcome::Succeeded)
    };
    artifact_journal_identity_matches(entry, operation_id)
        && step.changed_target.as_ref().is_some_and(|target| {
            entry.targets.contains(target) && entry.ownership == target.ownership
        })
        && entry.status == status
        && entry.outcome == Some(outcome)
        && entry.failure_point.as_deref() == failure_point
        && operation_journal_completed_step_is_visible(entry, step)
}

fn artifact_journal_identity_matches(
    entry: &OperationJournalEntry,
    operation_id: &OperationId,
) -> bool {
    entry.operation_id == *operation_id
        && entry.command == CommandKind::RepairInstance
        && entry.owner == StabilizationSystem::Guardian
}

fn repair_step(
    step_id: &str,
    result: OperationStepResult,
    target: Option<TargetDescriptor>,
    facts: Vec<String>,
    rollback: RollbackState,
) -> OperationJournalStep {
    let mut step =
        OperationJournalStep::new(safe_id(step_id, "repair_step"), OperationPhase::Repairing);
    step.result = result;
    step.changed_target = target;
    step.generated_facts = facts;
    step.rollback = rollback;
    step
}

fn artifact_repair_steps(quarantines_existing: bool) -> &'static [(&'static str, RollbackState)] {
    const QUARANTINE_REDOWNLOAD: [(&str, RollbackState); 6] = [
        ("journal_repair_start", RollbackState::NotApplicable),
        (
            "quarantine_launcher_managed_target",
            RollbackState::Available,
        ),
        ("download_artifact_to_temp", RollbackState::Available),
        ("verify_artifact_checksum", RollbackState::NotApplicable),
        ("promote_verified_artifact", RollbackState::Available),
        ("record_repair_outcome", RollbackState::NotApplicable),
    ];
    const MISSING_DOWNLOAD: [(&str, RollbackState); 5] = [
        ("journal_repair_start", RollbackState::NotApplicable),
        ("download_artifact_to_temp", RollbackState::Available),
        ("verify_artifact_checksum", RollbackState::NotApplicable),
        ("promote_verified_artifact", RollbackState::Available),
        ("record_repair_outcome", RollbackState::NotApplicable),
    ];
    if quarantines_existing {
        &QUARANTINE_REDOWNLOAD
    } else {
        &MISSING_DOWNLOAD
    }
}

#[allow(clippy::too_many_arguments)]
fn record_artifact_repair_memory(
    failure_memory: &GuardianFailureMemoryStore,
    diagnosis_id: &DiagnosisId,
    mode: GuardianMode,
    target: &TargetDescriptor,
    outcome: FailureMemoryActionOutcome,
    observed_at: &str,
    suppression_until: Option<&str>,
    repair_attempt: bool,
    quarantined_target: Option<TargetDescriptor>,
) {
    let mut entry = GuardianFailureMemoryEntry::observed(
        *diagnosis_id,
        GuardianDomain::Install,
        target.clone(),
        mode,
        None,
        observed_at,
    )
    .with_action(GuardianActionKind::Repair, outcome);
    if repair_attempt {
        entry = entry.with_repair_attempt();
    }
    if let Some(suppression_until) = suppression_until {
        entry = entry.with_suppression_until(suppression_until);
    }
    if let Some(quarantined_target) = quarantined_target {
        entry = entry.with_quarantined_target(quarantined_target);
    }
    if let Err(error) = failure_memory.record(entry) {
        warn!(
            error_kind = error.class(),
            "failed to record Guardian artifact-repair failure memory"
        );
    }
}

fn default_suppression_until(observed_at: &str) -> Option<String> {
    DateTime::parse_from_rfc3339(observed_at)
        .ok()
        .map(|observed_at| {
            (observed_at + Duration::minutes(DEFAULT_ARTIFACT_REPAIR_SUPPRESSION_MINUTES))
                .to_rfc3339()
        })
}

fn fact_ids(facts: &[ExecutionFact]) -> Vec<String> {
    facts
        .iter()
        .map(|fact| fact.kind.as_str())
        .map(|fact| safe_id(fact, "execution_fact"))
        .collect()
}

fn artifact_repair_outcome(
    operation_id: OperationId,
    diagnosis_id: DiagnosisId,
    action: GuardianActionKind,
    status: GuardianArtifactRepairStatus,
    facts: Vec<String>,
    summary: &str,
) -> GuardianArtifactRepairOutcome {
    GuardianArtifactRepairOutcome {
        operation_id: OperationId::new(safe_id(operation_id.as_str(), "operation")),
        diagnosis_id,
        action,
        status,
        facts,
        summary: safe_id(summary, "guardian_artifact_repair"),
    }
}

fn safe_id(value: &str, fallback: &str) -> String {
    sanitize_evidence_token(value, RedactionAudience::UserVisible, 96)
        .unwrap_or_else(|| fallback.to_string())
}

fn new_repair_operation_id() -> OperationId {
    OperationId::new(format!("guardian-artifact-repair:{}", uuid::Uuid::new_v4()))
}

#[cfg(test)]
mod tests {
    use super::{GuardianArtifactRepairOutcome, GuardianArtifactRepairStatus};
    use crate::execution::file::{FileWriteRequest, write_file_atomically};
    use crate::execution::persistence::{AtomicWriteBackend, PersistenceCoordinator};
    use crate::guardian::{
        ActionPlanPrerequisite, DiagnosisId, GuardianAction, GuardianActionKind,
        GuardianActionPlan, GuardianConfidence, GuardianDecision,
        GuardianMinecraftArtifactRepairDescriptor, GuardianMode, RepairAuthorizationContext,
        RepairAuthorizationRejection, authorize_launcher_managed_artifact_repair,
        authorize_launcher_managed_missing_artifact_repair,
    };
    use crate::state::OperationJournalStore;
    use crate::state::contracts::{
        OperationId, OperationOutcome, OperationStatus, OperationStepResult, OwnershipClass,
        RollbackState, StabilizationSystem, TargetDescriptor, TargetKind,
    };
    use crate::state::failure_memory::{
        FailureMemoryActionOutcome, FailureMemoryKey, GuardianFailureMemoryEntry,
        GuardianFailureMemoryStore,
    };
    use axial_config::AppPaths;
    use reqwest::Client;
    use sha1::Sha1;
    use sha2::{Digest, Sha256};
    use std::fs;
    use std::io::{self, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    #[test]
    fn artifact_repair_status_ids_round_trip_strictly() {
        for status in [
            GuardianArtifactRepairStatus::Repaired,
            GuardianArtifactRepairStatus::Blocked,
            GuardianArtifactRepairStatus::Failed,
            GuardianArtifactRepairStatus::Suppressed,
        ] {
            assert_eq!(
                GuardianArtifactRepairStatus::from_persisted_id(status.as_persisted_id()),
                Some(status)
            );
        }
        assert_eq!(
            GuardianArtifactRepairStatus::from_persisted_id("Repaired"),
            None
        );
        assert_eq!(
            GuardianArtifactRepairStatus::from_persisted_id("legacy_repaired"),
            None
        );
    }

    async fn execute_quarantine(
        input: ArtifactRepairTestInput<'_>,
    ) -> GuardianArtifactRepairOutcome {
        execute_quarantine_result(input)
            .await
            .expect("persist Guardian artifact repair journal")
    }

    struct FailOnAttemptBackend {
        attempts: AtomicUsize,
        fail_on_attempt: usize,
    }

    impl AtomicWriteBackend for FailOnAttemptBackend {
        fn write(
            &self,
            target: &TargetDescriptor,
            destination: &std::path::Path,
            contents: &[u8],
        ) -> io::Result<()> {
            let attempt = self.attempts.fetch_add(1, Ordering::SeqCst) + 1;
            if attempt == self.fail_on_attempt {
                return Err(io::Error::other("injected Guardian journal failure"));
            }
            write_file_atomically(FileWriteRequest::new(target.clone(), destination, contents))
                .map(|_| ())
                .map_err(io::Error::from)
        }
    }

    #[test]
    fn artifact_repair_fact_ids_preserve_download_failure_family() {
        assert_eq!(
            crate::execution::ExecutionFactKind::DownloadPromotionFailed.as_str(),
            "download_promotion_failed"
        );
        assert_eq!(
            crate::execution::ExecutionFactKind::DownloadTempWriteFailed.as_str(),
            "download_temp_write_failed"
        );
        assert_eq!(
            crate::execution::ExecutionFactKind::DownloadTempDiscarded.as_str(),
            "download_temp_discarded"
        );
        assert_eq!(
            crate::execution::ExecutionFactKind::ProviderDataInvalid.as_str(),
            "provider_data_invalid"
        );
    }

    #[tokio::test]
    async fn repairs_launcher_managed_artifact_with_sha256_source() {
        let root = test_root("success");
        fs::create_dir_all(&root).expect("root");
        let destination = root.join("bad.jar");
        fs::write(&destination, b"corrupt").expect("corrupt artifact");
        let replacement = b"fresh artifact".to_vec();
        let server = TestByteServer::start(replacement.clone());
        let stores = stores();

        let outcome = execute_quarantine(quarantine_input(
            &destination,
            &server.url,
            &sha256_hex(&replacement),
            Some(replacement.len() as u64),
            &stores,
            "2026-06-15T10:00:00Z",
        ))
        .await;

        assert_eq!(outcome.status, GuardianArtifactRepairStatus::Repaired);
        assert_eq!(fs::read(&destination).expect("replacement"), replacement);
        assert!(root_contains_quarantine(&root, b"corrupt"));
        assert_eq!(server.request_count(), 1);
        assert!(outcome.facts.iter().any(|fact| fact == "download_promoted"));
        let journal = stores.journals.get(&outcome.operation_id).expect("journal");
        assert_eq!(journal.status, OperationStatus::Succeeded);
        assert_eq!(journal.outcome, Some(OperationOutcome::Succeeded));
        let memory = stores.failure_memory.list();
        assert_eq!(
            memory[0].last_action_outcome,
            Some(FailureMemoryActionOutcome::Repaired)
        );
        assert_eq!(memory[0].repair_attempt_count, 1);
        assert!(memory[0].quarantined_target.is_some());

        server.stop();
        cleanup(&root);
    }

    #[tokio::test]
    async fn planned_commit_failure_prevents_artifact_mutation() {
        let root = test_root("planned-commit-gate");
        fs::create_dir_all(&root).expect("root");
        let destination = root.join("bad.jar");
        fs::write(&destination, b"corrupt").expect("corrupt artifact");
        let replacement = b"fresh artifact".to_vec();
        let server = TestByteServer::start(replacement.clone());
        let stores = persistent_stores(&root, 1);

        let result = execute_quarantine_result(quarantine_input(
            &destination,
            &server.url,
            &sha256_hex(&replacement),
            Some(replacement.len() as u64),
            &stores,
            "2026-06-15T10:00:00Z",
        ))
        .await;

        assert!(result.is_err());
        assert_eq!(fs::read(&destination).expect("original"), b"corrupt");
        assert!(!root_contains_quarantine(&root, b"corrupt"));
        assert_eq!(server.request_count(), 0);
        let journal = stores
            .journals
            .get(&OperationId::new("guardian-artifact-repair-test"))
            .expect("recovered repair journal");
        assert_eq!(journal.status, OperationStatus::Failed);
        assert_eq!(journal.outcome, Some(OperationOutcome::Failed));
        assert!(!stores.journals.has_retry_candidate());

        server.stop();
        cleanup(&root);
    }

    #[tokio::test]
    async fn quarantine_checkpoint_failure_prevents_redownload() {
        let root = test_root("quarantine-checkpoint-gate");
        fs::create_dir_all(&root).expect("root");
        let destination = root.join("bad.jar");
        fs::write(&destination, b"corrupt").expect("corrupt artifact");
        let replacement = b"fresh artifact".to_vec();
        let server = TestByteServer::start(replacement.clone());
        let stores = persistent_stores(&root, 2);

        let result = execute_quarantine_result(quarantine_input(
            &destination,
            &server.url,
            &sha256_hex(&replacement),
            Some(replacement.len() as u64),
            &stores,
            "2026-06-15T10:00:00Z",
        ))
        .await;

        assert!(result.is_err());
        assert!(!destination.exists());
        assert!(root_contains_quarantine(&root, b"corrupt"));
        assert_eq!(server.request_count(), 0);
        let journal = stores
            .journals
            .get(&OperationId::new("guardian-artifact-repair-test"))
            .expect("terminalized checkpoint journal");
        assert_eq!(journal.status, OperationStatus::Failed);
        assert_eq!(journal.outcome, Some(OperationOutcome::Failed));
        assert!(journal.completed_steps.iter().any(|step| {
            step.step_id == "quarantine_launcher_managed_target"
                && step.result == OperationStepResult::Completed
        }));
        assert!(!stores.journals.has_retry_candidate());

        server.stop();
        cleanup(&root);
    }

    #[tokio::test]
    async fn repairs_launcher_managed_artifact_with_sha1_source() {
        let root = test_root("sha1-success");
        fs::create_dir_all(&root).expect("root");
        let destination = root.join("bad.jar");
        fs::write(&destination, b"corrupt").expect("corrupt artifact");
        let replacement = b"fresh minecraft artifact".to_vec();
        let server = TestByteServer::start(replacement.clone());
        let stores = stores();

        let outcome = execute_quarantine(quarantine_input_with_checksum(
            &destination,
            &server.url,
            "sha1",
            &sha1_hex(&replacement),
            Some(replacement.len() as u64),
            &stores,
            "2026-06-15T10:00:00Z",
        ))
        .await;

        assert_eq!(outcome.status, GuardianArtifactRepairStatus::Repaired);
        assert_eq!(fs::read(&destination).expect("replacement"), replacement);
        assert!(root_contains_quarantine(&root, b"corrupt"));
        assert_eq!(server.request_count(), 1);
        assert!(outcome.facts.iter().any(|fact| fact == "download_promoted"));

        server.stop();
        cleanup(&root);
    }

    #[tokio::test]
    async fn repairs_missing_launcher_managed_artifact_without_quarantine() {
        let root = test_root("missing-success");
        fs::create_dir_all(&root).expect("root");
        let destination = root.join("missing.jar");
        let replacement = b"fresh missing artifact".to_vec();
        let server = TestByteServer::start(replacement.clone());
        let stores = stores();

        let outcome = execute_missing(missing_input_with_checksum(
            &destination,
            &server.url,
            "sha1",
            &sha1_hex(&replacement),
            Some(replacement.len() as u64),
            &stores,
            "2026-06-15T10:00:00Z",
        ))
        .await;

        assert_eq!(outcome.status, GuardianArtifactRepairStatus::Repaired);
        assert_eq!(fs::read(&destination).expect("replacement"), replacement);
        assert!(!root_contains_quarantine(&root, b"fresh missing artifact"));
        assert_eq!(server.request_count(), 1);
        let journal = stores.journals.get(&outcome.operation_id).expect("journal");
        assert_eq!(journal.status, OperationStatus::Succeeded);
        assert!(
            !journal
                .completed_steps
                .iter()
                .any(|step| { step.step_id.contains("quarantine_launcher_managed_target") })
        );
        let memory = stores.failure_memory.list();
        assert_eq!(
            memory[0].last_action_outcome,
            Some(FailureMemoryActionOutcome::Repaired)
        );
        assert!(memory[0].quarantined_target.is_none());

        server.stop();
        cleanup(&root);
    }

    #[tokio::test]
    async fn checksum_failure_records_failed_without_repaired_status() {
        let root = test_root("checksum-failure");
        fs::create_dir_all(&root).expect("root");
        let destination = root.join("bad.jar");
        fs::write(&destination, b"corrupt").expect("corrupt artifact");
        let replacement = b"fresh artifact".to_vec();
        let server = TestByteServer::start(replacement);
        let stores = stores();

        let outcome = execute_quarantine(quarantine_input(
            &destination,
            &server.url,
            &sha256_hex(b"different artifact"),
            None,
            &stores,
            "2026-06-15T10:00:00Z",
        ))
        .await;

        assert_eq!(outcome.status, GuardianArtifactRepairStatus::Failed);
        assert!(!destination.exists());
        assert!(root_contains_quarantine(&root, b"corrupt"));
        assert!(
            outcome
                .facts
                .iter()
                .any(|fact| fact == "download_checksum_mismatch")
        );
        let journal = stores.journals.get(&outcome.operation_id).expect("journal");
        assert_eq!(journal.status, OperationStatus::Failed);
        assert_eq!(journal.outcome, Some(OperationOutcome::Failed));
        let memory = stores.failure_memory.list();
        assert_eq!(
            memory[0].last_action_outcome,
            Some(FailureMemoryActionOutcome::Failed)
        );
        assert_eq!(memory[0].repair_attempt_count, 1);

        server.stop();
        cleanup(&root);
    }

    #[tokio::test]
    async fn missing_artifact_download_failure_records_unavailable_rollback() {
        let root = test_root("missing-checksum-failure");
        fs::create_dir_all(&root).expect("root");
        let destination = root.join("missing.jar");
        let replacement = b"fresh missing artifact".to_vec();
        let server = TestByteServer::start(replacement);
        let stores = stores();

        let outcome = execute_missing(missing_input_with_checksum(
            &destination,
            &server.url,
            "sha256",
            &sha256_hex(b"different artifact"),
            None,
            &stores,
            "2026-06-15T10:00:00Z",
        ))
        .await;

        assert_eq!(outcome.status, GuardianArtifactRepairStatus::Failed);
        assert!(!destination.exists());
        assert_eq!(server.request_count(), 1);
        let journal = stores.journals.get(&outcome.operation_id).expect("journal");
        assert_eq!(journal.status, OperationStatus::Failed);
        assert_eq!(journal.outcome, Some(OperationOutcome::Failed));
        let failure_step = journal.completed_steps.last().expect("failure step");
        assert_eq!(failure_step.step_id, "download_artifact_to_temp");
        assert_eq!(failure_step.result, OperationStepResult::Failed);
        assert_eq!(failure_step.rollback, RollbackState::Unavailable);
        assert!(
            !journal
                .completed_steps
                .iter()
                .any(|step| step.step_id == "quarantine_launcher_managed_target")
        );
        let memory = stores.failure_memory.list();
        assert_eq!(
            memory[0].last_action_outcome,
            Some(FailureMemoryActionOutcome::Failed)
        );
        assert!(memory[0].quarantined_target.is_none());

        server.stop();
        cleanup(&root);
    }

    #[tokio::test]
    async fn suppression_blocks_before_filesystem_or_network_mutation() {
        let root = test_root("suppressed");
        fs::create_dir_all(&root).expect("root");
        let destination = root.join("bad.jar");
        fs::write(&destination, b"corrupt").expect("corrupt artifact");
        let replacement = b"fresh artifact".to_vec();
        let server = TestByteServer::start(replacement.clone());
        let stores = stores();
        stores
            .failure_memory
            .record(
                GuardianFailureMemoryEntry::observed(
                    DiagnosisId::LauncherManagedArtifactCorrupt,
                    crate::guardian::GuardianDomain::Install,
                    artifact_repair_target(),
                    GuardianMode::Managed,
                    None,
                    "2026-06-15T09:00:00Z",
                )
                .with_action(
                    crate::guardian::GuardianActionKind::Repair,
                    FailureMemoryActionOutcome::Failed,
                )
                .with_repair_attempt()
                .with_suppression_until("2026-06-15T10:30:00Z"),
            )
            .expect("memory");

        let outcome = execute_quarantine(quarantine_input(
            &destination,
            &server.url,
            &sha256_hex(&replacement),
            Some(replacement.len() as u64),
            &stores,
            "2026-06-15T10:00:00Z",
        ))
        .await;

        assert_eq!(outcome.status, GuardianArtifactRepairStatus::Suppressed);
        assert_eq!(fs::read(&destination).expect("original"), b"corrupt");
        assert_eq!(server.request_count(), 0);
        let key = FailureMemoryKey::for_observation(
            crate::guardian::GuardianDomain::Install,
            &DiagnosisId::LauncherManagedArtifactCorrupt,
            &artifact_repair_target(),
            GuardianMode::Managed,
            None,
        );
        let memory = stores.failure_memory.get(&key).expect("memory");
        assert_eq!(
            memory.last_action_outcome,
            Some(FailureMemoryActionOutcome::Failed)
        );
        assert_eq!(memory.occurrence_count, 1);
        assert_eq!(memory.repair_attempt_count, 1);

        server.stop();
        cleanup(&root);
    }

    #[tokio::test]
    async fn historical_artifact_attempt_without_cooldown_allows_safe_retry() {
        let root = test_root("historical-attempt-without-cooldown");
        fs::create_dir_all(&root).expect("root");
        let destination = root.join("bad.jar");
        fs::write(&destination, b"corrupt").expect("corrupt artifact");
        let replacement = b"fresh artifact".to_vec();
        let server = TestByteServer::start(replacement.clone());
        let stores = stores();
        stores
            .failure_memory
            .record(
                GuardianFailureMemoryEntry::observed(
                    DiagnosisId::LauncherManagedArtifactCorrupt,
                    crate::guardian::GuardianDomain::Install,
                    artifact_repair_target(),
                    GuardianMode::Managed,
                    None,
                    "2026-06-15T09:00:00Z",
                )
                .with_action(
                    crate::guardian::GuardianActionKind::Repair,
                    FailureMemoryActionOutcome::Failed,
                )
                .with_repair_attempt(),
            )
            .expect("memory");

        let outcome = execute_quarantine(quarantine_input(
            &destination,
            &server.url,
            &sha256_hex(&replacement),
            Some(replacement.len() as u64),
            &stores,
            "2026-06-15T10:00:00Z",
        ))
        .await;

        assert_eq!(outcome.status, GuardianArtifactRepairStatus::Repaired);
        assert_eq!(fs::read(&destination).expect("replacement"), replacement);
        assert_eq!(server.request_count(), 1);
        let memory = stores.failure_memory.list();
        assert_eq!(memory[0].repair_attempt_count, 2);
        assert_eq!(
            memory[0].last_action_outcome,
            Some(FailureMemoryActionOutcome::Repaired)
        );
        assert!(memory[0].suppression_until.is_some());

        server.stop();
        cleanup(&root);
    }

    #[tokio::test]
    async fn expired_artifact_repair_cooldown_allows_new_safe_attempt() {
        let root = test_root("expired-artifact-cooldown");
        fs::create_dir_all(&root).expect("root");
        let destination = root.join("bad.jar");
        fs::write(&destination, b"corrupt").expect("corrupt artifact");
        let replacement = b"fresh artifact after cooldown".to_vec();
        let server = TestByteServer::start(replacement.clone());
        let stores = stores();
        stores
            .failure_memory
            .record(
                GuardianFailureMemoryEntry::observed(
                    DiagnosisId::LauncherManagedArtifactCorrupt,
                    crate::guardian::GuardianDomain::Install,
                    artifact_repair_target(),
                    GuardianMode::Managed,
                    None,
                    "2026-06-15T09:00:00Z",
                )
                .with_action(
                    crate::guardian::GuardianActionKind::Repair,
                    FailureMemoryActionOutcome::Failed,
                )
                .with_repair_attempt()
                .with_suppression_until("2026-06-15T09:30:00Z"),
            )
            .expect("memory");

        let outcome = execute_quarantine(quarantine_input(
            &destination,
            &server.url,
            &sha256_hex(&replacement),
            Some(replacement.len() as u64),
            &stores,
            "2026-06-15T10:00:00Z",
        ))
        .await;

        assert_eq!(outcome.status, GuardianArtifactRepairStatus::Repaired);
        assert_eq!(fs::read(&destination).expect("replacement"), replacement);
        assert!(root_contains_quarantine(&root, b"corrupt"));
        assert_eq!(server.request_count(), 1);
        let memory = stores.failure_memory.list();
        assert_eq!(memory[0].repair_attempt_count, 2);
        assert_eq!(
            memory[0].last_action_outcome,
            Some(FailureMemoryActionOutcome::Repaired)
        );
        assert_ne!(
            memory[0].suppression_until.as_deref(),
            Some("2026-06-15T09:30:00Z")
        );

        server.stop();
        cleanup(&root);
    }

    #[tokio::test]
    async fn expired_missing_artifact_repair_cooldown_allows_new_safe_attempt() {
        let root = test_root("expired-missing-artifact-cooldown");
        fs::create_dir_all(&root).expect("root");
        let destination = root.join("missing.jar");
        let replacement = b"fresh missing artifact after cooldown".to_vec();
        let server = TestByteServer::start(replacement.clone());
        let stores = stores();
        stores
            .failure_memory
            .record(
                GuardianFailureMemoryEntry::observed(
                    DiagnosisId::LauncherManagedArtifactCorrupt,
                    crate::guardian::GuardianDomain::Install,
                    artifact_repair_target(),
                    GuardianMode::Managed,
                    None,
                    "2026-06-15T09:00:00Z",
                )
                .with_action(
                    crate::guardian::GuardianActionKind::Repair,
                    FailureMemoryActionOutcome::Failed,
                )
                .with_repair_attempt()
                .with_suppression_until("2026-06-15T09:30:00Z"),
            )
            .expect("memory");

        let outcome = execute_missing(missing_input_with_checksum(
            &destination,
            &server.url,
            "sha1",
            &sha1_hex(&replacement),
            Some(replacement.len() as u64),
            &stores,
            "2026-06-15T10:00:00Z",
        ))
        .await;

        assert_eq!(outcome.status, GuardianArtifactRepairStatus::Repaired);
        assert_eq!(fs::read(&destination).expect("replacement"), replacement);
        assert!(!root_contains_quarantine(
            &root,
            b"fresh missing artifact after cooldown"
        ));
        assert_eq!(server.request_count(), 1);
        let memory = stores.failure_memory.list();
        assert_eq!(memory[0].repair_attempt_count, 2);
        assert_eq!(
            memory[0].last_action_outcome,
            Some(FailureMemoryActionOutcome::Repaired)
        );
        assert!(memory[0].quarantined_target.is_none());

        server.stop();
        cleanup(&root);
    }

    #[test]
    fn descriptor_target_mismatch_rejects_without_journal_or_effect() {
        let root = test_root("descriptor-target-mismatch");
        fs::create_dir_all(&root).expect("root");
        let destination = root.join("bad.jar");
        fs::write(&destination, b"corrupt").expect("corrupt artifact");
        let server = TestByteServer::start(b"fresh artifact".to_vec());
        let stores = stores();
        let descriptor = GuardianMinecraftArtifactRepairDescriptor::for_test(
            TargetDescriptor::new(
                StabilizationSystem::Execution,
                TargetKind::Artifact,
                "different_artifact",
                OwnershipClass::LauncherManaged,
            ),
            &destination,
            &server.url,
            "sha256",
            &sha256_hex(b"fresh artifact"),
            Some(14),
            1024,
        )
        .expect("valid mismatched descriptor");

        let error = match authorize_launcher_managed_artifact_repair(
            &artifact_repair_decision(),
            RepairAuthorizationContext::current_operation(),
            descriptor,
        ) {
            Ok(_) => panic!("descriptor target mismatch must reject"),
            Err(error) => error,
        };

        assert_eq!(
            error,
            RepairAuthorizationRejection::DescriptorTargetMismatch
        );
        assert_eq!(fs::read(&destination).expect("original"), b"corrupt");
        assert_eq!(server.request_count(), 0);
        assert!(stores.journals.list().is_empty());
        assert!(stores.failure_memory.list().is_empty());
        assert!(!root_contains_quarantine(&root, b"corrupt"));

        server.stop();
        cleanup(&root);
    }

    #[tokio::test]
    async fn public_outcome_does_not_expose_source_or_paths() {
        let root = test_root("redaction");
        fs::create_dir_all(&root).expect("root");
        let destination = root.join("bad.jar");
        fs::write(&destination, b"corrupt").expect("corrupt artifact");
        let server = TestByteServer::start(b"fresh artifact".to_vec());
        let stores = stores();
        let url = format!("{}?token=secret", server.url);

        let outcome = execute_quarantine(quarantine_input(
            &destination,
            &url,
            &sha256_hex(b"different artifact"),
            None,
            &stores,
            "2026-06-15T10:00:00Z",
        ))
        .await;
        let encoded = serde_json::to_string(&outcome).expect("outcome json");
        let lower = encoded.to_ascii_lowercase();

        assert_eq!(outcome.status, GuardianArtifactRepairStatus::Failed);
        assert!(!lower.contains("token"));
        assert!(!lower.contains("secret"));
        assert!(!lower.contains(root.to_string_lossy().as_ref()));

        server.stop();
        cleanup(&root);
    }

    struct ArtifactRepairTestInput<'a> {
        descriptor: GuardianMinecraftArtifactRepairDescriptor,
        stores: &'a Stores,
        observed_at: &'a str,
    }

    fn quarantine_input<'a>(
        destination: &std::path::Path,
        url: &str,
        expected_sha256: &str,
        expected_size: Option<u64>,
        stores: &'a Stores,
        observed_at: &'a str,
    ) -> ArtifactRepairTestInput<'a> {
        quarantine_input_with_checksum(
            destination,
            url,
            "sha256",
            expected_sha256,
            expected_size,
            stores,
            observed_at,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn quarantine_input_with_checksum<'a>(
        destination: &std::path::Path,
        url: &str,
        checksum_algorithm: &str,
        expected_checksum: &str,
        expected_size: Option<u64>,
        stores: &'a Stores,
        observed_at: &'a str,
    ) -> ArtifactRepairTestInput<'a> {
        ArtifactRepairTestInput {
            descriptor: GuardianMinecraftArtifactRepairDescriptor::for_test(
                artifact_repair_target(),
                destination,
                url,
                checksum_algorithm,
                expected_checksum,
                expected_size,
                1024,
            )
            .expect("valid artifact repair test descriptor"),
            stores,
            observed_at,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn missing_input_with_checksum<'a>(
        destination: &std::path::Path,
        url: &str,
        checksum_algorithm: &str,
        expected_checksum: &str,
        expected_size: Option<u64>,
        stores: &'a Stores,
        observed_at: &'a str,
    ) -> ArtifactRepairTestInput<'a> {
        quarantine_input_with_checksum(
            destination,
            url,
            checksum_algorithm,
            expected_checksum,
            expected_size,
            stores,
            observed_at,
        )
    }

    async fn execute_quarantine_result(
        input: ArtifactRepairTestInput<'_>,
    ) -> Result<GuardianArtifactRepairOutcome, crate::state::OperationJournalStoreError> {
        let ArtifactRepairTestInput {
            descriptor,
            stores,
            observed_at,
        } = input;
        let authorization = authorize_launcher_managed_artifact_repair(
            &artifact_repair_decision(),
            RepairAuthorizationContext::current_operation(),
            descriptor,
        )
        .expect("quarantine-redownload authorization");
        super::execute_guardian_quarantine_redownload(
            authorization,
            Some(OperationId::new("guardian-artifact-repair-test")),
            &stores.client,
            &stores.journals,
            &stores.failure_memory,
            observed_at,
        )
        .await
    }

    async fn execute_missing(input: ArtifactRepairTestInput<'_>) -> GuardianArtifactRepairOutcome {
        let ArtifactRepairTestInput {
            descriptor,
            stores,
            observed_at,
        } = input;
        let authorization = authorize_launcher_managed_missing_artifact_repair(
            &artifact_repair_decision(),
            RepairAuthorizationContext::current_operation(),
            descriptor,
        )
        .expect("missing-download authorization");
        super::execute_guardian_missing_download(
            authorization,
            Some(OperationId::new("guardian-artifact-repair-test")),
            &stores.client,
            &stores.journals,
            &stores.failure_memory,
            observed_at,
        )
        .await
        .expect("persist Guardian missing-artifact repair journal")
    }

    fn artifact_repair_decision() -> GuardianDecision {
        let target = artifact_repair_target();
        GuardianDecision::for_test(
            Some(OperationId::new("operation-install-repair")),
            GuardianMode::Managed,
            GuardianActionKind::Repair,
            vec![DiagnosisId::LauncherManagedArtifactCorrupt],
            Some(GuardianActionPlan::new(
                StabilizationSystem::Guardian,
                ActionPlanPrerequisite {
                    diagnosis_id: DiagnosisId::LauncherManagedArtifactCorrupt,
                    ownership: OwnershipClass::LauncherManaged,
                    confidence: GuardianConfidence::Confirmed,
                    affected_targets: vec![target.clone()],
                    candidate_actions: vec![
                        GuardianActionKind::Quarantine,
                        GuardianActionKind::Repair,
                        GuardianActionKind::Block,
                    ],
                },
                vec![GuardianAction {
                    kind: GuardianActionKind::Repair,
                    target: Some(target),
                    reason: DiagnosisId::LauncherManagedArtifactCorrupt,
                }],
            )),
        )
    }

    fn artifact_repair_target() -> TargetDescriptor {
        TargetDescriptor::new(
            StabilizationSystem::Execution,
            TargetKind::Artifact,
            "libraries_com_example_bad-1.0.jar",
            OwnershipClass::LauncherManaged,
        )
    }

    struct Stores {
        journals: OperationJournalStore,
        failure_memory: GuardianFailureMemoryStore,
        client: Client,
    }

    fn stores() -> Stores {
        Stores {
            journals: OperationJournalStore::new(),
            failure_memory: GuardianFailureMemoryStore::new(),
            client: Client::new(),
        }
    }

    fn persistent_stores(root: &std::path::Path, fail_on_attempt: usize) -> Stores {
        let backend = Arc::new(FailOnAttemptBackend {
            attempts: AtomicUsize::new(0),
            fail_on_attempt,
        });
        let coordinator = PersistenceCoordinator::for_test(
            backend,
            Duration::from_millis(20),
            Duration::from_millis(100),
        );
        let journals = OperationJournalStore::try_load_from_paths_with_coordinator(
            &test_paths(root),
            coordinator,
        )
        .expect("claim operation journal persistence");
        Stores {
            journals,
            failure_memory: GuardianFailureMemoryStore::new(),
            client: Client::new(),
        }
    }

    fn test_paths(root: &std::path::Path) -> AppPaths {
        AppPaths {
            config_file: root.join("config").join("config.json"),
            instances_file: root.join("config").join("instances.json"),
            instances_dir: root.join("instances"),
            music_dir: root.join("music"),
            library_dir: root.join("library"),
            config_dir: root.join("config"),
        }
    }

    fn sha256_hex(bytes: impl AsRef<[u8]>) -> String {
        format!("{:x}", Sha256::digest(bytes.as_ref()))
    }

    fn sha1_hex(bytes: impl AsRef<[u8]>) -> String {
        format!("{:x}", Sha1::digest(bytes.as_ref()))
    }

    fn root_contains_quarantine(root: &std::path::Path, bytes: &[u8]) -> bool {
        fs::read_dir(root)
            .expect("read root")
            .filter_map(Result::ok)
            .any(|entry| {
                entry.file_name().to_string_lossy().contains(".quarantine-")
                    && fs::read(entry.path()).is_ok_and(|value| value == bytes)
            })
    }

    fn test_root(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or_default();
        std::env::temp_dir().join(format!(
            "axial-artifact-repair-{prefix}-{}-{nanos:x}",
            std::process::id()
        ))
    }

    fn cleanup(root: &PathBuf) {
        let _ = fs::remove_dir_all(root);
    }

    struct TestByteServer {
        url: String,
        request_count: Arc<AtomicUsize>,
        stop_server: mpsc::Sender<()>,
        server: thread::JoinHandle<()>,
    }

    impl TestByteServer {
        fn start(body: Vec<u8>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
            listener
                .set_nonblocking(true)
                .expect("set test server nonblocking");
            let url = format!(
                "http://{}/artifact.jar",
                listener.local_addr().expect("server addr")
            );
            let request_count = Arc::new(AtomicUsize::new(0));
            let server_request_count = Arc::clone(&request_count);
            let (stop_server, server_stopped) = mpsc::channel();
            let server = thread::spawn(move || {
                loop {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            server_request_count.fetch_add(1, Ordering::SeqCst);
                            respond_ok(stream, &body);
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            if server_stopped.try_recv().is_ok() {
                                break;
                            }
                            thread::sleep(Duration::from_millis(10));
                        }
                        Err(error) => panic!("accept connection: {error}"),
                    }
                }
            });

            Self {
                url,
                request_count,
                stop_server,
                server,
            }
        }

        fn request_count(&self) -> usize {
            self.request_count.load(Ordering::SeqCst)
        }

        fn stop(self) {
            self.stop_server.send(()).expect("stop test server");
            self.server.join().expect("server thread");
        }
    }

    fn respond_ok(mut stream: TcpStream, body: &[u8]) {
        let mut buffer = [0_u8; 1024];
        let _ = stream.read(&mut buffer);
        let header = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream
            .write_all(header.as_bytes())
            .expect("write response header");
        stream.write_all(body).expect("write response body");
    }
}
