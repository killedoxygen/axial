use super::{
    BASE_INSTALL_FAILED_MESSAGE, DurableInstallPublication, InstallApplicationError,
    InstallForegroundActivity, InstallProgressCommand, InstallProgressSender,
    InstallProgressViewModel, InstallQueueStartFailure, InstallRequestDrain, InstallStartResponse,
    LOADER_INSTALL_INTERRUPTED_MESSAGE, LoaderInstallStartRequest, ManagedPublicationConvergence,
    RecoveringInstallAdmission, await_managed_install_settlement_retaining,
    begin_install_journal_with_owned_reconciliation, emit_install_failed,
    finish_install_progress_task, generate_install_id, install_journal_error_response,
    mint_available_install_operation_id, operation::InstallJournalIdentity,
    operation::InstallPublicationCheckpointKind, operation::install_progress_with_terminal_error,
    operation::publication_indeterminate_install_progress, own_install_progress,
    publish_install_progress, publish_install_progress_durably,
    reconcile_install_operation_terminal, reconcile_install_worker_interruption,
    record_install_failure_outcome, record_install_failure_outcome_for_error,
    record_loader_base_install_dependency_guardian_failure_outcome,
    record_loader_install_operation_guardian_failure_outcome, register_install_foreground,
    retain_install_foreground, sanitize_install_progress, settle_managed_install_publication,
    spawn_install_foreground_retention,
};
use crate::state::{
    AppState, InstallAdmissionMarker, InstallInitializationStatus, InstallProgressRecord,
    InstallQueueAdmission, InstallQueueStartAuthority, InstallSnapshot, InstallStore,
    IntegrityForegroundLease, ProducerLease,
};
use axial_minecraft::loaders::{
    LoaderActiveInstallFailure, LoaderInstallBaseContinuation, LoaderInstallPublicationOutcome,
    LoaderInstallPublicationRecovery,
};
use axial_minecraft::{
    DownloadProgress, LoaderError, LoaderInstallError, LoaderInstallFailureKind,
    LoaderPreOperationFailureKind, LoaderProviderFailureKind, ManagedInstallCommittedEvidence,
    ManagedInstallDurableOutcome, ManagedInstallPublicationCandidates,
    ManagedInstallRolledBackEvidence, classify_managed_install_publication,
    classify_managed_install_publication_candidates, continue_install_build_after_base,
    install_build, resolve_build_record_for_install, resume_install_build_after_base,
    verify_managed_install_loader_base_checkpoint,
    verify_managed_install_publication_evidence_root,
    verify_managed_install_reconstruction_checkpoint,
};
use axum::{Json, http::StatusCode};
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

enum DurableLoaderBasePublication {
    Activated(LoaderInstallBaseContinuation),
    RolledBack(axial_minecraft::DownloadError),
    Refused(axial_minecraft::DownloadError),
    DeferredNonterminal,
}

async fn settle_loader_base_publication(
    managed_root: axial_minecraft::managed_path::ManagedLibraryOperation,
    expected_version_id: &str,
    journals: &crate::state::OperationJournalStore,
    operation_id: &crate::state::contracts::OperationId,
    progress_tx: &InstallProgressSender,
    request_drain: &mut InstallRequestDrain,
    state: &AppState,
    foreground: &IntegrityForegroundLease,
    library_operation: &crate::state::LibraryOperation,
    commit: axial_minecraft::LoaderInstallBaseCommit,
) -> DurableLoaderBasePublication {
    if !publish_install_progress_durably(progress_tx, publication_indeterminate_install_progress())
        .await
    {
        return DurableLoaderBasePublication::DeferredNonterminal;
    }
    let outcome =
        classify_managed_install_publication(managed_root, expected_version_id.to_string()).await;
    let Some(outcome) =
        super::converge_managed_install_durable_outcome(outcome, request_drain).await
    else {
        return DurableLoaderBasePublication::DeferredNonterminal;
    };
    match outcome {
        ManagedInstallDurableOutcome::Mismatch => DurableLoaderBasePublication::Refused(
            super::managed_install_publication_mismatch_error(),
        ),
        ManagedInstallDurableOutcome::NoEffect => DurableLoaderBasePublication::DeferredNonterminal,
        ManagedInstallDurableOutcome::Committed(evidence)
            if evidence.id().matches_version_id(expected_version_id)
                && commit.base_version_id() == expected_version_id =>
        {
            let evidence_id = evidence.id().clone();
            let verified = match evidence.verify_loader_base_commit(commit) {
                Ok(verified) => verified,
                Err(_) => return DurableLoaderBasePublication::DeferredNonterminal,
            };
            let checkpoint = super::operation::InstallPublicationCheckpoint {
                kind: InstallPublicationCheckpointKind::BaseCommitted,
                version_id: expected_version_id.to_string(),
                evidence: evidence_id,
                activation_contract_id: Some(verified.activation_contract_id().clone()),
            };
            if super::operation::record_install_publication_checkpoint(
                journals,
                operation_id,
                &checkpoint,
            )
            .await
            .is_err()
            {
                return DurableLoaderBasePublication::DeferredNonterminal;
            }
            let (continuation, acknowledgement) = match state
                .accept_verified_loader_base_commit(foreground, library_operation, verified)
                .await
            {
                Ok(activated) => activated,
                Err(_) => return DurableLoaderBasePublication::DeferredNonterminal,
            };
            if !super::converge_managed_install_acknowledgement(
                acknowledgement.acknowledge().await,
                request_drain,
            )
            .await
            {
                return DurableLoaderBasePublication::DeferredNonterminal;
            }
            DurableLoaderBasePublication::Activated(continuation)
        }
        ManagedInstallDurableOutcome::RolledBack {
            evidence, effect, ..
        } if evidence.id().matches_version_id(expected_version_id) => {
            tracing::warn!(
                operation_id = %operation_id,
                expected_version_id,
                rollback_effect = ?effect,
                "managed loader base publication rolled back durably"
            );
            let checkpoint = super::operation::InstallPublicationCheckpoint {
                kind: InstallPublicationCheckpointKind::RolledBack,
                version_id: expected_version_id.to_string(),
                evidence: evidence.id().clone(),
                activation_contract_id: None,
            };
            if super::operation::record_install_publication_checkpoint(
                journals,
                operation_id,
                &checkpoint,
            )
            .await
            .is_err()
                || !super::converge_managed_install_acknowledgement(
                    evidence.acknowledge().await,
                    request_drain,
                )
                .await
            {
                return DurableLoaderBasePublication::DeferredNonterminal;
            }
            DurableLoaderBasePublication::RolledBack(axial_minecraft::DownloadError::FileOperation(
                std::io::Error::other("managed loader base publication was rolled back"),
            ))
        }
        ManagedInstallDurableOutcome::Committed(_)
        | ManagedInstallDurableOutcome::RolledBack { .. }
        | ManagedInstallDurableOutcome::Indeterminate(_) => {
            DurableLoaderBasePublication::DeferredNonterminal
        }
    }
}

async fn converge_loader_install_publication(
    mut recovery: LoaderInstallPublicationRecovery,
    request_drain: &mut InstallRequestDrain,
    defer_on_request_drain: bool,
) -> ManagedPublicationConvergence<LoaderInstallPublicationOutcome, LoaderInstallError> {
    let mut retry_delay = Duration::from_millis(100);
    let maximum_retry_delay = Duration::from_secs(5);
    if defer_on_request_drain
        && !super::wait_for_managed_publication_retry(request_drain, Duration::ZERO).await
    {
        return ManagedPublicationConvergence::DeferredNonterminal;
    }
    loop {
        match recovery.retry().await {
            Err(LoaderInstallError::PublicationIndeterminate(next)) => {
                recovery = next;
                if defer_on_request_drain {
                    if !super::wait_for_managed_publication_retry(request_drain, retry_delay).await
                    {
                        return ManagedPublicationConvergence::DeferredNonterminal;
                    }
                } else {
                    tokio::time::sleep(retry_delay).await;
                }
                retry_delay = retry_delay.saturating_mul(2).min(maximum_retry_delay);
            }
            settled => return ManagedPublicationConvergence::Settled(settled),
        }
    }
}

enum RecoveringLoaderPublication {
    Fresh,
    Base {
        evidence: Option<ManagedInstallCommittedEvidence>,
        checkpoint: super::operation::InstallPublicationCheckpoint,
        checkpoint_recorded: bool,
    },
    Child {
        evidence: Option<ManagedInstallCommittedEvidence>,
        checkpoint: super::operation::InstallPublicationCheckpoint,
        checkpoint_recorded: bool,
    },
    RolledBack(Option<ManagedInstallRolledBackEvidence>),
    Refused(LoaderInstallError),
    DeferredNonterminal,
}

pub(super) async fn resolve_after_releasing_loader_recovery_authority<
    Mutation,
    Library,
    Resolution,
>(
    mutation: Mutation,
    library: Library,
    resolution: Resolution,
) -> Resolution::Output
where
    Resolution: Future,
{
    drop(mutation);
    drop(library);
    resolution.await
}

async fn classify_loader_candidates(
    managed_root: axial_minecraft::managed_path::ManagedLibraryOperation,
    candidates: ManagedInstallPublicationCandidates,
    request_drain: &mut InstallRequestDrain,
    convergence: super::RecoveringInstallConvergence,
) -> Option<ManagedInstallDurableOutcome> {
    match convergence {
        super::RecoveringInstallConvergence::StartupBounded => {
            tokio::time::timeout(super::STARTUP_PUBLICATION_SETTLEMENT_TIMEOUT, async {
                let outcome =
                    classify_managed_install_publication_candidates(managed_root, candidates).await;
                super::converge_startup_managed_install_durable_outcome(outcome, request_drain)
                    .await
            })
            .await
            .ok()
            .flatten()
        }
        super::RecoveringInstallConvergence::SameProcess => {
            let outcome =
                classify_managed_install_publication_candidates(managed_root, candidates).await;
            super::converge_managed_install_durable_outcome(outcome, request_drain).await
        }
    }
}

