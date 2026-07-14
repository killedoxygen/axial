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
    OperationOutcome, OperationPhase, OperationStatus, OperationStepResult, ReconciliationAttempt,
    ReconciliationScope, ReconciliationTerminal, ReconciliationTerminalOutcome, RollbackState,
    StabilizationSystem, TargetDescriptor,
};
use crate::state::failure_memory::GuardianFailureMemoryStore;
use crate::state::{
    OperationJournalReconciliation, OperationJournalStore, OperationJournalStoreError,
    ReconciliationAttemptReservation, RegisteredArtifactRepairAdmission,
    RegisteredArtifactRepairEffect, commit_reconciliation_memory,
    install_operation_reconciliation_attempt, operation_journal_completed_step_is_visible,
    operation_journal_plan_is_visible, operation_journal_terminal_is_visible,
    reconciliation_attempt_key, reconciliation_instance_target, reconciliation_journal_attempt,
    reconciliation_memory_entry, reconciliation_terminal, record_reconciliation_journal_failure,
    record_reconciliation_journal_success, reserve_reconciliation_attempt,
    settle_reconciliation_memory,
};
use chrono::{DateTime, Duration};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::time::Duration as StdDuration;

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
}

impl GuardianArtifactRepairStatus {
    pub const fn as_persisted_id(self) -> &'static str {
        match self {
            Self::Repaired => "repaired",
            Self::Blocked => "blocked",
            Self::Failed => "failed",
        }
    }

    pub fn from_persisted_id(value: &str) -> Option<Self> {
        match value {
            "repaired" => Some(Self::Repaired),
            "blocked" => Some(Self::Blocked),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }
}

enum ArtifactTerminal {
    Blocked(&'static str),
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
        quarantined_target: Option<TargetDescriptor>,
    },
}

enum ArtifactTerminalJournal {
    Create(OperationOutcome),
    Record(&'static str, RollbackState, Option<&'static str>),
}

struct ArtifactRepairContext<'a> {
    authorization: ArtifactAuthorization,
    descriptor: Option<GuardianMinecraftArtifactRepairDescriptor>,
    client: &'a Client,
    journals: &'a OperationJournalStore,
    failure_memory: &'a GuardianFailureMemoryStore,
    observed_at: String,
    quarantines_existing: bool,
    attempt: Option<ReconciliationAttempt>,
    reservation: Option<ReconciliationAttemptReservation>,
    registered_admission: Option<&'a RegisteredArtifactRepairAdmission>,
}

struct ArtifactAuthorization {
    diagnosis_id: DiagnosisId,
    target: TargetDescriptor,
    ownership: crate::state::contracts::OwnershipClass,
    mode: GuardianMode,
    action: GuardianActionKind,
    max_attempts: u32,
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
    let reconciliation_target = descriptor.reconciliation_target().clone();
    let mut context = ArtifactRepairContext {
        authorization: ArtifactAuthorization {
            diagnosis_id: authorization.diagnosis_id,
            target: reconciliation_target,
            ownership: authorization.ownership,
            mode: authorization.mode,
            action: authorization.action,
            max_attempts: authorization.max_attempts,
        },
        descriptor: Some(descriptor),
        client,
        journals,
        failure_memory,
        observed_at: observed_at.to_string(),
        quarantines_existing: K::QUARANTINES_EXISTING,
        attempt: None,
        reservation: None,
        registered_admission: None,
    };
    if let Err(block_reason) = validate_artifact_repair_input(
        context
            .descriptor
            .as_ref()
            .expect("install repair has descriptor"),
        context.quarantines_existing,
    ) {
        return finish_artifact_repair(
            &context,
            operation_id,
            ArtifactTerminal::Blocked(block_reason),
        )
        .await;
    }

    if context.authorization.max_attempts == 0 {
        return finish_artifact_repair(
            &context,
            operation_id,
            ArtifactTerminal::Blocked("guardian_artifact_repair_blocked_by_policy"),
        )
        .await;
    }
    settle_reconciliation_memory(context.failure_memory)
        .await
        .map_err(artifact_memory_error)?;
    let suppression_until = default_suppression_until(&context.observed_at).ok_or_else(|| {
        OperationJournalStoreError::Persistence(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "Guardian artifact repair observation timestamp is invalid",
        ))
    })?;
    let attempt = install_operation_reconciliation_attempt(
        operation_id.clone(),
        context.authorization.diagnosis_id,
        GuardianDomain::Install,
        context
            .descriptor
            .as_ref()
            .expect("install repair has descriptor")
            .component(),
        context.authorization.target.clone(),
        context.authorization.mode,
        &context.observed_at,
        &suppression_until,
    )
    .map_err(artifact_reconciliation_error)?;
    let attempt_key = reconciliation_attempt_key(&attempt);
    let reservation = reserve_reconciliation_attempt(
        context.failure_memory,
        context.journals,
        attempt_key.clone(),
    )
    .map_err(|_| {
        OperationJournalStoreError::Persistence(std::io::Error::new(
            std::io::ErrorKind::WouldBlock,
            "Guardian artifact reconciliation attempt is already active",
        ))
    })?;
    context.attempt = Some(attempt);
    context.reservation = Some(reservation);
    execute_admitted_artifact_repair(context, operation_id).await
}