async fn classify_recovering_loader_publication(
    managed_root: axial_minecraft::managed_path::ManagedLibraryOperation,
    journal: &super::operation::RecoveringInstallJournal,
    base_version_id: &str,
    request_drain: &mut InstallRequestDrain,
    journals: &crate::state::OperationJournalStore,
    convergence: super::RecoveringInstallConvergence,
) -> RecoveringLoaderPublication {
    let target_version_id = journal.identity.target_version_id();
    let checkpoint = journal.checkpoints.last();
    let candidates = match checkpoint {
        Some(checkpoint) if checkpoint.kind == InstallPublicationCheckpointKind::BaseCommitted => {
            ManagedInstallPublicationCandidates::pair(base_version_id, target_version_id)
        }
        Some(checkpoint) => ManagedInstallPublicationCandidates::one(&checkpoint.version_id),
        None => ManagedInstallPublicationCandidates::one(base_version_id),
    };
    let Ok(candidates) = candidates else {
        return RecoveringLoaderPublication::DeferredNonterminal;
    };
    let verification_root = managed_root.clone();
    let Some(outcome) =
        classify_loader_candidates(managed_root, candidates, request_drain, convergence).await
    else {
        return RecoveringLoaderPublication::DeferredNonterminal;
    };
    match (checkpoint, outcome) {
        (_, ManagedInstallDurableOutcome::Mismatch) => {
            RecoveringLoaderPublication::Refused(loader_publication_refusal_error())
        }
        (None, ManagedInstallDurableOutcome::NoEffect) => RecoveringLoaderPublication::Fresh,
        (None, ManagedInstallDurableOutcome::Committed(evidence))
            if evidence.id().matches_version_id(base_version_id) =>
        {
            let Some(activation_contract_id) = evidence.committed_activation_contract_id().cloned()
            else {
                return RecoveringLoaderPublication::DeferredNonterminal;
            };
            RecoveringLoaderPublication::Base {
                checkpoint: super::operation::InstallPublicationCheckpoint {
                    kind: InstallPublicationCheckpointKind::BaseCommitted,
                    version_id: base_version_id.to_string(),
                    evidence: evidence.id().clone(),
                    activation_contract_id: Some(activation_contract_id),
                },
                evidence: Some(evidence),
                checkpoint_recorded: false,
            }
        }
        (
            None,
            ManagedInstallDurableOutcome::RolledBack {
                evidence, effect, ..
            },
        ) if evidence.id().matches_version_id(base_version_id) => {
            tracing::warn!(
                operation_id = %journal.operation_id,
                rollback_effect = ?effect,
                "startup loader recovery found a durable base rollback"
            );
            let checkpoint = super::operation::InstallPublicationCheckpoint {
                kind: InstallPublicationCheckpointKind::RolledBack,
                version_id: base_version_id.to_string(),
                evidence: evidence.id().clone(),
                activation_contract_id: None,
            };
            if super::operation::record_install_publication_checkpoint(
                journals,
                &journal.operation_id,
                &checkpoint,
            )
            .await
            .is_ok()
            {
                RecoveringLoaderPublication::RolledBack(Some(evidence))
            } else {
                RecoveringLoaderPublication::DeferredNonterminal
            }
        }
        (
            Some(
                checkpoint @ super::operation::InstallPublicationCheckpoint {
                    kind: InstallPublicationCheckpointKind::BaseCommitted,
                    ..
                },
            ),
            ManagedInstallDurableOutcome::Committed(evidence),
        ) if evidence.id().matches_version_id(base_version_id)
            && evidence.id() == &checkpoint.evidence
            && evidence.committed_activation_contract_id()
                == checkpoint.activation_contract_id.as_ref() =>
        {
            RecoveringLoaderPublication::Base {
                evidence: Some(evidence),
                checkpoint: checkpoint.clone(),
                checkpoint_recorded: true,
            }
        }
        (
            Some(super::operation::InstallPublicationCheckpoint {
                kind: InstallPublicationCheckpointKind::BaseCommitted,
                ..
            }),
            ManagedInstallDurableOutcome::Committed(evidence),
        ) if evidence.id().matches_version_id(target_version_id) => {
            let Some(activation_contract_id) = evidence.committed_activation_contract_id().cloned()
            else {
                return RecoveringLoaderPublication::DeferredNonterminal;
            };
            RecoveringLoaderPublication::Child {
                checkpoint: super::operation::InstallPublicationCheckpoint {
                    kind: InstallPublicationCheckpointKind::ChildCommitted,
                    version_id: target_version_id.to_string(),
                    evidence: evidence.id().clone(),
                    activation_contract_id: Some(activation_contract_id),
                },
                evidence: Some(evidence),
                checkpoint_recorded: false,
            }
        }
        (
            Some(super::operation::InstallPublicationCheckpoint {
                kind: InstallPublicationCheckpointKind::BaseCommitted,
                ..
            }),
            ManagedInstallDurableOutcome::RolledBack {
                evidence, effect, ..
            },
        ) if evidence.id().matches_version_id(target_version_id) => {
            tracing::warn!(
                operation_id = %journal.operation_id,
                rollback_effect = ?effect,
                "startup loader recovery found a durable child rollback"
            );
            let rollback = super::operation::InstallPublicationCheckpoint {
                kind: InstallPublicationCheckpointKind::RolledBack,
                version_id: target_version_id.to_string(),
                evidence: evidence.id().clone(),
                activation_contract_id: None,
            };
            if super::operation::record_install_publication_checkpoint(
                journals,
                &journal.operation_id,
                &rollback,
            )
            .await
            .is_ok()
            {
                RecoveringLoaderPublication::RolledBack(Some(evidence))
            } else {
                RecoveringLoaderPublication::DeferredNonterminal
            }
        }
        (Some(checkpoint), ManagedInstallDurableOutcome::Committed(evidence))
            if checkpoint.kind == InstallPublicationCheckpointKind::ChildCommitted
                && evidence.id().matches_version_id(target_version_id)
                && evidence.id() == &checkpoint.evidence
                && evidence.committed_activation_contract_id()
                    == checkpoint.activation_contract_id.as_ref() =>
        {
            RecoveringLoaderPublication::Child {
                evidence: Some(evidence),
                checkpoint: checkpoint.clone(),
                checkpoint_recorded: true,
            }
        }
        (Some(checkpoint), ManagedInstallDurableOutcome::RolledBack { evidence, .. })
            if checkpoint.kind == InstallPublicationCheckpointKind::RolledBack
                && evidence.id().matches_version_id(&checkpoint.version_id)
                && evidence.id() == &checkpoint.evidence =>
        {
            RecoveringLoaderPublication::RolledBack(Some(evidence))
        }
        (Some(checkpoint), ManagedInstallDurableOutcome::NoEffect)
            if verify_managed_install_publication_evidence_root(
                &verification_root,
                &checkpoint.evidence,
            ) =>
        {
            match checkpoint.kind {
                InstallPublicationCheckpointKind::BaseCommitted => {
                    RecoveringLoaderPublication::Base {
                        evidence: None,
                        checkpoint: checkpoint.clone(),
                        checkpoint_recorded: true,
                    }
                }
                InstallPublicationCheckpointKind::ChildCommitted => {
                    RecoveringLoaderPublication::Child {
                        evidence: None,
                        checkpoint: checkpoint.clone(),
                        checkpoint_recorded: true,
                    }
                }
                InstallPublicationCheckpointKind::RolledBack => {
                    RecoveringLoaderPublication::RolledBack(None)
                }
                InstallPublicationCheckpointKind::Committed => {
                    RecoveringLoaderPublication::DeferredNonterminal
                }
            }
        }
        _ => RecoveringLoaderPublication::DeferredNonterminal,
    }
}

async fn activate_recovered_loader_base(
    state: &AppState,
    foreground: &IntegrityForegroundLease,
    library_operation: &crate::state::LibraryOperation,
    journals: &crate::state::OperationJournalStore,
    operation_id: &crate::state::contracts::OperationId,
    expected_version_id: &str,
    evidence: Option<ManagedInstallCommittedEvidence>,
    checkpoint: super::operation::InstallPublicationCheckpoint,
    checkpoint_recorded: bool,
    commit: axial_minecraft::LoaderInstallBaseCommit,
) -> Result<
    (
        LoaderInstallBaseContinuation,
        Option<axial_minecraft::ManagedInstallPostActivationAcknowledgement>,
    ),
    (),
> {
    if checkpoint.kind != InstallPublicationCheckpointKind::BaseCommitted
        || checkpoint.version_id != expected_version_id
        || commit.base_version_id() != expected_version_id
    {
        return Err(());
    }
    let expected_contract = checkpoint.activation_contract_id.as_ref().ok_or(())?;
    match evidence {
        Some(evidence)
            if evidence.id() == &checkpoint.evidence
                && evidence.committed_activation_contract_id() == Some(expected_contract) =>
        {
            let verified = evidence.verify_loader_base_commit(commit).map_err(|_| ())?;
            if verified.activation_contract_id() != expected_contract {
                return Err(());
            }
            if !checkpoint_recorded {
                super::operation::record_install_publication_checkpoint(
                    journals,
                    operation_id,
                    &checkpoint,
                )
                .await
                .map_err(|_| ())?;
            }
            let (continuation, acknowledgement) = state
                .accept_verified_loader_base_commit(foreground, library_operation, verified)
                .await
                .map_err(|_| ())?;
            Ok((continuation, Some(acknowledgement)))
        }
        None if checkpoint_recorded => {
            let verified = verify_managed_install_loader_base_checkpoint(expected_contract, commit)
                .map_err(|_| ())?;
            let continuation = state
                .accept_verified_loader_base_checkpoint(foreground, library_operation, verified)
                .await
                .map_err(|_| ())?;
            Ok((continuation, None))
        }
        _ => Err(()),
    }
}

async fn activate_recovered_loader_child(
    state: &AppState,
    foreground: &IntegrityForegroundLease,
    library_operation: &crate::state::LibraryOperation,
    journals: &crate::state::OperationJournalStore,
    operation_id: &crate::state::contracts::OperationId,
    expected_version_id: &str,
    evidence: Option<ManagedInstallCommittedEvidence>,
    checkpoint: super::operation::InstallPublicationCheckpoint,
    checkpoint_recorded: bool,
    receipt: axial_minecraft::KnownGoodReconstructionReceipt,
) -> Result<Option<axial_minecraft::ManagedInstallPostActivationAcknowledgement>, ()> {
    if checkpoint.kind != InstallPublicationCheckpointKind::ChildCommitted
        || checkpoint.version_id != expected_version_id
        || receipt.version_id() != expected_version_id
    {
        return Err(());
    }
    let expected_contract = checkpoint.activation_contract_id.as_ref().ok_or(())?;
    match evidence {
        Some(evidence)
            if evidence.id() == &checkpoint.evidence
                && evidence.committed_activation_contract_id() == Some(expected_contract) =>
        {
            let verified = evidence
                .verify_reconstruction_receipt(receipt)
                .map_err(|_| ())?;
            if verified.activation_contract_id() != expected_contract {
                return Err(());
            }
            if !checkpoint_recorded {
                super::operation::record_install_publication_checkpoint(
                    journals,
                    operation_id,
                    &checkpoint,
                )
                .await
                .map_err(|_| ())?;
            }
            let acknowledgement = state
                .accept_verified_known_good_reconstruction_receipt(
                    foreground,
                    library_operation,
                    verified,
                )
                .await
                .map_err(|_| ())?;
            Ok(Some(acknowledgement))
        }
        None if checkpoint_recorded => {
            let verified =
                verify_managed_install_reconstruction_checkpoint(expected_contract, receipt)
                    .map_err(|_| ())?;
            state
                .accept_verified_known_good_checkpoint(foreground, library_operation, verified)
                .await
                .map_err(|_| ())?;
            Ok(None)
        }
        _ => Err(()),
    }
}

async fn settle_recovered_loader_child_after_worker_failure(
    state: &AppState,
    foreground: &IntegrityForegroundLease,
    library_operation: &crate::state::LibraryOperation,
    journals: &crate::state::OperationJournalStore,
    operation_id: &crate::state::contracts::OperationId,
    target_version_id: &str,
    receipt: axial_minecraft::KnownGoodInstallReceipt,
    request_drain: &mut InstallRequestDrain,
) -> Option<DownloadProgress> {
    require_exact_loader_receipt_version(target_version_id, receipt.version_id()).ok()?;
    let candidates = ManagedInstallPublicationCandidates::one(target_version_id).ok()?;
    let outcome = classify_loader_candidates(
        library_operation.retained_core(),
        candidates,
        request_drain,
        super::RecoveringInstallConvergence::SameProcess,
    )
    .await?;
    match outcome {
        ManagedInstallDurableOutcome::Committed(evidence)
            if evidence.id().matches_version_id(target_version_id) =>
        {
            let evidence_id = evidence.id().clone();
            let verified = evidence.verify_install_receipt(receipt).ok()?;
            let checkpoint = super::operation::InstallPublicationCheckpoint {
                kind: InstallPublicationCheckpointKind::ChildCommitted,
                version_id: target_version_id.to_string(),
                evidence: evidence_id,
                activation_contract_id: Some(verified.activation_contract_id().clone()),
            };
            super::operation::record_install_publication_checkpoint(
                journals,
                operation_id,
                &checkpoint,
            )
            .await
            .ok()?;
            let acknowledgement = state
                .accept_verified_known_good_install_receipt(foreground, library_operation, verified)
                .await
                .ok()?;
            if !super::converge_managed_install_acknowledgement(
                acknowledgement.acknowledge().await,
                request_drain,
            )
            .await
            {
                return None;
            }
            Some(sanitize_install_progress(loader_install_done_progress()))
        }
        ManagedInstallDurableOutcome::RolledBack {
            evidence, effect, ..
        } if evidence.id().matches_version_id(target_version_id) => {
            tracing::warn!(
                operation_id = %operation_id,
                rollback_effect = ?effect,
                "worker-failure loader recovery found a durable child rollback"
            );
            let checkpoint = super::operation::InstallPublicationCheckpoint {
                kind: InstallPublicationCheckpointKind::RolledBack,
                version_id: target_version_id.to_string(),
                evidence: evidence.id().clone(),
                activation_contract_id: None,
            };
            super::operation::record_install_publication_checkpoint(
                journals,
                operation_id,
                &checkpoint,
            )
            .await
            .ok()?;
            if !super::converge_managed_install_acknowledgement(
                evidence.acknowledge().await,
                request_drain,
            )
            .await
            {
                return None;
            }
            Some(loader_install_error_progress(
                &loader_publication_rollback_error(axial_minecraft::DownloadError::FileOperation(
                    std::io::Error::other("managed loader child publication was rolled back"),
                )),
            ))
        }
        ManagedInstallDurableOutcome::Mismatch => Some(loader_install_error_progress(
            &loader_publication_refusal_error(),
        )),
        ManagedInstallDurableOutcome::NoEffect
        | ManagedInstallDurableOutcome::Committed(_)
        | ManagedInstallDurableOutcome::RolledBack { .. }
        | ManagedInstallDurableOutcome::Indeterminate(_) => None,
    }
}

pub(super) async fn recover_loader_install_after_worker_failure(
    state: &AppState,
    foreground: &InstallForegroundActivity,
    rebuild_owner: &ProducerLease,
    journals: &crate::state::OperationJournalStore,
    operation_id: &crate::state::contracts::OperationId,
    install_id: &str,
    request_drain: &mut InstallRequestDrain,
) -> Option<DownloadProgress> {
    if let Some(progress) =
        super::operation::authoritative_install_terminal_progress(journals, operation_id)
    {
        return Some(progress);
    }
    let journal = super::operation::recovering_install_journal(journals, operation_id).ok()?;
    if journal.install_id != install_id {
        return None;
    }
    let InstallJournalIdentity::Loader {
        target_version_id,
        component_id: _,
        build_id: _,
        base_version_id,
    } = &journal.identity
    else {
        return None;
    };
    let target_version_id = target_version_id.clone();
    let base_version_id = base_version_id.clone();
    if !super::record_worker_failure_recovery_marker(state, journals, operation_id, install_id)
        .await
    {
        return None;
    }
    let foreground = retain_install_foreground(state, foreground).await?;
    let mutation = state.admit_managed_artifact_mutation().ok()?;
    let library_operation = state.try_acquire_managed_library().ok()?;
    let publication = classify_recovering_loader_publication(
        library_operation.retained_core(),
        &journal,
        &base_version_id,
        request_drain,
        journals,
        super::RecoveringInstallConvergence::SameProcess,
    )
    .await;
    let terminal = match publication {
        RecoveringLoaderPublication::Fresh => {
            drop(mutation);
            interrupted_loader_install_progress()
        }
        RecoveringLoaderPublication::Base {
            evidence,
            checkpoint,
            checkpoint_recorded,
        } => {
            drop(mutation);
            if evidence.is_none()
                && !super::rebuild_checkpoint_registered_known_good(
                    state,
                    &foreground,
                    rebuild_owner,
                    &library_operation,
                    &base_version_id,
                    &checkpoint,
                    request_drain,
                    super::RecoveringInstallConvergence::SameProcess,
                )
                .await
            {
                return None;
            }
            let receipt = super::converge_known_good_reconstruction(
                &base_version_id,
                request_drain,
                super::RecoveringInstallConvergence::SameProcess,
            )
            .await?;
            let mutation = state.admit_managed_artifact_mutation().ok()?;
            let commit = resume_install_build_after_base(&target_version_id, receipt).ok()?;
            let (continuation, acknowledgement) = activate_recovered_loader_base(
                state,
                &foreground,
                &library_operation,
                journals,
                operation_id,
                &base_version_id,
                evidence,
                checkpoint,
                checkpoint_recorded,
                commit,
            )
            .await
            .ok()?;
            drop(mutation);
            if let Some(acknowledgement) = acknowledgement
                && !super::converge_managed_install_acknowledgement(
                    acknowledgement.acknowledge().await,
                    request_drain,
                )
                .await
            {
                return None;
            }
            let mutation = state.admit_managed_artifact_mutation().ok()?;
            let result =
                continue_install_build_after_base(library_operation.core(), continuation, |_| {})
                    .await;
            let result = match result {
                Err(LoaderInstallError::PublicationIndeterminate(recovery)) => {
                    match converge_loader_install_publication(recovery, request_drain, true).await {
                        ManagedPublicationConvergence::Settled(result) => result,
                        ManagedPublicationConvergence::DeferredNonterminal => return None,
                    }
                }
                Ok(receipt) => Ok(LoaderInstallPublicationOutcome::ChildCommitted(receipt)),
                Err(error) => Err(error),
            };
            match result {
                Ok(LoaderInstallPublicationOutcome::ChildCommitted(receipt)) => {
                    let terminal = settle_recovered_loader_child_after_worker_failure(
                        state,
                        &foreground,
                        &library_operation,
                        journals,
                        operation_id,
                        &target_version_id,
                        receipt,
                        request_drain,
                    )
                    .await?;
                    drop(mutation);
                    terminal
                }
                Ok(LoaderInstallPublicationOutcome::BaseCommitted(_)) => return None,
                Err(error) => {
                    drop(mutation);
                    loader_install_error_progress(&error)
                }
            }
        }
        RecoveringLoaderPublication::Child {
            evidence,
            checkpoint,
            checkpoint_recorded,
        } => {
            drop(mutation);
            if evidence.is_none() {
                if !super::rebuild_checkpoint_registered_known_good(
                    state,
                    &foreground,
                    rebuild_owner,
                    &library_operation,
                    &target_version_id,
                    &checkpoint,
                    request_drain,
                    super::RecoveringInstallConvergence::SameProcess,
                )
                .await
                {
                    return None;
                }
            } else {
                let receipt = super::converge_known_good_reconstruction(
                    &target_version_id,
                    request_drain,
                    super::RecoveringInstallConvergence::SameProcess,
                )
                .await?;
                let mutation = state.admit_managed_artifact_mutation().ok()?;
                let acknowledgement = activate_recovered_loader_child(
                    state,
                    &foreground,
                    &library_operation,
                    journals,
                    operation_id,
                    &target_version_id,
                    evidence,
                    checkpoint,
                    checkpoint_recorded,
                    receipt,
                )
                .await
                .ok()?;
                drop(mutation);
                if let Some(acknowledgement) = acknowledgement
                    && !super::converge_managed_install_acknowledgement(
                        acknowledgement.acknowledge().await,
                        request_drain,
                    )
                    .await
                {
                    return None;
                }
            }
            sanitize_install_progress(loader_install_done_progress())
        }
        RecoveringLoaderPublication::RolledBack(evidence) => {
            drop(mutation);
            if let Some(evidence) = evidence
                && !super::converge_managed_install_acknowledgement(
                    evidence.acknowledge().await,
                    request_drain,
                )
                .await
            {
                return None;
            }
            loader_install_error_progress(&loader_publication_rollback_error(
                axial_minecraft::DownloadError::FileOperation(std::io::Error::other(
                    "managed loader install publication was rolled back",
                )),
            ))
        }
        RecoveringLoaderPublication::Refused(error) => {
            drop(mutation);
            loader_install_error_progress(&error)
        }
        RecoveringLoaderPublication::DeferredNonterminal => {
            drop(mutation);
            return None;
        }
    };
    drop(library_operation);
    Some(sanitize_install_progress(terminal))
}