pub(crate) async fn execute_registered_guardian_artifact_repair(
    admission: RegisteredArtifactRepairAdmission,
    client: &Client,
) -> Result<GuardianArtifactRepairOutcome, OperationJournalStoreError> {
    let attempt = admission.attempt().clone();
    let operation_id = attempt.operation_id().clone();
    let quarantines_existing =
        admission.effect() == RegisteredArtifactRepairEffect::QuarantineRedownload;
    let mut context = ArtifactRepairContext {
        authorization: ArtifactAuthorization {
            diagnosis_id: attempt.diagnosis_id(),
            target: attempt.target().clone(),
            ownership: attempt.ownership(),
            mode: attempt.mode(),
            action: GuardianActionKind::Repair,
            max_attempts: 1,
        },
        descriptor: None,
        client,
        journals: admission.authority().journals(),
        failure_memory: admission.authority().failure_memory(),
        observed_at: attempt.observed_at().to_string(),
        quarantines_existing,
        attempt: Some(attempt),
        reservation: None,
        registered_admission: Some(&admission),
    };

    settle_reconciliation_memory(context.failure_memory)
        .await
        .map_err(artifact_memory_error)?;
    let attempt_key = reconciliation_attempt_key(
        context
            .attempt
            .as_ref()
            .expect("registered repair has typed attempt"),
    );
    context.reservation = Some(
        reserve_reconciliation_attempt(context.failure_memory, context.journals, attempt_key)
            .map_err(|_| {
                OperationJournalStoreError::Persistence(std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    "Guardian registered artifact reconciliation attempt is already active",
                ))
            })?,
    );
    execute_admitted_artifact_repair(context, operation_id).await
}

async fn execute_admitted_artifact_repair(
    context: ArtifactRepairContext<'_>,
    operation_id: OperationId,
) -> Result<GuardianArtifactRepairOutcome, OperationJournalStoreError> {
    let target = context.authorization.target.clone();
    let attempt_key = reconciliation_attempt_key(
        context
            .attempt
            .as_ref()
            .expect("admitted repair has typed attempt"),
    );
    if let Some(outcome) = recover_artifact_evidence(&context, &attempt_key).await? {
        return Ok(outcome);
    }
    if let Some(error) =
        create_planned_journal_reconciled(context.journals, &operation_id, &context).await?
    {
        finish_artifact_repair(
            &context,
            operation_id.clone(),
            ArtifactTerminal::Failed {
                step_id: "journal_repair_start",
                rollback: RollbackState::NotApplicable,
                facts: Vec::new(),
                summary: "guardian_artifact_repair_initialization_failed",
                quarantined_target: None,
            },
        )
        .await?;
        return Err(error);
    }

    if let Some(admission) = context.registered_admission {
        if !admission.evidence_is_current() {
            return finish_artifact_repair(
                &context,
                operation_id,
                ArtifactTerminal::Failed {
                    step_id: "revalidate_registered_artifact_authority",
                    rollback: RollbackState::NotApplicable,
                    facts: Vec::new(),
                    summary: "guardian_artifact_repair_authority_changed",
                    quarantined_target: None,
                },
            )
            .await;
        }
        let state = admission.physical_state().await;
        if state
            == Some(crate::execution::registered_artifact::RegisteredArtifactPhysicalState::Exact)
        {
            let (_, expected_sha1, expected_size) = admission.download_contract();
            if admission
                .mutation()
                .verify_exact(expected_sha1, expected_size)
                .await
                && admission.evidence_is_current()
            {
                return finish_artifact_repair(
                    &context,
                    operation_id,
                    ArtifactTerminal::Repaired {
                        step_id: "registered_artifact_already_exact",
                        facts: Vec::new(),
                        quarantined_target: None,
                    },
                )
                .await;
            }
        }
        let expected = match admission.effect() {
            RegisteredArtifactRepairEffect::DownloadMissing => {
                crate::execution::registered_artifact::RegisteredArtifactPhysicalState::Missing
            }
            RegisteredArtifactRepairEffect::QuarantineRedownload => {
                crate::execution::registered_artifact::RegisteredArtifactPhysicalState::Corrupt
            }
            RegisteredArtifactRepairEffect::AlreadyExact => {
                return finish_artifact_repair(
                    &context,
                    operation_id,
                    ArtifactTerminal::Failed {
                        step_id: "revalidate_registered_artifact_condition",
                        rollback: RollbackState::NotApplicable,
                        facts: Vec::new(),
                        summary: "guardian_artifact_repair_condition_changed",
                        quarantined_target: None,
                    },
                )
                .await;
            }
        };
        if state != Some(expected) {
            return finish_artifact_repair(
                &context,
                operation_id,
                ArtifactTerminal::Failed {
                    step_id: "revalidate_registered_artifact_condition",
                    rollback: RollbackState::NotApplicable,
                    facts: Vec::new(),
                    summary: "guardian_artifact_repair_condition_changed",
                    quarantined_target: None,
                },
            )
            .await;
        }
    }

    let quarantined_target = if context.quarantines_existing {
        let quarantine_facts = if let Some(admission) = context.registered_admission {
            match admission
                .mutation()
                .quarantine_existing(&operation_id, &target)
            {
                Ok(report) => fact_ids(&report.facts),
                Err(error) => {
                    return finish_artifact_repair(
                        &context,
                        operation_id,
                        ArtifactTerminal::Failed {
                            step_id: "quarantine_launcher_managed_target",
                            rollback: RollbackState::Unavailable,
                            facts: fact_ids(&error.facts),
                            summary: "guardian_artifact_quarantine_failed",
                            quarantined_target: None,
                        },
                    )
                    .await;
                }
            }
        } else {
            let descriptor = context
                .descriptor
                .as_ref()
                .expect("install repair has descriptor");
            match quarantine_launcher_managed_file(QuarantineFileRequest {
                operation_id: Some(operation_id.clone()),
                target: target.clone(),
                source: descriptor.destination(),
            }) {
                Ok(report) => fact_ids(&report.facts),
                Err(error) => {
                    return finish_artifact_repair(
                        &context,
                        operation_id,
                        ArtifactTerminal::Failed {
                            step_id: "quarantine_launcher_managed_target",
                            rollback: RollbackState::Unavailable,
                            facts: fact_ids(&error.facts),
                            summary: "guardian_artifact_quarantine_failed",
                            quarantined_target: None,
                        },
                    )
                    .await;
                }
            }
        };
        let quarantined_target = TargetDescriptor::new(
            StabilizationSystem::Execution,
            target.kind,
            format!("quarantine-{}", target.id),
            target.ownership,
        );
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
                        finish_artifact_repair(
                            &context,
                            operation_id.clone(),
                            ArtifactTerminal::Failed {
                                step_id: "record_quarantine_checkpoint",
                                rollback: RollbackState::Available,
                                facts: Vec::new(),
                                summary: "guardian_artifact_repair_checkpoint_failed",
                                quarantined_target: Some(quarantined_target),
                            },
                        )
                        .await?;
                        return Err(error);
                    }
                    ArtifactJournalReconciliation::RetryMutation => {}
                },
            }
        }
        Some(quarantined_target)
    } else {
        None
    };

    let download_result = if let Some(admission) = context.registered_admission {
        if !admission.evidence_is_current() {
            Err(Vec::new())
        } else {
            let (provider_url, expected_sha1, expected_size) = admission.download_contract();
            admission
                .mutation()
                .download_verify_promote(
                    &operation_id,
                    &target,
                    provider_url,
                    expected_sha1,
                    expected_size,
                    context.client,
                )
                .await
                .map(|report| report.facts)
                .map_err(|error| error.facts)
        }
    } else {
        let descriptor = context
            .descriptor
            .as_ref()
            .expect("install repair has descriptor");
        let source = descriptor.repair_source();
        let checksum = source_download_checksum(&source).ok_or_else(|| {
            OperationJournalStoreError::Persistence(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Guardian admitted artifact repair checksum is invalid",
            ))
        })?;
        let mut request =
            DownloadToTempRequest::new(target.clone(), descriptor.destination(), source.url)
                .with_expected_checksum(checksum);
        if let Some(max_bytes) = source.max_bytes {
            request = request.with_max_bytes(max_bytes);
        }
        if let Some(expected_size) = source.expected_size {
            request = request.with_expected_size(expected_size);
        }
        request.operation_id = Some(operation_id.clone());
        download_url_to_temp(request, context.client)
            .await
            .map(|report| report.facts)
            .map_err(|error| error.facts)
    };

    match download_result {
        Ok(facts) => {
            let fact_ids = fact_ids(&facts);
            if let Some(admission) = context.registered_admission {
                let (_, expected_sha1, expected_size) = admission.download_contract();
                if !admission
                    .mutation()
                    .verify_exact(expected_sha1, expected_size)
                    .await
                    || !admission.evidence_is_current()
                {
                    return finish_artifact_repair(
                        &context,
                        operation_id,
                        ArtifactTerminal::Failed {
                            step_id: "verify_registered_artifact_postcondition",
                            rollback: if context.quarantines_existing {
                                RollbackState::Available
                            } else {
                                RollbackState::Unavailable
                            },
                            facts: fact_ids,
                            summary: "guardian_artifact_repair_postcondition_failed",
                            quarantined_target,
                        },
                    )
                    .await;
                }
            }
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
        Err(facts) => {
            let fact_ids = fact_ids(&facts);
            if context
                .registered_admission
                .is_some_and(|admission| !admission.evidence_is_current())
            {
                return finish_artifact_repair(
                    &context,
                    operation_id,
                    ArtifactTerminal::Failed {
                        step_id: "revalidate_registered_artifact_authority",
                        rollback: if context.quarantines_existing {
                            RollbackState::Available
                        } else {
                            RollbackState::Unavailable
                        },
                        facts: fact_ids,
                        summary: "guardian_artifact_repair_authority_changed",
                        quarantined_target,
                    },
                )
                .await;
            }
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
                    quarantined_target,
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
    let (journal, reconciliation_outcome, status, facts, summary, suppression_until, quarantined) =
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
            ArtifactTerminal::Repaired {
                step_id,
                facts,
                quarantined_target,
            } => (
                ArtifactTerminalJournal::Record(step_id, RollbackState::Available, None),
                Some(ReconciliationTerminalOutcome::Succeeded),
                GuardianArtifactRepairStatus::Repaired,
                facts,
                "guardian_artifact_repaired",
                Some(
                    context
                        .attempt
                        .as_ref()
                        .expect("attempted repair has typed attempt")
                        .suppression_until()
                        .to_string(),
                ),
                quarantined_target,
            ),
            ArtifactTerminal::Failed {
                step_id,
                rollback,
                facts,
                summary,
                quarantined_target,
            } => (
                ArtifactTerminalJournal::Record(step_id, rollback, Some(step_id)),
                Some(ReconciliationTerminalOutcome::Failed),
                GuardianArtifactRepairStatus::Failed,
                facts,
                summary,
                Some(
                    context
                        .attempt
                        .as_ref()
                        .expect("attempted repair has typed attempt")
                        .suppression_until()
                        .to_string(),
                ),
                quarantined_target,
            ),
        };
    let reconciliation_terminal = match reconciliation_outcome {
        Some(outcome) => {
            let _suppression_until = suppression_until.as_deref().ok_or_else(|| {
                OperationJournalStoreError::Persistence(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "Guardian artifact repair has no valid suppression window",
                ))
            })?;
            let attempt = context
                .attempt
                .as_ref()
                .expect("attempted repair has typed attempt")
                .clone();
            Some(if let Some(admission) = context.registered_admission {
                admission
                    .terminal(attempt, outcome, quarantined.clone())
                    .map_err(artifact_reconciliation_error)?
            } else {
                reconciliation_terminal(attempt, outcome, quarantined.clone())
            })
        }
        None => None,
    };
    let journal_operation_id = operation_id.clone();
    let journal_facts = facts.clone();
    let journal_terminal = reconciliation_terminal.clone();
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
                    journal_terminal
                        .as_ref()
                        .expect("attempted repair has typed terminal"),
                )
                .await
            }
        }
    };
    if let Some(error) = complete_journal.await? {
        return Err(error);
    }
    if let Some(terminal) = reconciliation_terminal {
        let memory = reconciliation_memory_entry(terminal).map_err(|_| {
            OperationJournalStoreError::Persistence(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Guardian artifact repair memory terminal is invalid",
            ))
        })?;
        commit_reconciliation_memory(
            context.failure_memory,
            memory,
            context
                .reservation
                .as_ref()
                .expect("attempted repair owns memory reservation"),
        )
        .await
        .map_err(|error| {
            OperationJournalStoreError::Persistence(std::io::Error::other(format!(
                "Guardian artifact repair memory commit failed: {}",
                error.class()
            )))
        })?;
    }
    Ok(artifact_repair_outcome(
        operation_id,
        context.authorization.diagnosis_id,
        context.authorization.action,
        status,
        facts,
        summary,
    ))
}