enum LoaderPublicationDrive {
    Terminal(Result<DownloadProgress, LoaderInstallError>),
    DeferredNonterminal,
}

async fn continue_recovered_loader_after_base(
    library_operation: &crate::state::LibraryOperation,
    continuation: LoaderInstallBaseContinuation,
    progress_tx: &InstallProgressSender,
    journal_failed: &tokio::sync::Notify,
    final_progress: &Arc<Mutex<Option<DownloadProgress>>>,
) -> Result<LoaderInstallPublicationOutcome, LoaderInstallError> {
    let rerun_progress = Arc::clone(final_progress);
    let rerun_progress_tx = progress_tx.clone();
    let rerun = continue_install_build_after_base(
        library_operation.core(),
        continuation,
        move |progress| {
            if progress.done {
                if let Ok(mut final_progress) = rerun_progress.lock() {
                    *final_progress = Some(progress);
                }
                return;
            }
            let _ = publish_install_progress(&rerun_progress_tx, progress);
        },
    );
    let (result, ()) =
        await_managed_install_settlement_retaining((), rerun, journal_failed.notified()).await;
    result.map(LoaderInstallPublicationOutcome::ChildCommitted)
}

#[allow(clippy::too_many_arguments)]
async fn drive_loader_install_publication(
    state: &AppState,
    foreground: &IntegrityForegroundLease,
    library_operation: &crate::state::LibraryOperation,
    journals: &crate::state::OperationJournalStore,
    operation_id: &crate::state::contracts::OperationId,
    base_version_id: &str,
    target_version_id: &str,
    progress_tx: &InstallProgressSender,
    request_drain: &mut InstallRequestDrain,
    journal_failed: &tokio::sync::Notify,
    final_progress: &Arc<Mutex<Option<DownloadProgress>>>,
    mut result: Result<LoaderInstallPublicationOutcome, LoaderInstallError>,
) -> LoaderPublicationDrive {
    loop {
        let publication = match result {
            Err(LoaderInstallError::PublicationIndeterminate(recovery)) => {
                tracing::warn!(
                    operation_id = %operation_id,
                    version_id = target_version_id,
                    failure_kind = "publication_indeterminate",
                    "loader install worker entered owned publication recovery"
                );
                let recovering_committed = publish_install_progress_durably(
                    progress_tx,
                    publication_indeterminate_install_progress(),
                )
                .await;
                match converge_loader_install_publication(
                    recovery,
                    request_drain,
                    recovering_committed,
                )
                .await
                {
                    ManagedPublicationConvergence::Settled(recovered) => recovered,
                    ManagedPublicationConvergence::DeferredNonterminal => {
                        return LoaderPublicationDrive::DeferredNonterminal;
                    }
                }
            }
            settled => settled,
        };
        match publication {
            Ok(LoaderInstallPublicationOutcome::BaseCommitted(commit)) => {
                if let Err(error) =
                    require_exact_loader_receipt_version(base_version_id, commit.base_version_id())
                {
                    return LoaderPublicationDrive::Terminal(Err(LoaderInstallError::from(
                        LoaderError::Verify(error.to_string()),
                    )));
                }
                let continuation = match settle_loader_base_publication(
                    library_operation.retained_core(),
                    base_version_id,
                    journals,
                    operation_id,
                    progress_tx,
                    request_drain,
                    state,
                    foreground,
                    library_operation,
                    commit,
                )
                .await
                {
                    DurableLoaderBasePublication::Activated(continuation) => continuation,
                    DurableLoaderBasePublication::RolledBack(error) => {
                        return LoaderPublicationDrive::Terminal(Err(
                            loader_publication_rollback_error(error),
                        ));
                    }
                    DurableLoaderBasePublication::Refused(error) => {
                        return LoaderPublicationDrive::Terminal(Err(LoaderInstallError::from(
                            LoaderError::Verify(error.to_string()),
                        )));
                    }
                    DurableLoaderBasePublication::DeferredNonterminal => {
                        return LoaderPublicationDrive::DeferredNonterminal;
                    }
                };
                result = continue_recovered_loader_after_base(
                    library_operation,
                    continuation,
                    progress_tx,
                    journal_failed,
                    final_progress,
                )
                .await;
            }
            Ok(LoaderInstallPublicationOutcome::ChildCommitted(receipt)) => {
                if let Err(error) =
                    require_exact_loader_receipt_version(target_version_id, receipt.version_id())
                {
                    return LoaderPublicationDrive::Terminal(Err(LoaderInstallError::from(
                        LoaderError::Verify(error.to_string()),
                    )));
                }
                match settle_managed_install_publication(
                    library_operation.retained_core(),
                    target_version_id,
                    InstallPublicationCheckpointKind::ChildCommitted,
                    journals,
                    operation_id,
                    progress_tx,
                    request_drain,
                    state,
                    foreground,
                    library_operation,
                    receipt,
                )
                .await
                {
                    DurableInstallPublication::Committed => {
                        let terminal = sanitize_install_progress(
                            final_progress
                                .lock()
                                .ok()
                                .and_then(|mut progress| progress.take())
                                .unwrap_or_else(loader_install_done_progress),
                        );
                        let _ = publish_install_progress(progress_tx, terminal.clone());
                        return LoaderPublicationDrive::Terminal(Ok(terminal));
                    }
                    DurableInstallPublication::RolledBack(error) => {
                        return LoaderPublicationDrive::Terminal(Err(
                            loader_publication_rollback_error(error),
                        ));
                    }
                    DurableInstallPublication::Refused(error) => {
                        return LoaderPublicationDrive::Terminal(Err(LoaderInstallError::from(
                            LoaderError::Verify(error.to_string()),
                        )));
                    }
                    DurableInstallPublication::DeferredNonterminal => {
                        return LoaderPublicationDrive::DeferredNonterminal;
                    }
                }
            }
            Err(error) => return LoaderPublicationDrive::Terminal(Err(error)),
        }
    }
}

pub(super) async fn start_loader_install_with_foreground(
    state: &AppState,
    request: LoaderInstallStartRequest,
    producer: &ProducerLease,
    inherited_foreground: Option<IntegrityForegroundLease>,
    queue_start: &InstallQueueStartAuthority,
) -> Result<InstallStartResponse, InstallQueueStartFailure> {
    let update_admission = state
        .try_admit_update_sensitive_operation()
        .map_err(super::install_update_admission_error_response)?;
    let build_id = request.build_id.trim().to_string();
    if build_id.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "build_id is required" })),
        )
            .into());
    }
    super::require_available_install_library(state)?;

    let foreground = match inherited_foreground {
        Some(foreground) => foreground,
        None => {
            register_install_foreground(state)?
                .wait_for_settlement()
                .await
        }
    };
    let build = resolve_build_record_for_install(request.component_id, &build_id)
        .await
        .map_err(loader_pre_operation_error_response)?;

    let journal_identity =
        InstallJournalIdentity::loader(&build).map_err(|_| install_journal_error_response())?;
    let store = state.installs().clone();
    let journals = state.journals().clone();
    let mut admitted_install = None;
    for _ in 0..super::OPERATION_ID_RESERVATION_ATTEMPTS {
        let candidate = generate_install_id("loader-install");
        if super::operation::install_operation_journal_for_session(state.journals(), &candidate)
            .is_some()
        {
            continue;
        }
        let Some(candidate_operation_id) = mint_available_install_operation_id(state).await else {
            break;
        };
        let admission = InstallAdmissionMarker::new();
        let reservation = super::InstallInitializationReservation::new(
            store.clone(),
            journals.clone(),
            candidate.clone(),
            candidate_operation_id.clone(),
            admission.clone(),
            producer.claim_child(),
            foreground.retained(),
        );
        match store
            .admit_queued_loader(
                queue_start,
                &admission,
                candidate.clone(),
                candidate_operation_id.clone(),
                build.component_id,
                build.build_id.clone(),
            )
            .await
        {
            InstallQueueAdmission::Inserted => {
                admitted_install = Some((candidate, candidate_operation_id, reservation));
                break;
            }
            InstallQueueAdmission::Existing {
                install_id,
                operation_id,
            } => {
                drop(reservation);
                match store.wait_for_initialization(&install_id).await {
                    InstallInitializationStatus::Initialized => {
                        return Ok(InstallStartResponse {
                            operation_id,
                            install_id,
                            view_model: InstallProgressViewModel::starting(),
                        });
                    }
                    InstallInitializationStatus::Reconciling => {
                        return Err(install_journal_error_response().into());
                    }
                    InstallInitializationStatus::Removed => {
                        if !store
                            .reset_removed_queued_install_start(queue_start, &install_id)
                            .await
                        {
                            return Err(super::install_queue_start_stopped_error_response().into());
                        }
                    }
                }
            }
            InstallQueueAdmission::BlockedByLiveSession => {
                drop(reservation);
                return Err(InstallQueueStartFailure::BlockedByLiveSession);
            }
            InstallQueueAdmission::InstallIdCollision
            | InstallQueueAdmission::OperationIdCollision => {
                drop(reservation);
            }
            InstallQueueAdmission::ReservationLost => {
                drop(reservation);
                return Err(super::install_queue_start_stopped_error_response().into());
            }
        };
    }
    let Some((install_id, operation_id, reservation)) = admitted_install else {
        return Err(install_journal_error_response().into());
    };
    drop(foreground);
    let reservation =
        begin_install_journal_with_owned_reconciliation(reservation, journal_identity, producer)
            .await
            .map_err(|_| InstallQueueStartFailure::Application(install_journal_error_response()))?;
    if !store.mark_initialized(&install_id).await {
        return Err(install_journal_error_response().into());
    }

    let telemetry = state.telemetry().clone();
    let install_id_task = install_id.clone();
    let operation_id_task = operation_id.clone();

    let worker_store = store.clone();
    let worker_install_id = install_id_task.clone();
    let worker_journals = journals.clone();
    let worker_operation_id = operation_id_task.clone();
    let worker_failure_memory = state.failure_memory().clone();
    let reconciliation_telemetry = telemetry.clone();
    let worker_state = state.clone();
    let worker_runtime_cache = state.managed_runtime_cache().clone();
    let progress_owner = producer.claim_child();
    let guardian_owner = producer.claim_child();
    let foreground = InstallForegroundActivity::new_with_update_admission(
        reservation.retained_foreground(),
        update_admission,
    );
    let worker_initialization = reservation;
    let worker_foreground = foreground.clone();
    let interrupted_foreground = foreground.clone();
    let interrupted_state = state.clone();
    let reconciliation_foreground = foreground.clone();
    let reconciliation_state = state.clone();
    let reconciliation_journals = journals.clone();
    let reconciliation_operation_id = operation_id_task.clone();
    let worker_failure_journals = journals.clone();
    let worker_failure_operation_id = operation_id_task.clone();
    let worker_failure_state = state.clone();
    let worker_failure_foreground = foreground.clone();
    let worker_failure_install_id = install_id_task.clone();
    let worker_failure_request_drain = producer.wait_for_request_drain_start();
    let worker_failure_rebuild_owner = producer.claim_child();
    let recovery_request_drain = producer.wait_for_request_drain_start();
    spawn_install_foreground_retention(
        state.clone(),
        install_id_task.clone(),
        producer.claim_child(),
        foreground,
    );
    InstallStore::spawn_tracked_worker_with_exit_handlers_owned(
        store,
        producer.claim_child(),
        install_id_task,
        interrupted_loader_install_progress(),
        async move {
            drop(worker_initialization.hand_off());
            let mut recovery_request_drain: InstallRequestDrain = Box::pin(recovery_request_drain);
            let (progress_tx, progress_rx) = mpsc::unbounded_channel::<InstallProgressCommand>();
            let journal_failed = Arc::new(tokio::sync::Notify::new());
            let store_task = {
                let store = worker_store.clone();
                let install_id = worker_install_id.clone();
                let journals = worker_journals.clone();
                let operation_id = worker_operation_id.clone();
                let journal_failed = journal_failed.clone();
                progress_owner.spawn_joinable(async move {
                    let committed = own_install_progress(
                        store,
                        journals,
                        operation_id,
                        install_id,
                        progress_rx,
                    )
                    .await;
                    if !committed {
                        journal_failed.notify_one();
                    }
                    committed
                })
            };

            let version_id = build.version_id.clone();
            let base_version_id = build.minecraft_version.clone();
            let loader_target_id = format!(
                "loader_{}_{}",
                build.component_id.short_key(),
                build.build_id
            );
            let observed_base =
                observe_active_vanilla_base_install(&worker_store, &base_version_id).await;
            let (loader_foreground, base_install) = match observed_base {
                Ok(Some(observed)) => {
                    worker_foreground.release();
                    let base_install =
                        wait_for_observed_vanilla_base_install(observed, &progress_tx).await;
                    let Some(foreground) =
                        retain_install_foreground(&worker_state, &worker_foreground).await
                    else {
                        drop(progress_tx);
                        let _ = finish_install_progress_task(store_task).await;
                        return InstallStore::worker_exit_interrupt_if_active();
                    };
                    (foreground, base_install)
                }
                Ok(None) => {
                    let Some(foreground) = worker_foreground.retained() else {
                        drop(progress_tx);
                        let _ = finish_install_progress_task(store_task).await;
                        return InstallStore::worker_exit_interrupt_if_active();
                    };
                    (foreground, Ok(()))
                }
                Err(progress) => {
                    let Some(foreground) = worker_foreground.retained() else {
                        drop(progress_tx);
                        let _ = finish_install_progress_task(store_task).await;
                        return InstallStore::worker_exit_interrupt_if_active();
                    };
                    (foreground, Err(progress))
                }
            };
            if let Err(progress) = base_install {
                record_loader_base_install_dependency_guardian_failure_outcome(
                    worker_journals.as_ref(),
                    &worker_operation_id,
                    &loader_target_id,
                    &base_version_id,
                )
                .await
                .ok();
                let exact_terminal = sanitize_install_progress(progress.clone());
                let _ = publish_install_progress(&progress_tx, progress);
                drop(progress_tx);
                let _ = finish_install_progress_task(store_task).await;
                return InstallStore::worker_exit_reconcile_terminal(exact_terminal);
            }

            let final_progress = Arc::new(Mutex::new(None::<DownloadProgress>));
            let final_progress_for_install = Arc::clone(&final_progress);
            let settlement = match worker_state.admit_managed_artifact_mutation() {
                Ok(mutation) => match worker_state.try_acquire_managed_library() {
                    Ok(library_operation) => {
                        let install = install_build(
                            library_operation.core(),
                            worker_runtime_cache.clone(),
                            build.clone(),
                            |progress| {
                                if progress.done {
                                    if let Ok(mut final_progress) =
                                        final_progress_for_install.lock()
                                    {
                                        *final_progress = Some(progress);
                                    }
                                    return;
                                }
                                let _ = publish_install_progress(&progress_tx, progress);
                            },
                        );
                        let (result, mutation) = await_managed_install_settlement_retaining(
                            mutation,
                            install,
                            journal_failed.notified(),
                        )
                        .await;
                        (result, Some((mutation, library_operation)))
                    }
                    Err(error) => {
                        drop(mutation);
                        (Err(LoaderInstallError::from(LoaderError::Io(error))), None)
                    }
                },
                Err(error) => (
                    Err(LoaderInstallError::from(LoaderError::Io(
                        std::io::Error::other(error),
                    ))),
                    None,
                ),
            };
            let (result, authority) = settlement;
            let result = match (result, authority.as_ref()) {
                (result, Some((_, library_operation))) => {
                    drive_loader_install_publication(
                        &worker_state,
                        &loader_foreground,
                        library_operation,
                        worker_journals.as_ref(),
                        &worker_operation_id,
                        &base_version_id,
                        &version_id,
                        &progress_tx,
                        &mut recovery_request_drain,
                        journal_failed.as_ref(),
                        &final_progress,
                        result,
                    )
                    .await
                }
                (Err(error), None) => LoaderPublicationDrive::Terminal(Err(error)),
                (Ok(_), None) => LoaderPublicationDrive::Terminal(Err(LoaderInstallError::from(
                    LoaderError::Verify(
                        "managed install authority ended before publication settlement".to_string(),
                    ),
                ))),
            };
            let LoaderPublicationDrive::Terminal(result) = result else {
                drop(progress_tx);
                let _ = finish_install_progress_task(store_task).await;
                drop(authority);
                return InstallStore::worker_exit_deferred_nonterminal();
            };

            let exact_terminal = match result {
                Err(error) => {
                    let progress = loader_install_error_progress(&error);
                    dispatch_loader_install_failure(
                        &guardian_owner,
                        worker_journals.clone(),
                        worker_failure_memory.clone(),
                        LoaderInstallFailureRequest {
                            operation_id: &worker_operation_id,
                            loader_target_id: &loader_target_id,
                            base_version_id: &base_version_id,
                            error,
                        },
                    )
                    .await;
                    let exact_terminal = sanitize_install_progress(progress.clone());
                    let _ = publish_install_progress(&progress_tx, progress);
                    exact_terminal
                }
                Ok(terminal) => terminal,
            };
            drop(progress_tx);
            let _ = finish_install_progress_task(store_task).await;
            drop(authority);
            InstallStore::worker_exit_reconcile_terminal(exact_terminal)
        },
        move |interrupted_progress| async move {
            let _foreground =
                retain_install_foreground(&interrupted_state, &interrupted_foreground).await;
            match reconcile_install_worker_interruption(
                journals.as_ref(),
                &operation_id_task,
                interrupted_progress,
            )
            .await
            {
                Ok(progress) => Some(progress),
                Err(_) => {
                    tracing::warn!("failed to reconcile interrupted loader-install journal");
                    None
                }
            }
        },
        move |progress| async move {
            let _foreground =
                retain_install_foreground(&reconciliation_state, &reconciliation_foreground).await;
            let progress = sanitize_install_progress(progress);
            let progress = match reconcile_install_operation_terminal(
                reconciliation_journals.as_ref(),
                &reconciliation_operation_id,
                &progress,
            )
            .await
            {
                Ok(progress) => progress,
                Err(_) => {
                    tracing::warn!("failed to reconcile exact loader-install terminal");
                    return None;
                }
            };
            if let Some(summary) = progress.error.as_deref() {
                emit_install_failed(reconciliation_telemetry.as_ref(), summary);
            }
            Some(progress)
        },
        move || async move {
            let mut request_drain: InstallRequestDrain = Box::pin(worker_failure_request_drain);
            recover_loader_install_after_worker_failure(
                &worker_failure_state,
                &worker_failure_foreground,
                &worker_failure_rebuild_owner,
                worker_failure_journals.as_ref(),
                &worker_failure_operation_id,
                &worker_failure_install_id,
                &mut request_drain,
            )
            .await
        },
    );

    Ok(InstallStartResponse {
        install_id,
        operation_id,
        view_model: InstallProgressViewModel::starting(),
    })
}