async fn recover_artifact_evidence(
    context: &ArtifactRepairContext<'_>,
    key: &crate::state::failure_memory::FailureMemoryKey,
) -> Result<Option<GuardianArtifactRepairOutcome>, OperationJournalStoreError> {
    let now = chrono::Utc::now();
    let operation_id = context
        .attempt
        .as_ref()
        .expect("artifact recovery has typed attempt")
        .operation_id();
    let mut active_terminal_candidate = None;
    for journal in context.journals.list() {
        let Some(attempt) = journal.reconciliation_attempt() else {
            continue;
        };
        if &reconciliation_attempt_key(attempt) != key
            || attempt.diagnosis_id() != context.authorization.diagnosis_id
        {
            continue;
        }
        if let Some(terminal) = journal.reconciliation_terminal().cloned() {
            if &journal.operation_id == operation_id {
                return reconcile_same_operation_artifact_terminal(context, journal, terminal)
                    .await
                    .map(Some);
            }
            let active = DateTime::parse_from_rfc3339(terminal.suppression_until())
                .is_ok_and(|until| until > now);
            if active
                && active_terminal_candidate.as_ref().is_none_or(
                    |current: &ReconciliationTerminal| {
                        current.observed_at() < terminal.observed_at()
                    },
                )
            {
                active_terminal_candidate = Some(terminal);
            }
        }
    }
    let Some(terminal) = active_terminal_candidate else {
        return Ok(None);
    };
    reconcile_artifact_terminal_memory(context, terminal).await?;
    finish_artifact_repair(
        context,
        operation_id.clone(),
        ArtifactTerminal::Blocked("guardian_artifact_repair_blocked_by_active_terminal"),
    )
    .await
    .map(Some)
}