pub(super) fn spawn_recovering_loader_install<Reconstruct, Reconstruction>(
    state: AppState,
    producer: ProducerLease,
    admission: RecoveringInstallAdmission,
    startup_settled: oneshot::Sender<()>,
    reconstruct: Reconstruct,
) where
    Reconstruct: Fn(String) -> Reconstruction + Clone + Send + Sync + 'static,
    Reconstruction: Future<
            Output = Result<
                axial_minecraft::KnownGoodReconstructionReceipt,
                axial_minecraft::KnownGoodReconstructionError,
            >,
        > + Send
        + 'static,
{
    let RecoveringInstallAdmission {
        journal,
        foreground,
    } = admission;
    let InstallJournalIdentity::Loader {
        target_version_id,
        component_id,
        build_id,
        base_version_id,
    } = &journal.identity
    else {
        return;
    };
    let target_version_id = target_version_id.clone();
    let component_id = *component_id;
    let build_id = build_id.clone();
    let base_version_id = base_version_id.clone();
    let install_id = journal.install_id.clone();
    let operation_id = journal.operation_id.clone();
    let store = state.installs().clone();
    let journals = state.journals().clone();
    let telemetry = state.telemetry().clone();
    let failure_memory = state.failure_memory().clone();
    let runtime_cache = state.managed_runtime_cache().clone();
    let worker_state = state.clone();
    let worker_store = store.clone();
    let worker_journals = journals.clone();
    let worker_operation_id = operation_id.clone();
    let worker_install_id = install_id.clone();
    let worker_foreground = foreground.clone();
    let interrupted_state = state.clone();
    let interrupted_foreground = foreground.clone();
    let terminal_state = state.clone();
    let terminal_foreground = foreground.clone();
    let terminal_journals = journals.clone();
    let terminal_operation_id = operation_id.clone();
    let terminal_telemetry = telemetry.clone();
    let worker_failure_journals = journals.clone();
    let worker_failure_operation_id = operation_id.clone();
    let worker_failure_state = state.clone();
    let worker_failure_foreground = foreground.clone();
    let worker_failure_install_id = install_id.clone();
    let worker_failure_request_drain = producer.wait_for_request_drain_start();
    let checkpoint_rebuild_owner = producer.claim_child();
    let worker_failure_rebuild_owner = producer.claim_child();
    let progress_owner = producer.claim_child();
    let guardian_owner = producer.claim_child();
    let request_drain = producer.wait_for_request_drain_start();
    spawn_install_foreground_retention(
        state,
        install_id.clone(),
        producer.claim_child(),
        foreground,
    );
    InstallStore::spawn_tracked_worker_with_exit_handlers_owned(
        store,
        producer.claim_child(),
        install_id,
        interrupted_loader_install_progress(),
        async move {
            let mut startup_settled = Some(startup_settled);
            let mut request_drain: InstallRequestDrain = Box::pin(request_drain);
            let (progress_tx, progress_rx) = mpsc::unbounded_channel::<InstallProgressCommand>();
            let journal_failed = Arc::new(tokio::sync::Notify::new());
            let progress_task = {
                let journal_failed = Arc::clone(&journal_failed);
                let journals = worker_journals.clone();
                let operation_id = worker_operation_id.clone();
                progress_owner.spawn_joinable(async move {
                    let committed = own_install_progress(
                        worker_store,
                        journals,
                        operation_id,
                        worker_install_id,
                        progress_rx,
                    )
                    .await;
                    if !committed {
                        journal_failed.notify_one();
                    }
                    committed
                })
            };
            if !publish_install_progress_durably(
                &progress_tx,
                publication_indeterminate_install_progress(),
            )
            .await
            {
                drop(progress_tx);
                let _ = finish_install_progress_task(progress_task).await;
                return InstallStore::worker_exit_deferred_nonterminal();
            }
            let mutation = match worker_state.admit_managed_artifact_mutation() {
                Ok(mutation) => mutation,
                Err(_) => {
                    drop(progress_tx);
                    let _ = finish_install_progress_task(progress_task).await;
                    return InstallStore::worker_exit_deferred_nonterminal();
                }
            };
            let library_operation = match worker_state.try_acquire_managed_library() {
                Ok(operation) => operation,
                Err(_) => {
                    drop(mutation);
                    drop(progress_tx);
                    let _ = finish_install_progress_task(progress_task).await;
                    return InstallStore::worker_exit_deferred_nonterminal();
                }
            };
            let publication = classify_recovering_loader_publication(
                library_operation.retained_core(),
                &journal,
                &base_version_id,
                &mut request_drain,
                worker_journals.as_ref(),
                super::RecoveringInstallConvergence::StartupBounded,
            )
            .await;
            let Some(loader_foreground) = worker_foreground.retained() else {
                drop(library_operation);
                drop(mutation);
                drop(progress_tx);
                let _ = finish_install_progress_task(progress_task).await;
                return InstallStore::worker_exit_deferred_nonterminal();
            };
            let final_progress = Arc::new(Mutex::new(None::<DownloadProgress>));
            let result = match publication {
                RecoveringLoaderPublication::Fresh => {
                    if super::signal_startup_install_settled(&mut startup_settled).is_err() {
                        drop(progress_tx);
                        let _ = finish_install_progress_task(progress_task).await;
                        return InstallStore::worker_exit_deferred_nonterminal();
                    }
                    let resolution = resolve_after_releasing_loader_recovery_authority(
                        mutation,
                        library_operation,
                        resolve_build_record_for_install(component_id, &build_id),
                    )
                    .await;
                    match resolution {
                        Ok(build)
                            if build.component_id == component_id
                                && build.build_id == build_id
                                && build.minecraft_version == base_version_id
                                && build.version_id == target_version_id =>
                        {
                            async {
                                let mutation = match worker_state.admit_managed_artifact_mutation()
                                {
                                    Ok(mutation) => mutation,
                                    Err(_) => {
                                        return LoaderPublicationDrive::DeferredNonterminal;
                                    }
                                };
                                let library_operation =
                                    match worker_state.try_acquire_managed_library() {
                                        Ok(operation) => operation,
                                        Err(_) => {
                                            drop(mutation);
                                            return LoaderPublicationDrive::DeferredNonterminal;
                                        }
                                    };
                                if !matches!(
                                    classify_recovering_loader_publication(
                                        library_operation.retained_core(),
                                        &journal,
                                        &base_version_id,
                                        &mut request_drain,
                                        worker_journals.as_ref(),
                                        super::RecoveringInstallConvergence::StartupBounded,
                                    )
                                    .await,
                                    RecoveringLoaderPublication::Fresh
                                ) {
                                    return LoaderPublicationDrive::DeferredNonterminal;
                                }
                                let final_progress_for_install = Arc::clone(&final_progress);
                                let progress_for_install = progress_tx.clone();
                                let install = install_build(
                                    library_operation.core(),
                                    runtime_cache,
                                    build,
                                    move |progress| {
                                        if progress.done {
                                            if let Ok(mut final_progress) =
                                                final_progress_for_install.lock()
                                            {
                                                *final_progress = Some(progress);
                                            }
                                            return;
                                        }
                                        let _ = publish_install_progress(
                                            &progress_for_install,
                                            progress,
                                        );
                                    },
                                );
                                let (result, _) = await_managed_install_settlement_retaining(
                                    (&mutation, &library_operation),
                                    install,
                                    journal_failed.notified(),
                                )
                                .await;
                                drive_loader_install_publication(
                                    &worker_state,
                                    &loader_foreground,
                                    &library_operation,
                                    worker_journals.as_ref(),
                                    &worker_operation_id,
                                    &base_version_id,
                                    &target_version_id,
                                    &progress_tx,
                                    &mut request_drain,
                                    journal_failed.as_ref(),
                                    &final_progress,
                                    result,
                                )
                                .await
                            }
                            .await
                        }
                        Ok(_) => LoaderPublicationDrive::Terminal(Err(LoaderInstallError::from(
                            LoaderError::Verify(
                                "resolved loader build did not match the recovery journal"
                                    .to_string(),
                            ),
                        ))),
                        Err(error) => {
                            LoaderPublicationDrive::Terminal(Err(LoaderInstallError::from(error)))
                        }
                    }
                }
                RecoveringLoaderPublication::Base {
                    evidence,
                    checkpoint,
                    checkpoint_recorded,
                } => {
                    let had_evidence = evidence.is_some();
                    if !had_evidence
                        && super::signal_startup_install_settled(&mut startup_settled).is_err()
                    {
                        drop(progress_tx);
                        let _ = finish_install_progress_task(progress_task).await;
                        return InstallStore::worker_exit_deferred_nonterminal();
                    }
                    drop(mutation);
                    if !had_evidence
                        && !super::rebuild_checkpoint_registered_known_good_with(
                            &worker_state,
                            &loader_foreground,
                            &checkpoint_rebuild_owner,
                            &library_operation,
                            &base_version_id,
                            &checkpoint,
                            reconstruct.clone(),
                            &mut request_drain,
                            super::RecoveringInstallConvergence::StartupBounded,
                        )
                        .await
                    {
                        drop(progress_tx);
                        let _ = finish_install_progress_task(progress_task).await;
                        return InstallStore::worker_exit_deferred_nonterminal();
                    }
                    let mut reconstruct_base = reconstruct.clone();
                    let receipt = match super::converge_known_good_reconstruction_with(
                        &base_version_id,
                        &mut reconstruct_base,
                        &mut request_drain,
                        super::RecoveringInstallConvergence::StartupBounded,
                    )
                    .await
                    {
                        Some(receipt) => receipt,
                        None => {
                            drop(progress_tx);
                            let _ = finish_install_progress_task(progress_task).await;
                            return InstallStore::worker_exit_deferred_nonterminal();
                        }
                    };
                    let activation_mutation = match worker_state.admit_managed_artifact_mutation() {
                        Ok(mutation) => mutation,
                        Err(_) => {
                            drop(progress_tx);
                            let _ = finish_install_progress_task(progress_task).await;
                            return InstallStore::worker_exit_deferred_nonterminal();
                        }
                    };
                    let commit = match resume_install_build_after_base(&target_version_id, receipt)
                    {
                        Ok(commit) => commit,
                        Err(_) => {
                            drop(progress_tx);
                            let _ = finish_install_progress_task(progress_task).await;
                            return InstallStore::worker_exit_deferred_nonterminal();
                        }
                    };
                    let (continuation, acknowledgement) = match activate_recovered_loader_base(
                        &worker_state,
                        &loader_foreground,
                        &library_operation,
                        worker_journals.as_ref(),
                        &worker_operation_id,
                        &base_version_id,
                        evidence,
                        checkpoint,
                        checkpoint_recorded,
                        commit,
                    )
                    .await
                    {
                        Ok(activated) => activated,
                        Err(()) => {
                            drop(progress_tx);
                            let _ = finish_install_progress_task(progress_task).await;
                            return InstallStore::worker_exit_deferred_nonterminal();
                        }
                    };
                    drop(activation_mutation);
                    if let Some(acknowledgement) = acknowledgement
                        && !super::acknowledge_startup_managed_install_publication(
                            acknowledgement.acknowledge(),
                            &mut request_drain,
                        )
                        .await
                    {
                        drop(progress_tx);
                        let _ = finish_install_progress_task(progress_task).await;
                        return InstallStore::worker_exit_deferred_nonterminal();
                    }
                    if startup_settled.is_some()
                        && super::signal_startup_install_settled(&mut startup_settled).is_err()
                    {
                        drop(progress_tx);
                        let _ = finish_install_progress_task(progress_task).await;
                        return InstallStore::worker_exit_deferred_nonterminal();
                    }
                    let _child_mutation = match worker_state.admit_managed_artifact_mutation() {
                        Ok(mutation) => mutation,
                        Err(_) => {
                            drop(progress_tx);
                            let _ = finish_install_progress_task(progress_task).await;
                            return InstallStore::worker_exit_deferred_nonterminal();
                        }
                    };
                    let result = continue_recovered_loader_after_base(
                        &library_operation,
                        continuation,
                        &progress_tx,
                        journal_failed.as_ref(),
                        &final_progress,
                    )
                    .await;
                    drive_loader_install_publication(
                        &worker_state,
                        &loader_foreground,
                        &library_operation,
                        worker_journals.as_ref(),
                        &worker_operation_id,
                        &base_version_id,
                        &target_version_id,
                        &progress_tx,
                        &mut request_drain,
                        journal_failed.as_ref(),
                        &final_progress,
                        result,
                    )
                    .await
                }
                RecoveringLoaderPublication::Child {
                    evidence,
                    checkpoint,
                    checkpoint_recorded,
                } => {
                    let had_evidence = evidence.is_some();
                    if !had_evidence
                        && super::signal_startup_install_settled(&mut startup_settled).is_err()
                    {
                        drop(progress_tx);
                        let _ = finish_install_progress_task(progress_task).await;
                        return InstallStore::worker_exit_deferred_nonterminal();
                    }
                    drop(mutation);
                    if !had_evidence {
                        if !super::rebuild_checkpoint_registered_known_good_with(
                            &worker_state,
                            &loader_foreground,
                            &checkpoint_rebuild_owner,
                            &library_operation,
                            &target_version_id,
                            &checkpoint,
                            reconstruct.clone(),
                            &mut request_drain,
                            super::RecoveringInstallConvergence::StartupBounded,
                        )
                        .await
                        {
                            drop(progress_tx);
                            let _ = finish_install_progress_task(progress_task).await;
                            return InstallStore::worker_exit_deferred_nonterminal();
                        }
                    } else {
                        let mut reconstruct_child = reconstruct.clone();
                        let receipt = match super::converge_known_good_reconstruction_with(
                            &target_version_id,
                            &mut reconstruct_child,
                            &mut request_drain,
                            super::RecoveringInstallConvergence::StartupBounded,
                        )
                        .await
                        {
                            Some(receipt) => receipt,
                            None => {
                                drop(progress_tx);
                                let _ = finish_install_progress_task(progress_task).await;
                                return InstallStore::worker_exit_deferred_nonterminal();
                            }
                        };
                        let mutation = match worker_state.admit_managed_artifact_mutation() {
                            Ok(mutation) => mutation,
                            Err(_) => {
                                drop(progress_tx);
                                let _ = finish_install_progress_task(progress_task).await;
                                return InstallStore::worker_exit_deferred_nonterminal();
                            }
                        };
                        let acknowledgement = match activate_recovered_loader_child(
                            &worker_state,
                            &loader_foreground,
                            &library_operation,
                            worker_journals.as_ref(),
                            &worker_operation_id,
                            &target_version_id,
                            evidence,
                            checkpoint,
                            checkpoint_recorded,
                            receipt,
                        )
                        .await
                        {
                            Ok(acknowledgement) => acknowledgement,
                            Err(()) => {
                                drop(progress_tx);
                                let _ = finish_install_progress_task(progress_task).await;
                                return InstallStore::worker_exit_deferred_nonterminal();
                            }
                        };
                        drop(mutation);
                        if let Some(acknowledgement) = acknowledgement
                            && !super::acknowledge_startup_managed_install_publication(
                                acknowledgement.acknowledge(),
                                &mut request_drain,
                            )
                            .await
                        {
                            drop(progress_tx);
                            let _ = finish_install_progress_task(progress_task).await;
                            return InstallStore::worker_exit_deferred_nonterminal();
                        }
                    }
                    let _ = super::signal_startup_install_settled(&mut startup_settled);
                    let terminal = sanitize_install_progress(loader_install_done_progress());
                    let _ = publish_install_progress(&progress_tx, terminal.clone());
                    LoaderPublicationDrive::Terminal(Ok(terminal))
                }
                RecoveringLoaderPublication::RolledBack(evidence) => {
                    if evidence.is_none()
                        && super::signal_startup_install_settled(&mut startup_settled).is_err()
                    {
                        drop(progress_tx);
                        let _ = finish_install_progress_task(progress_task).await;
                        return InstallStore::worker_exit_deferred_nonterminal();
                    }
                    drop(mutation);
                    if let Some(evidence) = evidence {
                        if !super::acknowledge_startup_managed_install_publication(
                            evidence.acknowledge(),
                            &mut request_drain,
                        )
                        .await
                        {
                            drop(progress_tx);
                            let _ = finish_install_progress_task(progress_task).await;
                            return InstallStore::worker_exit_deferred_nonterminal();
                        }
                        let _ = super::signal_startup_install_settled(&mut startup_settled);
                    }
                    LoaderPublicationDrive::Terminal(Err(LoaderInstallError::from(
                        LoaderError::Verify(
                            "managed loader install publication was rolled back".to_string(),
                        ),
                    )))
                }
                RecoveringLoaderPublication::Refused(error) => {
                    drop(mutation);
                    let _ = super::signal_startup_install_settled(&mut startup_settled);
                    LoaderPublicationDrive::Terminal(Err(error))
                }
                RecoveringLoaderPublication::DeferredNonterminal => {
                    drop(progress_tx);
                    let _ = finish_install_progress_task(progress_task).await;
                    return InstallStore::worker_exit_deferred_nonterminal();
                }
            };
            let LoaderPublicationDrive::Terminal(result) = result else {
                drop(progress_tx);
                let _ = finish_install_progress_task(progress_task).await;
                return InstallStore::worker_exit_deferred_nonterminal();
            };
            let exact_terminal = match result {
                Ok(progress) => progress,
                Err(error) => {
                    let progress = loader_install_error_progress(&error);
                    let loader_target_id =
                        format!("loader_{}_{}", component_id.short_key(), build_id);
                    dispatch_loader_install_failure(
                        &guardian_owner,
                        worker_journals.clone(),
                        failure_memory,
                        LoaderInstallFailureRequest {
                            operation_id: &worker_operation_id,
                            loader_target_id: &loader_target_id,
                            base_version_id: &base_version_id,
                            error,
                        },
                    )
                    .await;
                    let exact_terminal = sanitize_install_progress(progress.clone());
                    let _ = publish_install_progress(&progress_tx, progress);
                    exact_terminal
                }
            };
            drop(progress_tx);
            let _ = finish_install_progress_task(progress_task).await;
            InstallStore::worker_exit_reconcile_terminal(exact_terminal)
        },
        move |interrupted_progress| async move {
            let _foreground =
                retain_install_foreground(&interrupted_state, &interrupted_foreground).await;
            match reconcile_install_worker_interruption(
                journals.as_ref(),
                &operation_id,
                interrupted_progress,
            )
            .await
            {
                Ok(progress) => Some(progress),
                Err(_) => {
                    tracing::warn!(
                        "failed to reconcile interrupted recovering loader-install journal"
                    );
                    None
                }
            }
        },
        move |progress| async move {
            let _foreground =
                retain_install_foreground(&terminal_state, &terminal_foreground).await;
            let progress = sanitize_install_progress(progress);
            let progress = match reconcile_install_operation_terminal(
                terminal_journals.as_ref(),
                &terminal_operation_id,
                &progress,
            )
            .await
            {
                Ok(progress) => progress,
                Err(_) => {
                    tracing::warn!("failed to reconcile exact recovering loader-install terminal");
                    return None;
                }
            };
            if let Some(summary) = progress.error.as_deref() {
                emit_install_failed(terminal_telemetry.as_ref(), summary);
            }
            Some(progress)
        },
        move || async move {
            let mut request_drain: InstallRequestDrain = Box::pin(worker_failure_request_drain);
            recover_loader_install_after_worker_failure(
                &worker_failure_state,
                &worker_failure_foreground,
                &worker_failure_rebuild_owner,
                worker_failure_journals.as_ref(),
                &worker_failure_operation_id,
                &worker_failure_install_id,
                &mut request_drain,
            )
            .await
        },
    );
}