async fn reconcile_same_operation_artifact_terminal(
    context: &ArtifactRepairContext<'_>,
    journal: OperationJournalEntry,
    terminal: ReconciliationTerminal,
) -> Result<GuardianArtifactRepairOutcome, OperationJournalStoreError> {
    reconcile_artifact_terminal_memory(context, terminal.clone()).await?;
    let (status, summary) = match terminal.outcome() {
        ReconciliationTerminalOutcome::Succeeded => (
            GuardianArtifactRepairStatus::Repaired,
            "guardian_artifact_repaired",
        ),
        ReconciliationTerminalOutcome::Failed => (
            GuardianArtifactRepairStatus::Failed,
            match journal.failure_point.as_deref() {
                Some("journal_repair_start") => "guardian_artifact_repair_initialization_failed",
                Some("quarantine_launcher_managed_target") => "guardian_artifact_quarantine_failed",
                Some("record_quarantine_checkpoint") => {
                    "guardian_artifact_repair_checkpoint_failed"
                }
                _ => "guardian_artifact_redownload_failed",
            },
        ),
    };
    let facts = journal
        .completed_steps
        .last()
        .map(|step| step.generated_facts.clone())
        .unwrap_or_default();
    Ok(artifact_repair_outcome(
        journal.operation_id,
        context.authorization.diagnosis_id,
        context.authorization.action,
        status,
        facts,
        summary,
    ))
}

async fn reconcile_artifact_terminal_memory(
    context: &ArtifactRepairContext<'_>,
    terminal: ReconciliationTerminal,
) -> Result<(), OperationJournalStoreError> {
    let memory = reconciliation_memory_entry(terminal.clone()).map_err(|_| {
        OperationJournalStoreError::Persistence(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "Guardian artifact repair journal terminal cannot reconcile memory",
        ))
    })?;
    commit_reconciliation_memory(
        context.failure_memory,
        memory,
        context
            .reservation
            .as_ref()
            .expect("artifact replay owns memory reservation"),
    )
    .await
    .map_err(|error| {
        OperationJournalStoreError::Persistence(std::io::Error::other(format!(
            "Guardian artifact repair memory reconciliation failed: {}",
            error.class()
        )))
    })?;
    Ok(())
}

fn validate_artifact_repair_input(
    descriptor: &GuardianMinecraftArtifactRepairDescriptor,
    quarantines_existing: bool,
) -> Result<DownloadChecksum<'_>, &'static str> {
    if !quarantines_existing {
        match descriptor.destination().try_exists() {
            Ok(false) => {}
            Ok(true) => return Err("guardian_missing_artifact_repair_blocked_target_exists"),
            Err(_) => return Err("guardian_missing_artifact_repair_blocked_target_unreadable"),
        }
    }
    let source = descriptor.repair_source();
    source_download_checksum(&source).ok_or("guardian_artifact_repair_blocked_invalid_checksum")
}

fn artifact_memory_error(error: impl std::fmt::Display) -> OperationJournalStoreError {
    OperationJournalStoreError::Persistence(std::io::Error::other(format!(
        "Guardian artifact reconciliation memory failed: {error}"
    )))
}

fn artifact_reconciliation_error(_error: impl std::fmt::Debug) -> OperationJournalStoreError {
    OperationJournalStoreError::Persistence(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        "Guardian artifact reconciliation evidence is invalid",
    ))
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
    append_artifact_journal_targets(&mut entry, context);
    entry.planned_steps = artifact_repair_steps(context)
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
    reconciliation_journal_attempt(
        entry,
        context
            .attempt
            .as_ref()
            .expect("planned repair has typed attempt")
            .clone(),
    )
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
    terminal: &ReconciliationTerminal,
) -> Result<Option<OperationJournalStoreError>, OperationJournalStoreError> {
    loop {
        let result = if let Some(failure_point) = failure_point {
            record_reconciliation_journal_failure(
                journals,
                operation_id,
                step.clone(),
                failure_point,
                terminal.clone(),
            )
            .await
        } else {
            record_reconciliation_journal_success(
                journals,
                operation_id,
                step.clone(),
                terminal.clone(),
            )
            .await
        };
        match result {
            Ok(()) => return Ok(None),
            Err(OperationJournalStoreError::AlreadyTerminal)
                if journals.get(operation_id).is_some_and(|entry| {
                    artifact_terminal_transition_matches(
                        &entry,
                        operation_id,
                        failure_point,
                        &step,
                        terminal,
                    )
                }) =>
            {
                return Ok(None);
            }
            Err(error) => {
                match reconcile_artifact_journal_error(journals, operation_id, error, |entry| {
                    artifact_terminal_transition_matches(
                        entry,
                        operation_id,
                        failure_point,
                        &step,
                        terminal,
                    )
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
    append_artifact_journal_targets(&mut entry, context);
    entry.planned_steps = artifact_repair_steps(context)
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

fn append_artifact_journal_targets(
    entry: &mut OperationJournalEntry,
    context: &ArtifactRepairContext<'_>,
) {
    entry.targets.push(context.authorization.target.clone());
    if let Some(ReconciliationScope::RegisteredInstance { instance_id, .. }) =
        context.attempt.as_ref().map(ReconciliationAttempt::scope)
    {
        entry
            .targets
            .push(reconciliation_instance_target(instance_id));
    }
}

fn artifact_terminal_transition_matches(
    entry: &OperationJournalEntry,
    operation_id: &OperationId,
    failure_point: Option<&str>,
    step: &OperationJournalStep,
    terminal: &ReconciliationTerminal,
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
        && entry.reconciliation_terminal() == Some(terminal)
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

fn artifact_repair_steps(
    context: &ArtifactRepairContext<'_>,
) -> &'static [(&'static str, RollbackState)] {
    const QUARANTINE_REDOWNLOAD: [(&str, RollbackState); 7] = [
        ("journal_repair_start", RollbackState::NotApplicable),
        (
            "quarantine_launcher_managed_target",
            RollbackState::Available,
        ),
        ("record_quarantine_checkpoint", RollbackState::Available),
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
    const REGISTERED_QUARANTINE_REDOWNLOAD: [(&str, RollbackState); 8] = [
        ("journal_repair_start", RollbackState::NotApplicable),
        (
            "registered_artifact_already_exact",
            RollbackState::NotApplicable,
        ),
        (
            "quarantine_launcher_managed_target",
            RollbackState::Available,
        ),
        ("record_quarantine_checkpoint", RollbackState::Available),
        ("download_artifact_to_temp", RollbackState::Available),
        ("verify_artifact_checksum", RollbackState::NotApplicable),
        ("promote_verified_artifact", RollbackState::Available),
        ("record_repair_outcome", RollbackState::NotApplicable),
    ];
    const REGISTERED_MISSING_DOWNLOAD: [(&str, RollbackState); 6] = [
        ("journal_repair_start", RollbackState::NotApplicable),
        (
            "registered_artifact_already_exact",
            RollbackState::NotApplicable,
        ),
        ("download_artifact_to_temp", RollbackState::Available),
        ("verify_artifact_checksum", RollbackState::NotApplicable),
        ("promote_verified_artifact", RollbackState::Available),
        ("record_repair_outcome", RollbackState::NotApplicable),
    ];
    if context.registered_admission.is_some() && context.quarantines_existing {
        &REGISTERED_QUARANTINE_REDOWNLOAD
    } else if context.registered_admission.is_some() {
        &REGISTERED_MISSING_DOWNLOAD
    } else if context.quarantines_existing {
        &QUARANTINE_REDOWNLOAD
    } else {
        &MISSING_DOWNLOAD
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
        GuardianMinecraftArtifactRepairDescriptor, GuardianMode, RepairAuthorizationRejection,
        authorize_launcher_managed_artifact_repair,
        authorize_launcher_managed_missing_artifact_repair,
    };
    use crate::state::OperationJournalStore;
    use crate::state::contracts::{
        OperationId, OperationOutcome, OperationStatus, OperationStepResult, OwnershipClass,
        ReconciliationTerminalOutcome, RollbackState, StabilizationSystem, TargetDescriptor,
        TargetKind,
    };
    use crate::state::failure_memory::{
        FailureMemoryActionOutcome, FailureMemorySnapshot, GuardianFailureMemoryStore,
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
    async fn active_success_terminal_blocks_a_different_operation_without_effects() {
        let root = test_root("active-success-blocks-new-operation");
        fs::create_dir_all(&root).expect("root");
        let destination = root.join("bad.jar");
        fs::write(&destination, b"initial corruption").expect("corrupt artifact");
        let replacement = b"fresh artifact".to_vec();
        let server = TestByteServer::start(replacement.clone());
        let stores = stores();
        let observed_at = chrono::Utc::now().to_rfc3339();

        let first = execute_quarantine_with_operation_id(
            quarantine_input(
                &destination,
                &server.url,
                &sha256_hex(&replacement),
                Some(replacement.len() as u64),
                &stores,
                &observed_at,
            ),
            "guardian-artifact-repair-first",
        )
        .await
        .expect("first repair");
        assert_eq!(first.status, GuardianArtifactRepairStatus::Repaired);

        fs::write(&destination, b"renewed corruption").expect("renew corruption");
        let journals_before = stores.journals.list();
        let prior_journal = stores
            .journals
            .get(&first.operation_id)
            .expect("prior successful terminal");
        let memory_before = stores.failure_memory.list();
        stores
            .failure_memory
            .load_snapshot(FailureMemorySnapshot::new(Vec::new()).expect("empty memory snapshot"))
            .expect("simulate missing terminal memory");
        assert!(stores.failure_memory.list().is_empty());
        let quarantines_before = quarantine_count(&root);

        let second = execute_quarantine_with_operation_id(
            quarantine_input(
                &destination,
                &server.url,
                &sha256_hex(&replacement),
                Some(replacement.len() as u64),
                &stores,
                &observed_at,
            ),
            "guardian-artifact-repair-second",
        )
        .await
        .expect("active terminal refusal");

        assert_eq!(second.status, GuardianArtifactRepairStatus::Blocked);
        assert_eq!(
            second.operation_id,
            OperationId::new("guardian-artifact-repair-second")
        );
        assert_eq!(
            second.summary,
            "guardian_artifact_repair_blocked_by_active_terminal"
        );
        assert_eq!(
            fs::read(&destination).expect("renewed corruption remains"),
            b"renewed corruption"
        );
        assert_eq!(server.request_count(), 1);
        assert_eq!(quarantine_count(&root), quarantines_before);
        assert_eq!(stores.journals.list().len(), journals_before.len() + 1);
        assert_eq!(
            stores
                .journals
                .get(&first.operation_id)
                .expect("prior successful terminal remains"),
            prior_journal
        );
        let blocked_journal = stores
            .journals
            .get(&second.operation_id)
            .expect("current blocked journal");
        assert_eq!(blocked_journal.status, OperationStatus::Blocked);
        assert_eq!(blocked_journal.outcome, Some(OperationOutcome::Blocked));
        assert!(blocked_journal.reconciliation_terminal().is_none());
        assert_eq!(stores.failure_memory.list(), memory_before);

        server.stop();
        cleanup(&root);
    }

    #[tokio::test]
    async fn expired_success_terminal_still_replays_the_same_operation() {
        let root = test_root("expired-success-replays-same-operation");
        fs::create_dir_all(&root).expect("root");
        let destination = root.join("bad.jar");
        fs::write(&destination, b"initial corruption").expect("corrupt artifact");
        let replacement = b"fresh artifact".to_vec();
        let server = TestByteServer::start(replacement.clone());
        let stores = stores();
        let observed_at = (chrono::Utc::now() - chrono::Duration::hours(2)).to_rfc3339();

        let first = execute_quarantine_with_operation_id(
            quarantine_input(
                &destination,
                &server.url,
                &sha256_hex(&replacement),
                Some(replacement.len() as u64),
                &stores,
                &observed_at,
            ),
            "guardian-artifact-repair-replay",
        )
        .await
        .expect("first repair");
        fs::write(&destination, b"renewed corruption").expect("renew corruption");
        let journals_before = stores.journals.list();
        let memory_before = stores.failure_memory.list();
        let quarantines_before = quarantine_count(&root);

        let replay = execute_quarantine_with_operation_id(
            quarantine_input(
                &destination,
                &server.url,
                &sha256_hex(&replacement),
                Some(replacement.len() as u64),
                &stores,
                &observed_at,
            ),
            "guardian-artifact-repair-replay",
        )
        .await
        .expect("same operation replay");

        assert_eq!(replay, first);
        assert_eq!(
            fs::read(&destination).expect("renewed corruption remains"),
            b"renewed corruption"
        );
        assert_eq!(server.request_count(), 1);
        assert_eq!(quarantine_count(&root), quarantines_before);
        assert_eq!(stores.journals.list(), journals_before);
        assert_eq!(stores.failure_memory.list(), memory_before);

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
        assert_eq!(
            journal
                .reconciliation_terminal()
                .expect("typed initialization terminal")
                .outcome(),
            ReconciliationTerminalOutcome::Failed
        );
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
        assert_eq!(
            journal
                .reconciliation_terminal()
                .expect("typed checkpoint terminal")
                .quarantined_target(),
            stores.failure_memory.list()[0].quarantined_target.as_ref()
        );
        assert!(
            journal
                .reconciliation_terminal()
                .expect("typed checkpoint terminal")
                .quarantined_target()
                .is_some()
        );
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
    async fn active_failure_terminal_replays_same_operation_and_blocks_a_different_one() {
        let root = test_root("active-failure-recovery");
        fs::create_dir_all(&root).expect("root");
        let destination = root.join("bad.jar");
        fs::write(&destination, b"initial corruption").expect("corrupt artifact");
        let replacement = b"fresh artifact".to_vec();
        let server = TestByteServer::start(replacement);
        let stores = stores();
        let observed_at = chrono::Utc::now().to_rfc3339();

        let first = execute_quarantine_with_operation_id(
            quarantine_input(
                &destination,
                &server.url,
                &sha256_hex(b"different artifact"),
                None,
                &stores,
                &observed_at,
            ),
            "guardian-artifact-repair-failed",
        )
        .await
        .expect("failed repair terminal");
        assert_eq!(first.status, GuardianArtifactRepairStatus::Failed);

        fs::write(&destination, b"renewed corruption").expect("renew corruption");
        let journals_before = stores.journals.list();
        let prior_journal = stores
            .journals
            .get(&first.operation_id)
            .expect("prior failed terminal");
        let memory_before = stores.failure_memory.list();
        let quarantines_before = quarantine_count(&root);

        let replay = execute_quarantine_with_operation_id(
            quarantine_input(
                &destination,
                &server.url,
                &sha256_hex(b"different artifact"),
                None,
                &stores,
                &observed_at,
            ),
            "guardian-artifact-repair-failed",
        )
        .await
        .expect("same failed operation replay");
        let blocked = execute_quarantine_with_operation_id(
            quarantine_input(
                &destination,
                &server.url,
                &sha256_hex(b"different artifact"),
                None,
                &stores,
                &observed_at,
            ),
            "guardian-artifact-repair-after-failure",
        )
        .await
        .expect("active failed terminal refusal");

        assert_eq!(replay, first);
        assert_eq!(blocked.status, GuardianArtifactRepairStatus::Blocked);
        assert_eq!(
            blocked.operation_id,
            OperationId::new("guardian-artifact-repair-after-failure")
        );
        assert_eq!(
            fs::read(&destination).expect("renewed corruption remains"),
            b"renewed corruption"
        );
        assert_eq!(server.request_count(), 1);
        assert_eq!(quarantine_count(&root), quarantines_before);
        assert_eq!(stores.journals.list().len(), journals_before.len() + 1);
        assert_eq!(
            stores
                .journals
                .get(&first.operation_id)
                .expect("prior failed terminal remains"),
            prior_journal
        );
        let blocked_journal = stores
            .journals
            .get(&blocked.operation_id)
            .expect("current blocked journal");
        assert_eq!(blocked_journal.status, OperationStatus::Blocked);
        assert_eq!(blocked_journal.outcome, Some(OperationOutcome::Blocked));
        assert!(blocked_journal.reconciliation_terminal().is_none());
        assert_eq!(stores.failure_memory.list(), memory_before);

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
        execute_quarantine_with_operation_id(input, "guardian-artifact-repair-test").await
    }

    async fn execute_quarantine_with_operation_id(
        input: ArtifactRepairTestInput<'_>,
        operation_id: &str,
    ) -> Result<GuardianArtifactRepairOutcome, crate::state::OperationJournalStoreError> {
        let ArtifactRepairTestInput {
            descriptor,
            stores,
            observed_at,
        } = input;
        let authorization =
            authorize_launcher_managed_artifact_repair(&artifact_repair_decision(), descriptor)
                .expect("quarantine-redownload authorization");
        super::execute_guardian_quarantine_redownload(
            authorization,
            Some(OperationId::new(operation_id)),
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

    fn quarantine_count(root: &std::path::Path) -> usize {
        fs::read_dir(root)
            .expect("read root")
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains(".quarantine-"))
            .count()
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