fn loader_publication_rollback_error(error: axial_minecraft::DownloadError) -> LoaderInstallError {
    LoaderInstallError::from(LoaderError::Verify(format!(
        "managed loader install publication was rolled back: {error}"
    )))
}

fn loader_publication_refusal_error() -> LoaderInstallError {
    LoaderInstallError::from(LoaderError::Verify(
        "managed loader install publication belongs to another operation".to_string(),
    ))
}

pub(super) fn require_exact_loader_receipt_version(
    expected_version_id: &str,
    receipt_version_id: &str,
) -> std::io::Result<()> {
    if expected_version_id != receipt_version_id {
        return Err(std::io::Error::other(
            "verified loader receipt identity did not match the resolved install target",
        ));
    }
    Ok(())
}

pub(super) struct LoaderInstallFailureRequest<'a> {
    pub(super) operation_id: &'a crate::state::contracts::OperationId,
    pub(super) loader_target_id: &'a str,
    pub(super) base_version_id: &'a str,
    pub(super) error: LoaderInstallError,
}

pub(super) async fn dispatch_loader_install_failure(
    producer: &ProducerLease,
    journals: Arc<crate::state::OperationJournalStore>,
    failure_memory: Arc<crate::state::GuardianFailureMemoryStore>,
    request: LoaderInstallFailureRequest<'_>,
) {
    let LoaderInstallFailureRequest {
        operation_id,
        loader_target_id,
        base_version_id,
        error,
    } = request;
    match error {
        LoaderInstallError::PublicationIndeterminate(_) => {}
        LoaderInstallError::BaseInstallFailed(failure) => {
            if failure.facts().is_empty() {
                record_loader_base_install_dependency_guardian_failure_outcome(
                    &journals,
                    operation_id,
                    loader_target_id,
                    base_version_id,
                )
                .await
                .ok();
                return;
            }
            record_install_failure_outcome_for_error(
                producer,
                journals,
                failure_memory,
                operation_id,
                failure.error(),
                failure.facts(),
            )
            .await
        }
        LoaderInstallError::ArtifactDownloadFailed(failure) => {
            record_install_failure_outcome(
                producer,
                journals,
                failure_memory,
                operation_id,
                failure.facts(),
            )
            .await
        }
        LoaderInstallError::Active(failure) => {
            record_loader_install_operation_guardian_failure_outcome(
                producer,
                journals,
                failure_memory,
                operation_id,
                loader_target_id,
                &failure,
            )
            .await
            .ok();
        }
    }
}

pub(super) struct ObservedVanillaBaseInstall {
    store: Arc<InstallStore>,
    install_id: String,
    snapshot: InstallSnapshot,
    receiver: tokio::sync::broadcast::Receiver<InstallProgressRecord>,
}

pub(super) async fn observe_active_vanilla_base_install(
    store: &Arc<InstallStore>,
    version_id: &str,
) -> Result<Option<ObservedVanillaBaseInstall>, DownloadProgress> {
    let Some(install_id) = store.active_vanilla_install(version_id).await else {
        return Ok(None);
    };
    let Some((snapshot, receiver)) = store.subscribe_records(&install_id).await else {
        return Err(base_install_failed_progress());
    };
    if let Some(record) = snapshot.latest.as_ref()
        && let Some(terminal) = explicit_base_install_terminal(&record.progress)
    {
        return terminal.map(|()| None);
    }
    if snapshot.done {
        return Err(base_install_failed_progress());
    }
    Ok(Some(ObservedVanillaBaseInstall {
        store: store.clone(),
        install_id,
        snapshot,
        receiver,
    }))
}

pub(super) async fn wait_for_observed_vanilla_base_install(
    observed: ObservedVanillaBaseInstall,
    progress_tx: &InstallProgressSender,
) -> Result<(), DownloadProgress> {
    let ObservedVanillaBaseInstall {
        store,
        install_id,
        snapshot,
        mut receiver,
    } = observed;
    debug_assert!(!snapshot.done);
    if let Some(record) = snapshot.latest {
        let _ = publish_install_progress(progress_tx, record.progress);
    }

    loop {
        match receiver.recv().await {
            Ok(record) => {
                if let Some(terminal) = explicit_base_install_terminal(&record.progress) {
                    return terminal;
                }
                let _ = publish_install_progress(progress_tx, record.progress);
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                let Some(snapshot) = store.snapshot(&install_id).await else {
                    return Err(base_install_failed_progress());
                };
                let Some(progress) = snapshot.latest.as_ref().map(|record| &record.progress) else {
                    return Err(base_install_failed_progress());
                };
                return explicit_base_install_terminal(progress)
                    .unwrap_or_else(|| Err(base_install_failed_progress()));
            }
        }
    }
}

fn explicit_base_install_terminal(
    progress: &DownloadProgress,
) -> Option<Result<(), DownloadProgress>> {
    progress.done.then(|| {
        if progress.error.is_some() {
            Err(base_install_failed_progress())
        } else {
            Ok(())
        }
    })
}

pub fn loader_pre_operation_error_response(error: LoaderError) -> InstallApplicationError {
    let failure_kind = error
        .pre_operation_failure_kind()
        .unwrap_or(LoaderPreOperationFailureKind::CatalogUnavailable);
    let copy_kind = if matches!(&error, LoaderError::CatalogUnavailable { .. }) {
        LoaderPreOperationFailureKind::CatalogUnavailable
    } else {
        failure_kind
    };
    let status = match failure_kind {
        LoaderPreOperationFailureKind::InvalidMinecraftVersion
        | LoaderPreOperationFailureKind::InvalidBuildId => StatusCode::BAD_REQUEST,
        LoaderPreOperationFailureKind::BuildNotFound => StatusCode::NOT_FOUND,
        LoaderPreOperationFailureKind::CatalogStale => StatusCode::PRECONDITION_FAILED,
        LoaderPreOperationFailureKind::ProviderHttpFailure
            if error.provider_failure_kind() == Some(LoaderProviderFailureKind::HttpNotFound) =>
        {
            StatusCode::NOT_FOUND
        }
        LoaderPreOperationFailureKind::CatalogUnavailable
        | LoaderPreOperationFailureKind::ProviderHttpFailure
        | LoaderPreOperationFailureKind::ProviderNetworkFailure
        | LoaderPreOperationFailureKind::ProviderRateLimited
        | LoaderPreOperationFailureKind::ProviderResponseTooLarge
        | LoaderPreOperationFailureKind::ProviderSchemaInvalid => StatusCode::BAD_GATEWAY,
    };
    (
        status,
        Json(serde_json::json!({
            "error": public_loader_pre_operation_error_message(copy_kind),
            "failure_kind": failure_kind,
        })),
    )
}

pub(crate) fn loader_install_error_progress(error: &LoaderInstallError) -> DownloadProgress {
    let progress = DownloadProgress {
        phase: "error".to_string(),
        current: 0,
        total: 0,
        file: None,
        error: Some(loader_install_error_message(error).to_string()),
        done: true,
        bytes_done: None,
        bytes_total: None,
    };
    if let LoaderInstallError::BaseInstallFailed(failure) = error {
        return install_progress_with_terminal_error(progress, failure.error());
    }
    progress
}

pub(crate) fn base_install_failed_progress() -> DownloadProgress {
    DownloadProgress {
        phase: "error".to_string(),
        current: 0,
        total: 0,
        file: None,
        error: Some(BASE_INSTALL_FAILED_MESSAGE.to_string()),
        done: true,
        bytes_done: None,
        bytes_total: None,
    }
}

pub(crate) fn loader_install_done_progress() -> DownloadProgress {
    DownloadProgress {
        phase: "done".to_string(),
        current: 1,
        total: 1,
        file: None,
        error: None,
        done: true,
        bytes_done: None,
        bytes_total: None,
    }
}

pub(crate) fn interrupted_loader_install_progress() -> DownloadProgress {
    DownloadProgress {
        phase: "error".to_string(),
        current: 0,
        total: 0,
        file: None,
        error: Some(LOADER_INSTALL_INTERRUPTED_MESSAGE.to_string()),
        done: true,
        bytes_done: None,
        bytes_total: None,
    }
}

fn public_loader_pre_operation_error_message(
    failure_kind: LoaderPreOperationFailureKind,
) -> &'static str {
    match failure_kind {
        LoaderPreOperationFailureKind::InvalidMinecraftVersion => "Invalid Minecraft version.",
        LoaderPreOperationFailureKind::InvalidBuildId => "Invalid loader build.",
        LoaderPreOperationFailureKind::CatalogUnavailable => {
            "Loader catalog is unavailable. Check your connection and try again."
        }
        LoaderPreOperationFailureKind::CatalogStale => {
            "Loader catalog needs a fresh provider check before this build can be installed."
        }
        LoaderPreOperationFailureKind::BuildNotFound => "Selected loader build is not available.",
        LoaderPreOperationFailureKind::ProviderHttpFailure
        | LoaderPreOperationFailureKind::ProviderNetworkFailure
        | LoaderPreOperationFailureKind::ProviderRateLimited => {
            "Loader provider is unavailable. Check your connection and try again."
        }
        LoaderPreOperationFailureKind::ProviderResponseTooLarge
        | LoaderPreOperationFailureKind::ProviderSchemaInvalid => {
            "Loader provider returned data Axial could not trust. Try again later."
        }
    }
}

fn loader_install_error_message(error: &LoaderInstallError) -> &'static str {
    match error {
        LoaderInstallError::PublicationIndeterminate(_) => {
            "Guardian is verifying loader install state."
        }
        LoaderInstallError::BaseInstallFailed(_) => {
            "Base game install failed. Retry the install from Downloads."
        }
        LoaderInstallError::ArtifactDownloadFailed(_) => {
            "Loader download failed. Check your connection and try again."
        }
        LoaderInstallError::Active(failure) => active_loader_install_error_message(failure),
    }
}

fn active_loader_install_error_message(failure: &LoaderActiveInstallFailure) -> &'static str {
    match failure.kind() {
        LoaderInstallFailureKind::ArtifactMissing => {
            "Loader artifact is unavailable. Try another build or component."
        }
        LoaderInstallFailureKind::InvalidProfile => "Loader profile is invalid. Try another build.",
        LoaderInstallFailureKind::ProviderHttpFailure
        | LoaderInstallFailureKind::ProviderNetworkFailure
        | LoaderInstallFailureKind::ProviderRateLimited => {
            "Loader provider is unavailable. Check your connection and try again."
        }
        LoaderInstallFailureKind::ProviderResponseTooLarge
        | LoaderInstallFailureKind::ProviderSchemaInvalid => {
            "Loader provider returned data Axial could not trust. Try again later."
        }
        LoaderInstallFailureKind::VerifyFailed => {
            "Loader install verification failed. Try again or choose another build."
        }
        LoaderInstallFailureKind::ParseFailed => {
            "Loader install data could not be read. Try again."
        }
        LoaderInstallFailureKind::ProcessorFailed => {
            "Loader installer processor failed. Retry or choose another build."
        }
        LoaderInstallFailureKind::InstallExecutionFailed
            if matches!(
                failure.source(),
                LoaderError::Io(error) if error.kind() == std::io::ErrorKind::PermissionDenied
            ) =>
        {
            "Could not write loader files. Check app data permissions and try again."
        }
        LoaderInstallFailureKind::InstallExecutionFailed => {
            "Loader installer could not complete. Restart Axial and try again."
        }
    }
}

#[cfg(test)]
mod managed_install_settlement_tests {
    #[tokio::test]
    async fn loader_journal_failure_retains_mutation_until_install_settles() {
        super::super::managed_install_settlement_tests::assert_journal_failure_retains_mutation(
            "loader",
        )
        .await;
    }
}
