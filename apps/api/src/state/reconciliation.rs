use super::contracts::{
    CommandKind, OperationId, OperationJournalEntry, OperationJournalStep, OperationOutcome,
    OperationPhase, OperationStatus, OperationStepResult, OwnershipClass, ReconciliationAttempt,
    ReconciliationComponent, ReconciliationIncarnationFingerprint,
    ReconciliationInventoryFingerprint, ReconciliationLineage, ReconciliationQuarantineCheckpoint,
    ReconciliationQuarantineRecord, ReconciliationRung, ReconciliationScope,
    ReconciliationTerminal, ReconciliationTerminalOutcome, RollbackState, StabilizationSystem,
    TargetDescriptor, TargetKind,
};
use super::failure_memory::{
    FailureMemoryActionOutcome, FailureMemoryKey, FailureMemoryStoreError,
    GuardianFailureMemoryEntry, GuardianFailureMemoryStore,
    ReconciliationAttemptReservation as StoreReconciliationAttemptReservation,
    ReconciliationAttemptReserveError,
};
use super::registered_artifact_findings::{
    RegisteredArtifactProvenance, RegisteredArtifactProvenanceContext,
    RegisteredArtifactRepairAuthorization, RegisteredArtifactRepairEffect,
    recorded_artifact_provenance_matches, registered_artifact_target,
    resolve_recorded_artifact_provenance,
};
use super::sessions::SharedComponentMutationLease;
use super::{
    AppState, InstanceLifecycleLease, KnownGoodVerificationLease, KnownGoodVerificationOwner,
    OperationJournalStore, OperationJournalStoreError,
};
use crate::execution::registered_artifact::{
    RegisteredArtifactExactProof, RegisteredArtifactExactVerification,
    RegisteredArtifactExactVerifier,
};
use crate::guardian::{
    DiagnosisId, GuardianActionKind, GuardianDecision, GuardianDomain, GuardianMode,
};
use axial_config::is_canonical_instance_id;
use axial_minecraft::known_good::{KnownGoodIntegrity, KnownGoodRoot, known_good_entry_path};
use axial_minecraft::runtime::{
    ManagedRuntimeCommitReceipt, ManagedRuntimeFailureReceipt, ManagedRuntimeQuarantineObligation,
    ManagedRuntimeQuarantineObservation, RuntimeId, is_known_runtime_component,
};
use axial_minecraft::{
    ManagedAssetsCommitReceipt, ManagedAssetsRollbackEffect, ManagedAssetsRollbackReceipt,
    ManagedLibrariesCommitReceipt, ManagedLibrariesRollbackEffect, ManagedLibrariesRollbackReceipt,
    ManagedRuntimeCache, ManagedVersionBundleCommitReceipt, ManagedVersionBundleRollbackReceipt,
    ManagedWholeInstanceCommitReceipt, ManagedWholeInstanceRollbackReceipt,
};
use sha2::{Digest, Sha256};
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

const RECONCILIATION_FINGERPRINT_DOMAIN: &[u8] = b"axial.guardian.reconciliation.incarnation.v1";
const RECONCILIATION_INVENTORY_FINGERPRINT_DOMAIN: &[u8] =
    b"axial.guardian.reconciliation.inventory.v1";
const RECONCILIATION_INVENTORY_ENTRY_DOMAIN: &[u8] =
    b"axial.guardian.reconciliation.inventory-entry.v1";
pub(crate) const REGISTERED_ARTIFACT_COMPONENT_REBUILD_FAILURE_POINT: &str =
    "require_registered_artifact_component_rebuild";
pub(crate) const COMPONENT_REBUILD_START_STEP: &str = "journal_component_rebuild_start";
pub(crate) const COMPONENT_QUARANTINE_STEP: &str = "quarantine_launcher_managed_target";
pub(crate) const RUNTIME_COMPONENT_REBUILD_STEP: &str = "rebuild_managed_runtime_component";
pub(crate) const VERSION_BUNDLE_COMPONENT_REBUILD_STEP: &str =
    "rebuild_managed_version_bundle_component";
pub(crate) const LIBRARIES_COMPONENT_REBUILD_STEP: &str = "rebuild_managed_libraries_component";
pub(crate) const ASSETS_COMPONENT_REBUILD_STEP: &str = "rebuild_managed_assets_component";
const WHOLE_INSTANCE_REMATERIALIZATION_START_STEP: &str =
    "journal_whole_instance_rematerialization_start";
const WHOLE_INSTANCE_REMATERIALIZATION_STEP: &str = "rematerialize_launcher_managed_instance";
const WHOLE_INSTANCE_PERSISTENCE_RETRY_INITIAL_DELAY: Duration = Duration::from_millis(25);
const WHOLE_INSTANCE_PERSISTENCE_RETRY_MAX_DELAY: Duration = Duration::from_secs(1);

pub(crate) struct RecordedRuntimeArtifactRepairFailure {
    evidence: RecordedReconciliationFailure,
}

#[must_use]
pub(crate) struct RegisteredArtifactFailedRepair {
    evidence: RecordedReconciliationFailure,
    verification: KnownGoodVerificationLease,
}

#[must_use]
pub(crate) enum RegisteredArtifactRecoveryEntry {
    Fresh(RegisteredArtifactRepairAuthorization),
    Resume(RegisteredArtifactFailedRepair),
}

pub(crate) struct RegisteredComponentRebuildAdmission {
    authority: RegisteredReconciliationAuthority,
    attempt: ReconciliationAttempt,
    failed_terminal: ReconciliationTerminal,
    known_good: RegisteredKnownGoodInventory,
    artifact_provenance: Option<RegisteredArtifactProvenance>,
    component_state: RegisteredComponentRebuildState,
    _component_mutation: SharedComponentMutationLease,
    _config_mutation: tokio::sync::OwnedMutexGuard<()>,
}

#[must_use]
pub(crate) struct RegisteredWholeInstanceRematerializationEligibility {
    instance_id: String,
    predecessor: ReconciliationTerminal,
}

#[must_use]
pub(crate) struct RegisteredWholeInstanceRematerializationAuthorization {
    instance_id: String,
    predecessor: ReconciliationTerminal,
    authorized_policy_mode: GuardianMode,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RegisteredWholeInstanceRematerializationAuthorizationRejection {
    InvalidDecision,
    InvalidActionPlan,
}

impl RegisteredWholeInstanceRematerializationEligibility {
    pub(crate) fn operation_id(&self) -> &OperationId {
        self.predecessor.operation_id()
    }

    pub(crate) const fn domain(&self) -> GuardianDomain {
        self.predecessor.domain()
    }

    pub(crate) fn target(&self) -> TargetDescriptor {
        reconciliation_instance_target(&self.instance_id)
    }

    pub(crate) fn authorize_decision(
        self,
        decision: &GuardianDecision,
    ) -> Result<
        RegisteredWholeInstanceRematerializationAuthorization,
        RegisteredWholeInstanceRematerializationAuthorizationRejection,
    > {
        let target = self.target();
        if decision.operation_id() != Some(self.predecessor.operation_id())
            || !matches!(
                decision.mode(),
                GuardianMode::Managed | GuardianMode::Custom
            )
            || decision.kind() != GuardianActionKind::AskUser
            || decision.diagnoses() != [DiagnosisId::LauncherManagedArtifactCorrupt]
            || self.predecessor.diagnosis_id() != DiagnosisId::LauncherManagedArtifactCorrupt
            || self.predecessor.attempt().ownership() != OwnershipClass::LauncherManaged
            || !matches!(
                self.predecessor.mode(),
                GuardianMode::Managed | GuardianMode::Custom
            )
        {
            return Err(
                RegisteredWholeInstanceRematerializationAuthorizationRejection::InvalidDecision,
            );
        }
        let plan = decision.action_plan().ok_or(
            RegisteredWholeInstanceRematerializationAuthorizationRejection::InvalidActionPlan,
        )?;
        if plan.owner != StabilizationSystem::Guardian
            || plan.prerequisite.diagnosis_id != DiagnosisId::LauncherManagedArtifactCorrupt
            || plan.prerequisite.ownership != OwnershipClass::LauncherManaged
            || plan.prerequisite.affected_targets != [target.clone()]
            || plan.prerequisite.candidate_actions
                != [GuardianActionKind::AskUser, GuardianActionKind::Block]
            || !matches!(
                plan.actions.as_slice(),
                [action]
                    if action.kind == GuardianActionKind::AskUser
                        && action.reason == DiagnosisId::LauncherManagedArtifactCorrupt
                        && action.target.as_ref() == Some(&target)
            )
        {
            return Err(
                RegisteredWholeInstanceRematerializationAuthorizationRejection::InvalidActionPlan,
            );
        }
        Ok(RegisteredWholeInstanceRematerializationAuthorization {
            instance_id: self.instance_id,
            predecessor: self.predecessor,
            authorized_policy_mode: decision.mode(),
        })
    }
}

pub(crate) struct RegisteredWholeInstanceRematerializationAdmission {
    state: AppState,
    attempt: ReconciliationAttempt,
    predecessor: ReconciliationTerminal,
    known_good: RegisteredKnownGoodInventory,
    managed_artifact_epoch: std::sync::Arc<std::sync::atomic::AtomicU64>,
    lifecycle: InstanceLifecycleLease,
    component_mutation: SharedComponentMutationLease,
    config_mutation: tokio::sync::OwnedMutexGuard<()>,
}

#[cfg(test)]
pub(crate) struct ReconciliationHandCoverage {
    pub(crate) admission_type: &'static str,
    pub(crate) rung: ReconciliationRung,
    pub(crate) max_attempts_per_suppression_window: usize,
}

#[cfg(test)]
pub(crate) fn reconciliation_hand_coverage() -> Vec<ReconciliationHandCoverage> {
    ReconciliationRung::ALL
        .iter()
        .copied()
        .map(|rung| match rung {
            ReconciliationRung::RepairArtifact => reconciliation_hand::<
                super::registered_artifact_findings::RegisteredArtifactRepairAdmission,
            >(rung),
            ReconciliationRung::RebuildComponent => {
                reconciliation_hand::<RegisteredComponentRebuildAdmission>(rung)
            }
            ReconciliationRung::RematerializeInstance => {
                reconciliation_hand::<RegisteredWholeInstanceRematerializationAdmission>(rung)
            }
        })
        .collect()
}

#[cfg(test)]
fn reconciliation_hand<Admission>(rung: ReconciliationRung) -> ReconciliationHandCoverage {
    ReconciliationHandCoverage {
        admission_type: std::any::type_name::<Admission>()
            .rsplit("::")
            .next()
            .expect("admission type name"),
        rung,
        max_attempts_per_suppression_window: rung.max_attempts_per_suppression_window(),
    }
}

pub(crate) enum RegisteredWholeInstancePreparation {
    Admitted {
        request: RegisteredWholeInstanceCoreRequest,
        completion: Box<RegisteredWholeInstanceCompletion>,
    },
    Closed(Box<RegisteredWholeInstanceDurableOutcome>),
}

pub(crate) enum RegisteredWholeInstancePreparationError {
    Reservation(ReconciliationAttemptRejection),
    Journal(OperationJournalStoreError),
    Memory(FailureMemoryStoreError),
    Settlement(RegisteredWholeInstanceSettlementError),
}

pub(crate) struct RegisteredWholeInstanceCoreRequest {
    library_root: PathBuf,
    runtime_cache: ManagedRuntimeCache,
    version_id: String,
}

pub(crate) struct RegisteredWholeInstanceCompletion {
    durable: WholeInstanceDurableAuthority,
    known_good: RegisteredKnownGoodInventory,
    runtime_cache: ManagedRuntimeCache,
    managed_artifact_epoch: std::sync::Arc<std::sync::atomic::AtomicU64>,
    user_config_snapshot: super::user_config_snapshots::UserConfigSnapshotReceipt,
    _mutation: super::ManagedArtifactMutationAdmission,
}

pub(crate) struct RegisteredWholeInstanceSettlement {
    durable: WholeInstanceDurableAuthority,
    terminal: ReconciliationTerminal,
    terminal_step: &'static str,
    _publication: Option<WholeInstancePublicationAuthority>,
    user_config_snapshot: Option<super::user_config_snapshots::UserConfigSnapshotReceipt>,
}

pub(crate) struct RegisteredWholeInstanceDurableOutcome {
    terminal: ReconciliationTerminal,
    user_config_restore: Option<RegisteredUserConfigRestoreEligibility>,
}

#[must_use]
pub(crate) struct RegisteredUserConfigRestoreEligibility {
    _snapshot: super::user_config_snapshots::UserConfigSnapshotReceipt,
}

pub(crate) enum RegisteredWholeInstanceSettlementError {
    Journal(OperationJournalStoreError),
    Memory(FailureMemoryStoreError),
    Evidence(ReconciliationEvidenceRejection),
}

impl RegisteredWholeInstancePreparationError {
    pub(crate) fn class(&self) -> &'static str {
        match self {
            Self::Reservation(ReconciliationAttemptRejection::PersistencePending) => {
                "persistence_pending"
            }
            Self::Reservation(ReconciliationAttemptRejection::AlreadyReserved) => {
                "already_reserved"
            }
            Self::Reservation(ReconciliationAttemptRejection::CapacityExhausted) => {
                "capacity_exhausted"
            }
            Self::Reservation(ReconciliationAttemptRejection::AmbiguousPriorAttempt) => {
                "ambiguous_prior_attempt"
            }
            Self::Journal(error) => error.class(),
            Self::Memory(error) => error.class(),
            Self::Settlement(error) => error.class(),
        }
    }
}

impl RegisteredWholeInstanceSettlementError {
    pub(crate) fn class(&self) -> &'static str {
        match self {
            Self::Journal(error) => error.class(),
            Self::Memory(error) => error.class(),
            Self::Evidence(error) => error.class(),
        }
    }
}

struct WholeInstanceDurableAuthority {
    state: AppState,
    attempt: ReconciliationAttempt,
    lifecycle: InstanceLifecycleLease,
    reservation: ReconciliationAttemptReservation,
    _component_mutation: SharedComponentMutationLease,
    _config_mutation: tokio::sync::OwnedMutexGuard<()>,
}

enum WholeInstancePublicationAuthority {
    Commit(ManagedWholeInstanceCommitReceipt),
    Rollback(ManagedWholeInstanceRollbackReceipt),
}

enum RegisteredComponentRebuildState {
    Runtime {
        runtime_cache: ManagedRuntimeCache,
        postcondition_failure_inventory:
            std::sync::OnceLock<std::sync::Arc<axial_minecraft::known_good::KnownGoodInventory>>,
    },
    VersionBundle,
    Libraries,
    Assets,
}

pub(crate) struct RegisteredVersionBundleComponentRebuildEffect {
    library_root: PathBuf,
    version_id: String,
}

pub(crate) struct RegisteredLibrariesComponentRebuildEffect {
    library_root: PathBuf,
    version_id: String,
}

pub(crate) struct RegisteredAssetsComponentRebuildEffect {
    library_root: PathBuf,
    version_id: String,
}

struct ManagedArtifactCoreRequest {
    library_root: PathBuf,
    version_id: String,
}

pub(crate) enum RegisteredManagedArtifactComponentEffectAdmission<Request> {
    Admitted {
        request: Request,
        completion: Box<RegisteredManagedArtifactComponentCompletion>,
    },
    Refused(Box<RegisteredManagedArtifactComponentSettlement>),
}

pub(crate) struct RegisteredManagedArtifactComponentCompletion {
    authority: ManagedArtifactCompletionAuthority,
}

pub(crate) enum RegisteredManagedArtifactCommitPostcheck {
    Verify {
        pending: RegisteredManagedArtifactPendingPostcheck,
        verifier: RegisteredArtifactExactVerifier,
    },
    Failed(RegisteredManagedArtifactComponentSettlement),
}

pub(crate) struct RegisteredManagedArtifactPendingPostcheck {
    authority: ManagedArtifactCompletionAuthority,
    verification: RegisteredArtifactExactVerification,
    publication: ManagedArtifactPublicationLease,
}

pub(crate) struct RegisteredManagedArtifactComponentSettlement {
    durable: ManagedArtifactDurableAuthority,
    terminal: ReconciliationTerminal,
    _lifecycle: Option<InstanceLifecycleLease>,
    _publication: Option<ManagedArtifactPublicationLease>,
    _proof: Option<RegisteredArtifactExactProof>,
}

struct ManagedArtifactCompletionAuthority {
    durable: ManagedArtifactDurableAuthority,
    owner: KnownGoodVerificationOwner,
    known_good: RegisteredKnownGoodInventory,
    runtime_cache: ManagedRuntimeCache,
    provenance: RegisteredArtifactProvenance,
    component: ManagedArtifactRebuildComponent,
    managed_artifact_epoch: Option<std::sync::Arc<std::sync::atomic::AtomicU64>>,
}

struct ManagedArtifactDurableAuthority {
    state: AppState,
    attempt: ReconciliationAttempt,
    failed_terminal: ReconciliationTerminal,
    _component_mutation: SharedComponentMutationLease,
    _config_mutation: tokio::sync::OwnedMutexGuard<()>,
}

enum ManagedArtifactPublicationLease {
    VersionBundleCommit {
        _receipt: ManagedVersionBundleCommitReceipt,
    },
    VersionBundleRollback {
        _receipt: ManagedVersionBundleRollbackReceipt,
    },
    LibrariesCommit(ManagedLibrariesCommitReceipt),
    LibrariesRollback(ManagedLibrariesRollbackReceipt),
    AssetsCommit(ManagedAssetsCommitReceipt),
    AssetsRollback(ManagedAssetsRollbackReceipt),
}

impl RegisteredWholeInstanceRematerializationAdmission {
    #[cfg(test)]
    pub(crate) fn attempt(&self) -> &ReconciliationAttempt {
        &self.attempt
    }

    pub(crate) async fn into_effect(
        self,
    ) -> Result<RegisteredWholeInstancePreparation, RegisteredWholeInstancePreparationError> {
        let reservation = reserve_whole_instance_attempt(
            &self.state.failure_memory,
            &self.state.journals,
            reconciliation_attempt_key(&self.attempt),
        )
        .await?;
        let durable = WholeInstanceDurableAuthority {
            state: self.state,
            attempt: self.attempt,
            lifecycle: self.lifecycle,
            reservation,
            _component_mutation: self.component_mutation,
            _config_mutation: self.config_mutation,
        };
        let expected_plan = whole_instance_rematerialization_journal_for_attempt(&durable.attempt);
        persist_whole_instance_plan(&durable.state.journals, expected_plan)
            .await
            .map_err(RegisteredWholeInstancePreparationError::Journal)?;
        let observed_at = chrono::Utc::now().fixed_offset();
        let current = durable
            .state
            .recorded_reconciliation_failure_at(
                &durable.lifecycle,
                self.predecessor.operation_id(),
                ReconciliationRung::RebuildComponent,
                observed_at,
                None,
            )
            .ok();
        let live = active_reconciliation_attempt_at(&durable.attempt, observed_at).unwrap_or(false)
            && current.is_some_and(|current| {
                current.terminal == self.predecessor
                    && current.roots.library == self.known_good.library_root
                    && std::sync::Arc::ptr_eq(&current.inventory, &self.known_good.inventory)
                    && adjacent_whole_instance_attempts_match(
                        current.terminal.attempt(),
                        &durable.attempt,
                    )
            });
        if !live {
            let outcome = durable
                .failed(
                    ReconciliationQuarantineCheckpoint::default(),
                    None,
                    None,
                    WHOLE_INSTANCE_REMATERIALIZATION_START_STEP,
                )
                .settle()
                .await
                .map_err(RegisteredWholeInstancePreparationError::Settlement)?;
            return Ok(RegisteredWholeInstancePreparation::Closed(Box::new(
                outcome,
            )));
        }
        let (instance_id, fingerprint, inventory_fingerprint) = match durable.attempt.scope() {
            ReconciliationScope::RegisteredInstance {
                instance_id,
                fingerprint,
                inventory_fingerprint,
                ..
            } => (
                instance_id.clone(),
                fingerprint.clone(),
                inventory_fingerprint.clone(),
            ),
        };
        let capture = match crate::execution::user_owned_state::capture_user_config(
            durable.state.instances.game_dir(&instance_id),
        )
        .await
        {
            Ok(capture) => capture,
            Err(_) => {
                let outcome = durable
                    .failed(
                        ReconciliationQuarantineCheckpoint::default(),
                        None,
                        None,
                        WHOLE_INSTANCE_REMATERIALIZATION_START_STEP,
                    )
                    .settle()
                    .await
                    .map_err(RegisteredWholeInstancePreparationError::Settlement)?;
                return Ok(RegisteredWholeInstancePreparation::Closed(Box::new(
                    outcome,
                )));
            }
        };
        let user_config_snapshot = match durable
            .state
            .user_config_snapshots
            .persist_before_core(
                super::user_config_snapshots::UserConfigSnapshotIdentity {
                    instance_id,
                    instance_created_at: self.known_good.created_at.clone(),
                    incarnation_fingerprint: fingerprint,
                    inventory_fingerprint,
                    r3_operation_id: durable.attempt.operation_id().clone(),
                    captured_at: chrono::Utc::now().fixed_offset(),
                },
                capture,
            )
            .await
        {
            Ok(snapshot) => snapshot,
            Err(_) => {
                let outcome = durable
                    .failed(
                        ReconciliationQuarantineCheckpoint::default(),
                        None,
                        None,
                        WHOLE_INSTANCE_REMATERIALIZATION_START_STEP,
                    )
                    .settle()
                    .await
                    .map_err(RegisteredWholeInstancePreparationError::Settlement)?;
                return Ok(RegisteredWholeInstancePreparation::Closed(Box::new(
                    outcome,
                )));
            }
        };
        let request = RegisteredWholeInstanceCoreRequest {
            library_root: self.known_good.library_root.clone(),
            runtime_cache: durable.state.managed_runtime_cache.clone(),
            version_id: self.known_good.version_id.clone(),
        };
        let mutation = match durable
            .state
            .managed_artifact_epoch
            .admit_from_expected(&self.managed_artifact_epoch)
        {
            Ok(mutation) => mutation,
            Err(_) => {
                let outcome = durable
                    .failed(
                        ReconciliationQuarantineCheckpoint::default(),
                        None,
                        Some(user_config_snapshot),
                        WHOLE_INSTANCE_REMATERIALIZATION_STEP,
                    )
                    .settle()
                    .await
                    .map_err(RegisteredWholeInstancePreparationError::Settlement)?;
                return Ok(RegisteredWholeInstancePreparation::Closed(Box::new(
                    outcome,
                )));
            }
        };
        let runtime_cache = request.runtime_cache.clone();
        Ok(RegisteredWholeInstancePreparation::Admitted {
            request,
            completion: Box::new(RegisteredWholeInstanceCompletion {
                durable,
                known_good: self.known_good,
                runtime_cache,
                managed_artifact_epoch: self.managed_artifact_epoch,
                user_config_snapshot,
                _mutation: mutation,
            }),
        })
    }
}

impl RegisteredWholeInstanceCoreRequest {
    pub(crate) fn core_request(&self) -> (&Path, &ManagedRuntimeCache, &str) {
        (&self.library_root, &self.runtime_cache, &self.version_id)
    }
}

impl RegisteredWholeInstanceCompletion {
    pub(crate) fn into_failed_settlement(self) -> RegisteredWholeInstanceSettlement {
        self.durable.failed(
            ReconciliationQuarantineCheckpoint::default(),
            None,
            Some(self.user_config_snapshot),
            WHOLE_INSTANCE_REMATERIALIZATION_STEP,
        )
    }

    pub(crate) async fn settle_commit(
        self,
        receipt: ManagedWholeInstanceCommitReceipt,
    ) -> RegisteredWholeInstanceSettlement {
        let publication = WholeInstancePublicationAuthority::Commit(receipt);
        let receipt_valid = match &publication {
            WholeInstancePublicationAuthority::Commit(receipt) => {
                receipt.version_id() == self.known_good.version_id
                    && receipt.matches_root(&self.known_good.library_root).await
                    && receipt.matches_runtime_cache(&self.runtime_cache)
                    && receipt.matches_known_good_inventory(&self.known_good.inventory)
                    && receipt.revalidate(&self.runtime_cache).await
                    && self.is_current()
            }
            WholeInstancePublicationAuthority::Rollback(_) => unreachable!(),
        };
        if receipt_valid {
            self.durable
                .succeeded(publication, self.user_config_snapshot)
        } else {
            self.durable.failed(
                ReconciliationQuarantineCheckpoint::default(),
                Some(publication),
                Some(self.user_config_snapshot),
                WHOLE_INSTANCE_REMATERIALIZATION_STEP,
            )
        }
    }

    pub(crate) async fn settle_rollback(
        self,
        receipt: ManagedWholeInstanceRollbackReceipt,
    ) -> RegisteredWholeInstanceSettlement {
        let publication = WholeInstancePublicationAuthority::Rollback(receipt);
        let checkpoint = match &publication {
            WholeInstancePublicationAuthority::Rollback(receipt) => {
                let valid = receipt.version_id() == self.known_good.version_id
                    && receipt.matches_root(&self.known_good.library_root).await
                    && receipt.matches_runtime_cache(&self.runtime_cache)
                    && receipt.matches_known_good_inventory(&self.known_good.inventory)
                    && self.is_current();
                let checkpoint = valid
                    .then(|| {
                        validated_runtime_quarantine_checkpoint(
                            &self.runtime_cache,
                            receipt.runtime_component(),
                            receipt.runtime_quarantine_obligation(),
                        )
                    })
                    .transpose()
                    .ok()
                    .flatten();
                checkpoint.unwrap_or_default()
            }
            WholeInstancePublicationAuthority::Commit(_) => unreachable!(),
        };
        self.durable.failed(
            checkpoint,
            Some(publication),
            Some(self.user_config_snapshot),
            WHOLE_INSTANCE_REMATERIALIZATION_STEP,
        )
    }

    fn is_current(&self) -> bool {
        let ReconciliationScope::RegisteredInstance {
            instance_id,
            fingerprint,
            inventory_fingerprint,
        } = self.durable.attempt.scope();
        self.durable.lifecycle.matches(instance_id)
            && self
                .durable
                .state
                .managed_artifact_mutation_epoch_is_current(Some(&self.managed_artifact_epoch))
            && self
                .durable
                .state
                .current_reconciliation_incarnation(instance_id)
                .is_ok_and(|current| {
                    fingerprint == &current.fingerprint
                        && inventory_fingerprint == &current.inventory_fingerprint
                        && current.roots.library == self.known_good.library_root
                        && current.roots.runtime == self.runtime_cache.root()
                        && std::sync::Arc::ptr_eq(&current.inventory, &self.known_good.inventory)
                })
    }
}

impl WholeInstanceDurableAuthority {
    fn succeeded(
        self,
        publication: WholeInstancePublicationAuthority,
        user_config_snapshot: super::user_config_snapshots::UserConfigSnapshotReceipt,
    ) -> RegisteredWholeInstanceSettlement {
        let terminal = ReconciliationTerminal::from_attempt(
            self.attempt.clone(),
            ReconciliationTerminalOutcome::Succeeded,
            ReconciliationQuarantineCheckpoint::default(),
        );
        RegisteredWholeInstanceSettlement {
            durable: self,
            terminal,
            terminal_step: WHOLE_INSTANCE_REMATERIALIZATION_STEP,
            _publication: Some(publication),
            user_config_snapshot: Some(user_config_snapshot),
        }
    }

    fn failed(
        self,
        checkpoint: ReconciliationQuarantineCheckpoint,
        publication: Option<WholeInstancePublicationAuthority>,
        user_config_snapshot: Option<super::user_config_snapshots::UserConfigSnapshotReceipt>,
        terminal_step: &'static str,
    ) -> RegisteredWholeInstanceSettlement {
        let terminal = ReconciliationTerminal::from_attempt(
            self.attempt.clone(),
            ReconciliationTerminalOutcome::Failed,
            checkpoint,
        );
        RegisteredWholeInstanceSettlement {
            durable: self,
            terminal,
            terminal_step,
            _publication: publication,
            user_config_snapshot,
        }
    }
}

impl RegisteredWholeInstanceSettlement {
    pub(crate) async fn settle(
        mut self,
    ) -> Result<RegisteredWholeInstanceDurableOutcome, RegisteredWholeInstanceSettlementError> {
        let operation_id = self.durable.attempt.operation_id().clone();
        if self.terminal.outcome() != ReconciliationTerminalOutcome::Succeeded
            && let Some(snapshot) = self.user_config_snapshot.take()
        {
            drop(snapshot);
            let _ = self
                .durable
                .state
                .user_config_snapshots
                .reconcile_pending_removals()
                .await;
        }
        if !self.terminal.quarantine_checkpoint().is_empty() {
            let mut checkpoint =
                OperationJournalStep::new(COMPONENT_QUARANTINE_STEP, OperationPhase::Repairing);
            checkpoint.result = OperationStepResult::Completed;
            checkpoint.changed_target = Some(self.durable.attempt.target().clone());
            checkpoint.rollback = RollbackState::Available;
            persist_whole_instance_checkpoint(
                &self.durable.state.journals,
                &operation_id,
                checkpoint,
            )
            .await
            .map_err(RegisteredWholeInstanceSettlementError::Journal)?;
        }
        let mut terminal_step =
            OperationJournalStep::new(self.terminal_step, OperationPhase::Repairing);
        terminal_step.changed_target = Some(self.durable.attempt.target().clone());
        terminal_step.rollback =
            if self.terminal_step == WHOLE_INSTANCE_REMATERIALIZATION_START_STEP {
                RollbackState::NotApplicable
            } else {
                RollbackState::Available
            };
        persist_whole_instance_terminal(
            &self.durable.state.journals,
            &operation_id,
            terminal_step,
            self.terminal_step,
            self.terminal.clone(),
        )
        .await
        .map_err(RegisteredWholeInstanceSettlementError::Journal)?;
        let memory = reconciliation_memory_entry(self.terminal.clone())
            .map_err(RegisteredWholeInstanceSettlementError::Evidence)?;
        persist_whole_instance_memory(
            &self.durable.state.failure_memory,
            memory,
            &self.durable.reservation,
        )
        .await
        .map_err(RegisteredWholeInstanceSettlementError::Memory)?;
        let user_config_restore =
            self.user_config_snapshot
                .map(|snapshot| RegisteredUserConfigRestoreEligibility {
                    _snapshot: snapshot.confirm(),
                });
        Ok(RegisteredWholeInstanceDurableOutcome {
            terminal: self.terminal,
            user_config_restore,
        })
    }
}

impl RegisteredWholeInstanceDurableOutcome {
    #[cfg(test)]
    pub(crate) fn terminal(&self) -> &ReconciliationTerminal {
        &self.terminal
    }

    pub(crate) fn succeeded(&self) -> bool {
        self.terminal.outcome() == ReconciliationTerminalOutcome::Succeeded
    }

    pub(crate) fn into_restore_eligibility(self) -> Option<RegisteredUserConfigRestoreEligibility> {
        self.user_config_restore
    }
}

async fn reserve_whole_instance_attempt(
    failure_memory: &GuardianFailureMemoryStore,
    journals: &OperationJournalStore,
    key: FailureMemoryKey,
) -> Result<ReconciliationAttemptReservation, RegisteredWholeInstancePreparationError> {
    let mut delay = WHOLE_INSTANCE_PERSISTENCE_RETRY_INITIAL_DELAY;
    loop {
        settle_reconciliation_pending_with_backoff(failure_memory)
            .await
            .map_err(RegisteredWholeInstancePreparationError::Memory)?;
        match reserve_reconciliation_attempt(failure_memory, journals, key.clone()) {
            Ok(reservation) => return Ok(reservation),
            Err(ReconciliationAttemptRejection::PersistencePending) => {
                tokio::time::sleep(delay).await;
                delay = delay
                    .saturating_mul(2)
                    .min(WHOLE_INSTANCE_PERSISTENCE_RETRY_MAX_DELAY);
            }
            Err(error) => return Err(RegisteredWholeInstancePreparationError::Reservation(error)),
        }
    }
}

async fn persist_whole_instance_plan(
    journals: &OperationJournalStore,
    expected: OperationJournalEntry,
) -> Result<(), OperationJournalStoreError> {
    let operation_id = expected.operation_id.clone();
    let mut delay = WHOLE_INSTANCE_PERSISTENCE_RETRY_INITIAL_DELAY;
    loop {
        match journals.create(expected.clone()).await {
            Ok(()) => return Ok(()),
            Err(OperationJournalStoreError::AlreadyExists)
                if journals.get(&operation_id).is_some_and(|entry| {
                    super::operation_journal_plan_is_visible(&entry, &expected)
                }) =>
            {
                return Ok(());
            }
            Err(
                error @ (OperationJournalStoreError::Persistence(_)
                | OperationJournalStoreError::RetryRequired),
            ) => {
                match journals
                    .reconcile_transition(
                        &operation_id,
                        error,
                        delay,
                        WHOLE_INSTANCE_PERSISTENCE_RETRY_MAX_DELAY,
                        |entry| super::operation_journal_plan_is_visible(entry, &expected),
                    )
                    .await
                {
                    Ok(_)
                        if journals.get(&operation_id).is_some_and(|entry| {
                            super::operation_journal_plan_is_visible(&entry, &expected)
                        }) =>
                    {
                        return Ok(());
                    }
                    Ok(_) | Err(OperationJournalStoreError::Persistence(_)) => {}
                    Err(error) => return Err(error),
                }
            }
            Err(error) => return Err(error),
        }
        tokio::time::sleep(delay).await;
        delay = delay
            .saturating_mul(2)
            .min(WHOLE_INSTANCE_PERSISTENCE_RETRY_MAX_DELAY);
    }
}

async fn persist_whole_instance_checkpoint(
    journals: &OperationJournalStore,
    operation_id: &OperationId,
    checkpoint: OperationJournalStep,
) -> Result<(), OperationJournalStoreError> {
    let mut delay = WHOLE_INSTANCE_PERSISTENCE_RETRY_INITIAL_DELAY;
    loop {
        if journals.get(operation_id).is_some_and(|entry| {
            super::operation_journal_completed_step_is_visible(&entry, &checkpoint)
        }) {
            return Ok(());
        }
        match journals
            .record_checkpoint(operation_id, checkpoint.clone())
            .await
        {
            Ok(()) => return Ok(()),
            Err(
                error @ (OperationJournalStoreError::Persistence(_)
                | OperationJournalStoreError::RetryRequired),
            ) => {
                match journals
                    .reconcile_transition(
                        operation_id,
                        error,
                        delay,
                        WHOLE_INSTANCE_PERSISTENCE_RETRY_MAX_DELAY,
                        |entry| {
                            super::operation_journal_completed_step_is_visible(entry, &checkpoint)
                        },
                    )
                    .await
                {
                    Ok(_)
                        if journals.get(operation_id).is_some_and(|entry| {
                            super::operation_journal_completed_step_is_visible(&entry, &checkpoint)
                        }) =>
                    {
                        return Ok(());
                    }
                    Ok(_) | Err(OperationJournalStoreError::Persistence(_)) => {}
                    Err(error) => return Err(error),
                }
            }
            Err(error) => return Err(error),
        }
        tokio::time::sleep(delay).await;
        delay = delay
            .saturating_mul(2)
            .min(WHOLE_INSTANCE_PERSISTENCE_RETRY_MAX_DELAY);
    }
}

async fn persist_whole_instance_terminal(
    journals: &OperationJournalStore,
    operation_id: &OperationId,
    mut terminal_step: OperationJournalStep,
    failure_point: &'static str,
    terminal: ReconciliationTerminal,
) -> Result<(), OperationJournalStoreError> {
    let mut delay = WHOLE_INSTANCE_PERSISTENCE_RETRY_INITIAL_DELAY;
    loop {
        if journals
            .get(operation_id)
            .is_some_and(|entry| whole_instance_terminal_matches(&entry, &terminal))
        {
            return Ok(());
        }
        let result = match terminal.outcome() {
            ReconciliationTerminalOutcome::Succeeded => {
                terminal_step.result = OperationStepResult::Completed;
                record_reconciliation_journal_success(
                    journals,
                    operation_id,
                    terminal_step.clone(),
                    terminal.clone(),
                )
                .await
            }
            ReconciliationTerminalOutcome::Failed => {
                terminal_step.result = OperationStepResult::Failed;
                record_reconciliation_journal_failure(
                    journals,
                    operation_id,
                    terminal_step.clone(),
                    failure_point,
                    terminal.clone(),
                )
                .await
            }
        };
        match result {
            Ok(()) => return Ok(()),
            Err(
                error @ (OperationJournalStoreError::Persistence(_)
                | OperationJournalStoreError::RetryRequired),
            ) => {
                match journals
                    .reconcile_transition(
                        operation_id,
                        error,
                        delay,
                        WHOLE_INSTANCE_PERSISTENCE_RETRY_MAX_DELAY,
                        |entry| whole_instance_terminal_matches(entry, &terminal),
                    )
                    .await
                {
                    Ok(_)
                        if journals.get(operation_id).is_some_and(|entry| {
                            whole_instance_terminal_matches(&entry, &terminal)
                        }) =>
                    {
                        return Ok(());
                    }
                    Ok(_) | Err(OperationJournalStoreError::Persistence(_)) => {}
                    Err(error) => return Err(error),
                }
            }
            Err(error) => return Err(error),
        }
        tokio::time::sleep(delay).await;
        delay = delay
            .saturating_mul(2)
            .min(WHOLE_INSTANCE_PERSISTENCE_RETRY_MAX_DELAY);
    }
}

async fn persist_whole_instance_memory(
    failure_memory: &GuardianFailureMemoryStore,
    memory: GuardianFailureMemoryEntry,
    reservation: &ReconciliationAttemptReservation,
) -> Result<(), FailureMemoryStoreError> {
    let mut delay = WHOLE_INSTANCE_PERSISTENCE_RETRY_INITIAL_DELAY;
    loop {
        if failure_memory
            .get(&memory.key)
            .as_ref()
            .is_some_and(|stored| stored == &memory)
        {
            return Ok(());
        }
        match commit_reconciliation_memory(failure_memory, memory.clone(), reservation).await {
            Ok(()) => return Ok(()),
            Err(FailureMemoryStoreError::Persistence(_)) => {
                tokio::time::sleep(delay).await;
                delay = delay
                    .saturating_mul(2)
                    .min(WHOLE_INSTANCE_PERSISTENCE_RETRY_MAX_DELAY);
                settle_reconciliation_pending_with_backoff(failure_memory).await?;
            }
            Err(error) => return Err(error),
        }
    }
}

async fn settle_reconciliation_pending_with_backoff(
    failure_memory: &GuardianFailureMemoryStore,
) -> Result<(), FailureMemoryStoreError> {
    let mut delay = WHOLE_INSTANCE_PERSISTENCE_RETRY_INITIAL_DELAY;
    loop {
        match failure_memory.settle_reconciliation_pending().await {
            Ok(()) => return Ok(()),
            Err(FailureMemoryStoreError::Persistence(_)) => {
                tokio::time::sleep(delay).await;
                delay = delay
                    .saturating_mul(2)
                    .min(WHOLE_INSTANCE_PERSISTENCE_RETRY_MAX_DELAY);
            }
            Err(error) => return Err(error),
        }
    }
}

impl RegisteredVersionBundleComponentRebuildEffect {
    pub(crate) fn core_request(&self) -> (&Path, &str) {
        (&self.library_root, &self.version_id)
    }
}

impl RegisteredLibrariesComponentRebuildEffect {
    pub(crate) fn core_request(&self) -> (&Path, &str) {
        (&self.library_root, &self.version_id)
    }
}

impl RegisteredAssetsComponentRebuildEffect {
    pub(crate) fn core_request(&self) -> (&Path, &str) {
        (&self.library_root, &self.version_id)
    }
}

impl RegisteredManagedArtifactComponentCompletion {
    pub(crate) fn into_failed_settlement(self) -> RegisteredManagedArtifactComponentSettlement {
        self.authority.durable.failed(None)
    }

    pub(crate) async fn begin_libraries_commit(
        self,
        receipt: ManagedLibrariesCommitReceipt,
    ) -> RegisteredManagedArtifactCommitPostcheck {
        let epoch_is_current = self.authority.managed_artifact_epoch_is_current();
        let publication = ManagedArtifactPublicationLease::LibrariesCommit(receipt);
        let valid = match &publication {
            ManagedArtifactPublicationLease::LibrariesCommit(receipt) => {
                epoch_is_current
                    && self.authority.component == ManagedArtifactRebuildComponent::Libraries
                    && receipt.version_id() == self.authority.known_good.version_id
                    && receipt
                        .matches_root(&self.authority.known_good.library_root)
                        .await
                    && receipt.matches_known_good_inventory(&self.authority.known_good.inventory)
                    && receipt.revalidate().await
            }
            _ => unreachable!(),
        };
        self.begin_commit_postcheck(publication, valid).await
    }

    pub(crate) async fn begin_version_bundle_commit(
        self,
        receipt: ManagedVersionBundleCommitReceipt,
    ) -> RegisteredManagedArtifactCommitPostcheck {
        let valid = self.authority.managed_artifact_epoch_is_current()
            && self.authority.component == ManagedArtifactRebuildComponent::VersionBundle
            && receipt.version_id() == self.authority.known_good.version_id
            && receipt
                .matches_root(&self.authority.known_good.library_root)
                .await
            && receipt.matches_known_good_inventory(&self.authority.known_good.inventory)
            && receipt.revalidate().await;
        let publication =
            ManagedArtifactPublicationLease::VersionBundleCommit { _receipt: receipt };
        self.begin_commit_postcheck(publication, valid).await
    }

    pub(crate) async fn begin_assets_commit(
        self,
        receipt: ManagedAssetsCommitReceipt,
    ) -> RegisteredManagedArtifactCommitPostcheck {
        let epoch_is_current = self.authority.managed_artifact_epoch_is_current();
        let publication = ManagedArtifactPublicationLease::AssetsCommit(receipt);
        let valid = match &publication {
            ManagedArtifactPublicationLease::AssetsCommit(receipt) => {
                epoch_is_current
                    && self.authority.component == ManagedArtifactRebuildComponent::Assets
                    && receipt.version_id() == self.authority.known_good.version_id
                    && receipt
                        .matches_root(&self.authority.known_good.library_root)
                        .await
                    && receipt.matches_known_good_inventory(&self.authority.known_good.inventory)
                    && receipt.revalidate().await
            }
            _ => unreachable!(),
        };
        self.begin_commit_postcheck(publication, valid).await
    }

    pub(crate) async fn settle_libraries_rollback(
        self,
        receipt: ManagedLibrariesRollbackReceipt,
    ) -> (RegisteredManagedArtifactComponentSettlement, bool) {
        let publication = ManagedArtifactPublicationLease::LibrariesRollback(receipt);
        let valid = match &publication {
            ManagedArtifactPublicationLease::LibrariesRollback(receipt) => {
                self.authority.component == ManagedArtifactRebuildComponent::Libraries
                    && receipt.version_id() == self.authority.known_good.version_id
                    && libraries_rollback_has_effect(receipt.effect())
                    && receipt
                        .matches_root(&self.authority.known_good.library_root)
                        .await
                    && receipt.matches_known_good_inventory(&self.authority.known_good.inventory)
            }
            _ => unreachable!(),
        };
        (self.authority.durable.failed(Some(publication)), valid)
    }

    pub(crate) async fn settle_version_bundle_rollback(
        self,
        receipt: ManagedVersionBundleRollbackReceipt,
    ) -> (RegisteredManagedArtifactComponentSettlement, bool) {
        let valid = self.authority.component == ManagedArtifactRebuildComponent::VersionBundle
            && receipt.version_id() == self.authority.known_good.version_id
            && receipt
                .matches_root(&self.authority.known_good.library_root)
                .await
            && receipt.matches_known_good_inventory(&self.authority.known_good.inventory);
        let publication =
            ManagedArtifactPublicationLease::VersionBundleRollback { _receipt: receipt };
        (self.authority.durable.failed(Some(publication)), valid)
    }

    pub(crate) async fn settle_assets_rollback(
        self,
        receipt: ManagedAssetsRollbackReceipt,
    ) -> (RegisteredManagedArtifactComponentSettlement, bool) {
        let publication = ManagedArtifactPublicationLease::AssetsRollback(receipt);
        let valid = match &publication {
            ManagedArtifactPublicationLease::AssetsRollback(receipt) => {
                self.authority.component == ManagedArtifactRebuildComponent::Assets
                    && receipt.version_id() == self.authority.known_good.version_id
                    && assets_rollback_has_effect(receipt.effect())
                    && receipt
                        .matches_root(&self.authority.known_good.library_root)
                        .await
                    && receipt.matches_known_good_inventory(&self.authority.known_good.inventory)
            }
            _ => unreachable!(),
        };
        (self.authority.durable.failed(Some(publication)), valid)
    }

    async fn begin_commit_postcheck(
        self,
        publication: ManagedArtifactPublicationLease,
        receipt_is_valid: bool,
    ) -> RegisteredManagedArtifactCommitPostcheck {
        if !receipt_is_valid || !self.authority.managed_artifact_epoch_is_current() {
            return RegisteredManagedArtifactCommitPostcheck::Failed(
                self.authority.durable.failed(Some(publication)),
            );
        }
        let Some(entry) = self
            .authority
            .known_good
            .inventory
            .entries()
            .get(self.authority.provenance.inventory_ordinal())
        else {
            return RegisteredManagedArtifactCommitPostcheck::Failed(
                self.authority.durable.failed(Some(publication)),
            );
        };
        let (expected_sha1, expected_size) = match entry.integrity() {
            KnownGoodIntegrity::Sha1 { digest, size }
            | KnownGoodIntegrity::ExactBytes { digest, size } => {
                (digest.as_str().to_string(), *size)
            }
            KnownGoodIntegrity::Directory | KnownGoodIntegrity::LinkTarget(_) => {
                return RegisteredManagedArtifactCommitPostcheck::Failed(
                    self.authority.durable.failed(Some(publication)),
                );
            }
        };
        let path = known_good_entry_path(
            &self.authority.known_good.library_root,
            &self.authority.runtime_cache,
            entry,
        );
        let Ok((verifier, verification)) =
            RegisteredArtifactExactVerifier::mint(path, expected_sha1, expected_size).await
        else {
            return RegisteredManagedArtifactCommitPostcheck::Failed(
                self.authority.durable.failed(Some(publication)),
            );
        };
        RegisteredManagedArtifactCommitPostcheck::Verify {
            pending: RegisteredManagedArtifactPendingPostcheck {
                authority: self.authority,
                verification,
                publication,
            },
            verifier,
        }
    }
}

impl RegisteredManagedArtifactPendingPostcheck {
    pub(crate) async fn settle(
        self,
        proof: Option<RegisteredArtifactExactProof>,
    ) -> RegisteredManagedArtifactComponentSettlement {
        let Some(proof) = proof else {
            return self.authority.durable.failed(Some(self.publication));
        };
        if !self.verification.matches(&proof) {
            return self.authority.durable.failed(Some(self.publication));
        }
        let instance_id = self.authority.known_good.instance_id.as_str();
        let Some(lifecycle) = self
            .authority
            .durable
            .state
            .try_acquire_instance_lifecycle(instance_id)
            .await
        else {
            return self.authority.durable.failed(Some(self.publication));
        };
        if !self.verification.matches(&proof) || !self.authority.is_live_with(&lifecycle) {
            return self.authority.durable.failed(Some(self.publication));
        }
        self.authority.succeeded(lifecycle, self.publication, proof)
    }
}

impl ManagedArtifactCompletionAuthority {
    fn managed_artifact_epoch_is_current(&self) -> bool {
        self.durable
            .state
            .managed_artifact_mutation_epoch_is_current(self.managed_artifact_epoch.as_ref())
    }

    fn is_live_with(&self, lifecycle: &InstanceLifecycleLease) -> bool {
        if !lifecycle.matches(&self.known_good.instance_id)
            || !self.managed_artifact_epoch_is_current()
            || !self.owner_is_live()
            || !self.durable.state.known_good_authority_is_current(
                &self.known_good.instance_id,
                &self.known_good.version_id,
                &self.known_good.created_at,
                &self.known_good.library_root,
                &self.runtime_cache,
                &self.known_good.inventory,
            )
        {
            return false;
        }
        let ReconciliationScope::RegisteredInstance {
            instance_id,
            fingerprint,
            inventory_fingerprint,
        } = self.durable.attempt.scope();
        let Ok(current) = self
            .durable
            .state
            .current_reconciliation_incarnation(instance_id)
        else {
            return false;
        };
        if instance_id != &self.known_good.instance_id
            || fingerprint != &current.fingerprint
            || inventory_fingerprint != &current.inventory_fingerprint
            || current.roots.library != self.known_good.library_root
        {
            return false;
        }
        recorded_artifact_provenance_matches(
            RegisteredArtifactProvenanceContext::new(
                &self.known_good.instance_id,
                &self.known_good.version_id,
                &self.known_good.created_at,
                &self.known_good.library_root,
                &current.roots.runtime,
                &self.known_good.inventory,
            ),
            &self.durable.attempt,
            self.provenance,
        ) && self.owner_is_live()
    }

    fn owner_is_live(&self) -> bool {
        match &self.owner {
            KnownGoodVerificationOwner::Foreground(foreground) => self
                .durable
                .state
                .validate_integrity_foreground(foreground)
                .is_ok(),
            KnownGoodVerificationOwner::IdleSweep(authority) => {
                self.durable.state.idle_sweep_authority_is_active(authority)
            }
        }
    }

    fn succeeded(
        self,
        lifecycle: InstanceLifecycleLease,
        publication: ManagedArtifactPublicationLease,
        proof: RegisteredArtifactExactProof,
    ) -> RegisteredManagedArtifactComponentSettlement {
        let terminal = ReconciliationTerminal::from_attempt(
            self.durable.attempt.clone(),
            ReconciliationTerminalOutcome::Succeeded,
            ReconciliationQuarantineCheckpoint::default(),
        );
        RegisteredManagedArtifactComponentSettlement {
            durable: self.durable,
            terminal,
            _lifecycle: Some(lifecycle),
            _publication: Some(publication),
            _proof: Some(proof),
        }
    }
}

impl ManagedArtifactDurableAuthority {
    fn failed(
        self,
        publication: Option<ManagedArtifactPublicationLease>,
    ) -> RegisteredManagedArtifactComponentSettlement {
        let terminal = self.failed_terminal.clone();
        RegisteredManagedArtifactComponentSettlement {
            durable: self,
            terminal,
            _lifecycle: None,
            _publication: publication,
            _proof: None,
        }
    }
}

impl RegisteredManagedArtifactComponentSettlement {
    pub(crate) fn journals(&self) -> &OperationJournalStore {
        &self.durable.state.journals
    }

    pub(crate) fn failure_memory(&self) -> &GuardianFailureMemoryStore {
        &self.durable.state.failure_memory
    }

    pub(crate) fn attempt(&self) -> &ReconciliationAttempt {
        &self.durable.attempt
    }

    pub(crate) fn terminal(&self) -> &ReconciliationTerminal {
        &self.terminal
    }

    pub(crate) fn succeeded(&self) -> bool {
        self.terminal.outcome() == ReconciliationTerminalOutcome::Succeeded
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum ManagedArtifactRebuildComponent {
    VersionBundle,
    Libraries,
    Assets,
}

impl ManagedArtifactRebuildComponent {
    fn from_artifact_attempt(
        attempt: &ReconciliationAttempt,
    ) -> Result<Self, ReconciliationEvidenceRejection> {
        let component = match (attempt.component(), attempt.domain()) {
            (ReconciliationComponent::VersionBundle, GuardianDomain::Launch) => Self::VersionBundle,
            (ReconciliationComponent::Libraries, GuardianDomain::Library) => Self::Libraries,
            (ReconciliationComponent::Assets, GuardianDomain::Download) => Self::Assets,
            _ => return Err(ReconciliationEvidenceRejection::ScopeMismatch),
        };
        if attempt.rung() != ReconciliationRung::RepairArtifact
            || attempt.diagnosis_id() != DiagnosisId::LauncherManagedArtifactCorrupt
            || attempt.mode() != GuardianMode::Managed
            || attempt.ownership() != OwnershipClass::LauncherManaged
            || attempt.target().system != StabilizationSystem::Execution
            || attempt.target().kind != TargetKind::Artifact
            || attempt.target().ownership != OwnershipClass::LauncherManaged
        {
            return Err(ReconciliationEvidenceRejection::ScopeMismatch);
        }
        Ok(component)
    }

    fn reconciliation_component(self) -> ReconciliationComponent {
        match self {
            Self::VersionBundle => ReconciliationComponent::VersionBundle,
            Self::Libraries => ReconciliationComponent::Libraries,
            Self::Assets => ReconciliationComponent::Assets,
        }
    }

    fn domain(self) -> GuardianDomain {
        match self {
            Self::VersionBundle => GuardianDomain::Launch,
            Self::Libraries => GuardianDomain::Library,
            Self::Assets => GuardianDomain::Download,
        }
    }

    fn matches_state(self, state: &RegisteredComponentRebuildState) -> bool {
        matches!(
            (self, state),
            (
                Self::VersionBundle,
                RegisteredComponentRebuildState::VersionBundle
            ) | (Self::Libraries, RegisteredComponentRebuildState::Libraries)
                | (Self::Assets, RegisteredComponentRebuildState::Assets)
        )
    }
}

fn validate_registered_artifact_provenance(
    verification: Option<&KnownGoodVerificationLease>,
    provenance: Option<RegisteredArtifactProvenance>,
    terminal: &ReconciliationTerminal,
) -> Result<(), ReconciliationEvidenceRejection> {
    match (verification, provenance) {
        (None, None) if terminal.component() == ReconciliationComponent::Runtime => Ok(()),
        (Some(verification), Some(provenance)) => {
            let component =
                ManagedArtifactRebuildComponent::from_artifact_attempt(terminal.attempt())?;
            if provenance.component() != component.reconciliation_component()
                || registered_artifact_target(verification, provenance.inventory_ordinal()).as_ref()
                    != Some(terminal.target())
            {
                return Err(ReconciliationEvidenceRejection::ScopeMismatch);
            }
            Ok(())
        }
        _ => Err(ReconciliationEvidenceRejection::ScopeMismatch),
    }
}

fn libraries_rollback_has_effect(effect: ManagedLibrariesRollbackEffect) -> bool {
    effect != ManagedLibrariesRollbackEffect::None
}

fn assets_rollback_has_effect(effect: ManagedAssetsRollbackEffect) -> bool {
    effect != ManagedAssetsRollbackEffect::None
}

fn validated_runtime_quarantine_checkpoint(
    runtime_cache: &ManagedRuntimeCache,
    component: &RuntimeId,
    quarantine: Option<&ManagedRuntimeQuarantineObligation>,
) -> Result<ReconciliationQuarantineCheckpoint, ReconciliationEvidenceRejection> {
    let Some(quarantine) = quarantine else {
        return Ok(ReconciliationQuarantineCheckpoint::default());
    };
    if quarantine.component() != component || !quarantine.matches_cache(runtime_cache) {
        return Err(ReconciliationEvidenceRejection::JournalMismatch);
    }
    if quarantine.observation() == ManagedRuntimeQuarantineObservation::Absent {
        return Ok(ReconciliationQuarantineCheckpoint::default());
    }
    Ok(ReconciliationQuarantineCheckpoint::new(vec![
        ReconciliationQuarantineRecord::runtime(component.as_str()),
    ]))
}

struct RegisteredKnownGoodInventory {
    instance_id: String,
    version_id: String,
    created_at: String,
    library_root: PathBuf,
    inventory: std::sync::Arc<axial_minecraft::known_good::KnownGoodInventory>,
}

pub(crate) struct ReconciliationAttemptReservation {
    reservation: StoreReconciliationAttemptReservation,
}

pub(crate) struct RegisteredReconciliationAuthority {
    state: AppState,
    lifecycle: InstanceLifecycleLease,
    verification: Option<KnownGoodVerificationLease>,
    managed_artifact_epoch: Option<std::sync::Arc<std::sync::atomic::AtomicU64>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ReconciliationAttemptRejection {
    PersistencePending,
    AlreadyReserved,
    CapacityExhausted,
    AmbiguousPriorAttempt,
}

struct RecordedReconciliationFailure {
    terminal: ReconciliationTerminal,
    lifecycle: InstanceLifecycleLease,
    roots: ReconciliationRoots,
    inventory: std::sync::Arc<axial_minecraft::known_good::KnownGoodInventory>,
    artifact_provenance: Option<RegisteredArtifactProvenance>,
}

#[derive(Eq, PartialEq)]
struct ReconciliationRoots {
    instance: PathBuf,
    library: PathBuf,
    runtime: PathBuf,
}

struct CurrentReconciliationIncarnation {
    fingerprint: ReconciliationIncarnationFingerprint,
    inventory_fingerprint: ReconciliationInventoryFingerprint,
    roots: ReconciliationRoots,
    inventory: std::sync::Arc<axial_minecraft::known_good::KnownGoodInventory>,
}

struct CurrentReconciliationIdentity {
    instance_id: String,
    version_id: String,
    created_at: String,
    fingerprint: ReconciliationIncarnationFingerprint,
    roots: ReconciliationRoots,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ReconciliationEvidenceRejection {
    InvalidInstanceIdentity,
    InstanceNotRegistered,
    RootAuthorityUnavailable,
    MemoryMissing,
    MemoryNotFailed,
    MemoryWindowInactive,
    JournalMissing,
    JournalMismatch,
    NonAdjacentRung,
    ScopeMismatch,
    IncarnationMismatch,
    OwnershipMismatch,
    ActiveSession,
    PolicyModeChanged,
    SuppressedPriorAttempt,
}

impl ReconciliationEvidenceRejection {
    pub(crate) const fn class(self) -> &'static str {
        match self {
            Self::InvalidInstanceIdentity => "invalid_instance_identity",
            Self::InstanceNotRegistered => "instance_not_registered",
            Self::RootAuthorityUnavailable => "root_authority_unavailable",
            Self::MemoryMissing => "memory_missing",
            Self::MemoryNotFailed => "memory_not_failed",
            Self::MemoryWindowInactive => "memory_window_inactive",
            Self::JournalMissing => "journal_missing",
            Self::JournalMismatch => "journal_mismatch",
            Self::NonAdjacentRung => "non_adjacent_rung",
            Self::ScopeMismatch => "scope_mismatch",
            Self::IncarnationMismatch => "incarnation_mismatch",
            Self::OwnershipMismatch => "ownership_mismatch",
            Self::ActiveSession => "active_session",
            Self::PolicyModeChanged => "policy_mode_changed",
            Self::SuppressedPriorAttempt => "suppressed_prior_attempt",
        }
    }
}

impl RecordedRuntimeArtifactRepairFailure {
    pub(crate) fn diagnosis_id(&self) -> DiagnosisId {
        self.evidence.terminal.diagnosis_id()
    }
}

impl RegisteredComponentRebuildAdmission {
    pub(crate) fn journals(&self) -> &OperationJournalStore {
        self.authority.journals()
    }

    pub(crate) fn failure_memory(&self) -> &GuardianFailureMemoryStore {
        self.authority.failure_memory()
    }

    pub(crate) fn attempt(&self) -> &ReconciliationAttempt {
        &self.attempt
    }

    #[cfg(test)]
    pub(crate) fn bind_managed_artifact_epoch_for_test(&mut self) {
        let epoch = self
            .authority
            .state
            .managed_artifact_mutation_epoch()
            .expect("test managed artifact epoch");
        let expected = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(epoch.value()));
        self.authority.managed_artifact_epoch = Some(expected.clone());
        if let Some(verification) = &mut self.authority.verification {
            verification.managed_artifact_epoch = Some(expected);
        }
    }

    pub(crate) fn admit_managed_artifact_mutation(
        &self,
    ) -> Result<
        super::ManagedArtifactMutationAdmission,
        super::ManagedArtifactMutationEpochUnavailable,
    > {
        self.authority.admit_managed_artifact_mutation()
    }

    pub(crate) fn runtime_core_request(
        &self,
    ) -> Result<(ManagedRuntimeCache, RuntimeId), ReconciliationEvidenceRejection> {
        let RegisteredComponentRebuildState::Runtime { runtime_cache, .. } = &self.component_state
        else {
            return Err(ReconciliationEvidenceRejection::ScopeMismatch);
        };
        if !runtime_cache.shares_identity_with(&self.authority.state.managed_runtime_cache) {
            return Err(ReconciliationEvidenceRejection::IncarnationMismatch);
        }
        Ok((
            runtime_cache.clone(),
            RuntimeId::from(self.attempt.target().id.clone()),
        ))
    }

    pub(crate) fn failed_terminal(
        &self,
    ) -> Result<ReconciliationTerminal, ReconciliationEvidenceRejection> {
        self.authority
            .terminal(self.attempt.clone(), ReconciliationTerminalOutcome::Failed)
    }

    pub(crate) fn into_libraries_effect(
        self,
    ) -> RegisteredManagedArtifactComponentEffectAdmission<RegisteredLibrariesComponentRebuildEffect>
    {
        match self.into_managed_artifact_completion(ManagedArtifactRebuildComponent::Libraries) {
            Ok((request, completion)) => {
                RegisteredManagedArtifactComponentEffectAdmission::Admitted {
                    request: RegisteredLibrariesComponentRebuildEffect {
                        library_root: request.library_root,
                        version_id: request.version_id,
                    },
                    completion: Box::new(completion),
                }
            }
            Err(settlement) => {
                RegisteredManagedArtifactComponentEffectAdmission::Refused(settlement)
            }
        }
    }

    pub(crate) fn into_version_bundle_effect(
        self,
    ) -> RegisteredManagedArtifactComponentEffectAdmission<
        RegisteredVersionBundleComponentRebuildEffect,
    > {
        match self.into_managed_artifact_completion(ManagedArtifactRebuildComponent::VersionBundle)
        {
            Ok((request, completion)) => {
                RegisteredManagedArtifactComponentEffectAdmission::Admitted {
                    request: RegisteredVersionBundleComponentRebuildEffect {
                        library_root: request.library_root,
                        version_id: request.version_id,
                    },
                    completion: Box::new(completion),
                }
            }
            Err(settlement) => {
                RegisteredManagedArtifactComponentEffectAdmission::Refused(settlement)
            }
        }
    }

    pub(crate) fn into_assets_effect(
        self,
    ) -> RegisteredManagedArtifactComponentEffectAdmission<RegisteredAssetsComponentRebuildEffect>
    {
        match self.into_managed_artifact_completion(ManagedArtifactRebuildComponent::Assets) {
            Ok((request, completion)) => {
                RegisteredManagedArtifactComponentEffectAdmission::Admitted {
                    request: RegisteredAssetsComponentRebuildEffect {
                        library_root: request.library_root,
                        version_id: request.version_id,
                    },
                    completion: Box::new(completion),
                }
            }
            Err(settlement) => {
                RegisteredManagedArtifactComponentEffectAdmission::Refused(settlement)
            }
        }
    }

    fn into_managed_artifact_completion(
        self,
        component: ManagedArtifactRebuildComponent,
    ) -> Result<
        (
            ManagedArtifactCoreRequest,
            RegisteredManagedArtifactComponentCompletion,
        ),
        Box<RegisteredManagedArtifactComponentSettlement>,
    > {
        let structurally_valid = self.validate_managed_artifact_admission(component).is_ok()
            && self.artifact_provenance.is_some_and(|provenance| {
                provenance.component() == component.reconciliation_component()
            })
            && self
                .authority
                .verification
                .as_ref()
                .is_some_and(|verification| {
                    self.authority
                        .state
                        .known_good_verification_lease_is_live(verification)
                        && verification.instance_id == self.known_good.instance_id
                        && verification.version_id == self.known_good.version_id
                        && verification.created_at == self.known_good.created_at
                        && verification.library_root == self.known_good.library_root
                        && std::sync::Arc::ptr_eq(
                            &verification.inventory,
                            &self.known_good.inventory,
                        )
                });
        let request = ManagedArtifactCoreRequest {
            library_root: self.known_good.library_root.clone(),
            version_id: self.known_good.version_id.clone(),
        };
        let RegisteredComponentRebuildAdmission {
            authority,
            attempt,
            failed_terminal,
            known_good,
            artifact_provenance,
            component_state: _,
            _component_mutation,
            _config_mutation,
        } = self;
        let RegisteredReconciliationAuthority {
            state,
            lifecycle,
            verification,
            managed_artifact_epoch,
        } = authority;
        let durable = ManagedArtifactDurableAuthority {
            state,
            attempt,
            failed_terminal,
            _component_mutation,
            _config_mutation,
        };
        drop(lifecycle);
        let (Some(verification), Some(provenance)) = (verification, artifact_provenance) else {
            return Err(Box::new(durable.failed(None)));
        };
        let KnownGoodVerificationLease {
            owner,
            _lifecycle,
            instance_id: _,
            version_id: _,
            created_at: _,
            library_root: _,
            managed_runtime_cache,
            inventory: _,
            managed_artifact_epoch: _,
        } = verification;
        drop(_lifecycle);
        if !structurally_valid {
            return Err(Box::new(durable.failed(None)));
        }
        Ok((
            request,
            RegisteredManagedArtifactComponentCompletion {
                authority: ManagedArtifactCompletionAuthority {
                    durable,
                    owner,
                    known_good,
                    runtime_cache: managed_runtime_cache,
                    provenance,
                    component,
                    managed_artifact_epoch,
                },
            },
        ))
    }

    pub(crate) async fn succeeded_terminal(
        &self,
        receipt: &ManagedRuntimeCommitReceipt,
    ) -> Result<ReconciliationTerminal, ReconciliationEvidenceRejection> {
        self.succeeded_terminal_with_activation_observer(receipt, || {})
            .await
    }

    async fn succeeded_terminal_with_activation_observer<AfterActivation>(
        &self,
        receipt: &ManagedRuntimeCommitReceipt,
        after_activation: AfterActivation,
    ) -> Result<ReconciliationTerminal, ReconciliationEvidenceRejection>
    where
        AfterActivation: FnOnce(),
    {
        self.require_managed_artifact_epoch_current()?;
        self.validate_runtime_receipt_identity(receipt)?;
        if !receipt
            .revalidate(
                &self.authority.state.managed_runtime_cache,
                receipt.component(),
            )
            .await
        {
            return Err(ReconciliationEvidenceRejection::JournalMismatch);
        }
        self.require_managed_artifact_epoch_current()?;
        self.validate_runtime_receipt_identity(receipt)?;
        let refreshed_inventory = std::sync::Arc::new(
            receipt
                .replace_known_good_runtime_projection(&self.known_good.inventory)
                .map_err(|_| ReconciliationEvidenceRejection::JournalMismatch)?,
        );
        if !receipt.matches_known_good_inventory(&refreshed_inventory) {
            return Err(ReconciliationEvidenceRejection::JournalMismatch);
        }
        let quarantine_checkpoint = self.validated_runtime_quarantine_checkpoint(
            receipt.component(),
            receipt.quarantine_obligation(),
        )?;
        self.authority
            .state
            .known_good
            .reconcile(
                &self.known_good.instance_id,
                &self.known_good.version_id,
                &self.known_good.created_at,
                &self.known_good.library_root,
                refreshed_inventory.clone(),
            )
            .await
            .map_err(|_| ReconciliationEvidenceRejection::RootAuthorityUnavailable)?;
        self.require_managed_artifact_epoch_current()?;
        after_activation();
        let activated_inventory = match self.validate_runtime_identity_against(
            receipt.component(),
            receipt.matches_cache(&self.authority.state.managed_runtime_cache),
            &refreshed_inventory,
        ) {
            Ok(inventory) => inventory,
            Err(rejection) => {
                self.seal_failed_runtime_projection(refreshed_inventory)?;
                return Err(rejection);
            }
        };
        if !std::sync::Arc::ptr_eq(&refreshed_inventory, &activated_inventory)
            || !receipt.matches_known_good_inventory(&activated_inventory)
        {
            self.seal_failed_runtime_projection(refreshed_inventory)?;
            return Err(ReconciliationEvidenceRejection::JournalMismatch);
        }
        let receipt_is_current = receipt
            .revalidate(
                &self.authority.state.managed_runtime_cache,
                receipt.component(),
            )
            .await;
        let active_after_postcheck = self.validate_runtime_identity_against(
            receipt.component(),
            receipt.matches_cache(&self.authority.state.managed_runtime_cache),
            &refreshed_inventory,
        );
        if let Err(rejection) = active_after_postcheck {
            self.seal_failed_runtime_projection(refreshed_inventory)?;
            return Err(rejection);
        }
        if !receipt_is_current {
            self.seal_failed_runtime_projection(refreshed_inventory)?;
            return Err(ReconciliationEvidenceRejection::JournalMismatch);
        }
        self.require_managed_artifact_epoch_current()?;
        Ok(ReconciliationTerminal::from_attempt(
            self.attempt.clone(),
            ReconciliationTerminalOutcome::Succeeded,
            quarantine_checkpoint,
        ))
    }

    pub(crate) fn failed_postcondition_terminal(
        &self,
        receipt: &ManagedRuntimeCommitReceipt,
    ) -> Result<ReconciliationTerminal, ReconciliationEvidenceRejection> {
        let postcondition_failure_inventory = self.runtime_postcondition_failure_inventory()?;
        if let Some(refreshed_inventory) = postcondition_failure_inventory.get() {
            self.validate_runtime_receipt_capability(
                receipt.component(),
                receipt.matches_cache(&self.authority.state.managed_runtime_cache),
            )?;
            if !receipt.matches_known_good_inventory(refreshed_inventory) {
                return Err(ReconciliationEvidenceRejection::JournalMismatch);
            }
        } else {
            self.validate_runtime_receipt_identity(receipt)?;
        }
        let quarantine_checkpoint = self.validated_runtime_quarantine_checkpoint(
            receipt.component(),
            receipt.quarantine_obligation(),
        )?;
        Ok(ReconciliationTerminal::from_attempt(
            self.attempt.clone(),
            ReconciliationTerminalOutcome::Failed,
            quarantine_checkpoint,
        ))
    }

    pub(crate) fn failed_effect_terminal(
        &self,
        receipt: &ManagedRuntimeFailureReceipt,
    ) -> Result<ReconciliationTerminal, ReconciliationEvidenceRejection> {
        self.validate_runtime_identity(
            receipt.component(),
            receipt.matches_cache(&self.authority.state.managed_runtime_cache),
        )?;
        Ok(ReconciliationTerminal::from_attempt(
            self.attempt.clone(),
            ReconciliationTerminalOutcome::Failed,
            self.validated_runtime_quarantine_checkpoint(
                receipt.component(),
                receipt.quarantine_obligation(),
            )?,
        ))
    }

    fn validate_runtime_receipt_identity(
        &self,
        receipt: &ManagedRuntimeCommitReceipt,
    ) -> Result<
        std::sync::Arc<axial_minecraft::known_good::KnownGoodInventory>,
        ReconciliationEvidenceRejection,
    > {
        self.validate_runtime_identity(
            receipt.component(),
            receipt.matches_cache(&self.authority.state.managed_runtime_cache),
        )
    }

    fn require_managed_artifact_epoch_current(
        &self,
    ) -> Result<(), ReconciliationEvidenceRejection> {
        self.authority
            .state
            .managed_artifact_mutation_epoch_is_current(
                self.authority.managed_artifact_epoch.as_ref(),
            )
            .then_some(())
            .ok_or(ReconciliationEvidenceRejection::IncarnationMismatch)
    }

    fn validate_managed_artifact_admission(
        &self,
        component: ManagedArtifactRebuildComponent,
    ) -> Result<(), ReconciliationEvidenceRejection> {
        if self.attempt.rung() != ReconciliationRung::RebuildComponent
            || self.attempt.component() != component.reconciliation_component()
            || self.attempt.domain() != component.domain()
            || self.attempt.mode() != GuardianMode::Managed
            || self.attempt.ownership() != OwnershipClass::LauncherManaged
            || self.attempt.target().system != StabilizationSystem::Execution
            || self.attempt.target().kind != TargetKind::Artifact
            || self.attempt.target().ownership != OwnershipClass::LauncherManaged
            || !component.matches_state(&self.component_state)
        {
            return Err(ReconciliationEvidenceRejection::ScopeMismatch);
        }
        Ok(())
    }

    fn validate_runtime_identity(
        &self,
        component: &RuntimeId,
        matches_cache: bool,
    ) -> Result<
        std::sync::Arc<axial_minecraft::known_good::KnownGoodInventory>,
        ReconciliationEvidenceRejection,
    > {
        let ReconciliationScope::RegisteredInstance {
            inventory_fingerprint,
            ..
        } = self.attempt.scope();
        if &reconciliation_inventory_fingerprint(&self.known_good.inventory)
            != inventory_fingerprint
        {
            return Err(ReconciliationEvidenceRejection::IncarnationMismatch);
        }
        self.validate_runtime_identity_against(component, matches_cache, &self.known_good.inventory)
    }

    fn validate_runtime_identity_against(
        &self,
        component: &RuntimeId,
        matches_cache: bool,
        expected_inventory: &std::sync::Arc<axial_minecraft::known_good::KnownGoodInventory>,
    ) -> Result<
        std::sync::Arc<axial_minecraft::known_good::KnownGoodInventory>,
        ReconciliationEvidenceRejection,
    > {
        let inventory =
            self.current_runtime_inventory(component, matches_cache, expected_inventory)?;
        if !std::sync::Arc::ptr_eq(&inventory, expected_inventory) {
            return Err(ReconciliationEvidenceRejection::IncarnationMismatch);
        }
        Ok(inventory)
    }

    fn current_runtime_inventory(
        &self,
        component: &RuntimeId,
        matches_cache: bool,
        expected_inventory: &std::sync::Arc<axial_minecraft::known_good::KnownGoodInventory>,
    ) -> Result<
        std::sync::Arc<axial_minecraft::known_good::KnownGoodInventory>,
        ReconciliationEvidenceRejection,
    > {
        self.validate_runtime_receipt_capability(component, matches_cache)?;
        let ReconciliationScope::RegisteredInstance {
            instance_id,
            fingerprint,
            ..
        } = self.attempt.scope();
        let identity = self
            .authority
            .state
            .current_reconciliation_identity(instance_id)?;
        if &identity.fingerprint != fingerprint {
            return Err(ReconciliationEvidenceRejection::IncarnationMismatch);
        }
        let current = self
            .authority
            .state
            .reconciliation_incarnation_from_identity(identity)?;
        let expected_inventory_fingerprint =
            reconciliation_inventory_fingerprint(expected_inventory);
        if current.inventory_fingerprint != expected_inventory_fingerprint {
            return Err(ReconciliationEvidenceRejection::IncarnationMismatch);
        }
        Ok(current.inventory)
    }

    fn validate_runtime_receipt_capability(
        &self,
        component: &RuntimeId,
        matches_cache: bool,
    ) -> Result<(), ReconciliationEvidenceRejection> {
        if self.attempt.component() != ReconciliationComponent::Runtime
            || self.attempt.target().kind != TargetKind::Runtime
            || self.attempt.target().id != component.as_str()
            || !matches_cache
        {
            return Err(ReconciliationEvidenceRejection::JournalMismatch);
        }
        let ReconciliationScope::RegisteredInstance { instance_id, .. } = self.attempt.scope();
        if instance_id != &self.known_good.instance_id {
            return Err(ReconciliationEvidenceRejection::IncarnationMismatch);
        }
        Ok(())
    }

    fn seal_failed_runtime_projection(
        &self,
        refreshed_inventory: std::sync::Arc<axial_minecraft::known_good::KnownGoodInventory>,
    ) -> Result<(), ReconciliationEvidenceRejection> {
        let _removed = self.authority.state.known_good.deactivate_exact_inventory(
            &self.known_good.instance_id,
            &self.known_good.version_id,
            &self.known_good.created_at,
            &self.known_good.library_root,
            &refreshed_inventory,
        );
        self.runtime_postcondition_failure_inventory()?
            .set(refreshed_inventory)
            .map_err(|_| ReconciliationEvidenceRejection::JournalMismatch)
    }

    fn runtime_postcondition_failure_inventory(
        &self,
    ) -> Result<
        &std::sync::OnceLock<std::sync::Arc<axial_minecraft::known_good::KnownGoodInventory>>,
        ReconciliationEvidenceRejection,
    > {
        match &self.component_state {
            RegisteredComponentRebuildState::Runtime {
                postcondition_failure_inventory,
                ..
            } => Ok(postcondition_failure_inventory),
            RegisteredComponentRebuildState::VersionBundle
            | RegisteredComponentRebuildState::Libraries => {
                Err(ReconciliationEvidenceRejection::ScopeMismatch)
            }
            RegisteredComponentRebuildState::Assets => {
                Err(ReconciliationEvidenceRejection::ScopeMismatch)
            }
        }
    }

    #[cfg(test)]
    fn runtime_postcondition_failure_inventory_for_test(
        &self,
    ) -> &std::sync::OnceLock<std::sync::Arc<axial_minecraft::known_good::KnownGoodInventory>> {
        self.runtime_postcondition_failure_inventory()
            .expect("Runtime admission state")
    }

    fn validated_runtime_quarantine_checkpoint(
        &self,
        component: &RuntimeId,
        quarantine: Option<&ManagedRuntimeQuarantineObligation>,
    ) -> Result<ReconciliationQuarantineCheckpoint, ReconciliationEvidenceRejection> {
        validated_runtime_quarantine_checkpoint(
            &self.authority.state.managed_runtime_cache,
            component,
            quarantine,
        )
    }

    #[cfg(test)]
    fn admitted_inventory(
        &self,
    ) -> &std::sync::Arc<axial_minecraft::known_good::KnownGoodInventory> {
        &self.known_good.inventory
    }
}

impl RegisteredReconciliationAuthority {
    pub(crate) fn journals(&self) -> &OperationJournalStore {
        self.state.journals.as_ref()
    }

    pub(crate) fn failure_memory(&self) -> &GuardianFailureMemoryStore {
        self.state.failure_memory.as_ref()
    }

    pub(crate) fn admit_managed_artifact_mutation(
        &self,
    ) -> Result<
        super::ManagedArtifactMutationAdmission,
        super::ManagedArtifactMutationEpochUnavailable,
    > {
        if let Some(expected) = &self.managed_artifact_epoch {
            self.state
                .managed_artifact_epoch
                .admit_from_expected(expected)
        } else {
            self.state
                .admit_managed_artifact_mutation()
                .map_err(super::ManagedArtifactMutationEpochUnavailable::from)
        }
    }

    pub(crate) fn registered_artifact_findings_are_live(
        &self,
        findings: &super::RegisteredArtifactFindings,
    ) -> bool {
        self.state.registered_artifact_findings_are_live(findings)
    }

    pub(crate) fn attempt_is_current(&self, attempt: &ReconciliationAttempt) -> bool {
        self.state
            .managed_artifact_mutation_epoch_is_current(self.managed_artifact_epoch.as_ref())
            && self.verification.as_ref().is_none_or(|verification| {
                self.state
                    .known_good_verification_lease_is_live(verification)
            })
            && self
                .state
                .current_reconciliation_incarnation(&self.lifecycle.instance_id)
                .is_ok_and(|current| {
                    matches!(
                        attempt.scope(),
                        ReconciliationScope::RegisteredInstance {
                            instance_id,
                            fingerprint,
                            inventory_fingerprint,
                        } if instance_id == &self.lifecycle.instance_id
                            && fingerprint == &current.fingerprint
                            && inventory_fingerprint == &current.inventory_fingerprint
                    )
                })
    }

    pub(super) fn into_registered_artifact_failed_repair(
        self,
        attempt: &ReconciliationAttempt,
    ) -> Result<RegisteredArtifactFailedRepair, ReconciliationEvidenceRejection> {
        let Self {
            state,
            lifecycle,
            verification,
            managed_artifact_epoch,
        } = self;
        let verification =
            verification.ok_or(ReconciliationEvidenceRejection::IncarnationMismatch)?;
        if !match (
            managed_artifact_epoch.as_ref(),
            verification.managed_artifact_epoch.as_ref(),
        ) {
            (Some(authority), Some(verification)) => {
                std::sync::Arc::ptr_eq(authority, verification)
            }
            (None, None) => true,
            _ => false,
        } {
            return Err(ReconciliationEvidenceRejection::IncarnationMismatch);
        }
        let evidence = state.recorded_reconciliation_failure_at(
            &lifecycle,
            attempt.operation_id(),
            ReconciliationRung::RepairArtifact,
            chrono::Utc::now().fixed_offset(),
            Some(&verification),
        )?;
        if evidence.terminal.attempt() != attempt
            || evidence.terminal.outcome() != ReconciliationTerminalOutcome::Failed
        {
            return Err(ReconciliationEvidenceRejection::JournalMismatch);
        }
        let (_, _, _, library_root, runtime_cache, _) = verification.execution_parts();
        if !std::sync::Arc::ptr_eq(&evidence.inventory, &verification.inventory)
            || evidence.roots.library != library_root
            || evidence.roots.runtime != runtime_cache.root()
        {
            return Err(ReconciliationEvidenceRejection::IncarnationMismatch);
        }
        validate_registered_artifact_provenance(
            Some(&verification),
            evidence.artifact_provenance,
            &evidence.terminal,
        )?;
        Ok(RegisteredArtifactFailedRepair {
            evidence,
            verification,
        })
    }

    pub(crate) fn owns_runtime_root(
        &self,
        runtime_root: &crate::execution::runtime::ManagedRuntimeRoot<'_>,
    ) -> bool {
        runtime_root.belongs_to(&self.state.managed_runtime_cache)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn repair_artifact_attempt(
        &self,
        operation_id: OperationId,
        diagnosis_id: DiagnosisId,
        domain: GuardianDomain,
        component: ReconciliationComponent,
        target: TargetDescriptor,
        mode: GuardianMode,
        suppression_for: chrono::Duration,
    ) -> Result<ReconciliationAttempt, ReconciliationEvidenceRejection> {
        let observed_at = chrono::Utc::now().fixed_offset();
        let suppression_until = observed_at
            .checked_add_signed(suppression_for)
            .filter(|until| *until > observed_at)
            .ok_or(ReconciliationEvidenceRejection::JournalMismatch)?;
        self.state.registered_reconciliation_attempt_at(
            &self.lifecycle,
            operation_id,
            diagnosis_id,
            domain,
            ReconciliationRung::RepairArtifact,
            component,
            target,
            mode,
            observed_at,
            suppression_until,
            ReconciliationLineage::Initial,
        )
    }

    pub(crate) fn terminal(
        &self,
        attempt: ReconciliationAttempt,
        outcome: ReconciliationTerminalOutcome,
    ) -> Result<ReconciliationTerminal, ReconciliationEvidenceRejection> {
        self.terminal_with_quarantine(
            attempt,
            outcome,
            ReconciliationQuarantineCheckpoint::default(),
        )
    }

    pub(crate) fn artifact_terminal(
        &self,
        attempt: ReconciliationAttempt,
        outcome: ReconciliationTerminalOutcome,
        quarantine_checkpoint: ReconciliationQuarantineCheckpoint,
    ) -> Result<ReconciliationTerminal, ReconciliationEvidenceRejection> {
        ManagedArtifactRebuildComponent::from_artifact_attempt(&attempt)?;
        self.terminal_with_quarantine(attempt, outcome, quarantine_checkpoint)
    }

    fn terminal_with_quarantine(
        &self,
        attempt: ReconciliationAttempt,
        outcome: ReconciliationTerminalOutcome,
        quarantine_checkpoint: ReconciliationQuarantineCheckpoint,
    ) -> Result<ReconciliationTerminal, ReconciliationEvidenceRejection> {
        if !self.attempt_is_current(&attempt) {
            return Err(ReconciliationEvidenceRejection::IncarnationMismatch);
        }
        let current = self
            .state
            .current_reconciliation_incarnation(&self.lifecycle.instance_id)?;
        let ReconciliationScope::RegisteredInstance {
            instance_id,
            fingerprint,
            inventory_fingerprint,
        } = attempt.scope();
        if instance_id != &self.lifecycle.instance_id
            || fingerprint != &current.fingerprint
            || inventory_fingerprint != &current.inventory_fingerprint
        {
            return Err(ReconciliationEvidenceRejection::IncarnationMismatch);
        }
        let terminal =
            ReconciliationTerminal::from_attempt(attempt, outcome, quarantine_checkpoint);
        terminal
            .validate()
            .map_err(|_| ReconciliationEvidenceRejection::OwnershipMismatch)?;
        Ok(terminal)
    }
}

pub(crate) fn reconciliation_journal_attempt(
    mut entry: OperationJournalEntry,
    attempt: ReconciliationAttempt,
) -> OperationJournalEntry {
    entry.reconciliation_attempt = Some(attempt);
    entry
}

pub(crate) fn component_rebuild_journal(
    admission: &RegisteredComponentRebuildAdmission,
) -> OperationJournalEntry {
    component_rebuild_journal_for_attempt(admission.attempt())
}

fn component_rebuild_journal_for_attempt(attempt: &ReconciliationAttempt) -> OperationJournalEntry {
    let target = attempt.target();
    let mut entry = OperationJournalEntry::new(
        super::contracts::JournalId::new(format!("journal-{}", attempt.operation_id().as_str())),
        attempt.operation_id().clone(),
        CommandKind::RepairInstance,
        StabilizationSystem::Guardian,
        OwnershipClass::LauncherManaged,
        RollbackState::NotApplicable,
    );
    entry.targets.push(target.clone());
    let ReconciliationScope::RegisteredInstance { instance_id, .. } = attempt.scope();
    entry
        .targets
        .push(reconciliation_instance_target(instance_id));
    entry.planned_steps.push(component_rebuild_step(
        COMPONENT_REBUILD_START_STEP,
        target,
        RollbackState::NotApplicable,
    ));
    match attempt.component() {
        ReconciliationComponent::Runtime => {
            entry.planned_steps.push(component_rebuild_step(
                COMPONENT_QUARANTINE_STEP,
                target,
                RollbackState::Available,
            ));
            entry.planned_steps.push(component_rebuild_step(
                RUNTIME_COMPONENT_REBUILD_STEP,
                target,
                RollbackState::NotApplicable,
            ));
        }
        ReconciliationComponent::VersionBundle
        | ReconciliationComponent::Libraries
        | ReconciliationComponent::Assets => {
            let step_id = match attempt.component() {
                ReconciliationComponent::VersionBundle => VERSION_BUNDLE_COMPONENT_REBUILD_STEP,
                ReconciliationComponent::Libraries => LIBRARIES_COMPONENT_REBUILD_STEP,
                ReconciliationComponent::Assets => ASSETS_COMPONENT_REBUILD_STEP,
                _ => unreachable!(),
            };
            entry.planned_steps.push(component_rebuild_step(
                step_id,
                target,
                RollbackState::NotApplicable,
            ));
        }
        ReconciliationComponent::WholeInstance => unreachable!("rung-two component plan"),
    }
    entry.guardian_diagnosis_ids.push(attempt.diagnosis_id());
    reconciliation_journal_attempt(entry, attempt.clone())
}

fn whole_instance_rematerialization_journal_for_attempt(
    attempt: &ReconciliationAttempt,
) -> OperationJournalEntry {
    let target = attempt.target();
    let mut entry = OperationJournalEntry::new(
        super::contracts::JournalId::new(format!("journal-{}", attempt.operation_id().as_str())),
        attempt.operation_id().clone(),
        CommandKind::RepairInstance,
        StabilizationSystem::Guardian,
        OwnershipClass::LauncherManaged,
        RollbackState::Available,
    );
    entry.targets.push(target.clone());
    entry.planned_steps.push(component_rebuild_step(
        WHOLE_INSTANCE_REMATERIALIZATION_START_STEP,
        target,
        RollbackState::NotApplicable,
    ));
    entry.planned_steps.push(component_rebuild_step(
        COMPONENT_QUARANTINE_STEP,
        target,
        RollbackState::Available,
    ));
    entry.planned_steps.push(component_rebuild_step(
        WHOLE_INSTANCE_REMATERIALIZATION_STEP,
        target,
        RollbackState::Available,
    ));
    entry.guardian_diagnosis_ids.push(attempt.diagnosis_id());
    reconciliation_journal_attempt(entry, attempt.clone())
}

fn component_rebuild_step(
    step_id: &'static str,
    target: &TargetDescriptor,
    rollback: RollbackState,
) -> OperationJournalStep {
    let mut step = OperationJournalStep::new(step_id, OperationPhase::Repairing);
    step.changed_target = Some(target.clone());
    step.rollback = rollback;
    step
}

pub(crate) fn reconciliation_attempt_key(attempt: &ReconciliationAttempt) -> FailureMemoryKey {
    FailureMemoryKey::for_reconciliation_parts(
        attempt.domain(),
        &attempt.diagnosis_id(),
        attempt.target(),
        attempt.mode(),
        attempt.rung(),
        attempt.component(),
        attempt.scope(),
    )
}

pub(crate) fn reconciliation_memory_entry(
    terminal: ReconciliationTerminal,
) -> Result<GuardianFailureMemoryEntry, ReconciliationEvidenceRejection> {
    let outcome = match terminal.outcome() {
        ReconciliationTerminalOutcome::Succeeded => FailureMemoryActionOutcome::Repaired,
        ReconciliationTerminalOutcome::Failed => FailureMemoryActionOutcome::Failed,
    };
    let quarantine_checkpoint = terminal.quarantine_checkpoint().clone();
    let entry = GuardianFailureMemoryEntry::observed(
        terminal.diagnosis_id(),
        terminal.domain(),
        terminal.target().clone(),
        terminal.mode(),
        None,
        terminal.observed_at(),
    )
    .with_action(GuardianActionKind::Repair, outcome)
    .with_repair_attempt()
    .with_suppression_until(terminal.suppression_until())
    .with_reconciliation_terminal(terminal)
    .with_quarantine_checkpoint(quarantine_checkpoint);
    entry
        .validate()
        .map_err(|_| ReconciliationEvidenceRejection::MemoryNotFailed)?;
    Ok(entry)
}

pub(crate) async fn record_reconciliation_journal_success(
    journals: &OperationJournalStore,
    operation_id: &OperationId,
    completed_step: OperationJournalStep,
    terminal: ReconciliationTerminal,
) -> Result<(), OperationJournalStoreError> {
    journals
        .record_reconciliation_success(operation_id, completed_step, terminal)
        .await
}

pub(crate) async fn record_reconciliation_journal_failure(
    journals: &OperationJournalStore,
    operation_id: &OperationId,
    failure_step: OperationJournalStep,
    failure_point: &str,
    terminal: ReconciliationTerminal,
) -> Result<(), OperationJournalStoreError> {
    journals
        .record_reconciliation_failure(operation_id, failure_step, failure_point, terminal)
        .await
}

pub(crate) async fn commit_reconciliation_memory(
    failure_memory: &GuardianFailureMemoryStore,
    entry: GuardianFailureMemoryEntry,
    reservation: &ReconciliationAttemptReservation,
) -> Result<(), FailureMemoryStoreError> {
    failure_memory
        .record_reconciliation_terminal(entry, &reservation.reservation)
        .await
}

pub(crate) fn validate_reconciliation_memory(
    failure_memory: &GuardianFailureMemoryStore,
    entry: &GuardianFailureMemoryEntry,
    reservation: &ReconciliationAttemptReservation,
) -> Result<(), FailureMemoryStoreError> {
    failure_memory.validate_reconciliation_terminal(entry, &reservation.reservation)
}

pub(crate) async fn settle_reconciliation_memory(
    failure_memory: &GuardianFailureMemoryStore,
) -> Result<(), FailureMemoryStoreError> {
    failure_memory.settle_reconciliation_pending().await
}

pub(crate) fn reserve_reconciliation_attempt(
    failure_memory: &GuardianFailureMemoryStore,
    journals: &OperationJournalStore,
    key: FailureMemoryKey,
) -> Result<ReconciliationAttemptReservation, ReconciliationAttemptRejection> {
    if journals.list().iter().any(|journal| {
        matches!(
            journal.status,
            OperationStatus::Planned | OperationStatus::Running
        ) && journal.reconciliation_terminal().is_none()
            && journal
                .reconciliation_attempt()
                .is_some_and(|attempt| reconciliation_attempt_key(attempt) == key)
    }) {
        return Err(ReconciliationAttemptRejection::AmbiguousPriorAttempt);
    }
    failure_memory
        .reserve_reconciliation_attempt(key)
        .map(|reservation| ReconciliationAttemptReservation { reservation })
        .map_err(|error| match error {
            ReconciliationAttemptReserveError::PersistencePending => {
                ReconciliationAttemptRejection::PersistencePending
            }
            ReconciliationAttemptReserveError::AlreadyReserved => {
                ReconciliationAttemptRejection::AlreadyReserved
            }
            ReconciliationAttemptReserveError::CapacityExhausted => {
                ReconciliationAttemptRejection::CapacityExhausted
            }
        })
}

pub(crate) async fn record_guardian_repair_refusal(
    journals: &OperationJournalStore,
    operation_id: &OperationId,
    skipped_step: OperationJournalStep,
) -> Result<(), OperationJournalStoreError> {
    journals
        .record_guardian_repair_refusal(operation_id, skipped_step)
        .await
}

impl AppState {
    pub(crate) async fn reconcile_reconciliation_startup(&self) -> io::Result<()> {
        self.failure_memory
            .settle_reconciliation_pending()
            .await
            .map_err(|error| {
                io::Error::other(format!(
                    "Guardian reconciliation memory settlement failed: {}",
                    error.class()
                ))
            })?;
        let now = chrono::Utc::now();
        let mut newest = std::collections::BTreeMap::new();
        let journals = self.journals.list();
        for journal in &journals {
            let Some(terminal) = journal.reconciliation_terminal().cloned() else {
                continue;
            };
            if !chrono::DateTime::parse_from_rfc3339(terminal.suppression_until())
                .is_ok_and(|until| until > now)
            {
                continue;
            }
            if terminal.rung() == ReconciliationRung::RematerializeInstance
                && !self.whole_instance_terminal_is_semantically_canonical(journal, &terminal)
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "active whole-instance reconciliation terminal is not canonical",
                ));
            }
            let key = reconciliation_attempt_key(terminal.attempt());
            if newest
                .insert(key.as_str().to_string(), (key, terminal))
                .is_some()
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "overlapping active reconciliation terminals share one memory key",
                ));
            }
        }
        for memory in self.failure_memory.list() {
            let Some(terminal) = memory.reconciliation_terminal() else {
                continue;
            };
            if !chrono::DateTime::parse_from_rfc3339(terminal.suppression_until())
                .is_ok_and(|until| until > now)
            {
                continue;
            }
            let exact_journal = journals.iter().any(|journal| {
                journal.operation_id == *terminal.operation_id()
                    && journal.reconciliation_terminal() == Some(terminal)
                    && (terminal.rung() != ReconciliationRung::RematerializeInstance
                        || self
                            .whole_instance_terminal_is_semantically_canonical(journal, terminal))
            });
            let canonical = reconciliation_memory_entry(terminal.clone()).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "active reconciliation memory cannot be derived from its terminal",
                )
            })?;
            if !exact_journal || canonical != memory {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "active reconciliation memory has no exact journal terminal",
                ));
            }
        }
        for (_, (key, terminal)) in newest {
            let memory = reconciliation_memory_entry(terminal).map_err(|_| {
                io::Error::other("typed reconciliation journal cannot rebuild failure memory")
            })?;
            if self.failure_memory.get(&memory.key).as_ref() == Some(&memory) {
                continue;
            }
            if let Some(existing) = self.failure_memory.get(&memory.key) {
                let prior_until = existing
                    .suppression_until
                    .as_deref()
                    .and_then(|until| chrono::DateTime::parse_from_rfc3339(until).ok());
                let next_observed = chrono::DateTime::parse_from_rfc3339(&memory.last_observed_at)
                    .map_err(|_| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            "typed reconciliation observation timestamp is invalid",
                        )
                    })?;
                if prior_until.is_none_or(|until| until > next_observed) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "overlapping reconciliation memory cannot be superseded",
                    ));
                }
            }
            let reservation = reserve_reconciliation_attempt(
                self.failure_memory.as_ref(),
                self.journals.as_ref(),
                key,
            )
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "typed reconciliation startup attempt is already reserved",
                )
            })?;
            commit_reconciliation_memory(self.failure_memory.as_ref(), memory, &reservation)
                .await
                .map_err(|error| {
                    io::Error::other(format!(
                        "typed reconciliation startup memory commit failed: {}",
                        error.class()
                    ))
                })?;
        }
        Ok(())
    }

    pub(crate) async fn reconcile_user_config_snapshot_startup(&self) -> io::Result<()> {
        if !self.instances.is_authoritative() {
            return Ok(());
        }
        let mut successful = std::collections::BTreeSet::new();
        for journal in self.journals.list() {
            let Some(terminal) = journal.reconciliation_terminal().cloned() else {
                continue;
            };
            if terminal.rung() != ReconciliationRung::RematerializeInstance
                || terminal.outcome() != ReconciliationTerminalOutcome::Succeeded
                || !self.whole_instance_terminal_is_semantically_canonical(&journal, &terminal)
            {
                continue;
            }
            let memory = reconciliation_memory_entry(terminal.clone()).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "successful R3 snapshot terminal cannot derive exact memory",
                )
            })?;
            if self.failure_memory.get(&memory.key).as_ref() != Some(&memory) {
                continue;
            }
            let ReconciliationScope::RegisteredInstance {
                instance_id,
                fingerprint,
                inventory_fingerprint,
                ..
            } = terminal.scope();
            let Some(instance) = self.instances.get(instance_id) else {
                continue;
            };
            if self
                .current_reconciliation_incarnation(instance_id)
                .is_ok_and(|current| {
                    current.fingerprint == *fingerprint
                        && current.inventory_fingerprint == *inventory_fingerprint
                })
            {
                successful.insert((
                    instance_id.clone(),
                    instance.created_at,
                    fingerprint.as_str().to_string(),
                    inventory_fingerprint.as_str().to_string(),
                    terminal.operation_id().as_str().to_string(),
                ));
            }
        }
        self.user_config_snapshots
            .retain_successful_incarnations(&successful)
            .await
    }

    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "Phase 4 backend contract; Phase 6 transport deferred"
        )
    )]
    pub(crate) async fn whole_instance_rematerialization_eligibility(
        &self,
        instance_id: &str,
    ) -> Result<RegisteredWholeInstanceRematerializationEligibility, ReconciliationEvidenceRejection>
    {
        let lifecycle = self
            .try_acquire_instance_lifecycle(instance_id)
            .await
            .ok_or(ReconciliationEvidenceRejection::ActiveSession)?;
        let predecessor = self.active_component_rebuild_failure(&lifecycle)?;
        self.refuse_active_whole_instance_window(instance_id, chrono::Utc::now().fixed_offset())?;
        let eligibility = RegisteredWholeInstanceRematerializationEligibility {
            instance_id: instance_id.to_string(),
            predecessor: predecessor.terminal,
        };
        drop((predecessor.lifecycle, lifecycle));
        Ok(eligibility)
    }

    pub(crate) async fn admit_whole_instance_rematerialization(
        &self,
        authorization: RegisteredWholeInstanceRematerializationAuthorization,
        operation_id: OperationId,
        suppression_for: chrono::Duration,
    ) -> Result<RegisteredWholeInstanceRematerializationAdmission, ReconciliationEvidenceRejection>
    {
        if operation_id == *authorization.predecessor.operation_id() {
            return Err(ReconciliationEvidenceRejection::JournalMismatch);
        }
        let lifecycle = self
            .try_acquire_instance_lifecycle(&authorization.instance_id)
            .await
            .ok_or(ReconciliationEvidenceRejection::ActiveSession)?;
        let identity = self.current_reconciliation_identity(&authorization.instance_id)?;
        let ReconciliationScope::RegisteredInstance {
            instance_id,
            fingerprint,
            inventory_fingerprint,
        } = authorization.predecessor.scope();
        if instance_id != &authorization.instance_id || fingerprint != &identity.fingerprint {
            return Err(ReconciliationEvidenceRejection::IncarnationMismatch);
        }
        let current = self.reconciliation_incarnation_from_identity(identity)?;
        if inventory_fingerprint != &current.inventory_fingerprint {
            return Err(ReconciliationEvidenceRejection::IncarnationMismatch);
        }
        let predecessor_before_wait = self.active_component_rebuild_failure(&lifecycle)?;
        if predecessor_before_wait.terminal != authorization.predecessor {
            return Err(ReconciliationEvidenceRejection::JournalMismatch);
        }
        let config_mutation = self
            .config
            .acquire_mutation()
            .await
            .map_err(|_| ReconciliationEvidenceRejection::RootAuthorityUnavailable)?;
        if GuardianMode::from_config(&self.config.current().guardian_mode)
            != authorization.authorized_policy_mode
        {
            return Err(ReconciliationEvidenceRejection::PolicyModeChanged);
        }
        let component_mutation = self
            .sessions
            .acquire_shared_component_mutation()
            .await
            .ok_or(ReconciliationEvidenceRejection::ActiveSession)?;
        let predecessor = self.active_component_rebuild_failure(&lifecycle)?;
        if predecessor.terminal != predecessor_before_wait.terminal
            || predecessor.roots != predecessor_before_wait.roots
        {
            return Err(ReconciliationEvidenceRejection::JournalMismatch);
        }
        if !std::sync::Arc::ptr_eq(&predecessor.inventory, &predecessor_before_wait.inventory) {
            return Err(ReconciliationEvidenceRejection::IncarnationMismatch);
        }
        let instance = self
            .instances
            .get(&authorization.instance_id)
            .filter(|instance| instance.id == authorization.instance_id)
            .ok_or(ReconciliationEvidenceRejection::InstanceNotRegistered)?;
        let known_good = RegisteredKnownGoodInventory {
            instance_id: instance.id,
            version_id: instance.version_id,
            created_at: instance.created_at,
            library_root: predecessor.roots.library.clone(),
            inventory: predecessor.inventory.clone(),
        };
        let observed_at = chrono::Utc::now().fixed_offset();
        let suppression_until = observed_at
            .checked_add_signed(suppression_for)
            .filter(|until| *until > observed_at)
            .ok_or(ReconciliationEvidenceRejection::JournalMismatch)?;
        let prior = &predecessor.terminal;
        let attempt = self.registered_reconciliation_attempt_at(
            &lifecycle,
            operation_id,
            prior.diagnosis_id(),
            prior.domain(),
            ReconciliationRung::RematerializeInstance,
            ReconciliationComponent::WholeInstance,
            reconciliation_instance_target(&authorization.instance_id),
            prior.mode(),
            observed_at,
            suppression_until,
            ReconciliationLineage::Predecessor {
                operation_id: prior.operation_id().clone(),
            },
        )?;
        if !adjacent_whole_instance_attempts_match(prior.attempt(), &attempt) {
            return Err(ReconciliationEvidenceRejection::NonAdjacentRung);
        }
        self.refuse_active_whole_instance_window(&authorization.instance_id, observed_at)?;
        let managed_artifact_epoch = self
            .capture_managed_artifact_mutation_epoch()
            .map_err(|_| ReconciliationEvidenceRejection::IncarnationMismatch)?;
        Ok(RegisteredWholeInstanceRematerializationAdmission {
            state: self.clone(),
            attempt,
            predecessor: predecessor.terminal,
            known_good,
            managed_artifact_epoch: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(
                managed_artifact_epoch.value(),
            )),
            lifecycle,
            component_mutation,
            config_mutation,
        })
    }

    pub(crate) fn registered_reconciliation_authority(
        &self,
        lifecycle: &InstanceLifecycleLease,
    ) -> Result<RegisteredReconciliationAuthority, ReconciliationEvidenceRejection> {
        if !self.instance_lifecycle_gates.owns(&lifecycle.owner) {
            return Err(ReconciliationEvidenceRejection::ScopeMismatch);
        }
        self.current_reconciliation_incarnation(&lifecycle.instance_id)?;
        Ok(RegisteredReconciliationAuthority {
            state: self.clone(),
            lifecycle: lifecycle.retained(),
            verification: None,
            managed_artifact_epoch: None,
        })
    }

    pub(super) fn registered_reconciliation_authority_for_verification(
        &self,
        verification: &KnownGoodVerificationLease,
    ) -> Result<RegisteredReconciliationAuthority, ReconciliationEvidenceRejection> {
        if !self.known_good_verification_lease_can_admit(verification) {
            return Err(ReconciliationEvidenceRejection::IncarnationMismatch);
        }
        self.current_reconciliation_incarnation(&verification._lifecycle.instance_id)?;
        Ok(RegisteredReconciliationAuthority {
            state: self.clone(),
            lifecycle: verification._lifecycle.retained(),
            managed_artifact_epoch: verification.managed_artifact_epoch.clone(),
            verification: Some(verification.retained()),
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn registered_reconciliation_attempt_at(
        &self,
        lifecycle: &InstanceLifecycleLease,
        operation_id: OperationId,
        diagnosis_id: DiagnosisId,
        domain: GuardianDomain,
        rung: ReconciliationRung,
        component: ReconciliationComponent,
        target: TargetDescriptor,
        mode: GuardianMode,
        observed_at: chrono::DateTime<chrono::FixedOffset>,
        suppression_until: chrono::DateTime<chrono::FixedOffset>,
        lineage: ReconciliationLineage,
    ) -> Result<ReconciliationAttempt, ReconciliationEvidenceRejection> {
        if !self.instance_lifecycle_gates.owns(&lifecycle.owner) {
            return Err(ReconciliationEvidenceRejection::ScopeMismatch);
        }
        let incarnation = self.current_reconciliation_incarnation(&lifecycle.instance_id)?;
        let attempt = ReconciliationAttempt::new(
            operation_id,
            diagnosis_id,
            domain,
            rung,
            ReconciliationScope::RegisteredInstance {
                instance_id: lifecycle.instance_id.clone(),
                fingerprint: incarnation.fingerprint,
                inventory_fingerprint: incarnation.inventory_fingerprint,
            },
            component,
            target,
            mode,
            OwnershipClass::LauncherManaged,
            observed_at.to_rfc3339(),
            suppression_until.to_rfc3339(),
            lineage,
        );
        attempt
            .validate()
            .map_err(|_| ReconciliationEvidenceRejection::JournalMismatch)?;
        Ok(attempt)
    }

    pub(crate) fn recorded_runtime_artifact_repair_failure(
        &self,
        lifecycle: &InstanceLifecycleLease,
        operation_id: &OperationId,
    ) -> Result<RecordedRuntimeArtifactRepairFailure, ReconciliationEvidenceRejection> {
        let evidence = self.recorded_reconciliation_failure_at(
            lifecycle,
            operation_id,
            ReconciliationRung::RepairArtifact,
            chrono::Utc::now().fixed_offset(),
            None,
        )?;
        let attempt = evidence.terminal.attempt();
        if attempt.rung() != ReconciliationRung::RepairArtifact
            || attempt.component() != ReconciliationComponent::Runtime
            || attempt.domain() != GuardianDomain::Runtime
            || attempt.mode() != GuardianMode::Managed
            || attempt.ownership() != OwnershipClass::LauncherManaged
            || attempt.target().system != StabilizationSystem::Execution
            || attempt.target().kind != TargetKind::Runtime
            || attempt.target().ownership != OwnershipClass::LauncherManaged
            || !is_known_runtime_component(&attempt.target().id)
        {
            return Err(ReconciliationEvidenceRejection::ScopeMismatch);
        }
        Ok(RecordedRuntimeArtifactRepairFailure { evidence })
    }

    #[cfg(test)]
    pub(crate) fn recorded_verified_registered_artifact_failure_for_test(
        &self,
        verification: KnownGoodVerificationLease,
        attempt: &ReconciliationAttempt,
    ) -> Result<RegisteredArtifactFailedRepair, ReconciliationEvidenceRejection> {
        self.registered_reconciliation_authority_for_verification(&verification)?
            .into_registered_artifact_failed_repair(attempt)
    }

    pub(crate) fn registered_artifact_recovery_entry(
        &self,
        authorization: RegisteredArtifactRepairAuthorization,
    ) -> Result<RegisteredArtifactRecoveryEntry, ReconciliationEvidenceRejection> {
        let (verification, inventory_ordinal, component, effect) =
            authorization.exact_recovery_identity(self)?;
        if effect != RegisteredArtifactRepairEffect::ComponentRebuildRequired {
            return Ok(RegisteredArtifactRecoveryEntry::Fresh(authorization));
        }
        let resume = self.recorded_registered_artifact_failure_for_authorization(
            verification,
            inventory_ordinal,
            component,
        )?;
        Ok(match resume {
            Some(continuation) => RegisteredArtifactRecoveryEntry::Resume(continuation),
            None => RegisteredArtifactRecoveryEntry::Fresh(authorization),
        })
    }

    fn recorded_registered_artifact_failure_for_authorization(
        &self,
        verification: &KnownGoodVerificationLease,
        inventory_ordinal: usize,
        component: ReconciliationComponent,
    ) -> Result<Option<RegisteredArtifactFailedRepair>, ReconciliationEvidenceRejection> {
        if !self.known_good_verification_lease_can_admit(verification) {
            return Err(ReconciliationEvidenceRejection::IncarnationMismatch);
        }
        let (instance_id, version_id, created_at, library_root, runtime_cache, inventory) =
            verification.execution_parts();
        let expected_target = registered_artifact_target(verification, inventory_ordinal)
            .ok_or(ReconciliationEvidenceRejection::ScopeMismatch)?;
        let current = self.current_reconciliation_incarnation(instance_id)?;
        if current.roots.library != library_root || current.roots.runtime != runtime_cache.root() {
            return Err(ReconciliationEvidenceRejection::IncarnationMismatch);
        }
        let observed_at = chrono::Utc::now().fixed_offset();
        let matches_exact_candidate = |attempt: &ReconciliationAttempt| {
            attempt.rung() == ReconciliationRung::RepairArtifact
                && attempt.target() == &expected_target
                && matches!(
                    attempt.scope(),
                    ReconciliationScope::RegisteredInstance {
                        instance_id: attempted_instance_id,
                        fingerprint,
                        inventory_fingerprint,
                    } if attempted_instance_id == instance_id
                        && fingerprint == &current.fingerprint
                        && inventory_fingerprint == &current.inventory_fingerprint
                )
        };

        let journals = self.journals.list();
        let mut active_journals = Vec::new();
        for journal in &journals {
            let Some(attempt) = journal.reconciliation_attempt() else {
                continue;
            };
            if !matches_exact_candidate(attempt) {
                continue;
            }
            let Some(terminal) = journal.reconciliation_terminal() else {
                return Err(ReconciliationEvidenceRejection::JournalMismatch);
            };
            if !active_reconciliation_terminal_at(terminal, observed_at)? {
                continue;
            }
            if terminal.outcome() != ReconciliationTerminalOutcome::Failed {
                return Err(ReconciliationEvidenceRejection::MemoryNotFailed);
            }
            if journal.failure_point.as_deref()
                != Some(REGISTERED_ARTIFACT_COMPONENT_REBUILD_FAILURE_POINT)
                || !registered_component_required_terminal_matches(journal, terminal, instance_id)
            {
                return Err(ReconciliationEvidenceRejection::JournalMismatch);
            }
            active_journals.push(terminal.clone());
        }

        let mut active_memories = Vec::new();
        for memory in self.failure_memory.list() {
            let Some(terminal) = memory.reconciliation_terminal().cloned() else {
                continue;
            };
            if !matches_exact_candidate(terminal.attempt())
                || !active_reconciliation_terminal_at(&terminal, observed_at)?
            {
                continue;
            }
            if terminal.outcome() != ReconciliationTerminalOutcome::Failed {
                return Err(ReconciliationEvidenceRejection::MemoryNotFailed);
            }
            if memory != reconciliation_memory_entry(terminal.clone())? {
                return Err(ReconciliationEvidenceRejection::MemoryNotFailed);
            }
            active_memories.push(terminal);
        }

        let terminal = match (active_journals.as_slice(), active_memories.as_slice()) {
            ([], []) => return Ok(None),
            ([_], []) => return Err(ReconciliationEvidenceRejection::MemoryMissing),
            ([], [_]) => return Err(ReconciliationEvidenceRejection::JournalMissing),
            ([journal], [memory]) if journal == memory => journal,
            _ => return Err(ReconciliationEvidenceRejection::JournalMismatch),
        };
        let provenance = resolve_recorded_artifact_provenance(
            instance_id,
            version_id,
            created_at,
            library_root,
            runtime_cache.root(),
            inventory,
            terminal.attempt(),
        )
        .ok_or(ReconciliationEvidenceRejection::ScopeMismatch)?;
        if ManagedArtifactRebuildComponent::from_artifact_attempt(terminal.attempt())?
            .reconciliation_component()
            != component
            || terminal.target() != &expected_target
            || provenance.component() != component
            || provenance.inventory_ordinal() != inventory_ordinal
        {
            return Err(ReconciliationEvidenceRejection::ScopeMismatch);
        }
        let evidence = self.recorded_reconciliation_failure_at(
            &verification._lifecycle,
            terminal.operation_id(),
            ReconciliationRung::RepairArtifact,
            observed_at,
            Some(verification),
        )?;
        if &evidence.terminal != terminal
            || evidence.artifact_provenance != Some(provenance)
            || !std::sync::Arc::ptr_eq(&evidence.inventory, &verification.inventory)
        {
            return Err(ReconciliationEvidenceRejection::IncarnationMismatch);
        }
        let authority = self.registered_reconciliation_authority_for_verification(verification)?;
        let continuation = authority.into_registered_artifact_failed_repair(terminal.attempt())?;
        if continuation.evidence.artifact_provenance != Some(provenance) {
            return Err(ReconciliationEvidenceRejection::ScopeMismatch);
        }
        Ok(Some(continuation))
    }

    pub(crate) fn active_recorded_runtime_artifact_failure(
        &self,
        lifecycle: &InstanceLifecycleLease,
    ) -> Result<RecordedRuntimeArtifactRepairFailure, ReconciliationEvidenceRejection> {
        if !self.instance_lifecycle_gates.owns(&lifecycle.owner) {
            return Err(ReconciliationEvidenceRejection::ScopeMismatch);
        }
        let current = self.current_reconciliation_incarnation(&lifecycle.instance_id)?;
        let observed_at = chrono::Utc::now().fixed_offset();
        let matches_current_runtime = |attempt: &ReconciliationAttempt| {
            attempt.rung() == ReconciliationRung::RepairArtifact
                && attempt.component() == ReconciliationComponent::Runtime
                && attempt.domain() == GuardianDomain::Runtime
                && attempt.mode() == GuardianMode::Managed
                && attempt.ownership() == OwnershipClass::LauncherManaged
                && attempt.target().system == StabilizationSystem::Execution
                && attempt.target().kind == TargetKind::Runtime
                && attempt.target().ownership == OwnershipClass::LauncherManaged
                && is_known_runtime_component(&attempt.target().id)
                && matches!(
                    attempt.scope(),
                    ReconciliationScope::RegisteredInstance {
                        instance_id,
                        fingerprint,
                        inventory_fingerprint,
                    } if instance_id == &lifecycle.instance_id
                        && fingerprint == &current.fingerprint
                        && inventory_fingerprint == &current.inventory_fingerprint
                )
        };

        let journals = self.journals.list();
        for journal in &journals {
            let Some(attempt) = journal.reconciliation_attempt() else {
                continue;
            };
            if matches!(
                journal.status,
                OperationStatus::Planned | OperationStatus::Running
            ) && journal.reconciliation_terminal().is_none()
                && matches_current_runtime(attempt)
            {
                return Err(ReconciliationEvidenceRejection::JournalMismatch);
            }
        }

        let active_journals = journals
            .iter()
            .filter_map(|journal| journal.reconciliation_terminal())
            .filter(|terminal| {
                terminal.outcome() == ReconciliationTerminalOutcome::Failed
                    && matches_current_runtime(terminal.attempt())
            })
            .map(|terminal| {
                active_reconciliation_terminal_at(terminal, observed_at)
                    .map(|active| active.then_some(terminal.clone()))
            })
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        let mut active_memories = Vec::new();
        for memory in self.failure_memory.list() {
            let Some(terminal) = memory.reconciliation_terminal().cloned() else {
                continue;
            };
            if terminal.outcome() != ReconciliationTerminalOutcome::Failed
                || !matches_current_runtime(terminal.attempt())
                || !active_reconciliation_terminal_at(&terminal, observed_at)?
            {
                continue;
            }
            if memory != reconciliation_memory_entry(terminal.clone())? {
                return Err(ReconciliationEvidenceRejection::JournalMismatch);
            }
            active_memories.push(terminal);
        }
        if active_journals.is_empty() && active_memories.is_empty() {
            return Err(ReconciliationEvidenceRejection::MemoryMissing);
        }
        if active_journals.len() != 1
            || active_memories.len() != 1
            || active_journals[0] != active_memories[0]
        {
            return Err(ReconciliationEvidenceRejection::JournalMismatch);
        }
        let evidence = self.recorded_reconciliation_failure_at(
            lifecycle,
            active_journals[0].operation_id(),
            ReconciliationRung::RepairArtifact,
            observed_at,
            None,
        )?;
        Ok(RecordedRuntimeArtifactRepairFailure { evidence })
    }

    fn active_component_rebuild_failure(
        &self,
        lifecycle: &InstanceLifecycleLease,
    ) -> Result<RecordedReconciliationFailure, ReconciliationEvidenceRejection> {
        if !self.instance_lifecycle_gates.owns(&lifecycle.owner) {
            return Err(ReconciliationEvidenceRejection::ScopeMismatch);
        }
        let current = self.current_reconciliation_incarnation(&lifecycle.instance_id)?;
        let observed_at = chrono::Utc::now().fixed_offset();
        let matches_current = |attempt: &ReconciliationAttempt| {
            attempt.rung() == ReconciliationRung::RebuildComponent
                && matches!(attempt.mode(), GuardianMode::Managed | GuardianMode::Custom)
                && attempt.ownership() == OwnershipClass::LauncherManaged
                && attempt.component() != ReconciliationComponent::WholeInstance
                && matches!(
                    attempt.scope(),
                    ReconciliationScope::RegisteredInstance {
                        instance_id,
                        fingerprint,
                        inventory_fingerprint,
                    } if instance_id == &lifecycle.instance_id
                        && fingerprint == &current.fingerprint
                        && inventory_fingerprint == &current.inventory_fingerprint
                )
        };
        let journals = self.journals.list();
        let mut active_journals = Vec::new();
        for journal in &journals {
            let Some(attempt) = journal.reconciliation_attempt() else {
                continue;
            };
            if !matches_current(attempt) {
                continue;
            }
            let Some(terminal) = journal.reconciliation_terminal() else {
                return Err(ReconciliationEvidenceRejection::JournalMismatch);
            };
            if !active_reconciliation_terminal_at(terminal, observed_at)? {
                continue;
            }
            if terminal.outcome() != ReconciliationTerminalOutcome::Failed {
                return Err(ReconciliationEvidenceRejection::MemoryNotFailed);
            }
            if !component_rebuild_terminal_matches(journal, terminal, &lifecycle.instance_id) {
                return Err(ReconciliationEvidenceRejection::JournalMismatch);
            }
            active_journals.push(terminal.clone());
        }
        let mut active_memories = Vec::new();
        for memory in self.failure_memory.list() {
            let Some(terminal) = memory.reconciliation_terminal().cloned() else {
                continue;
            };
            if !matches_current(terminal.attempt())
                || !active_reconciliation_terminal_at(&terminal, observed_at)?
            {
                continue;
            }
            if terminal.outcome() != ReconciliationTerminalOutcome::Failed {
                return Err(ReconciliationEvidenceRejection::MemoryNotFailed);
            }
            if memory != reconciliation_memory_entry(terminal.clone())? {
                return Err(ReconciliationEvidenceRejection::MemoryNotFailed);
            }
            active_memories.push(terminal);
        }
        let terminal = match (active_journals.as_slice(), active_memories.as_slice()) {
            ([], []) => return Err(ReconciliationEvidenceRejection::MemoryMissing),
            ([_], []) => return Err(ReconciliationEvidenceRejection::MemoryMissing),
            ([], [_]) => return Err(ReconciliationEvidenceRejection::JournalMissing),
            ([journal], [memory]) if journal == memory => journal,
            _ => return Err(ReconciliationEvidenceRejection::JournalMismatch),
        };
        let evidence = self.recorded_reconciliation_failure_at(
            lifecycle,
            terminal.operation_id(),
            ReconciliationRung::RebuildComponent,
            observed_at,
            None,
        )?;
        if &evidence.terminal != terminal {
            return Err(ReconciliationEvidenceRejection::JournalMismatch);
        }
        Ok(evidence)
    }

    pub(crate) async fn admit_runtime_component_rebuild(
        &self,
        evidence: RecordedRuntimeArtifactRepairFailure,
        operation_id: OperationId,
        suppression_for: chrono::Duration,
    ) -> Result<RegisteredComponentRebuildAdmission, ReconciliationEvidenceRejection> {
        let attempt = evidence.evidence.terminal.attempt();
        if attempt.component() != ReconciliationComponent::Runtime
            || attempt.domain() != GuardianDomain::Runtime
            || attempt.mode() != GuardianMode::Managed
            || attempt.ownership() != OwnershipClass::LauncherManaged
            || attempt.target().system != StabilizationSystem::Execution
            || attempt.target().kind != TargetKind::Runtime
            || attempt.target().ownership != OwnershipClass::LauncherManaged
            || !is_known_runtime_component(&attempt.target().id)
        {
            return Err(ReconciliationEvidenceRejection::ScopeMismatch);
        }
        self.admit_component_rebuild_with_config_observer(
            evidence,
            None,
            operation_id,
            suppression_for,
            || {},
        )
        .await
    }

    pub(crate) async fn admit_registered_artifact_component_rebuild(
        &self,
        continuation: RegisteredArtifactFailedRepair,
        operation_id: OperationId,
        suppression_for: chrono::Duration,
    ) -> Result<RegisteredComponentRebuildAdmission, ReconciliationEvidenceRejection> {
        let RegisteredArtifactFailedRepair {
            evidence,
            verification,
        } = continuation;
        self.admit_component_rebuild_with_config_observer(
            RecordedRuntimeArtifactRepairFailure { evidence },
            Some(verification),
            operation_id,
            suppression_for,
            || {},
        )
        .await
    }

    async fn admit_component_rebuild_with_config_observer<AfterConfig>(
        &self,
        evidence: RecordedRuntimeArtifactRepairFailure,
        verification: Option<KnownGoodVerificationLease>,
        operation_id: OperationId,
        suppression_for: chrono::Duration,
        after_config: AfterConfig,
    ) -> Result<RegisteredComponentRebuildAdmission, ReconciliationEvidenceRejection>
    where
        AfterConfig: FnOnce(),
    {
        if operation_id == *evidence.evidence.terminal.operation_id() {
            return Err(ReconciliationEvidenceRejection::JournalMismatch);
        }
        if verification
            .as_ref()
            .is_some_and(|verification| !self.known_good_verification_lease_can_admit(verification))
        {
            return Err(ReconciliationEvidenceRejection::IncarnationMismatch);
        }
        validate_registered_artifact_provenance(
            verification.as_ref(),
            evidence.evidence.artifact_provenance,
            &evidence.evidence.terminal,
        )?;
        let predecessor_before_wait = self.recorded_reconciliation_failure_at(
            &evidence.evidence.lifecycle,
            evidence.evidence.terminal.operation_id(),
            ReconciliationRung::RepairArtifact,
            chrono::Utc::now().fixed_offset(),
            verification.as_ref(),
        )?;
        if predecessor_before_wait.terminal != evidence.evidence.terminal
            || predecessor_before_wait.roots != evidence.evidence.roots
            || predecessor_before_wait.artifact_provenance != evidence.evidence.artifact_provenance
        {
            return Err(ReconciliationEvidenceRejection::JournalMismatch);
        }
        if !std::sync::Arc::ptr_eq(
            &predecessor_before_wait.inventory,
            &evidence.evidence.inventory,
        ) {
            return Err(ReconciliationEvidenceRejection::IncarnationMismatch);
        }
        validate_registered_artifact_provenance(
            verification.as_ref(),
            predecessor_before_wait.artifact_provenance,
            &predecessor_before_wait.terminal,
        )?;
        // Config precedes the shared-component writer: config mutations never acquire
        // session admission, while session admission owns only the component reader.
        let config_mutation = self
            .config
            .acquire_mutation()
            .await
            .map_err(|_| ReconciliationEvidenceRejection::RootAuthorityUnavailable)?;
        after_config();
        if verification
            .as_ref()
            .is_some_and(|verification| !self.known_good_verification_lease_can_admit(verification))
        {
            return Err(ReconciliationEvidenceRejection::IncarnationMismatch);
        }
        let component_mutation = self
            .sessions
            .acquire_shared_component_mutation()
            .await
            .ok_or(ReconciliationEvidenceRejection::ActiveSession)?;
        let predecessor = self.recorded_reconciliation_failure_at(
            &predecessor_before_wait.lifecycle,
            predecessor_before_wait.terminal.operation_id(),
            ReconciliationRung::RepairArtifact,
            chrono::Utc::now().fixed_offset(),
            verification.as_ref(),
        )?;
        if predecessor.terminal != predecessor_before_wait.terminal
            || predecessor.roots != predecessor_before_wait.roots
            || predecessor.artifact_provenance != predecessor_before_wait.artifact_provenance
        {
            return Err(ReconciliationEvidenceRejection::JournalMismatch);
        }
        if !std::sync::Arc::ptr_eq(&predecessor.inventory, &predecessor_before_wait.inventory) {
            return Err(ReconciliationEvidenceRejection::IncarnationMismatch);
        }
        validate_registered_artifact_provenance(
            verification.as_ref(),
            predecessor.artifact_provenance,
            &predecessor.terminal,
        )?;
        if verification
            .as_ref()
            .is_some_and(|verification| !self.known_good_verification_lease_can_admit(verification))
        {
            return Err(ReconciliationEvidenceRejection::IncarnationMismatch);
        }
        let instance = self
            .instances
            .get(&predecessor.lifecycle.instance_id)
            .filter(|instance| instance.id == predecessor.lifecycle.instance_id.as_str())
            .ok_or(ReconciliationEvidenceRejection::InstanceNotRegistered)?;
        let known_good = RegisteredKnownGoodInventory {
            instance_id: instance.id,
            version_id: instance.version_id,
            created_at: instance.created_at,
            library_root: predecessor.roots.library.clone(),
            inventory: predecessor.inventory.clone(),
        };
        let authority = match verification.as_ref() {
            Some(verification) => {
                self.registered_reconciliation_authority_for_verification(verification)?
            }
            None => self.registered_reconciliation_authority(&predecessor.lifecycle)?,
        };
        let artifact_provenance = predecessor.artifact_provenance;
        let prior = predecessor.terminal;
        let component_state = match prior.component() {
            ReconciliationComponent::Runtime => RegisteredComponentRebuildState::Runtime {
                runtime_cache: self.managed_runtime_cache.clone(),
                postcondition_failure_inventory: std::sync::OnceLock::new(),
            },
            ReconciliationComponent::VersionBundle
            | ReconciliationComponent::Libraries
            | ReconciliationComponent::Assets => {
                let component =
                    ManagedArtifactRebuildComponent::from_artifact_attempt(prior.attempt())?;
                if artifact_provenance.is_none_or(|provenance| {
                    provenance.component() != component.reconciliation_component()
                }) {
                    return Err(ReconciliationEvidenceRejection::ScopeMismatch);
                }
                match component {
                    ManagedArtifactRebuildComponent::VersionBundle => {
                        RegisteredComponentRebuildState::VersionBundle
                    }
                    ManagedArtifactRebuildComponent::Libraries => {
                        RegisteredComponentRebuildState::Libraries
                    }
                    ManagedArtifactRebuildComponent::Assets => {
                        RegisteredComponentRebuildState::Assets
                    }
                }
            }
            ReconciliationComponent::WholeInstance => {
                return Err(ReconciliationEvidenceRejection::ScopeMismatch);
            }
        };
        let observed_at = chrono::Utc::now().fixed_offset();
        let suppression_until = observed_at
            .checked_add_signed(suppression_for)
            .filter(|until| *until > observed_at)
            .ok_or(ReconciliationEvidenceRejection::JournalMismatch)?;
        let attempt = self.registered_reconciliation_attempt_at(
            &predecessor.lifecycle,
            operation_id,
            prior.diagnosis_id(),
            prior.domain(),
            ReconciliationRung::RebuildComponent,
            prior.component(),
            prior.target().clone(),
            prior.mode(),
            observed_at,
            suppression_until,
            ReconciliationLineage::Predecessor {
                operation_id: prior.operation_id().clone(),
            },
        )?;
        self.refuse_active_component_rebuild_window(&attempt, observed_at)?;
        let failed_terminal =
            authority.terminal(attempt.clone(), ReconciliationTerminalOutcome::Failed)?;
        Ok(RegisteredComponentRebuildAdmission {
            authority,
            attempt,
            failed_terminal,
            known_good,
            artifact_provenance,
            component_state,
            _component_mutation: component_mutation,
            _config_mutation: config_mutation,
        })
    }

    fn refuse_active_component_rebuild_window(
        &self,
        attempt: &ReconciliationAttempt,
        observed_at: chrono::DateTime<chrono::FixedOffset>,
    ) -> Result<(), ReconciliationEvidenceRejection> {
        let matches_suppression = |candidate: &ReconciliationAttempt| {
            if attempt.component() == ReconciliationComponent::Runtime {
                candidate.component() == ReconciliationComponent::Runtime
                    && candidate.target() == attempt.target()
            } else {
                reconciliation_attempt_key(candidate) == reconciliation_attempt_key(attempt)
            }
        };
        self.refuse_active_reconciliation_window(
            ReconciliationRung::RebuildComponent,
            observed_at,
            matches_suppression,
        )
    }

    fn refuse_active_whole_instance_window(
        &self,
        instance_id: &str,
        observed_at: chrono::DateTime<chrono::FixedOffset>,
    ) -> Result<(), ReconciliationEvidenceRejection> {
        for journal in self.journals.list() {
            let Some(terminal) = journal.reconciliation_terminal() else {
                continue;
            };
            let ReconciliationScope::RegisteredInstance {
                instance_id: candidate_instance_id,
                ..
            } = terminal.scope();
            if terminal.rung() == ReconciliationRung::RematerializeInstance
                && candidate_instance_id == instance_id
                && active_reconciliation_terminal_at(terminal, observed_at)?
                && !self.whole_instance_terminal_is_semantically_canonical(&journal, terminal)
            {
                return Err(ReconciliationEvidenceRejection::JournalMismatch);
            }
        }
        self.refuse_active_reconciliation_window(
            ReconciliationRung::RematerializeInstance,
            observed_at,
            |candidate| {
                matches!(
                    candidate.scope(),
                    ReconciliationScope::RegisteredInstance {
                        instance_id: candidate_instance_id,
                        ..
                    } if candidate_instance_id == instance_id
                )
            },
        )
    }

    fn whole_instance_terminal_is_semantically_canonical(
        &self,
        journal: &OperationJournalEntry,
        terminal: &ReconciliationTerminal,
    ) -> bool {
        if !whole_instance_terminal_matches(journal, terminal) {
            return false;
        }
        let ReconciliationScope::RegisteredInstance {
            instance_id,
            fingerprint,
            inventory_fingerprint,
        } = terminal.scope();
        let Ok(current) = self.current_reconciliation_incarnation(instance_id) else {
            return false;
        };
        if fingerprint != &current.fingerprint
            || inventory_fingerprint != &current.inventory_fingerprint
        {
            return false;
        }
        if let [ReconciliationQuarantineRecord::RuntimeComponent { component_id }] =
            terminal.quarantine_checkpoint().records()
            && !current.inventory.entries().iter().any(|entry| {
                matches!(entry.root(), KnownGoodRoot::ManagedRuntime { .. })
                    && entry.root().scope_id() == component_id
            })
        {
            return false;
        }
        let ReconciliationLineage::Predecessor {
            operation_id: predecessor_operation_id,
        } = terminal.attempt().lineage()
        else {
            return false;
        };
        let Some(predecessor_journal) = self.journals.get(predecessor_operation_id) else {
            return false;
        };
        let Some(predecessor) = predecessor_journal.reconciliation_terminal() else {
            return false;
        };
        if predecessor.operation_id() != predecessor_operation_id
            || predecessor.outcome() != ReconciliationTerminalOutcome::Failed
            || !component_rebuild_terminal_matches(&predecessor_journal, predecessor, instance_id)
            || !adjacent_whole_instance_attempts_match(predecessor.attempt(), terminal.attempt())
        {
            return false;
        }
        let key = reconciliation_attempt_key(predecessor.attempt());
        let Ok(expected_memory) = reconciliation_memory_entry(predecessor.clone()) else {
            return false;
        };
        self.failure_memory.get(&key).as_ref() == Some(&expected_memory)
    }

    pub(crate) fn refuse_active_artifact_repair_window(
        &self,
        attempt: &ReconciliationAttempt,
    ) -> Result<(), ReconciliationEvidenceRejection> {
        let key = reconciliation_attempt_key(attempt);
        self.refuse_active_reconciliation_window(
            ReconciliationRung::RepairArtifact,
            chrono::Utc::now().fixed_offset(),
            move |candidate| reconciliation_attempt_key(candidate) == key,
        )
    }

    fn refuse_active_reconciliation_window<Matches>(
        &self,
        rung: ReconciliationRung,
        observed_at: chrono::DateTime<chrono::FixedOffset>,
        matches_suppression: Matches,
    ) -> Result<(), ReconciliationEvidenceRejection>
    where
        Matches: Fn(&ReconciliationAttempt) -> bool,
    {
        let matches_window = |attempt: &ReconciliationAttempt| {
            attempt.rung() == rung && matches_suppression(attempt)
        };
        let journals = self.journals.list();
        if journals.iter().any(|journal| {
            matches!(
                journal.status,
                OperationStatus::Planned | OperationStatus::Running
            ) && journal.reconciliation_terminal().is_none()
                && journal
                    .reconciliation_attempt()
                    .is_some_and(&matches_window)
        }) {
            return Err(ReconciliationEvidenceRejection::JournalMismatch);
        }
        let mut active_journals = Vec::new();
        for journal in journals {
            let Some(terminal) = journal.reconciliation_terminal().cloned() else {
                continue;
            };
            if !matches_window(terminal.attempt())
                || !active_reconciliation_terminal_at(&terminal, observed_at)?
            {
                continue;
            }
            active_journals.push(terminal);
        }

        let mut active_memories = Vec::new();
        for memory in self.failure_memory.list() {
            let Some(terminal) = memory.reconciliation_terminal().cloned() else {
                continue;
            };
            if !matches_window(terminal.attempt())
                || !active_reconciliation_terminal_at(&terminal, observed_at)?
            {
                continue;
            }
            let canonical = reconciliation_memory_entry(terminal.clone())?;
            if memory != canonical {
                return Err(ReconciliationEvidenceRejection::JournalMismatch);
            }
            active_memories.push(terminal);
        }

        if active_journals.len() != active_memories.len()
            || active_journals.iter().any(|journal| {
                active_memories
                    .iter()
                    .filter(|memory| *memory == journal)
                    .count()
                    != 1
            })
            || active_memories.iter().any(|memory| {
                active_journals
                    .iter()
                    .filter(|journal| *journal == memory)
                    .count()
                    != 1
            })
        {
            return Err(ReconciliationEvidenceRejection::JournalMismatch);
        }
        let max_attempts = rung.max_attempts_per_suppression_window();
        if active_journals.len() > max_attempts {
            return Err(ReconciliationEvidenceRejection::JournalMismatch);
        }
        if active_journals.len() == max_attempts {
            return Err(ReconciliationEvidenceRejection::SuppressedPriorAttempt);
        }
        Ok(())
    }

    fn recorded_reconciliation_failure_at(
        &self,
        lifecycle: &InstanceLifecycleLease,
        operation_id: &OperationId,
        expected_rung: ReconciliationRung,
        observed_at: chrono::DateTime<chrono::FixedOffset>,
        verification: Option<&KnownGoodVerificationLease>,
    ) -> Result<RecordedReconciliationFailure, ReconciliationEvidenceRejection> {
        if !self.instance_lifecycle_gates.owns(&lifecycle.owner) {
            return Err(ReconciliationEvidenceRejection::ScopeMismatch);
        }
        let instance_id = lifecycle.instance_id.as_str();
        if verification.is_some_and(|verification| {
            !verification._lifecycle.matches(instance_id)
                || !self.known_good_verification_lease_is_live(verification)
        }) {
            return Err(ReconciliationEvidenceRejection::IncarnationMismatch);
        }
        if !is_canonical_instance_id(instance_id) {
            return Err(ReconciliationEvidenceRejection::InvalidInstanceIdentity);
        }
        let before_identity = self.current_reconciliation_identity(instance_id)?;
        let journal = self
            .journals
            .get(operation_id)
            .ok_or(ReconciliationEvidenceRejection::JournalMissing)?;
        let terminal = journal
            .reconciliation_terminal()
            .cloned()
            .filter(|terminal| terminal.operation_id() == operation_id)
            .ok_or(ReconciliationEvidenceRejection::JournalMismatch)?;
        let key = reconciliation_attempt_key(terminal.attempt());
        let memory = self
            .failure_memory
            .get(&key)
            .ok_or(ReconciliationEvidenceRejection::MemoryMissing)?;
        if memory.reconciliation_terminal() != Some(&terminal)
            || memory != reconciliation_memory_entry(terminal.clone())?
        {
            return Err(ReconciliationEvidenceRejection::MemoryNotFailed);
        }
        if terminal.rung() != expected_rung {
            return Err(ReconciliationEvidenceRejection::NonAdjacentRung);
        }
        if terminal.outcome() != ReconciliationTerminalOutcome::Failed {
            return Err(ReconciliationEvidenceRejection::MemoryNotFailed);
        }
        if terminal.ownership() != OwnershipClass::LauncherManaged {
            return Err(ReconciliationEvidenceRejection::OwnershipMismatch);
        }
        let last_observed_at = chrono::DateTime::parse_from_rfc3339(&memory.last_observed_at)
            .map_err(|_| ReconciliationEvidenceRejection::MemoryWindowInactive)?;
        let suppression_until = chrono::DateTime::parse_from_rfc3339(
            memory
                .suppression_until
                .as_deref()
                .ok_or(ReconciliationEvidenceRejection::MemoryWindowInactive)?,
        )
        .map_err(|_| ReconciliationEvidenceRejection::MemoryWindowInactive)?;
        if observed_at < last_observed_at || observed_at >= suppression_until {
            return Err(ReconciliationEvidenceRejection::MemoryWindowInactive);
        }
        let ReconciliationScope::RegisteredInstance {
            instance_id: terminal_instance_id,
            fingerprint,
            inventory_fingerprint,
        } = terminal.scope();
        if terminal_instance_id != instance_id || fingerprint != &before_identity.fingerprint {
            return Err(ReconciliationEvidenceRejection::IncarnationMismatch);
        }
        let before = self.reconciliation_incarnation_from_identity(before_identity)?;
        if inventory_fingerprint != &before.inventory_fingerprint {
            return Err(ReconciliationEvidenceRejection::IncarnationMismatch);
        }
        if journal.operation_id != *terminal.operation_id()
            || journal.command != CommandKind::RepairInstance
            || journal.owner != StabilizationSystem::Guardian
            || journal.ownership != OwnershipClass::LauncherManaged
            || journal.status != OperationStatus::Failed
            || journal.outcome != Some(OperationOutcome::Failed)
            || journal.failure_point.is_none()
            || journal.reconciliation_terminal() != Some(&terminal)
            || !journal.targets.contains(terminal.target())
            || !journal
                .targets
                .contains(&reconciliation_instance_target(instance_id))
        {
            return Err(ReconciliationEvidenceRejection::JournalMismatch);
        }
        let after_identity = self.current_reconciliation_identity(instance_id)?;
        if before.fingerprint != after_identity.fingerprint || before.roots != after_identity.roots
        {
            return Err(ReconciliationEvidenceRejection::IncarnationMismatch);
        }
        let after = self.reconciliation_incarnation_from_identity(after_identity)?;
        if before.inventory_fingerprint != after.inventory_fingerprint
            || !std::sync::Arc::ptr_eq(&before.inventory, &after.inventory)
        {
            return Err(ReconciliationEvidenceRejection::IncarnationMismatch);
        }
        let instance = self
            .instances
            .get(instance_id)
            .filter(|instance| instance.id == instance_id)
            .ok_or(ReconciliationEvidenceRejection::InstanceNotRegistered)?;
        let inventory = after.inventory.clone();
        if verification.is_some_and(|verification| {
            !std::sync::Arc::ptr_eq(&inventory, &verification.inventory)
                || verification.version_id != instance.version_id
                || verification.created_at != instance.created_at
                || verification.library_root != after.roots.library
                || !self.known_good_verification_lease_is_live(verification)
        }) {
            return Err(ReconciliationEvidenceRejection::IncarnationMismatch);
        }
        let artifact_provenance = if expected_rung == ReconciliationRung::RepairArtifact
            && matches!(
                terminal.component(),
                ReconciliationComponent::VersionBundle
                    | ReconciliationComponent::Libraries
                    | ReconciliationComponent::Assets
            ) {
            Some(
                resolve_recorded_artifact_provenance(
                    &instance.id,
                    &instance.version_id,
                    &instance.created_at,
                    &after.roots.library,
                    &after.roots.runtime,
                    &inventory,
                    terminal.attempt(),
                )
                .ok_or(ReconciliationEvidenceRejection::ScopeMismatch)?,
            )
        } else {
            None
        };
        if expected_rung == ReconciliationRung::RebuildComponent {
            if !component_rebuild_terminal_matches(&journal, &terminal, instance_id) {
                return Err(ReconciliationEvidenceRejection::JournalMismatch);
            }
            let ReconciliationLineage::Predecessor {
                operation_id: predecessor_operation_id,
            } = terminal.attempt().lineage()
            else {
                return Err(ReconciliationEvidenceRejection::NonAdjacentRung);
            };
            let predecessor = self.recorded_reconciliation_failure_at(
                lifecycle,
                predecessor_operation_id,
                ReconciliationRung::RepairArtifact,
                observed_at,
                verification,
            )?;
            if !adjacent_reconciliation_attempts_match(
                predecessor.terminal.attempt(),
                terminal.attempt(),
            ) {
                return Err(ReconciliationEvidenceRejection::NonAdjacentRung);
            }
        }
        if expected_rung == ReconciliationRung::RematerializeInstance
            && !self.whole_instance_terminal_is_semantically_canonical(&journal, &terminal)
        {
            return Err(ReconciliationEvidenceRejection::JournalMismatch);
        }
        Ok(RecordedReconciliationFailure {
            terminal,
            lifecycle: lifecycle.retained(),
            roots: after.roots,
            inventory,
            artifact_provenance,
        })
    }

    fn current_reconciliation_incarnation(
        &self,
        instance_id: &str,
    ) -> Result<CurrentReconciliationIncarnation, ReconciliationEvidenceRejection> {
        let identity = self.current_reconciliation_identity(instance_id)?;
        self.reconciliation_incarnation_from_identity(identity)
    }

    fn current_reconciliation_identity(
        &self,
        instance_id: &str,
    ) -> Result<CurrentReconciliationIdentity, ReconciliationEvidenceRejection> {
        let instance = self
            .instances
            .get(instance_id)
            .filter(|instance| instance.id == instance_id && is_canonical_instance_id(&instance.id))
            .ok_or(ReconciliationEvidenceRejection::InstanceNotRegistered)?;
        let instance_root = canonical_directory(&self.instances.game_dir(instance_id))
            .map_err(|_| ReconciliationEvidenceRejection::RootAuthorityUnavailable)?;
        let library_root = self
            .library_dir()
            .map(PathBuf::from)
            .ok_or(ReconciliationEvidenceRejection::RootAuthorityUnavailable)
            .and_then(|root| {
                canonical_directory(&root)
                    .map_err(|_| ReconciliationEvidenceRejection::RootAuthorityUnavailable)
            })?;
        let runtime_root = canonical_directory(self.managed_runtime_cache.root())
            .map_err(|_| ReconciliationEvidenceRejection::RootAuthorityUnavailable)?;
        let roots = ReconciliationRoots {
            instance: instance_root,
            library: library_root,
            runtime: runtime_root,
        };
        let fingerprint = reconciliation_fingerprint(
            &instance.id,
            &instance.version_id,
            &instance.created_at,
            &roots,
        );
        Ok(CurrentReconciliationIdentity {
            instance_id: instance.id,
            version_id: instance.version_id,
            created_at: instance.created_at,
            fingerprint,
            roots,
        })
    }

    fn reconciliation_incarnation_from_identity(
        &self,
        identity: CurrentReconciliationIdentity,
    ) -> Result<CurrentReconciliationIncarnation, ReconciliationEvidenceRejection> {
        let inventory = self
            .known_good
            .active_inventory(
                &identity.instance_id,
                &identity.version_id,
                &identity.created_at,
                &identity.roots.library,
            )
            .ok_or(ReconciliationEvidenceRejection::RootAuthorityUnavailable)?;
        let inventory_fingerprint = reconciliation_inventory_fingerprint(&inventory);
        Ok(CurrentReconciliationIncarnation {
            fingerprint: identity.fingerprint,
            inventory_fingerprint,
            roots: identity.roots,
            inventory,
        })
    }
}

fn registered_component_required_terminal_matches(
    journal: &OperationJournalEntry,
    terminal: &ReconciliationTerminal,
    instance_id: &str,
) -> bool {
    const PLAN: [(&str, RollbackState); 4] = [
        ("journal_repair_start", RollbackState::NotApplicable),
        (
            "registered_artifact_already_exact",
            RollbackState::NotApplicable,
        ),
        (
            REGISTERED_ARTIFACT_COMPONENT_REBUILD_FAILURE_POINT,
            RollbackState::NotApplicable,
        ),
        ("record_repair_outcome", RollbackState::NotApplicable),
    ];
    let target = terminal.target();
    let exact_plan = journal.planned_steps.len() == PLAN.len()
        && journal
            .planned_steps
            .iter()
            .zip(PLAN)
            .all(|(step, (step_id, rollback))| {
                step.step_id == step_id
                    && step.phase == OperationPhase::Repairing
                    && step.result == OperationStepResult::Planned
                    && step.changed_target.as_ref() == Some(target)
                    && step.generated_facts.is_empty()
                    && step.rollback == rollback
            });
    let exact_failure = matches!(
        journal.completed_steps.as_slice(),
        [step]
            if step.step_id == REGISTERED_ARTIFACT_COMPONENT_REBUILD_FAILURE_POINT
                && step.phase == OperationPhase::Repairing
                && step.result == OperationStepResult::Failed
                && step.changed_target.as_ref() == Some(target)
                && step.generated_facts.is_empty()
                && step.rollback == RollbackState::NotApplicable
    );
    exact_plan
        && exact_failure
        && terminal.quarantine_checkpoint().is_empty()
        && journal.rollback == RollbackState::Available
        && journal.targets == [target.clone(), reconciliation_instance_target(instance_id)]
        && journal.guardian_diagnosis_ids == [terminal.diagnosis_id()]
}

fn adjacent_reconciliation_attempts_match(
    predecessor: &ReconciliationAttempt,
    successor: &ReconciliationAttempt,
) -> bool {
    matches!(predecessor.lineage(), ReconciliationLineage::Initial)
        && matches!(
            successor.lineage(),
            ReconciliationLineage::Predecessor { operation_id }
                if operation_id == predecessor.operation_id()
        )
        && predecessor.rung() == ReconciliationRung::RepairArtifact
        && successor.rung() == ReconciliationRung::RebuildComponent
        && predecessor.diagnosis_id() == successor.diagnosis_id()
        && predecessor.domain() == successor.domain()
        && predecessor.component() == successor.component()
        && predecessor.target() == successor.target()
        && predecessor.mode() == successor.mode()
        && predecessor.ownership() == successor.ownership()
        && predecessor.scope() == successor.scope()
}

fn adjacent_whole_instance_attempts_match(
    predecessor: &ReconciliationAttempt,
    successor: &ReconciliationAttempt,
) -> bool {
    matches!(
        predecessor.lineage(),
        ReconciliationLineage::Predecessor { .. }
    ) && matches!(
        successor.lineage(),
        ReconciliationLineage::Predecessor { operation_id }
            if operation_id == predecessor.operation_id()
    ) && predecessor.rung() == ReconciliationRung::RebuildComponent
        && successor.rung() == ReconciliationRung::RematerializeInstance
        && successor.component() == ReconciliationComponent::WholeInstance
        && predecessor.diagnosis_id() == successor.diagnosis_id()
        && predecessor.domain() == successor.domain()
        && predecessor.mode() == successor.mode()
        && predecessor.ownership() == successor.ownership()
        && predecessor.scope() == successor.scope()
        && matches!(
            successor.scope(),
            ReconciliationScope::RegisteredInstance { instance_id, .. }
                if successor.target()
                    == &reconciliation_instance_target(instance_id)
        )
}

fn whole_instance_terminal_matches(
    journal: &OperationJournalEntry,
    terminal: &ReconciliationTerminal,
) -> bool {
    if terminal.validate().is_err()
        || terminal.rung() != ReconciliationRung::RematerializeInstance
        || terminal.component() != ReconciliationComponent::WholeInstance
        || journal.operation_id != *terminal.operation_id()
        || journal.command != CommandKind::RepairInstance
        || journal.owner != StabilizationSystem::Guardian
        || journal.ownership != OwnershipClass::LauncherManaged
        || journal.targets != [terminal.target().clone()]
        || journal.guardian_diagnosis_ids != [terminal.diagnosis_id()]
        || journal.reconciliation_attempt() != Some(terminal.attempt())
        || journal.reconciliation_terminal() != Some(terminal)
    {
        return false;
    }
    let expected_plan = [
        (
            WHOLE_INSTANCE_REMATERIALIZATION_START_STEP,
            RollbackState::NotApplicable,
        ),
        (COMPONENT_QUARANTINE_STEP, RollbackState::Available),
        (
            WHOLE_INSTANCE_REMATERIALIZATION_STEP,
            RollbackState::Available,
        ),
    ];
    if journal.planned_steps.len() != expected_plan.len()
        || journal
            .planned_steps
            .iter()
            .zip(expected_plan)
            .any(|(step, (step_id, rollback))| {
                step.step_id != step_id
                    || step.phase != OperationPhase::Repairing
                    || step.result != OperationStepResult::Planned
                    || step.changed_target.as_ref() != Some(terminal.target())
                    || !step.generated_facts.is_empty()
                    || step.rollback != rollback
            })
    {
        return false;
    }
    let checkpoint = terminal.quarantine_checkpoint();
    let terminal_steps = if checkpoint.is_empty() {
        journal.completed_steps.as_slice()
    } else {
        let [ReconciliationQuarantineRecord::RuntimeComponent { component_id }] =
            checkpoint.records()
        else {
            return false;
        };
        if !is_known_runtime_component(component_id) {
            return false;
        }
        let Some((quarantine, terminal_steps)) = journal.completed_steps.split_first() else {
            return false;
        };
        if quarantine.step_id != COMPONENT_QUARANTINE_STEP
            || quarantine.phase != OperationPhase::Repairing
            || quarantine.result != OperationStepResult::Completed
            || quarantine.changed_target.as_ref() != Some(terminal.target())
            || !quarantine.generated_facts.is_empty()
            || quarantine.rollback != RollbackState::Available
        {
            return false;
        }
        terminal_steps
    };
    let [terminal_step] = terminal_steps else {
        return false;
    };
    if terminal_step.phase != OperationPhase::Repairing
        || terminal_step.changed_target.as_ref() != Some(terminal.target())
        || !terminal_step.generated_facts.is_empty()
    {
        return false;
    }
    match terminal.outcome() {
        ReconciliationTerminalOutcome::Succeeded => {
            checkpoint.is_empty()
                && terminal_step.step_id == WHOLE_INSTANCE_REMATERIALIZATION_STEP
                && terminal_step.result == OperationStepResult::Completed
                && terminal_step.rollback == RollbackState::Available
                && journal.status == OperationStatus::Succeeded
                && journal.outcome == Some(OperationOutcome::Succeeded)
                && journal.failure_point.is_none()
                && journal.rollback == RollbackState::Available
        }
        ReconciliationTerminalOutcome::Failed => {
            let before_effect = terminal_step.step_id
                == WHOLE_INSTANCE_REMATERIALIZATION_START_STEP
                && checkpoint.is_empty()
                && terminal_step.rollback == RollbackState::NotApplicable;
            let effect_failed = terminal_step.step_id == WHOLE_INSTANCE_REMATERIALIZATION_STEP
                && terminal_step.rollback == RollbackState::Available;
            (before_effect || effect_failed)
                && terminal_step.result == OperationStepResult::Failed
                && journal.status == OperationStatus::Failed
                && journal.outcome == Some(OperationOutcome::Failed)
                && journal.failure_point.as_deref() == Some(terminal_step.step_id.as_str())
                && journal.rollback == RollbackState::Available
        }
    }
}

fn component_rebuild_terminal_matches(
    journal: &OperationJournalEntry,
    terminal: &ReconciliationTerminal,
    instance_id: &str,
) -> bool {
    if terminal.rung() != ReconciliationRung::RebuildComponent
        || terminal.outcome() != ReconciliationTerminalOutcome::Failed
        || journal.targets
            != [
                terminal.target().clone(),
                reconciliation_instance_target(instance_id),
            ]
        || journal.guardian_diagnosis_ids != [terminal.diagnosis_id()]
        || journal.reconciliation_attempt() != Some(terminal.attempt())
        || journal.reconciliation_terminal() != Some(terminal)
    {
        return false;
    }
    let expected_plan = match terminal.component() {
        ReconciliationComponent::Runtime => vec![
            (COMPONENT_REBUILD_START_STEP, RollbackState::NotApplicable),
            (COMPONENT_QUARANTINE_STEP, RollbackState::Available),
            (RUNTIME_COMPONENT_REBUILD_STEP, RollbackState::NotApplicable),
        ],
        ReconciliationComponent::VersionBundle => vec![
            (COMPONENT_REBUILD_START_STEP, RollbackState::NotApplicable),
            (
                VERSION_BUNDLE_COMPONENT_REBUILD_STEP,
                RollbackState::NotApplicable,
            ),
        ],
        ReconciliationComponent::Libraries => vec![
            (COMPONENT_REBUILD_START_STEP, RollbackState::NotApplicable),
            (
                LIBRARIES_COMPONENT_REBUILD_STEP,
                RollbackState::NotApplicable,
            ),
        ],
        ReconciliationComponent::Assets => vec![
            (COMPONENT_REBUILD_START_STEP, RollbackState::NotApplicable),
            (ASSETS_COMPONENT_REBUILD_STEP, RollbackState::NotApplicable),
        ],
        ReconciliationComponent::WholeInstance => return false,
    };
    if journal.planned_steps.len() != expected_plan.len()
        || journal
            .planned_steps
            .iter()
            .zip(expected_plan)
            .any(|(step, (step_id, rollback))| {
                step.step_id != step_id
                    || step.phase != OperationPhase::Repairing
                    || step.result != OperationStepResult::Planned
                    || step.changed_target.as_ref() != Some(terminal.target())
                    || !step.generated_facts.is_empty()
                    || step.rollback != rollback
            })
    {
        return false;
    }
    let effect_step_id = match terminal.component() {
        ReconciliationComponent::Runtime => RUNTIME_COMPONENT_REBUILD_STEP,
        ReconciliationComponent::VersionBundle => VERSION_BUNDLE_COMPONENT_REBUILD_STEP,
        ReconciliationComponent::Libraries => LIBRARIES_COMPONENT_REBUILD_STEP,
        ReconciliationComponent::Assets => ASSETS_COMPONENT_REBUILD_STEP,
        ReconciliationComponent::WholeInstance => return false,
    };
    let terminal_steps = if terminal.quarantine_checkpoint().is_empty() {
        journal.completed_steps.as_slice()
    } else {
        let Some((quarantine, terminal_steps)) = journal.completed_steps.split_first() else {
            return false;
        };
        if terminal.component() != ReconciliationComponent::Runtime
            || quarantine.step_id != COMPONENT_QUARANTINE_STEP
            || quarantine.phase != OperationPhase::Repairing
            || quarantine.result != OperationStepResult::Completed
            || quarantine.changed_target.as_ref() != Some(terminal.target())
            || quarantine.rollback != RollbackState::Available
        {
            return false;
        }
        terminal_steps
    };
    matches!(
        terminal_steps,
        [step]
            if step.step_id == COMPONENT_REBUILD_START_STEP || step.step_id == effect_step_id
    ) && terminal_steps[0].phase == OperationPhase::Repairing
        && terminal_steps[0].result == OperationStepResult::Failed
        && terminal_steps[0].changed_target.as_ref() == Some(terminal.target())
        && journal.failure_point.as_deref() == Some(terminal_steps[0].step_id.as_str())
        && journal.rollback == terminal_steps[0].rollback
}

fn active_reconciliation_terminal_at(
    terminal: &ReconciliationTerminal,
    observed_at: chrono::DateTime<chrono::FixedOffset>,
) -> Result<bool, ReconciliationEvidenceRejection> {
    active_reconciliation_attempt_at(terminal.attempt(), observed_at)
}

fn active_reconciliation_attempt_at(
    attempt: &ReconciliationAttempt,
    observed_at: chrono::DateTime<chrono::FixedOffset>,
) -> Result<bool, ReconciliationEvidenceRejection> {
    let suppression_until = chrono::DateTime::parse_from_rfc3339(attempt.suppression_until())
        .map_err(|_| ReconciliationEvidenceRejection::JournalMismatch)?;
    Ok(observed_at < suppression_until)
}

pub(crate) fn reconciliation_instance_target(instance_id: &str) -> TargetDescriptor {
    TargetDescriptor::new(
        StabilizationSystem::State,
        TargetKind::Instance,
        instance_id,
        OwnershipClass::LauncherManaged,
    )
}

fn canonical_directory(path: &Path) -> io::Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    if absolute.components().any(|component| {
        matches!(
            component,
            std::path::Component::CurDir | std::path::Component::ParentDir
        )
    }) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "reconciliation root cannot contain relative traversal",
        ));
    }
    let mut ancestor = absolute.as_path();
    let mut missing = Vec::new();
    loop {
        match std::fs::symlink_metadata(ancestor) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "reconciliation root ancestor must be a real directory",
                    ));
                }
                let mut canonical = std::fs::canonicalize(ancestor)?;
                if !same_canonical_directory(&canonical, ancestor) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "reconciliation root cannot traverse filesystem indirection",
                    ));
                }
                for component in missing.iter().rev() {
                    canonical.push(component);
                }
                return Ok(canonical);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let component = ancestor.file_name().ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotFound,
                        "reconciliation root has no existing trusted ancestor",
                    )
                })?;
                missing.push(component.to_os_string());
                ancestor = ancestor.parent().ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotFound,
                        "reconciliation root has no existing trusted ancestor",
                    )
                })?;
            }
            Err(error) => return Err(error),
        }
    }
}

#[cfg(not(windows))]
fn same_canonical_directory(canonical: &Path, configured: &Path) -> bool {
    canonical == configured
}

#[cfg(windows)]
fn same_canonical_directory(canonical: &Path, configured: &Path) -> bool {
    use std::path::{Component, Prefix};

    #[derive(Eq, PartialEq)]
    enum PrefixIdentity<'a> {
        Disk(u8),
        Unc(&'a std::ffi::OsStr, &'a std::ffi::OsStr),
        Verbatim(&'a std::ffi::OsStr),
        DeviceNamespace(&'a std::ffi::OsStr),
    }

    fn identity(prefix: Prefix<'_>) -> PrefixIdentity<'_> {
        match prefix {
            Prefix::Disk(drive) | Prefix::VerbatimDisk(drive) => {
                PrefixIdentity::Disk(drive.to_ascii_uppercase())
            }
            Prefix::UNC(server, share) | Prefix::VerbatimUNC(server, share) => {
                PrefixIdentity::Unc(server, share)
            }
            Prefix::Verbatim(value) => PrefixIdentity::Verbatim(value),
            Prefix::DeviceNS(value) => PrefixIdentity::DeviceNamespace(value),
        }
    }

    let mut canonical_components = canonical.components();
    let mut configured_components = configured.components();
    let (Some(Component::Prefix(canonical_prefix)), Some(Component::Prefix(configured_prefix))) =
        (canonical_components.next(), configured_components.next())
    else {
        return false;
    };

    identity(canonical_prefix.kind()) == identity(configured_prefix.kind())
        && canonical_components.eq(configured_components)
}

fn reconciliation_fingerprint(
    instance_id: &str,
    version_id: &str,
    created_at: &str,
    roots: &ReconciliationRoots,
) -> ReconciliationIncarnationFingerprint {
    let mut hasher = Sha256::new();
    update_frame(&mut hasher, b"domain", RECONCILIATION_FINGERPRINT_DOMAIN);
    update_frame(&mut hasher, b"instance_id", instance_id.as_bytes());
    update_frame(&mut hasher, b"version_id", version_id.as_bytes());
    update_frame(&mut hasher, b"created_at", created_at.as_bytes());
    update_path_frame(&mut hasher, b"instance_root", &roots.instance);
    update_path_frame(&mut hasher, b"library_root", &roots.library);
    update_path_frame(&mut hasher, b"runtime_root", &roots.runtime);
    let hex = format!("{:x}", hasher.finalize());
    let dotted = hex
        .as_bytes()
        .chunks(8)
        .map(|chunk| std::str::from_utf8(chunk).expect("SHA-256 hex is ASCII"))
        .collect::<Vec<_>>()
        .join(".");
    ReconciliationIncarnationFingerprint::from_digest(format!("sha256.{dotted}"))
}

fn reconciliation_inventory_fingerprint(
    inventory: &axial_minecraft::known_good::KnownGoodInventory,
) -> ReconciliationInventoryFingerprint {
    let mut entry_digests = Vec::with_capacity(inventory.entries().len());
    for (ordinal, entry) in inventory.entries().iter().enumerate() {
        let mut entry_hasher = Sha256::new();
        update_frame(
            &mut entry_hasher,
            b"domain",
            RECONCILIATION_INVENTORY_ENTRY_DOMAIN,
        );
        update_frame(
            &mut entry_hasher,
            b"ordinal",
            &(ordinal as u64).to_le_bytes(),
        );
        update_frame(
            &mut entry_hasher,
            b"root",
            entry.root().stable_id().as_bytes(),
        );
        update_frame(
            &mut entry_hasher,
            b"root_scope",
            entry.root().scope_id().as_bytes(),
        );
        update_frame(&mut entry_hasher, b"path", entry.path().as_str().as_bytes());
        update_frame(
            &mut entry_hasher,
            b"kind",
            entry.kind().stable_id().as_bytes(),
        );
        match entry.integrity() {
            KnownGoodIntegrity::Sha1 { digest, size } => {
                update_frame(&mut entry_hasher, b"integrity", b"sha1");
                update_frame(&mut entry_hasher, b"digest", digest.as_str().as_bytes());
                update_frame(&mut entry_hasher, b"size", &size.to_le_bytes());
            }
            KnownGoodIntegrity::ExactBytes { digest, size } => {
                update_frame(&mut entry_hasher, b"integrity", b"exact_bytes");
                update_frame(&mut entry_hasher, b"digest", digest.as_str().as_bytes());
                update_frame(&mut entry_hasher, b"size", &size.to_le_bytes());
            }
            KnownGoodIntegrity::Directory => {
                update_frame(&mut entry_hasher, b"integrity", b"directory");
            }
            KnownGoodIntegrity::LinkTarget(target) => {
                update_frame(&mut entry_hasher, b"integrity", b"link_target");
                update_frame(
                    &mut entry_hasher,
                    b"link_target",
                    target.as_str().as_bytes(),
                );
            }
        }
        match inventory.bind_standalone_leaf_repair_source(ordinal) {
            Ok(source) => {
                update_frame(&mut entry_hasher, b"source", b"standalone");
                update_frame(
                    &mut entry_hasher,
                    b"source_root",
                    source.root().stable_id().as_bytes(),
                );
                update_frame(
                    &mut entry_hasher,
                    b"source_scope",
                    source.root().scope_id().as_bytes(),
                );
                update_frame(
                    &mut entry_hasher,
                    b"source_path",
                    source.path().as_str().as_bytes(),
                );
                update_frame(
                    &mut entry_hasher,
                    b"source_kind",
                    source.kind().stable_id().as_bytes(),
                );
                update_frame(
                    &mut entry_hasher,
                    b"source_digest",
                    source.sha1().as_str().as_bytes(),
                );
                update_frame(
                    &mut entry_hasher,
                    b"source_size",
                    &source.size().to_le_bytes(),
                );
                update_frame(
                    &mut entry_hasher,
                    b"source_provider",
                    source.provider_url().as_bytes(),
                );
            }
            Err(_) => update_frame(&mut entry_hasher, b"source", b"component_rebuild"),
        }
        entry_digests.push(entry_hasher.finalize());
    }
    entry_digests.sort_unstable();
    let mut hasher = Sha256::new();
    update_frame(
        &mut hasher,
        b"domain",
        RECONCILIATION_INVENTORY_FINGERPRINT_DOMAIN,
    );
    update_frame(
        &mut hasher,
        b"entry_count",
        &(entry_digests.len() as u64).to_le_bytes(),
    );
    for digest in entry_digests {
        update_frame(&mut hasher, b"entry", digest.as_slice());
    }
    let hex = format!("{:x}", hasher.finalize());
    let dotted = hex
        .as_bytes()
        .chunks(8)
        .map(|chunk| std::str::from_utf8(chunk).expect("SHA-256 hex is ASCII"))
        .collect::<Vec<_>>()
        .join(".");
    ReconciliationInventoryFingerprint::from_digest(format!("sha256.{dotted}"))
}

fn update_frame(hasher: &mut Sha256, label: &[u8], value: &[u8]) {
    hasher.update((label.len() as u64).to_le_bytes());
    hasher.update(label);
    hasher.update((value.len() as u64).to_le_bytes());
    hasher.update(value);
}

#[cfg(unix)]
fn update_path_frame(hasher: &mut Sha256, label: &[u8], path: &Path) {
    use std::os::unix::ffi::OsStrExt;
    update_frame(hasher, label, path.as_os_str().as_bytes());
}

#[cfg(windows)]
fn update_path_frame(hasher: &mut Sha256, label: &[u8], path: &Path) {
    use std::os::windows::ffi::OsStrExt;
    let encoded = path
        .as_os_str()
        .encode_wide()
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>();
    update_frame(hasher, label, &encoded);
}

#[cfg(not(any(unix, windows)))]
fn update_path_frame(hasher: &mut Sha256, label: &[u8], path: &Path) {
    update_frame(hasher, label, path.to_string_lossy().as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::persistence::{AtomicWriteBackend, PersistenceCoordinator};
    use crate::guardian::{
        ActionPlanPrerequisite, GuardianAction, GuardianActionPlan, GuardianConfidence,
        GuardianDecision, GuardianWholeInstanceRematerializationDisposition,
        GuardianWholeInstanceRematerializationOffer, assess_whole_instance_rematerialization,
    };
    use crate::state::contracts::{JournalId, OperationPhase, OperationStepResult, RollbackState};
    use crate::state::failure_memory::FailureMemorySnapshot;
    use crate::state::{AppStateInit, InstallStore, SessionStore, new_instance};
    use axial_config::{AppPaths, InstanceRegistrySnapshot};
    use axial_minecraft::known_good::{
        KnownGoodArtifactKind, KnownGoodInventory, TestKnownGoodEntry, TestKnownGoodIntegrity,
        TestKnownGoodRoot,
    };
    use sha1::Sha1;
    use std::fs;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    const INSTANCE_ID: &str = "0000000000000001";
    const DIAGNOSIS_ID: DiagnosisId = DiagnosisId::LauncherManagedArtifactCorrupt;

    #[derive(Default)]
    struct ControlledWriteBackend {
        attempts: AtomicUsize,
        failed_attempt: AtomicUsize,
        gated_attempt: AtomicUsize,
        release_gate: AtomicBool,
    }

    impl ControlledWriteBackend {
        fn fail_attempt(&self, attempt: usize) {
            self.failed_attempt.store(attempt, Ordering::SeqCst);
        }

        fn gate_attempt(&self, attempt: usize) {
            self.gated_attempt.store(attempt, Ordering::SeqCst);
            self.release_gate.store(false, Ordering::SeqCst);
        }

        fn next_attempt(&self) -> usize {
            self.attempts.load(Ordering::SeqCst) + 1
        }

        fn release(&self) {
            self.release_gate.store(true, Ordering::SeqCst);
        }

        async fn wait_for_attempt(&self, expected: usize) {
            tokio::time::timeout(Duration::from_secs(2), async {
                while self.attempts.load(Ordering::SeqCst) < expected {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("whole-instance persistence attempt");
        }
    }

    impl AtomicWriteBackend for ControlledWriteBackend {
        fn write(
            &self,
            target: &TargetDescriptor,
            destination: &Path,
            contents: &[u8],
        ) -> io::Result<()> {
            let attempt = self.attempts.fetch_add(1, Ordering::SeqCst) + 1;
            if self.gated_attempt.load(Ordering::SeqCst) == attempt {
                while !self.release_gate.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
            if self.failed_attempt.load(Ordering::SeqCst) == attempt {
                return Err(io::Error::other(
                    "injected whole-instance persistence failure",
                ));
            }
            crate::execution::file::write_file_atomically(
                crate::execution::file::FileWriteRequest::new(
                    target.clone(),
                    destination,
                    contents,
                ),
            )
            .map(|_| ())
            .map_err(io::Error::from)
        }
    }

    trait AmbiguousIfClone<Marker> {
        fn assert_not_clone() {}
    }

    struct CloneMarker;

    impl<T: ?Sized> AmbiguousIfClone<()> for T {}
    impl<T: Clone> AmbiguousIfClone<CloneMarker> for T {}

    const _: fn() = || {
        let _ = <RegisteredArtifactFailedRepair as AmbiguousIfClone<_>>::assert_not_clone;
        let _ = <RegisteredArtifactRecoveryEntry as AmbiguousIfClone<_>>::assert_not_clone;
        let _ = <crate::state::RegisteredArtifactRepairAdmission as AmbiguousIfClone<_>>::assert_not_clone;
        let _ = <RegisteredComponentRebuildAdmission as AmbiguousIfClone<_>>::assert_not_clone;
        let _ = <RegisteredWholeInstanceRematerializationEligibility as AmbiguousIfClone<_>>::assert_not_clone;
        let _ = <RegisteredWholeInstanceRematerializationAdmission as AmbiguousIfClone<_>>::assert_not_clone;
        let _ = <RegisteredWholeInstanceCoreRequest as AmbiguousIfClone<_>>::assert_not_clone;
        let _ = <RegisteredWholeInstanceCompletion as AmbiguousIfClone<_>>::assert_not_clone;
        let _ = <RegisteredWholeInstanceSettlement as AmbiguousIfClone<_>>::assert_not_clone;
        let _ = <RegisteredWholeInstanceDurableOutcome as AmbiguousIfClone<_>>::assert_not_clone;
    };
    static_assertions::assert_not_impl_any!(
        RegisteredUserConfigRestoreEligibility: Clone, std::fmt::Debug, serde::Serialize, serde::de::DeserializeOwned
    );
    static_assertions::assert_not_impl_any!(
        RegisteredWholeInstanceDurableOutcome: Clone, std::fmt::Debug, serde::Serialize, serde::de::DeserializeOwned
    );

    struct Fixture {
        state: AppState,
        journals: Arc<OperationJournalStore>,
        failure_memory: Arc<GuardianFailureMemoryStore>,
        root: PathBuf,
    }

    #[cfg(windows)]
    #[test]
    fn canonical_directory_identity_accepts_windows_verbatim_prefixes() {
        assert!(same_canonical_directory(
            Path::new(r"\\?\C:\Users\Alice\Axial"),
            Path::new(r"c:\Users\Alice\Axial"),
        ));
        assert!(same_canonical_directory(
            Path::new(r"\\?\UNC\server\share\Axial"),
            Path::new(r"\\server\share\Axial"),
        ));
    }

    #[cfg(windows)]
    #[test]
    fn canonical_directory_identity_rejects_distinct_windows_locations() {
        assert!(!same_canonical_directory(
            Path::new(r"\\?\C:\Users\Alice\Axial"),
            Path::new(r"C:\Users\Alice\Other"),
        ));
        assert!(!same_canonical_directory(
            Path::new(r"\\?\UNC\server\share\Axial"),
            Path::new(r"\\server\other-share\Axial"),
        ));
        assert!(!same_canonical_directory(
            Path::new(r"\\?\C:\Users\Alice\Axial"),
            Path::new(r"C:\Users\alice\Axial"),
        ));
    }

    fn fixture(label: &str) -> Fixture {
        fixture_with_backends(label, None, None)
    }

    fn fixture_with_backends(
        label: &str,
        journal_backend: Option<Arc<ControlledWriteBackend>>,
        memory_backend: Option<Arc<ControlledWriteBackend>>,
    ) -> Fixture {
        static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(1);

        let root = std::env::temp_dir().join(format!(
            "axial-reconciliation-{label}-{}-{}",
            std::process::id(),
            NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&root);
        let config_dir = root.join("config");
        let paths = AppPaths {
            config_file: config_dir.join("config.json"),
            instances_file: config_dir.join("instances.json"),
            instances_dir: root.join("instances"),
            music_dir: root.join("music"),
            library_dir: root.join("library"),
            config_dir,
        };
        fs::create_dir_all(&paths.config_dir).expect("config root");
        fs::create_dir_all(paths.instances_dir.join(INSTANCE_ID)).expect("instance root");
        fs::create_dir_all(&paths.library_dir).expect("library root");
        let config = Arc::new(
            axial_config::ConfigStore::load_from(paths.clone()).expect("load test config"),
        );
        let instances = Arc::new(
            axial_config::InstanceStore::from_snapshot(
                paths.clone(),
                InstanceRegistrySnapshot::new(
                    vec![new_instance(
                        INSTANCE_ID.to_string(),
                        "Reconciliation Test".to_string(),
                        "1.21.1".to_string(),
                        String::new(),
                        String::new(),
                    )],
                    INSTANCE_ID.to_string(),
                    Vec::new(),
                )
                .expect("registered instance snapshot"),
            )
            .expect("load test instances"),
        );
        let journals = Arc::new(match journal_backend {
            Some(backend) => OperationJournalStore::try_load_from_paths_with_coordinator(
                &paths,
                PersistenceCoordinator::for_test(
                    backend,
                    Duration::from_millis(1),
                    Duration::from_millis(5),
                ),
            )
            .expect("persistent whole-instance journals"),
            None => OperationJournalStore::new(),
        });
        let failure_memory = Arc::new(match memory_backend {
            Some(backend) => GuardianFailureMemoryStore::try_load_from_paths_with_coordinator(
                &paths,
                PersistenceCoordinator::for_test(
                    backend,
                    Duration::from_millis(1),
                    Duration::from_millis(5),
                ),
            )
            .expect("persistent whole-instance memory"),
            None => GuardianFailureMemoryStore::new(),
        });
        let state = AppState::new(AppStateInit {
            app_name: "Axial".to_string(),
            version: "test".to_string(),
            config,
            instances,
            installs: Arc::new(InstallStore::new()),
            sessions: Arc::new(SessionStore::new()),
            performance: Arc::new(
                axial_performance::PerformanceManager::load_for_startup(&paths.config_dir)
                    .expect("load test performance state"),
            ),
            startup_warnings: Vec::new(),
            frontend_dir: root.join("frontend"),
        })
        .with_reconciliation_stores(journals.clone(), failure_memory.clone());
        state.set_library_dir_for_test(paths.library_dir.to_string_lossy().into_owned());
        activate_empty_inventory(&state, INSTANCE_ID);
        Fixture {
            state,
            journals,
            failure_memory,
            root,
        }
    }

    fn empty_inventory() -> KnownGoodInventory {
        KnownGoodInventory::from_test_entries(Vec::<TestKnownGoodEntry>::new())
            .expect("empty known-good inventory")
    }

    const ASSETS_FIXTURE_OBJECT_BYTES: &[u8] = b"axial managed Assets fixture";

    fn assets_fixture_index_bytes() -> Vec<u8> {
        let object_digest = format!("{:x}", Sha1::digest(ASSETS_FIXTURE_OBJECT_BYTES));
        let empty_digest = format!("{:x}", Sha1::digest([]));
        serde_json::to_vec(&serde_json::json!({
            "objects": {
                "fixture/object": {
                    "hash": object_digest.as_str(),
                    "size": ASSETS_FIXTURE_OBJECT_BYTES.len()
                },
                "fixture/empty": {
                    "hash": empty_digest.as_str(),
                    "size": 0
                }
            }
        }))
        .expect("Assets fixture index")
    }

    fn assets_fixture_inventory() -> KnownGoodInventory {
        let object_digest = format!("{:x}", Sha1::digest(ASSETS_FIXTURE_OBJECT_BYTES));
        let empty_digest = format!("{:x}", Sha1::digest([]));
        let index_bytes = assets_fixture_index_bytes();
        let entries = [
            TestKnownGoodEntry {
                root: TestKnownGoodRoot::Assets,
                path: "indexes/fixture-assets.json".to_string(),
                kind: KnownGoodArtifactKind::AssetIndex,
                integrity: TestKnownGoodIntegrity::Sha1 {
                    digest: format!("{:x}", Sha1::digest(&index_bytes)),
                    size: index_bytes.len() as u64,
                },
            },
            TestKnownGoodEntry {
                root: TestKnownGoodRoot::Assets,
                path: format!("objects/{}/{}", &object_digest[..2], object_digest),
                kind: KnownGoodArtifactKind::AssetObject,
                integrity: TestKnownGoodIntegrity::Sha1 {
                    digest: object_digest.clone(),
                    size: ASSETS_FIXTURE_OBJECT_BYTES.len() as u64,
                },
            },
            TestKnownGoodEntry {
                root: TestKnownGoodRoot::Assets,
                path: format!("objects/{}/{}", &empty_digest[..2], empty_digest),
                kind: KnownGoodArtifactKind::AssetObject,
                integrity: TestKnownGoodIntegrity::Sha1 {
                    digest: empty_digest.clone(),
                    size: 0,
                },
            },
        ];
        KnownGoodInventory::from_test_entries(entries)
            .expect("Assets fixture inventory")
            .with_test_standalone_leaf_repair_source(
                0,
                "https://example.invalid/fixture-assets.json",
            )
            .expect("Assets index fixture repair source")
            .with_test_standalone_leaf_repair_source(
                1,
                &format!(
                    "https://resources.download.minecraft.net/{}/{}",
                    &object_digest[..2],
                    object_digest
                ),
            )
            .expect("Assets object fixture repair source")
            .with_test_standalone_leaf_repair_source(
                2,
                &format!(
                    "https://resources.download.minecraft.net/{}/{}",
                    &empty_digest[..2],
                    empty_digest
                ),
            )
            .expect("empty Assets object fixture repair source")
    }

    fn version_bundle_fixture_inventory() -> KnownGoodInventory {
        const CLIENT_BYTES: &[u8] = b"axial managed VersionBundle client fixture";
        const LOG_ID: &str = "guardian-version-bundle.xml";
        const LOG_BYTES: &[u8] = b"<Configuration/>";
        let version_json = serde_json::to_vec(&serde_json::json!({
            "id": "1.21.1",
            "type": "release",
            "mainClass": "org.axial.GuardianFixture"
        }))
        .expect("VersionBundle fixture metadata");
        KnownGoodInventory::from_test_entries([
            TestKnownGoodEntry {
                root: TestKnownGoodRoot::Versions,
                path: "1.21.1/1.21.1.json".to_string(),
                kind: KnownGoodArtifactKind::VersionMetadata,
                integrity: TestKnownGoodIntegrity::Sha1 {
                    digest: format!("{:x}", Sha1::digest(&version_json)),
                    size: version_json.len() as u64,
                },
            },
            TestKnownGoodEntry {
                root: TestKnownGoodRoot::Versions,
                path: "1.21.1/1.21.1.jar".to_string(),
                kind: KnownGoodArtifactKind::ClientJar,
                integrity: TestKnownGoodIntegrity::Sha1 {
                    digest: format!("{:x}", Sha1::digest(CLIENT_BYTES)),
                    size: CLIENT_BYTES.len() as u64,
                },
            },
            TestKnownGoodEntry {
                root: TestKnownGoodRoot::Assets,
                path: format!("log_configs/{LOG_ID}"),
                kind: KnownGoodArtifactKind::LogConfig,
                integrity: TestKnownGoodIntegrity::Sha1 {
                    digest: format!("{:x}", Sha1::digest(LOG_BYTES)),
                    size: LOG_BYTES.len() as u64,
                },
            },
        ])
        .expect("VersionBundle fixture inventory")
    }

    #[test]
    fn reconciliation_inventory_fingerprint_is_stable_and_full_inventory_bound() {
        let first = assets_fixture_inventory();
        let same = assets_fixture_inventory();
        let different = version_bundle_fixture_inventory();

        assert_eq!(
            reconciliation_inventory_fingerprint(&first),
            reconciliation_inventory_fingerprint(&same),
        );
        assert_ne!(
            reconciliation_inventory_fingerprint(&first),
            reconciliation_inventory_fingerprint(&different),
        );
    }

    fn activate_assets_fixture_inventory(
        state: &AppState,
        instance_id: &str,
    ) -> Arc<KnownGoodInventory> {
        state.activate_known_good_inventory_for_test(instance_id, assets_fixture_inventory());
        let instance = state.instances().get(instance_id).expect("test instance");
        let library_root = PathBuf::from(state.library_dir().expect("test library root"));
        state
            .known_good
            .active_inventory(
                &instance.id,
                &instance.version_id,
                &instance.created_at,
                &library_root,
            )
            .expect("active Assets fixture inventory")
    }

    fn activate_version_bundle_fixture_inventory(
        state: &AppState,
        instance_id: &str,
    ) -> Arc<KnownGoodInventory> {
        state.activate_known_good_inventory_for_test(
            instance_id,
            version_bundle_fixture_inventory(),
        );
        let instance = state.instances().get(instance_id).expect("test instance");
        let library_root = PathBuf::from(state.library_dir().expect("test library root"));
        state
            .known_good
            .active_inventory(
                &instance.id,
                &instance.version_id,
                &instance.created_at,
                &library_root,
            )
            .expect("active VersionBundle fixture inventory")
    }

    fn activate_empty_inventory(state: &AppState, instance_id: &str) -> Arc<KnownGoodInventory> {
        state.activate_known_good_inventory_for_test(instance_id, empty_inventory());
        let instance = state.instances().get(instance_id).expect("test instance");
        let library_root = PathBuf::from(state.library_dir().expect("test library root"));
        state
            .known_good
            .active_inventory(
                &instance.id,
                &instance.version_id,
                &instance.created_at,
                &library_root,
            )
            .expect("active test known-good inventory")
    }

    fn registered_artifact_target_for_ordinal(
        fixture: &Fixture,
        inventory_ordinal: usize,
    ) -> TargetDescriptor {
        let instance = fixture
            .state
            .instances
            .get(INSTANCE_ID)
            .expect("registered artifact target instance");
        let current = fixture
            .state
            .current_reconciliation_incarnation(INSTANCE_ID)
            .expect("registered artifact target incarnation");
        let inventory = fixture
            .state
            .known_good
            .active_inventory(
                &instance.id,
                &instance.version_id,
                &instance.created_at,
                &current.roots.library,
            )
            .expect("registered artifact target inventory");
        super::super::registered_artifact_findings::registered_artifact_target_for_test(
            &instance.id,
            &instance.version_id,
            &instance.created_at,
            &current.roots.library,
            &current.roots.runtime,
            &inventory,
            inventory_ordinal,
        )
        .expect("registered artifact target")
    }

    fn registered_artifact_repair_decision(target: TargetDescriptor) -> GuardianDecision {
        GuardianDecision::for_test(
            None,
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
                    candidate_actions: vec![GuardianActionKind::Repair],
                },
                vec![GuardianAction {
                    kind: GuardianActionKind::Repair,
                    target: Some(target),
                    reason: DiagnosisId::LauncherManagedArtifactCorrupt,
                }],
            )),
        )
    }

    async fn authorized_registered_artifact_repair(
        fixture: &Fixture,
        inventory_ordinal: usize,
        condition: super::super::RegisteredArtifactCondition,
    ) -> RegisteredArtifactRepairAuthorization {
        let lifecycle = fixture.state.acquire_instance_lifecycle(INSTANCE_ID).await;
        let foreground = fixture
            .state
            .register_integrity_foreground()
            .expect("register artifact recovery foreground")
            .wait_for_settlement()
            .await;
        let verification = fixture
            .state
            .mint_known_good_verification_lease(
                &foreground,
                &lifecycle,
                &PathBuf::from(fixture.state.library_dir().expect("artifact recovery root")),
            )
            .expect("mint artifact recovery verification");
        let observation = verification
            .registered_artifact_observation(inventory_ordinal, condition)
            .expect("registered artifact observation");
        let findings = fixture
            .state
            .seal_registered_artifact_findings(verification, vec![observation])
            .expect("seal artifact recovery finding");
        let target = findings
            .repair_candidate()
            .map(|candidate| candidate.target())
            .expect("artifact recovery target")
            .clone();
        let authorization = findings
            .authorize_repair(&registered_artifact_repair_decision(target))
            .expect("authorize artifact recovery");
        drop((foreground, lifecycle));
        authorization
    }

    async fn cleanup(fixture: Fixture) {
        fixture
            .state
            .close_user_config_snapshots()
            .await
            .expect("close user config snapshot store");
        fixture
            .state
            .close_user_mod_witnesses()
            .await
            .expect("close user mod witness store");
        fixture
            .state
            .close_known_good_inventories()
            .await
            .expect("close known-good store");
        fixture
            .state
            .close_instance_registry()
            .await
            .expect("close instance registry");
        fixture
            .journals
            .close()
            .await
            .expect("close reconciliation journals");
        fixture
            .failure_memory
            .close()
            .await
            .expect("close reconciliation memory");
        let Fixture {
            state,
            journals,
            failure_memory,
            root,
        } = fixture;
        drop((state, journals, failure_memory));
        let _ = fs::remove_dir_all(root);
    }

    fn artifact_target() -> TargetDescriptor {
        TargetDescriptor::new(
            StabilizationSystem::Execution,
            TargetKind::Artifact,
            "version-bundle",
            OwnershipClass::LauncherManaged,
        )
    }

    fn runtime_target() -> TargetDescriptor {
        TargetDescriptor::new(
            StabilizationSystem::Execution,
            TargetKind::Runtime,
            "java-runtime-delta",
            OwnershipClass::LauncherManaged,
        )
    }

    async fn registered_attempt(
        fixture: &Fixture,
        operation_id: &str,
        component: ReconciliationComponent,
    ) -> (ReconciliationAttempt, ReconciliationTerminal) {
        let lifecycle = fixture.state.acquire_instance_lifecycle(INSTANCE_ID).await;
        let authority = fixture
            .state
            .registered_reconciliation_authority(&lifecycle)
            .expect("registered authority");
        let attempt = authority
            .repair_artifact_attempt(
                OperationId::new(operation_id),
                DIAGNOSIS_ID,
                GuardianDomain::Launch,
                component,
                artifact_target(),
                GuardianMode::Managed,
                chrono::Duration::minutes(30),
            )
            .expect("typed reconciliation attempt");
        let terminal = authority
            .terminal(attempt.clone(), ReconciliationTerminalOutcome::Failed)
            .expect("typed reconciliation terminal");
        (attempt, terminal)
    }

    fn planned_journal(attempt: &ReconciliationAttempt) -> OperationJournalEntry {
        let mut entry = OperationJournalEntry::new(
            JournalId::new(format!("journal-{}", attempt.operation_id().as_str())),
            attempt.operation_id().clone(),
            CommandKind::RepairInstance,
            StabilizationSystem::Guardian,
            OwnershipClass::LauncherManaged,
            RollbackState::Available,
        );
        entry.targets.push(attempt.target().clone());
        let ReconciliationScope::RegisteredInstance { instance_id, .. } = attempt.scope();
        entry
            .targets
            .push(reconciliation_instance_target(instance_id));
        let mut step = OperationJournalStep::new("repair_artifact", OperationPhase::Repairing);
        step.changed_target = Some(attempt.target().clone());
        entry.planned_steps.push(step);
        entry.guardian_diagnosis_ids.push(attempt.diagnosis_id());
        reconciliation_journal_attempt(entry, attempt.clone())
    }

    fn component_required_journal(attempt: &ReconciliationAttempt) -> OperationJournalEntry {
        let mut entry = OperationJournalEntry::new(
            JournalId::new(format!("journal-{}", attempt.operation_id().as_str())),
            attempt.operation_id().clone(),
            CommandKind::RepairInstance,
            StabilizationSystem::Guardian,
            OwnershipClass::LauncherManaged,
            RollbackState::Available,
        );
        entry.targets.push(attempt.target().clone());
        let ReconciliationScope::RegisteredInstance { instance_id, .. } = attempt.scope();
        entry
            .targets
            .push(reconciliation_instance_target(instance_id));
        for step_id in [
            "journal_repair_start",
            "registered_artifact_already_exact",
            REGISTERED_ARTIFACT_COMPONENT_REBUILD_FAILURE_POINT,
            "record_repair_outcome",
        ] {
            let mut step = OperationJournalStep::new(step_id, OperationPhase::Repairing);
            step.changed_target = Some(attempt.target().clone());
            entry.planned_steps.push(step);
        }
        entry.guardian_diagnosis_ids.push(attempt.diagnosis_id());
        reconciliation_journal_attempt(entry, attempt.clone())
    }

    fn failed_step(target: &TargetDescriptor) -> OperationJournalStep {
        let mut step = OperationJournalStep::new("repair_artifact", OperationPhase::Failed);
        step.result = OperationStepResult::Failed;
        step.changed_target = Some(target.clone());
        step
    }

    fn component_required_failed_step(target: &TargetDescriptor) -> OperationJournalStep {
        let mut step = OperationJournalStep::new(
            REGISTERED_ARTIFACT_COMPONENT_REBUILD_FAILURE_POINT,
            OperationPhase::Repairing,
        );
        step.result = OperationStepResult::Failed;
        step.changed_target = Some(target.clone());
        step
    }

    async fn persist_failed_journal(
        fixture: &Fixture,
        attempt: &ReconciliationAttempt,
        terminal: ReconciliationTerminal,
    ) {
        persist_failed_journal_at(fixture, attempt, terminal, "repair_failed").await;
    }

    async fn persist_failed_journal_at(
        fixture: &Fixture,
        attempt: &ReconciliationAttempt,
        terminal: ReconciliationTerminal,
        failure_point: &str,
    ) {
        fixture
            .journals
            .create(planned_journal(attempt))
            .await
            .expect("persist planned reconciliation");
        record_reconciliation_journal_failure(
            fixture.journals.as_ref(),
            attempt.operation_id(),
            failed_step(attempt.target()),
            failure_point,
            terminal,
        )
        .await
        .expect("persist failed reconciliation");
    }

    async fn persist_component_required_failure(
        fixture: &Fixture,
        attempt: &ReconciliationAttempt,
        terminal: ReconciliationTerminal,
    ) {
        fixture
            .journals
            .create(component_required_journal(attempt))
            .await
            .expect("persist planned component-required repair");
        record_reconciliation_journal_failure(
            fixture.journals.as_ref(),
            attempt.operation_id(),
            component_required_failed_step(attempt.target()),
            REGISTERED_ARTIFACT_COMPONENT_REBUILD_FAILURE_POINT,
            terminal,
        )
        .await
        .expect("persist failed component-required repair");
    }

    async fn registered_artifact_failure_attempt_at(
        fixture: &Fixture,
        operation_id: &str,
        inventory_ordinal: usize,
        domain: GuardianDomain,
        component: ReconciliationComponent,
    ) -> (ReconciliationAttempt, ReconciliationTerminal) {
        let lifecycle = fixture.state.acquire_instance_lifecycle(INSTANCE_ID).await;
        let foreground = fixture
            .state
            .register_integrity_foreground()
            .expect("register persisted artifact failure foreground")
            .wait_for_settlement()
            .await;
        let verification = fixture
            .state
            .mint_known_good_verification_lease(
                &foreground,
                &lifecycle,
                &PathBuf::from(
                    fixture
                        .state
                        .library_dir()
                        .expect("persisted artifact failure root"),
                ),
            )
            .expect("mint persisted artifact failure verification");
        let authority = fixture
            .state
            .registered_reconciliation_authority_for_verification(&verification)
            .expect("persisted artifact failure authority");
        let attempt = authority
            .repair_artifact_attempt(
                OperationId::new(operation_id),
                DIAGNOSIS_ID,
                domain,
                component,
                registered_artifact_target_for_ordinal(fixture, inventory_ordinal),
                GuardianMode::Managed,
                chrono::Duration::minutes(30),
            )
            .expect("persisted artifact failure attempt");
        let terminal = authority
            .artifact_terminal(
                attempt.clone(),
                ReconciliationTerminalOutcome::Failed,
                ReconciliationQuarantineCheckpoint::default(),
            )
            .expect("persisted artifact failure terminal");
        drop((authority, verification, foreground, lifecycle));
        (attempt, terminal)
    }

    async fn assets_artifact_failure_attempt(
        fixture: &Fixture,
        operation_id: &str,
    ) -> (ReconciliationAttempt, ReconciliationTerminal) {
        registered_artifact_failure_attempt_at(
            fixture,
            operation_id,
            0,
            GuardianDomain::Download,
            ReconciliationComponent::Assets,
        )
        .await
    }

    async fn persist_component_required_pair(
        fixture: &Fixture,
        attempt: &ReconciliationAttempt,
        terminal: ReconciliationTerminal,
    ) {
        let reservation = reserve_reconciliation_attempt(
            fixture.failure_memory.as_ref(),
            fixture.journals.as_ref(),
            reconciliation_attempt_key(attempt),
        )
        .expect("reserve component-required failure");
        persist_component_required_failure(fixture, attempt, terminal.clone()).await;
        commit_reconciliation_memory(
            fixture.failure_memory.as_ref(),
            reconciliation_memory_entry(terminal).expect("canonical artifact failure memory"),
            &reservation,
        )
        .await
        .expect("commit component-required failure memory");
        drop(reservation);
    }

    #[tokio::test]
    async fn registered_artifact_recovery_entry_is_fresh_or_exact_resume() {
        let fresh = fixture("assets-recovery-entry-fresh");
        activate_assets_fixture_inventory(&fresh.state, INSTANCE_ID);
        for condition in [
            super::super::RegisteredArtifactCondition::Missing,
            super::super::RegisteredArtifactCondition::Corrupt,
        ] {
            let authorization = authorized_registered_artifact_repair(&fresh, 0, condition).await;
            let RegisteredArtifactRecoveryEntry::Fresh(authorization) = fresh
                .state
                .registered_artifact_recovery_entry(authorization)
                .expect("Assets finding without exact persistence stays fresh")
            else {
                panic!("Assets finding without exact persistence must not resume");
            };
            drop(authorization);
        }
        cleanup(fresh).await;

        let resumed = fixture("assets-recovery-entry-resume");
        activate_assets_fixture_inventory(&resumed.state, INSTANCE_ID);
        let (attempt, terminal) =
            assets_artifact_failure_attempt(&resumed, "assets-recovery-entry-resume").await;
        persist_component_required_pair(&resumed, &attempt, terminal).await;
        let missing = authorized_registered_artifact_repair(
            &resumed,
            0,
            super::super::RegisteredArtifactCondition::Missing,
        )
        .await;
        let RegisteredArtifactRecoveryEntry::Fresh(missing) = resumed
            .state
            .registered_artifact_recovery_entry(missing)
            .expect("missing Assets remains fresh despite persisted corrupt evidence")
        else {
            panic!("missing Assets must never resume component recovery");
        };
        drop(missing);
        let authorization = authorized_registered_artifact_repair(
            &resumed,
            0,
            super::super::RegisteredArtifactCondition::Corrupt,
        )
        .await;
        let RegisteredArtifactRecoveryEntry::Resume(continuation) = resumed
            .state
            .registered_artifact_recovery_entry(authorization)
            .expect("exact persisted Assets failure resumes")
        else {
            panic!("exact persisted Assets failure must resume");
        };
        assert_eq!(
            continuation
                .evidence
                .artifact_provenance
                .map(|provenance| provenance.inventory_ordinal()),
            Some(0)
        );
        drop(continuation);

        let lifecycle = resumed.state.acquire_instance_lifecycle(INSTANCE_ID).await;
        assert_eq!(
            resumed
                .state
                .recorded_runtime_artifact_repair_failure(&lifecycle, attempt.operation_id())
                .err(),
            Some(ReconciliationEvidenceRejection::ScopeMismatch)
        );
        drop(lifecycle);
        cleanup(resumed).await;

        let unrelated = fixture("assets-recovery-entry-unrelated-leaf");
        activate_assets_fixture_inventory(&unrelated.state, INSTANCE_ID);
        let (attempt, terminal) = registered_artifact_failure_attempt_at(
            &unrelated,
            "assets-recovery-entry-unrelated-leaf",
            1,
            GuardianDomain::Download,
            ReconciliationComponent::Assets,
        )
        .await;
        persist_component_required_pair(&unrelated, &attempt, terminal).await;
        let authorization = authorized_registered_artifact_repair(
            &unrelated,
            0,
            super::super::RegisteredArtifactCondition::Corrupt,
        )
        .await;
        let RegisteredArtifactRecoveryEntry::Fresh(authorization) = unrelated
            .state
            .registered_artifact_recovery_entry(authorization)
            .expect("unrelated Assets predecessor does not block selected leaf")
        else {
            panic!("unrelated Assets predecessor must not resume selected leaf");
        };
        drop(authorization);
        cleanup(unrelated).await;
    }

    #[tokio::test]
    async fn version_bundle_recovery_entry_resumes_only_the_exact_canonical_leaf_failure() {
        let fresh = fixture("version-bundle-recovery-fresh");
        activate_version_bundle_fixture_inventory(&fresh.state, INSTANCE_ID);
        for inventory_ordinal in 0..3 {
            for condition in [
                super::super::RegisteredArtifactCondition::Missing,
                super::super::RegisteredArtifactCondition::Corrupt,
            ] {
                let authorization =
                    authorized_registered_artifact_repair(&fresh, inventory_ordinal, condition)
                        .await;
                let RegisteredArtifactRecoveryEntry::Fresh(authorization) = fresh
                    .state
                    .registered_artifact_recovery_entry(authorization)
                    .expect("VersionBundle finding without durable evidence stays fresh")
                else {
                    panic!("VersionBundle finding without durable evidence must not resume");
                };
                drop(authorization);
            }
        }
        cleanup(fresh).await;

        let resumed = fixture("version-bundle-recovery-resume");
        activate_version_bundle_fixture_inventory(&resumed.state, INSTANCE_ID);
        let (attempt, terminal) = registered_artifact_failure_attempt_at(
            &resumed,
            "version-bundle-recovery-resume",
            1,
            GuardianDomain::Launch,
            ReconciliationComponent::VersionBundle,
        )
        .await;
        persist_component_required_pair(&resumed, &attempt, terminal).await;
        let exact = authorized_registered_artifact_repair(
            &resumed,
            1,
            super::super::RegisteredArtifactCondition::Missing,
        )
        .await;
        let RegisteredArtifactRecoveryEntry::Resume(continuation) = resumed
            .state
            .registered_artifact_recovery_entry(exact)
            .expect("exact durable VersionBundle failure resumes")
        else {
            panic!("exact durable VersionBundle failure must resume");
        };
        assert_eq!(
            continuation
                .evidence
                .artifact_provenance
                .map(|provenance| (provenance.inventory_ordinal(), provenance.component())),
            Some((1, ReconciliationComponent::VersionBundle))
        );
        drop(continuation);
        let unrelated = authorized_registered_artifact_repair(
            &resumed,
            0,
            super::super::RegisteredArtifactCondition::Corrupt,
        )
        .await;
        let RegisteredArtifactRecoveryEntry::Fresh(unrelated) = resumed
            .state
            .registered_artifact_recovery_entry(unrelated)
            .expect("different VersionBundle leaf stays fresh")
        else {
            panic!("different VersionBundle leaf must not resume");
        };
        drop(unrelated);
        cleanup(resumed).await;

        let mismatched = fixture("version-bundle-recovery-component-mismatch");
        activate_version_bundle_fixture_inventory(&mismatched.state, INSTANCE_ID);
        let (attempt, terminal) = registered_artifact_failure_attempt_at(
            &mismatched,
            "version-bundle-recovery-component-mismatch",
            1,
            GuardianDomain::Download,
            ReconciliationComponent::Assets,
        )
        .await;
        persist_component_required_pair(&mismatched, &attempt, terminal).await;
        let authorization = authorized_registered_artifact_repair(
            &mismatched,
            1,
            super::super::RegisteredArtifactCondition::Corrupt,
        )
        .await;
        assert_eq!(
            mismatched
                .state
                .registered_artifact_recovery_entry(authorization)
                .err(),
            Some(ReconciliationEvidenceRejection::ScopeMismatch)
        );
        cleanup(mismatched).await;
    }

    #[tokio::test]
    async fn registered_assets_resume_refuses_partial_duplicate_and_drifted_evidence() {
        let nonterminal = fixture("assets-resume-nonterminal");
        activate_assets_fixture_inventory(&nonterminal.state, INSTANCE_ID);
        let (attempt, _) =
            assets_artifact_failure_attempt(&nonterminal, "assets-resume-nonterminal").await;
        nonterminal
            .journals
            .create(component_required_journal(&attempt))
            .await
            .expect("persist nonterminal Assets candidate");
        let authorization = authorized_registered_artifact_repair(
            &nonterminal,
            0,
            super::super::RegisteredArtifactCondition::Corrupt,
        )
        .await;
        assert_eq!(
            nonterminal
                .state
                .registered_artifact_recovery_entry(authorization)
                .err(),
            Some(ReconciliationEvidenceRejection::JournalMismatch)
        );
        cleanup(nonterminal).await;

        let journal_only = fixture("assets-resume-journal-only");
        activate_assets_fixture_inventory(&journal_only.state, INSTANCE_ID);
        let (attempt, terminal) =
            assets_artifact_failure_attempt(&journal_only, "assets-resume-journal-only").await;
        persist_component_required_failure(&journal_only, &attempt, terminal).await;
        let authorization = authorized_registered_artifact_repair(
            &journal_only,
            0,
            super::super::RegisteredArtifactCondition::Corrupt,
        )
        .await;
        assert_eq!(
            journal_only
                .state
                .registered_artifact_recovery_entry(authorization)
                .err(),
            Some(ReconciliationEvidenceRejection::MemoryMissing)
        );
        cleanup(journal_only).await;

        let memory_only = fixture("assets-resume-memory-only");
        activate_assets_fixture_inventory(&memory_only.state, INSTANCE_ID);
        let (attempt, terminal) =
            assets_artifact_failure_attempt(&memory_only, "assets-resume-memory-only").await;
        let reservation = reserve_reconciliation_attempt(
            memory_only.failure_memory.as_ref(),
            memory_only.journals.as_ref(),
            reconciliation_attempt_key(&attempt),
        )
        .expect("reserve memory-only Assets failure");
        commit_reconciliation_memory(
            memory_only.failure_memory.as_ref(),
            reconciliation_memory_entry(terminal).expect("memory-only Assets entry"),
            &reservation,
        )
        .await
        .expect("commit memory-only Assets failure");
        drop(reservation);
        let authorization = authorized_registered_artifact_repair(
            &memory_only,
            0,
            super::super::RegisteredArtifactCondition::Corrupt,
        )
        .await;
        assert_eq!(
            memory_only
                .state
                .registered_artifact_recovery_entry(authorization)
                .err(),
            Some(ReconciliationEvidenceRejection::JournalMissing)
        );
        cleanup(memory_only).await;

        let disagreed = fixture("assets-resume-memory-disagreement");
        activate_assets_fixture_inventory(&disagreed.state, INSTANCE_ID);
        let (attempt, terminal) =
            assets_artifact_failure_attempt(&disagreed, "assets-resume-memory-disagreement").await;
        persist_component_required_pair(&disagreed, &attempt, terminal).await;
        let mut snapshot = disagreed
            .failure_memory
            .snapshot()
            .expect("Assets memory snapshot");
        snapshot.entries[0].occurrence_count = 2;
        let drifted_memory = Arc::new(GuardianFailureMemoryStore::new());
        drifted_memory
            .load_snapshot(snapshot)
            .expect("load valid noncanonical Assets memory");
        let drifted_state = disagreed
            .state
            .clone()
            .with_reconciliation_stores(disagreed.journals.clone(), drifted_memory);
        let authorization = authorized_registered_artifact_repair(
            &disagreed,
            0,
            super::super::RegisteredArtifactCondition::Corrupt,
        )
        .await;
        assert_eq!(
            drifted_state
                .registered_artifact_recovery_entry(authorization)
                .err(),
            Some(ReconciliationEvidenceRejection::MemoryNotFailed)
        );
        drop(drifted_state);
        cleanup(disagreed).await;

        let duplicate = fixture("assets-resume-duplicate");
        activate_assets_fixture_inventory(&duplicate.state, INSTANCE_ID);
        let (first, first_terminal) =
            assets_artifact_failure_attempt(&duplicate, "assets-resume-duplicate-first").await;
        persist_component_required_pair(&duplicate, &first, first_terminal).await;
        let (second, second_terminal) =
            assets_artifact_failure_attempt(&duplicate, "assets-resume-duplicate-second").await;
        persist_component_required_failure(&duplicate, &second, second_terminal).await;
        let authorization = authorized_registered_artifact_repair(
            &duplicate,
            0,
            super::super::RegisteredArtifactCondition::Corrupt,
        )
        .await;
        assert_eq!(
            duplicate
                .state
                .registered_artifact_recovery_entry(authorization)
                .err(),
            Some(ReconciliationEvidenceRejection::JournalMismatch)
        );
        cleanup(duplicate).await;

        let drifted = fixture("assets-resume-inventory-drift");
        activate_assets_fixture_inventory(&drifted.state, INSTANCE_ID);
        let authorization = authorized_registered_artifact_repair(
            &drifted,
            0,
            super::super::RegisteredArtifactCondition::Corrupt,
        )
        .await;
        let replacement = activate_assets_fixture_inventory(&drifted.state, INSTANCE_ID);
        assert_eq!(
            drifted
                .state
                .registered_artifact_recovery_entry(authorization)
                .err(),
            Some(ReconciliationEvidenceRejection::IncarnationMismatch)
        );
        drop(replacement);
        cleanup(drifted).await;
    }

    #[tokio::test]
    async fn registered_assets_resume_requires_exact_component_failure_shape() {
        let fixture = fixture("assets-resume-terminal-shape");
        activate_assets_fixture_inventory(&fixture.state, INSTANCE_ID);
        let (attempt, terminal) =
            assets_artifact_failure_attempt(&fixture, "assets-resume-terminal-shape").await;
        let reservation = reserve_reconciliation_attempt(
            fixture.failure_memory.as_ref(),
            fixture.journals.as_ref(),
            reconciliation_attempt_key(&attempt),
        )
        .expect("reserve malformed Assets failure");
        persist_failed_journal_at(
            &fixture,
            &attempt,
            terminal.clone(),
            REGISTERED_ARTIFACT_COMPONENT_REBUILD_FAILURE_POINT,
        )
        .await;
        commit_reconciliation_memory(
            fixture.failure_memory.as_ref(),
            reconciliation_memory_entry(terminal).expect("malformed canonical memory"),
            &reservation,
        )
        .await
        .expect("commit malformed Assets failure memory");
        drop(reservation);
        let authorization = authorized_registered_artifact_repair(
            &fixture,
            0,
            super::super::RegisteredArtifactCondition::Corrupt,
        )
        .await;
        assert_eq!(
            fixture
                .state
                .registered_artifact_recovery_entry(authorization)
                .err(),
            Some(ReconciliationEvidenceRejection::JournalMismatch)
        );
        cleanup(fixture).await;
    }

    async fn recorded_runtime_artifact_failure(
        fixture: &Fixture,
        instance_id: &str,
        operation_id: &str,
    ) -> (RecordedRuntimeArtifactRepairFailure, ReconciliationAttempt) {
        let lifecycle = fixture.state.acquire_instance_lifecycle(instance_id).await;
        let authority = fixture
            .state
            .registered_reconciliation_authority(&lifecycle)
            .expect("registered Runtime authority");
        let attempt = authority
            .repair_artifact_attempt(
                OperationId::new(operation_id),
                DIAGNOSIS_ID,
                GuardianDomain::Runtime,
                ReconciliationComponent::Runtime,
                runtime_target(),
                GuardianMode::Managed,
                chrono::Duration::minutes(30),
            )
            .expect("Runtime artifact attempt");
        let terminal = authority
            .terminal(attempt.clone(), ReconciliationTerminalOutcome::Failed)
            .expect("Runtime artifact failure");
        let reservation = reserve_reconciliation_attempt(
            fixture.failure_memory.as_ref(),
            fixture.journals.as_ref(),
            reconciliation_attempt_key(&attempt),
        )
        .expect("reserve Runtime artifact attempt");
        persist_failed_journal(fixture, &attempt, terminal.clone()).await;
        commit_reconciliation_memory(
            fixture.failure_memory.as_ref(),
            reconciliation_memory_entry(terminal).expect("Runtime artifact memory"),
            &reservation,
        )
        .await
        .expect("commit Runtime artifact memory");
        drop(reservation);
        let evidence = fixture
            .state
            .recorded_runtime_artifact_repair_failure(&lifecycle, attempt.operation_id())
            .expect("recorded Runtime artifact failure");
        drop((authority, lifecycle));
        (evidence, attempt)
    }

    async fn persist_runtime_component_ladder(
        fixture: &Fixture,
        label: &str,
        component_id: &str,
        mode: GuardianMode,
    ) -> ReconciliationTerminal {
        let lifecycle = fixture.state.acquire_instance_lifecycle(INSTANCE_ID).await;
        let authority = fixture
            .state
            .registered_reconciliation_authority(&lifecycle)
            .expect("registered ladder authority");
        let target = TargetDescriptor::new(
            StabilizationSystem::Execution,
            TargetKind::Runtime,
            component_id,
            OwnershipClass::LauncherManaged,
        );
        let artifact_attempt = authority
            .repair_artifact_attempt(
                OperationId::new(format!("{label}-artifact")),
                DIAGNOSIS_ID,
                GuardianDomain::Runtime,
                ReconciliationComponent::Runtime,
                target.clone(),
                mode,
                chrono::Duration::minutes(30),
            )
            .expect("ladder artifact attempt");
        let artifact_terminal = authority
            .terminal(
                artifact_attempt.clone(),
                ReconciliationTerminalOutcome::Failed,
            )
            .expect("ladder artifact terminal");
        let artifact_reservation = reserve_reconciliation_attempt(
            fixture.failure_memory.as_ref(),
            fixture.journals.as_ref(),
            reconciliation_attempt_key(&artifact_attempt),
        )
        .expect("reserve ladder artifact");
        persist_failed_journal(fixture, &artifact_attempt, artifact_terminal.clone()).await;
        commit_reconciliation_memory(
            fixture.failure_memory.as_ref(),
            reconciliation_memory_entry(artifact_terminal).expect("ladder artifact memory"),
            &artifact_reservation,
        )
        .await
        .expect("commit ladder artifact memory");
        drop(artifact_reservation);

        let observed_at = chrono::Utc::now().fixed_offset();
        let component_attempt = fixture
            .state
            .registered_reconciliation_attempt_at(
                &lifecycle,
                OperationId::new(format!("{label}-component")),
                DIAGNOSIS_ID,
                GuardianDomain::Runtime,
                ReconciliationRung::RebuildComponent,
                ReconciliationComponent::Runtime,
                target,
                mode,
                observed_at,
                observed_at + chrono::Duration::minutes(30),
                ReconciliationLineage::Predecessor {
                    operation_id: artifact_attempt.operation_id().clone(),
                },
            )
            .expect("ladder component attempt");
        let component_terminal = authority
            .terminal(
                component_attempt.clone(),
                ReconciliationTerminalOutcome::Failed,
            )
            .expect("ladder component terminal");
        let component_reservation = reserve_reconciliation_attempt(
            fixture.failure_memory.as_ref(),
            fixture.journals.as_ref(),
            reconciliation_attempt_key(&component_attempt),
        )
        .expect("reserve ladder component");
        fixture
            .journals
            .create(component_rebuild_journal_for_attempt(&component_attempt))
            .await
            .expect("persist ladder component plan");
        let mut failure =
            OperationJournalStep::new(RUNTIME_COMPONENT_REBUILD_STEP, OperationPhase::Repairing);
        failure.result = OperationStepResult::Failed;
        failure.changed_target = Some(component_attempt.target().clone());
        record_reconciliation_journal_failure(
            fixture.journals.as_ref(),
            component_attempt.operation_id(),
            failure,
            RUNTIME_COMPONENT_REBUILD_STEP,
            component_terminal.clone(),
        )
        .await
        .expect("persist ladder component terminal");
        commit_reconciliation_memory(
            fixture.failure_memory.as_ref(),
            reconciliation_memory_entry(component_terminal.clone())
                .expect("ladder component memory"),
            &component_reservation,
        )
        .await
        .expect("commit ladder component memory");
        drop((component_reservation, authority, lifecycle));
        component_terminal
    }

    fn whole_instance_terminal_journal_for_shape(
        attempt: &ReconciliationAttempt,
        outcome: ReconciliationTerminalOutcome,
        checkpoint: ReconciliationQuarantineCheckpoint,
        terminal_step_id: &'static str,
    ) -> (OperationJournalEntry, ReconciliationTerminal) {
        let terminal = ReconciliationTerminal::from_attempt(attempt.clone(), outcome, checkpoint);
        let mut journal = whole_instance_rematerialization_journal_for_attempt(attempt);
        if !terminal.quarantine_checkpoint().is_empty() {
            let mut quarantine =
                OperationJournalStep::new(COMPONENT_QUARANTINE_STEP, OperationPhase::Repairing);
            quarantine.result = OperationStepResult::Completed;
            quarantine.changed_target = Some(attempt.target().clone());
            quarantine.rollback = RollbackState::Available;
            journal.completed_steps.push(quarantine);
        }
        let mut terminal_step =
            OperationJournalStep::new(terminal_step_id, OperationPhase::Repairing);
        terminal_step.changed_target = Some(attempt.target().clone());
        terminal_step.rollback = if terminal_step_id == WHOLE_INSTANCE_REMATERIALIZATION_START_STEP
        {
            RollbackState::NotApplicable
        } else {
            RollbackState::Available
        };
        match outcome {
            ReconciliationTerminalOutcome::Succeeded => {
                terminal_step.result = OperationStepResult::Completed;
                journal.status = OperationStatus::Succeeded;
                journal.outcome = Some(OperationOutcome::Succeeded);
            }
            ReconciliationTerminalOutcome::Failed => {
                terminal_step.result = OperationStepResult::Failed;
                journal.status = OperationStatus::Failed;
                journal.outcome = Some(OperationOutcome::Failed);
                journal.failure_point = Some(terminal_step_id.to_string());
            }
        }
        journal.completed_steps.push(terminal_step);
        journal.reconciliation_terminal = Some(terminal.clone());
        (journal, terminal)
    }

    async fn whole_instance_admission(
        fixture: &Fixture,
        label: &str,
        mode: GuardianMode,
    ) -> RegisteredWholeInstanceRematerializationAdmission {
        persist_runtime_component_ladder(fixture, label, "java-runtime-delta", mode).await;
        set_guardian_mode(fixture, mode);
        let offer = whole_instance_offer(fixture, mode).await;
        fixture
            .state
            .admit_whole_instance_rematerialization(
                offer.into_authorization(),
                OperationId::new(format!("{label}-whole")),
                chrono::Duration::minutes(30),
            )
            .await
            .expect("whole-instance admission")
    }

    async fn whole_instance_offer(
        fixture: &Fixture,
        mode: GuardianMode,
    ) -> GuardianWholeInstanceRematerializationOffer {
        let eligibility = fixture
            .state
            .whole_instance_rematerialization_eligibility(INSTANCE_ID)
            .await
            .expect("whole-instance eligibility");
        let assessment = assess_whole_instance_rematerialization(eligibility, mode)
            .expect("whole-instance assessment");
        let GuardianWholeInstanceRematerializationDisposition::Offered(offer) = assessment else {
            panic!("Managed and Custom assessments must mint a whole-instance offer");
        };
        offer
    }

    fn set_guardian_mode(fixture: &Fixture, mode: GuardianMode) {
        let mut config = fixture.state.config.current();
        config.guardian_mode = match mode {
            GuardianMode::Managed => "managed",
            GuardianMode::Custom => "custom",
            GuardianMode::Disabled => "disabled",
        }
        .to_string();
        fixture.state.replace_config_for_test(config);
    }

    async fn assert_whole_instance_ownership_is_held(fixture: &Fixture) {
        assert!(fixture.state.instance_lifecycle_is_held(INSTANCE_ID).await);
        assert!(
            tokio::time::timeout(
                Duration::from_millis(25),
                fixture.state.config.acquire_mutation()
            )
            .await
            .is_err(),
            "whole-instance owner must retain config exclusion"
        );
        assert!(
            tokio::time::timeout(
                Duration::from_millis(25),
                fixture.state.sessions.acquire_shared_component_mutation(),
            )
            .await
            .is_err(),
            "whole-instance owner must retain component exclusion"
        );
    }

    async fn prepare_core_whole_instance_fixture(
        fixture: &Fixture,
        runtime_bytes: &'static [u8],
    ) -> axial_minecraft::ManagedWholeInstanceFixtureForTest {
        let runtime_url = serve_runtime_bytes(runtime_bytes).await;
        axial_minecraft::prepare_managed_whole_instance_fixture_for_test(
            PathBuf::from(fixture.state.library_dir().expect("fixture library root")),
            "1.21.1",
            RuntimeId::from("java-runtime-delta"),
            &runtime_url,
            runtime_bytes,
        )
        .await
        .expect("prepare sealed Core whole-instance fixture")
    }

    async fn serve_runtime_bytes(bytes: &'static [u8]) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("whole-instance Runtime fixture server");
        let address = listener
            .local_addr()
            .expect("whole-instance Runtime fixture address");
        tokio::spawn(async move {
            for _ in 0..2 {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let mut request = [0_u8; 1024];
                let _ = socket.read(&mut request).await;
                let headers = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    bytes.len()
                );
                if socket.write_all(headers.as_bytes()).await.is_ok() {
                    let _ = socket.write_all(bytes).await;
                }
            }
        });
        format!("http://{address}/java")
    }

    #[tokio::test]
    async fn registered_authority_rejects_foreign_lifecycle_and_changed_root() {
        let owner = fixture("authority-owner");
        let foreign = fixture("authority-foreign");
        let owner_lifecycle = owner.state.acquire_instance_lifecycle(INSTANCE_ID).await;
        let foreign_lifecycle = foreign.state.acquire_instance_lifecycle(INSTANCE_ID).await;

        assert_eq!(
            foreign
                .state
                .registered_reconciliation_authority(&owner_lifecycle)
                .err(),
            Some(ReconciliationEvidenceRejection::ScopeMismatch)
        );
        assert_eq!(
            owner
                .state
                .registered_reconciliation_authority(&foreign_lifecycle)
                .err(),
            Some(ReconciliationEvidenceRejection::ScopeMismatch)
        );

        let authority = owner
            .state
            .registered_reconciliation_authority(&owner_lifecycle)
            .expect("owner authority");
        let attempt = authority
            .repair_artifact_attempt(
                OperationId::new("authority-root-change"),
                DIAGNOSIS_ID,
                GuardianDomain::Launch,
                ReconciliationComponent::VersionBundle,
                artifact_target(),
                GuardianMode::Managed,
                chrono::Duration::minutes(30),
            )
            .expect("attempt before root change");
        let replacement_library = owner.root.join("replacement-library");
        fs::create_dir_all(&replacement_library).expect("replacement library root");
        owner
            .state
            .set_library_dir_for_test(replacement_library.to_string_lossy().into_owned());
        assert_eq!(
            authority
                .terminal(attempt, ReconciliationTerminalOutcome::Failed)
                .err(),
            Some(ReconciliationEvidenceRejection::IncarnationMismatch)
        );

        drop((authority, owner_lifecycle, foreign_lifecycle));
        cleanup(owner).await;
        cleanup(foreign).await;
    }

    #[tokio::test]
    async fn ambiguous_running_attempt_survives_startup_and_blocks_a_new_operation() {
        let fixture = fixture("ambiguous-running");
        let (first, _) = registered_attempt(
            &fixture,
            "ambiguous-first",
            ReconciliationComponent::VersionBundle,
        )
        .await;
        fixture
            .journals
            .create(planned_journal(&first))
            .await
            .expect("persist ambiguous plan");
        let mut checkpoint = OperationJournalStep::new("effect_started", OperationPhase::Repairing);
        checkpoint.result = OperationStepResult::Completed;
        fixture
            .journals
            .record_checkpoint(first.operation_id(), checkpoint)
            .await
            .expect("persist running transition");

        fixture
            .state
            .reconcile_reconciliation_startup()
            .await
            .expect("nonterminal startup scan");
        assert_eq!(
            fixture
                .journals
                .get(first.operation_id())
                .expect("running attempt survives")
                .status,
            OperationStatus::Running
        );

        let (second, _) = registered_attempt(
            &fixture,
            "ambiguous-second",
            ReconciliationComponent::VersionBundle,
        )
        .await;
        assert_eq!(
            reconciliation_attempt_key(&first),
            reconciliation_attempt_key(&second)
        );
        assert_eq!(
            reserve_reconciliation_attempt(
                fixture.failure_memory.as_ref(),
                fixture.journals.as_ref(),
                reconciliation_attempt_key(&second),
            )
            .err(),
            Some(ReconciliationAttemptRejection::AmbiguousPriorAttempt)
        );

        cleanup(fixture).await;
    }

    #[tokio::test]
    async fn startup_replays_exact_active_terminal_once() {
        let fixture = fixture("terminal-replay");
        let (attempt, terminal) = registered_attempt(
            &fixture,
            "terminal-replay",
            ReconciliationComponent::VersionBundle,
        )
        .await;
        let expected_memory = reconciliation_memory_entry(terminal.clone()).expect("typed memory");
        persist_failed_journal(&fixture, &attempt, terminal).await;
        assert!(fixture.failure_memory.get(&expected_memory.key).is_none());

        fixture
            .state
            .reconcile_reconciliation_startup()
            .await
            .expect("replay terminal into memory");
        assert_eq!(
            fixture.failure_memory.get(&expected_memory.key),
            Some(expected_memory.clone())
        );
        let first_replay = fixture.failure_memory.list();
        fixture
            .state
            .reconcile_reconciliation_startup()
            .await
            .expect("idempotent second replay");
        assert_eq!(fixture.failure_memory.list(), first_replay);

        cleanup(fixture).await;
    }

    #[tokio::test]
    async fn startup_rejects_orphan_memory_and_overlapping_active_terminals() {
        let orphan = fixture("orphan-memory");
        let (_, orphan_terminal) = registered_attempt(
            &orphan,
            "orphan-memory",
            ReconciliationComponent::VersionBundle,
        )
        .await;
        let orphan_memory = reconciliation_memory_entry(orphan_terminal).expect("orphan memory");
        orphan
            .failure_memory
            .load_snapshot(
                FailureMemorySnapshot::new(vec![orphan_memory]).expect("valid memory snapshot"),
            )
            .expect("load orphan memory");
        assert_eq!(
            orphan
                .state
                .reconcile_reconciliation_startup()
                .await
                .expect_err("orphan active memory must fail startup")
                .kind(),
            io::ErrorKind::InvalidData
        );
        cleanup(orphan).await;

        let overlap = fixture("overlapping-terminals");
        let (first, first_terminal) = registered_attempt(
            &overlap,
            "overlap-first",
            ReconciliationComponent::VersionBundle,
        )
        .await;
        let (second, second_terminal) = registered_attempt(
            &overlap,
            "overlap-second",
            ReconciliationComponent::VersionBundle,
        )
        .await;
        assert_eq!(
            reconciliation_attempt_key(&first),
            reconciliation_attempt_key(&second)
        );
        persist_failed_journal(&overlap, &first, first_terminal).await;
        persist_failed_journal(&overlap, &second, second_terminal).await;
        assert_eq!(
            overlap
                .state
                .reconcile_reconciliation_startup()
                .await
                .expect_err("overlapping active terminals must fail startup")
                .kind(),
            io::ErrorKind::InvalidData
        );
        cleanup(overlap).await;
    }

    #[tokio::test]
    async fn assets_component_requires_the_closed_persisted_predecessor_shape() {
        let fixture = fixture("assets-component-leaf-shape");
        let lifecycle = fixture.state.acquire_instance_lifecycle(INSTANCE_ID).await;
        let authority = fixture
            .state
            .registered_reconciliation_authority(&lifecycle)
            .expect("registered authority");
        let wrong_domain = authority
            .repair_artifact_attempt(
                OperationId::new("assets-wrong-domain-artifact"),
                DIAGNOSIS_ID,
                GuardianDomain::Library,
                ReconciliationComponent::Assets,
                artifact_target(),
                GuardianMode::Managed,
                chrono::Duration::minutes(30),
            )
            .expect("structurally valid attempt carrier");
        assert_eq!(
            authority
                .artifact_terminal(
                    wrong_domain,
                    ReconciliationTerminalOutcome::Failed,
                    ReconciliationQuarantineCheckpoint::default(),
                )
                .err(),
            Some(ReconciliationEvidenceRejection::ScopeMismatch)
        );
        drop((authority, lifecycle));
        cleanup(fixture).await;
    }

    #[test]
    fn component_rollback_effect_gates_reject_no_effect() {
        assert!(!libraries_rollback_has_effect(
            ManagedLibrariesRollbackEffect::None
        ));
        assert!(!assets_rollback_has_effect(
            ManagedAssetsRollbackEffect::None
        ));
        assert!(libraries_rollback_has_effect(
            ManagedLibrariesRollbackEffect::Execution
        ));
        assert!(assets_rollback_has_effect(
            ManagedAssetsRollbackEffect::Reconciliation
        ));
    }

    #[tokio::test]
    async fn runtime_component_admission_rejects_custom_predecessor() {
        let fixture = fixture("runtime-component-custom-predecessor");
        let lifecycle = fixture.state.acquire_instance_lifecycle(INSTANCE_ID).await;
        let authority = fixture
            .state
            .registered_reconciliation_authority(&lifecycle)
            .expect("registered Runtime authority");
        let attempt = authority
            .repair_artifact_attempt(
                OperationId::new("runtime-component-custom-artifact"),
                DIAGNOSIS_ID,
                GuardianDomain::Runtime,
                ReconciliationComponent::Runtime,
                runtime_target(),
                GuardianMode::Custom,
                chrono::Duration::minutes(30),
            )
            .expect("Custom Runtime predecessor attempt");
        let terminal = authority
            .terminal(attempt.clone(), ReconciliationTerminalOutcome::Failed)
            .expect("Custom Runtime predecessor terminal");
        let reservation = reserve_reconciliation_attempt(
            fixture.failure_memory.as_ref(),
            fixture.journals.as_ref(),
            reconciliation_attempt_key(&attempt),
        )
        .expect("reserve Custom Runtime predecessor");
        persist_failed_journal(&fixture, &attempt, terminal.clone()).await;
        commit_reconciliation_memory(
            fixture.failure_memory.as_ref(),
            reconciliation_memory_entry(terminal).expect("Custom Runtime memory"),
            &reservation,
        )
        .await
        .expect("commit Custom Runtime memory");
        drop(reservation);
        let evidence = RecordedRuntimeArtifactRepairFailure {
            evidence: fixture
                .state
                .recorded_reconciliation_failure_at(
                    &lifecycle,
                    attempt.operation_id(),
                    ReconciliationRung::RepairArtifact,
                    chrono::Utc::now().fixed_offset(),
                    None,
                )
                .expect("record exact Custom Runtime predecessor"),
        };

        assert_eq!(
            fixture
                .state
                .admit_runtime_component_rebuild(
                    evidence,
                    OperationId::new("runtime-component-custom-rebuild"),
                    chrono::Duration::minutes(30),
                )
                .await
                .err(),
            Some(ReconciliationEvidenceRejection::ScopeMismatch)
        );

        drop((authority, lifecycle));
        cleanup(fixture).await;
    }

    #[tokio::test]
    async fn verified_continuation_rejects_noncanonical_memory() {
        let fixture = fixture("verified-continuation-memory-drift");
        activate_assets_fixture_inventory(&fixture.state, INSTANCE_ID);
        let lifecycle = fixture.state.acquire_instance_lifecycle(INSTANCE_ID).await;
        let foreground = fixture
            .state
            .register_integrity_foreground()
            .expect("register verified predecessor foreground")
            .wait_for_settlement()
            .await;
        let verification = fixture
            .state
            .mint_known_good_verification_lease(
                &foreground,
                &lifecycle,
                &PathBuf::from(
                    fixture
                        .state
                        .library_dir()
                        .expect("verified predecessor root"),
                ),
            )
            .expect("mint verified predecessor lease");
        let authority = fixture
            .state
            .registered_reconciliation_authority_for_verification(&verification)
            .expect("verified predecessor authority");
        let attempt = authority
            .repair_artifact_attempt(
                OperationId::new("verified-continuation-memory-drift"),
                DIAGNOSIS_ID,
                GuardianDomain::Download,
                ReconciliationComponent::Assets,
                registered_artifact_target_for_ordinal(&fixture, 0),
                GuardianMode::Managed,
                chrono::Duration::minutes(30),
            )
            .expect("verified predecessor attempt");
        let terminal = authority
            .artifact_terminal(
                attempt.clone(),
                ReconciliationTerminalOutcome::Failed,
                ReconciliationQuarantineCheckpoint::default(),
            )
            .expect("verified predecessor terminal");
        let reservation = reserve_reconciliation_attempt(
            fixture.failure_memory.as_ref(),
            fixture.journals.as_ref(),
            reconciliation_attempt_key(&attempt),
        )
        .expect("reserve verified predecessor");
        persist_failed_journal(&fixture, &attempt, terminal.clone()).await;
        commit_reconciliation_memory(
            fixture.failure_memory.as_ref(),
            reconciliation_memory_entry(terminal).expect("canonical predecessor memory"),
            &reservation,
        )
        .await
        .expect("commit canonical predecessor memory");
        drop(reservation);
        let mut snapshot = fixture.failure_memory.snapshot().expect("memory snapshot");
        snapshot.entries[0].occurrence_count = 2;
        let drifted_memory = Arc::new(GuardianFailureMemoryStore::new());
        drifted_memory
            .load_snapshot(snapshot)
            .expect("load valid but noncanonical memory");
        let drifted_state = fixture
            .state
            .clone()
            .with_reconciliation_stores(fixture.journals.clone(), drifted_memory);
        let drifted_authority = drifted_state
            .registered_reconciliation_authority_for_verification(&verification)
            .expect("drifted verified authority");

        assert_eq!(
            drifted_authority
                .into_registered_artifact_failed_repair(&attempt)
                .err(),
            Some(ReconciliationEvidenceRejection::MemoryNotFailed)
        );

        drop((
            drifted_state,
            authority,
            verification,
            foreground,
            lifecycle,
        ));
        cleanup(fixture).await;
    }

    #[tokio::test]
    async fn component_rebuild_admission_revalidates_root_after_both_mutation_waits() {
        let fixture = fixture("component-admission-root-drift");
        let (evidence, _) = recorded_runtime_artifact_failure(
            &fixture,
            INSTANCE_ID,
            "component-admission-root-drift-artifact",
        )
        .await;
        let component_writer = fixture
            .state
            .sessions
            .acquire_shared_component_mutation()
            .await
            .expect("hold component writer");
        let (config_acquired_tx, config_acquired_rx) = tokio::sync::oneshot::channel();
        let state = fixture.state.clone();
        let admission = tokio::spawn(async move {
            state
                .admit_component_rebuild_with_config_observer(
                    evidence,
                    None,
                    OperationId::new("component-admission-root-drift-rebuild"),
                    chrono::Duration::minutes(30),
                    move || {
                        let _ = config_acquired_tx.send(());
                    },
                )
                .await
        });
        config_acquired_rx
            .await
            .expect("admission owns config before waiting for component writer");

        let replacement_library = fixture.root.join("replacement-library-during-admission");
        fs::create_dir_all(&replacement_library).expect("replacement library root");
        fixture
            .state
            .set_library_dir_for_test(replacement_library.to_string_lossy().into_owned());
        drop(component_writer);

        assert_eq!(
            admission.await.expect("admission task").err(),
            Some(ReconciliationEvidenceRejection::IncarnationMismatch)
        );
        cleanup(fixture).await;
    }

    #[tokio::test]
    async fn component_rebuild_admission_rejects_inventory_replacement_during_wait() {
        let fixture = fixture("component-admission-inventory-wait-drift");
        let original_inventory = {
            let instance = fixture
                .state
                .instances()
                .get(INSTANCE_ID)
                .expect("test instance");
            fixture
                .state
                .known_good
                .active_inventory(
                    &instance.id,
                    &instance.version_id,
                    &instance.created_at,
                    &PathBuf::from(fixture.state.library_dir().expect("library root")),
                )
                .expect("original inventory")
        };
        let (evidence, _) = recorded_runtime_artifact_failure(
            &fixture,
            INSTANCE_ID,
            "component-admission-inventory-artifact",
        )
        .await;
        let component_writer = fixture
            .state
            .sessions
            .acquire_shared_component_mutation()
            .await
            .expect("hold component writer");
        let (config_acquired_tx, config_acquired_rx) = tokio::sync::oneshot::channel();
        let state = fixture.state.clone();
        let admission = tokio::spawn(async move {
            state
                .admit_component_rebuild_with_config_observer(
                    evidence,
                    None,
                    OperationId::new("component-admission-inventory-rebuild"),
                    chrono::Duration::minutes(30),
                    move || {
                        let _ = config_acquired_tx.send(());
                    },
                )
                .await
        });
        config_acquired_rx
            .await
            .expect("admission owns config before waiting for component writer");
        let admitted_inventory = activate_empty_inventory(&fixture.state, INSTANCE_ID);
        assert!(!Arc::ptr_eq(&original_inventory, &admitted_inventory));
        drop(component_writer);

        assert_eq!(
            admission.await.expect("admission task").err(),
            Some(ReconciliationEvidenceRejection::IncarnationMismatch)
        );
        cleanup(fixture).await;
    }

    #[tokio::test]
    async fn component_rebuild_admission_pins_inventory_after_admission() {
        let fixture = fixture("component-admission-inventory-post-admission-drift");
        let (evidence, _) = recorded_runtime_artifact_failure(
            &fixture,
            INSTANCE_ID,
            "component-admission-inventory-post-admission-artifact",
        )
        .await;
        let admission = fixture
            .state
            .admit_runtime_component_rebuild(
                evidence,
                OperationId::new("component-admission-inventory-post-admission-rebuild"),
                chrono::Duration::minutes(30),
            )
            .await
            .expect("component admission");
        let admitted_inventory = admission.admitted_inventory().clone();
        admission
            .validate_runtime_identity(&RuntimeId::from("java-runtime-delta"), true)
            .expect("admitted Runtime inventory is current");

        let later_inventory = activate_empty_inventory(&fixture.state, INSTANCE_ID);
        assert!(!Arc::ptr_eq(&admitted_inventory, &later_inventory));
        assert_eq!(
            admission
                .validate_runtime_identity(&RuntimeId::from("java-runtime-delta"), true)
                .err(),
            Some(ReconciliationEvidenceRejection::IncarnationMismatch)
        );

        drop(admission);
        cleanup(fixture).await;
    }

    #[tokio::test]
    async fn component_rebuild_postactivation_failure_invalidates_refreshed_inventory() {
        let fixture = fixture("component-postactivation-failure");
        let (evidence, _) = recorded_runtime_artifact_failure(
            &fixture,
            INSTANCE_ID,
            "component-postactivation-failure-artifact",
        )
        .await;
        let admission = fixture
            .state
            .admit_runtime_component_rebuild(
                evidence,
                OperationId::new("component-postactivation-failure-rebuild"),
                chrono::Duration::minutes(30),
            )
            .await
            .expect("component admission");
        let component = RuntimeId::from("java-runtime-delta");
        let receipt = axial_minecraft::rebuild_managed_runtime_fixture_for_test(
            fixture.state.managed_runtime_cache(),
            component.clone(),
        )
        .await
        .expect("sealed Runtime rebuild receipt");
        let runtime_root = fixture
            .state
            .managed_runtime_cache()
            .component_root(component.as_str())
            .expect("managed Runtime root");
        let java = if cfg!(target_os = "windows") {
            runtime_root.join("bin").join("javaw.exe")
        } else if cfg!(target_os = "macos") {
            runtime_root
                .join("jre.bundle")
                .join("Contents")
                .join("Home")
                .join("bin")
                .join("java")
        } else {
            runtime_root.join("bin").join("java")
        };

        assert_eq!(
            admission
                .succeeded_terminal_with_activation_observer(&receipt, || {
                    fs::write(&java, b"invalidated after known-good activation")
                        .expect("invalidate sealed Runtime receipt");
                })
                .await
                .err(),
            Some(ReconciliationEvidenceRejection::JournalMismatch)
        );
        assert!(
            admission
                .runtime_postcondition_failure_inventory_for_test()
                .get()
                .is_some()
        );
        let instance = fixture
            .state
            .instances()
            .get(INSTANCE_ID)
            .expect("test instance");
        assert!(
            fixture
                .state
                .known_good
                .active_inventory(
                    &instance.id,
                    &instance.version_id,
                    &instance.created_at,
                    &PathBuf::from(fixture.state.library_dir().expect("library root")),
                )
                .is_none(),
            "failed refreshed projection must not remain live authority"
        );
        let terminal = admission
            .failed_postcondition_terminal(&receipt)
            .expect("failed terminal retains exact refreshed projection proof");
        assert_eq!(terminal.outcome(), ReconciliationTerminalOutcome::Failed);

        drop((receipt, admission));
        cleanup(fixture).await;
    }

    #[tokio::test]
    async fn component_rebuild_postactivation_cleanup_retains_replacement_inventory() {
        let fixture = fixture("component-postactivation-inventory-replacement");
        let (evidence, _) = recorded_runtime_artifact_failure(
            &fixture,
            INSTANCE_ID,
            "component-postactivation-inventory-replacement-artifact",
        )
        .await;
        let admission = fixture
            .state
            .admit_runtime_component_rebuild(
                evidence,
                OperationId::new("component-postactivation-inventory-replacement-rebuild"),
                chrono::Duration::minutes(30),
            )
            .await
            .expect("component admission");
        let component = RuntimeId::from("java-runtime-delta");
        let receipt = axial_minecraft::rebuild_managed_runtime_fixture_for_test(
            fixture.state.managed_runtime_cache(),
            component,
        )
        .await
        .expect("sealed Runtime rebuild receipt");
        let replacement = Arc::new(std::sync::Mutex::new(None));
        let observed_replacement = replacement.clone();
        let state = fixture.state.clone();

        assert_eq!(
            admission
                .succeeded_terminal_with_activation_observer(&receipt, move || {
                    let inventory = activate_empty_inventory(&state, INSTANCE_ID);
                    *observed_replacement
                        .lock()
                        .expect("replacement inventory observation") = Some(inventory);
                })
                .await
                .err(),
            Some(ReconciliationEvidenceRejection::IncarnationMismatch)
        );
        let replacement = replacement
            .lock()
            .expect("replacement inventory observation")
            .clone()
            .expect("replacement inventory");
        let active = fixture
            .state
            .known_good
            .active_inventory(
                &admission.known_good.instance_id,
                &admission.known_good.version_id,
                &admission.known_good.created_at,
                &admission.known_good.library_root,
            )
            .expect("replacement remains active");
        assert!(Arc::ptr_eq(&active, &replacement));
        assert!(
            admission
                .runtime_postcondition_failure_inventory_for_test()
                .get()
                .is_some()
        );
        let terminal = admission
            .failed_postcondition_terminal(&receipt)
            .expect("sealed failure proof does not adopt the replacement");
        assert_eq!(terminal.outcome(), ReconciliationTerminalOutcome::Failed);
        assert!(Arc::ptr_eq(
            &fixture
                .state
                .known_good
                .active_inventory(
                    &admission.known_good.instance_id,
                    &admission.known_good.version_id,
                    &admission.known_good.created_at,
                    &admission.known_good.library_root,
                )
                .expect("replacement remains active after terminalization"),
            &replacement,
        ));

        drop((receipt, admission));
        cleanup(fixture).await;
    }

    #[tokio::test]
    async fn component_rebuild_postactivation_root_drift_keeps_sealed_failure_proof() {
        let fixture = fixture("component-postactivation-root-drift");
        let (evidence, _) = recorded_runtime_artifact_failure(
            &fixture,
            INSTANCE_ID,
            "component-postactivation-root-drift-artifact",
        )
        .await;
        let admission = fixture
            .state
            .admit_runtime_component_rebuild(
                evidence,
                OperationId::new("component-postactivation-root-drift-rebuild"),
                chrono::Duration::minutes(30),
            )
            .await
            .expect("component admission");
        let component = RuntimeId::from("java-runtime-delta");
        let receipt = axial_minecraft::rebuild_managed_runtime_fixture_for_test(
            fixture.state.managed_runtime_cache(),
            component,
        )
        .await
        .expect("sealed Runtime rebuild receipt");
        let replacement_library = fixture.root.join("postactivation-replacement-library");
        fs::create_dir_all(&replacement_library).expect("replacement library root");
        let replacement_library = replacement_library.to_string_lossy().into_owned();
        let state = fixture.state.clone();

        assert_eq!(
            admission
                .succeeded_terminal_with_activation_observer(&receipt, move || {
                    state.set_library_dir_for_test(replacement_library);
                })
                .await
                .err(),
            Some(ReconciliationEvidenceRejection::IncarnationMismatch)
        );
        assert!(
            admission
                .runtime_postcondition_failure_inventory_for_test()
                .get()
                .is_some()
        );
        assert!(
            fixture
                .state
                .known_good
                .active_inventory(
                    &admission.known_good.instance_id,
                    &admission.known_good.version_id,
                    &admission.known_good.created_at,
                    &admission.known_good.library_root,
                )
                .is_none(),
            "cleanup uses the admitted root binding after config drift"
        );
        let terminal = admission
            .failed_postcondition_terminal(&receipt)
            .expect("sealed failure proof survives current root drift");
        assert_eq!(terminal.outcome(), ReconciliationTerminalOutcome::Failed);

        drop((receipt, admission));
        cleanup(fixture).await;
    }

    #[tokio::test]
    async fn component_rebuild_admission_retains_config_mutation_until_drop() {
        let fixture = fixture("component-admission-config-retention");
        let (evidence, _) = recorded_runtime_artifact_failure(
            &fixture,
            INSTANCE_ID,
            "component-admission-config-retention-artifact",
        )
        .await;
        let admission = fixture
            .state
            .admit_runtime_component_rebuild(
                evidence,
                OperationId::new("component-admission-config-retention-rebuild"),
                chrono::Duration::minutes(30),
            )
            .await
            .expect("component admission");
        let (mutation_entered_tx, mut mutation_entered_rx) = tokio::sync::oneshot::channel();
        let state = fixture.state.clone();
        let mutation = tokio::spawn(async move {
            state
                .mutate_config(move |config| {
                    let _ = mutation_entered_tx.send(());
                    config.theme = "component-admission-released".to_string();
                    Ok(())
                })
                .await
        });
        tokio::task::yield_now().await;
        assert!(
            mutation_entered_rx.try_recv().is_err(),
            "config mutation must not enter while component admission is live"
        );

        drop(admission);
        mutation_entered_rx
            .await
            .expect("config mutation enters after admission drops");
        mutation
            .await
            .expect("config mutation task")
            .expect("config mutation commits");
        cleanup(fixture).await;
    }

    #[tokio::test]
    async fn shared_runtime_failure_terminal_suppresses_queued_cross_instance_rebuild() {
        let fixture = fixture("component-admission-shared-runtime");
        let second = fixture
            .state
            .instances()
            .insert_for_test("Second Runtime instance", "1.21.1")
            .expect("register second instance");
        activate_empty_inventory(&fixture.state, &second.id);
        let (first_evidence, _) = recorded_runtime_artifact_failure(
            &fixture,
            INSTANCE_ID,
            "shared-runtime-first-artifact",
        )
        .await;
        let (second_evidence, _) = recorded_runtime_artifact_failure(
            &fixture,
            &second.id,
            "shared-runtime-second-artifact",
        )
        .await;
        let first_admission = fixture
            .state
            .admit_runtime_component_rebuild(
                first_evidence,
                OperationId::new("shared-runtime-first-rebuild"),
                chrono::Duration::minutes(30),
            )
            .await
            .expect("first Runtime rebuild admission");

        let second_state = fixture.state.clone();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let mut second_admission = tokio::spawn(async move {
            let _ = started_tx.send(());
            second_state
                .admit_runtime_component_rebuild(
                    second_evidence,
                    OperationId::new("shared-runtime-second-rebuild"),
                    chrono::Duration::minutes(30),
                )
                .await
        });
        started_rx
            .await
            .expect("second admission reaches shared Runtime writer");
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), &mut second_admission)
                .await
                .is_err(),
            "second Runtime admission must wait behind the active component writer"
        );

        let first_attempt = first_admission.attempt().clone();
        let first_terminal = first_admission
            .failed_terminal()
            .expect("first Runtime rebuild failure terminal");
        let reservation = reserve_reconciliation_attempt(
            fixture.failure_memory.as_ref(),
            fixture.journals.as_ref(),
            reconciliation_attempt_key(&first_attempt),
        )
        .expect("reserve first Runtime rebuild");
        persist_failed_journal(&fixture, &first_attempt, first_terminal.clone()).await;
        commit_reconciliation_memory(
            fixture.failure_memory.as_ref(),
            reconciliation_memory_entry(first_terminal).expect("first Runtime rebuild memory"),
            &reservation,
        )
        .await
        .expect("settle first successful Runtime rebuild memory");
        drop((reservation, first_admission));

        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(1), second_admission)
                .await
                .expect("queued Runtime admission resumes")
                .expect("queued Runtime admission task completes")
                .err(),
            Some(ReconciliationEvidenceRejection::SuppressedPriorAttempt)
        );

        cleanup(fixture).await;
    }

    #[tokio::test]
    async fn reconciliation_window_rejects_durable_attempts_above_state_bound() {
        let fixture = fixture("reconciliation-window-bound");
        let rung = ReconciliationRung::RepairArtifact;
        let components = [
            ReconciliationComponent::VersionBundle,
            ReconciliationComponent::Libraries,
        ];
        assert_eq!(
            components.len(),
            rung.max_attempts_per_suppression_window() + 1
        );

        for (index, component) in components.into_iter().enumerate() {
            let (attempt, terminal) = registered_attempt(
                &fixture,
                &format!("reconciliation-window-bound-{index}"),
                component,
            )
            .await;
            let reservation = reserve_reconciliation_attempt(
                fixture.failure_memory.as_ref(),
                fixture.journals.as_ref(),
                reconciliation_attempt_key(&attempt),
            )
            .expect("reserve bounded reconciliation attempt");
            persist_failed_journal(&fixture, &attempt, terminal.clone()).await;
            commit_reconciliation_memory(
                fixture.failure_memory.as_ref(),
                reconciliation_memory_entry(terminal).expect("bounded reconciliation memory"),
                &reservation,
            )
            .await
            .expect("commit bounded reconciliation memory");
        }

        assert_eq!(
            fixture
                .state
                .refuse_active_reconciliation_window(
                    rung,
                    chrono::Utc::now().fixed_offset(),
                    |_| true,
                )
                .err(),
            Some(ReconciliationEvidenceRejection::JournalMismatch)
        );

        cleanup(fixture).await;
    }

    #[tokio::test]
    async fn runtime_artifact_recovery_refuses_stale_indeterminate_later_attempt() {
        let fixture = fixture("runtime-artifact-recovery-ambiguous");
        let (prior, _) = recorded_runtime_artifact_failure(
            &fixture,
            INSTANCE_ID,
            "runtime-recovery-prior-failure",
        )
        .await;
        drop(prior);
        let lifecycle = fixture.state.acquire_instance_lifecycle(INSTANCE_ID).await;
        let observed_at = chrono::Utc::now().fixed_offset() - chrono::Duration::hours(2);
        let suppression_until = observed_at + chrono::Duration::minutes(30);
        let ambiguous = fixture
            .state
            .registered_reconciliation_attempt_at(
                &lifecycle,
                OperationId::new("runtime-recovery-stale-running"),
                DIAGNOSIS_ID,
                GuardianDomain::Runtime,
                ReconciliationRung::RepairArtifact,
                ReconciliationComponent::Runtime,
                runtime_target(),
                GuardianMode::Managed,
                observed_at,
                suppression_until,
                ReconciliationLineage::Initial,
            )
            .expect("stale indeterminate Runtime attempt");
        fixture
            .journals
            .create(planned_journal(&ambiguous))
            .await
            .expect("persist stale indeterminate Runtime attempt");

        assert_eq!(
            fixture
                .state
                .active_recorded_runtime_artifact_failure(&lifecycle)
                .err(),
            Some(ReconciliationEvidenceRejection::JournalMismatch)
        );

        drop(lifecycle);
        cleanup(fixture).await;
    }

    #[tokio::test]
    async fn repair_ladder_admission_suppresses_active_terminals_after_store_reload() {
        let fixture = fixture("component-admission-restart-suppression");
        let lifecycle = fixture.state.acquire_instance_lifecycle(INSTANCE_ID).await;
        let authority = fixture
            .state
            .registered_reconciliation_authority(&lifecycle)
            .expect("registered authority");
        let artifact_attempt = authority
            .repair_artifact_attempt(
                OperationId::new("component-restart-artifact"),
                DIAGNOSIS_ID,
                GuardianDomain::Runtime,
                ReconciliationComponent::Runtime,
                runtime_target(),
                GuardianMode::Managed,
                chrono::Duration::minutes(30),
            )
            .expect("runtime artifact attempt");
        let artifact_terminal = authority
            .terminal(
                artifact_attempt.clone(),
                ReconciliationTerminalOutcome::Failed,
            )
            .expect("runtime artifact failure");
        let artifact_reservation = reserve_reconciliation_attempt(
            fixture.failure_memory.as_ref(),
            fixture.journals.as_ref(),
            reconciliation_attempt_key(&artifact_attempt),
        )
        .expect("reserve artifact attempt");
        persist_failed_journal(&fixture, &artifact_attempt, artifact_terminal.clone()).await;
        commit_reconciliation_memory(
            fixture.failure_memory.as_ref(),
            reconciliation_memory_entry(artifact_terminal).expect("artifact memory"),
            &artifact_reservation,
        )
        .await
        .expect("commit artifact memory");
        drop(artifact_reservation);

        let evidence = fixture
            .state
            .recorded_runtime_artifact_repair_failure(&lifecycle, artifact_attempt.operation_id())
            .expect("recorded artifact failure");
        let admission = fixture
            .state
            .admit_runtime_component_rebuild(
                evidence,
                OperationId::new("component-restart-first"),
                chrono::Duration::minutes(30),
            )
            .await
            .expect("first component admission");
        let component_attempt = admission.attempt().clone();
        let component_terminal = admission
            .failed_terminal()
            .expect("component failure terminal");
        let component_reservation = reserve_reconciliation_attempt(
            fixture.failure_memory.as_ref(),
            fixture.journals.as_ref(),
            reconciliation_attempt_key(&component_attempt),
        )
        .expect("reserve component attempt");
        persist_failed_journal(&fixture, &component_attempt, component_terminal.clone()).await;
        commit_reconciliation_memory(
            fixture.failure_memory.as_ref(),
            reconciliation_memory_entry(component_terminal).expect("component memory"),
            &component_reservation,
        )
        .await
        .expect("commit component memory");
        drop((component_reservation, admission, authority, lifecycle));

        let restarted_journals = Arc::new(OperationJournalStore::new());
        restarted_journals
            .load_snapshot(fixture.journals.snapshot().expect("journal snapshot"))
            .expect("reload journal snapshot");
        let restarted_memory = Arc::new(GuardianFailureMemoryStore::new());
        restarted_memory
            .load_snapshot(fixture.failure_memory.snapshot().expect("memory snapshot"))
            .expect("reload memory snapshot");
        let restarted_state = fixture
            .state
            .clone()
            .with_reconciliation_stores(restarted_journals, restarted_memory);
        let restarted_lifecycle = restarted_state
            .acquire_instance_lifecycle(INSTANCE_ID)
            .await;
        let restarted_authority = restarted_state
            .registered_reconciliation_authority(&restarted_lifecycle)
            .expect("restarted registered authority");
        let restarted_artifact_attempt = restarted_authority
            .repair_artifact_attempt(
                OperationId::new("artifact-restart-repeated"),
                DIAGNOSIS_ID,
                GuardianDomain::Runtime,
                ReconciliationComponent::Runtime,
                runtime_target(),
                GuardianMode::Managed,
                chrono::Duration::minutes(30),
            )
            .expect("equivalent restarted artifact attempt");
        assert_eq!(
            reconciliation_attempt_key(&restarted_artifact_attempt),
            reconciliation_attempt_key(&artifact_attempt),
            "the restarted rung-one attempt must address the same suppression window"
        );
        assert_eq!(
            restarted_state
                .refuse_active_artifact_repair_window(&restarted_artifact_attempt)
                .err(),
            Some(ReconciliationEvidenceRejection::SuppressedPriorAttempt)
        );
        let restarted_evidence = restarted_state
            .recorded_runtime_artifact_repair_failure(
                &restarted_lifecycle,
                artifact_attempt.operation_id(),
            )
            .expect("reloaded artifact failure");

        assert_eq!(
            restarted_state
                .admit_runtime_component_rebuild(
                    restarted_evidence,
                    OperationId::new("component-restart-repeated"),
                    chrono::Duration::minutes(30),
                )
                .await
                .err(),
            Some(ReconciliationEvidenceRejection::SuppressedPriorAttempt)
        );

        let component_key = reconciliation_attempt_key(&component_attempt);
        let disagreed_journals = Arc::new(OperationJournalStore::new());
        disagreed_journals
            .load_snapshot(fixture.journals.snapshot().expect("journal snapshot"))
            .expect("reload disagreed journal snapshot");
        let mut memory_without_component = fixture
            .failure_memory
            .snapshot()
            .expect("memory snapshot without component");
        memory_without_component
            .entries
            .retain(|entry| entry.key.as_str() != component_key.as_str());
        let disagreed_memory = Arc::new(GuardianFailureMemoryStore::new());
        disagreed_memory
            .load_snapshot(memory_without_component)
            .expect("reload disagreed memory snapshot");
        let disagreed_state = fixture
            .state
            .clone()
            .with_reconciliation_stores(disagreed_journals, disagreed_memory);
        let disagreed_evidence = disagreed_state
            .recorded_runtime_artifact_repair_failure(
                &restarted_lifecycle,
                artifact_attempt.operation_id(),
            )
            .expect("artifact failure remains available");
        assert_eq!(
            disagreed_state
                .admit_runtime_component_rebuild(
                    disagreed_evidence,
                    OperationId::new("component-restart-disagreed"),
                    chrono::Duration::minutes(30),
                )
                .await
                .err(),
            Some(ReconciliationEvidenceRejection::JournalMismatch)
        );

        drop((
            disagreed_state,
            restarted_authority,
            restarted_lifecycle,
            restarted_state,
        ));
        cleanup(fixture).await;
    }

    #[tokio::test]
    async fn whole_instance_expiry_settles_at_start_and_releases_all_ownership() {
        let fixture = fixture("whole-instance-expired-before-effect");
        persist_runtime_component_ladder(
            &fixture,
            "whole-instance-expired-before-effect",
            "java-runtime-delta",
            GuardianMode::Managed,
        )
        .await;
        let offer = whole_instance_offer(&fixture, GuardianMode::Managed).await;
        let admission = fixture
            .state
            .admit_whole_instance_rematerialization(
                offer.into_authorization(),
                OperationId::new("whole-instance-expired-before-effect-whole"),
                chrono::Duration::milliseconds(100),
            )
            .await;
        let Ok(admission) = admission else {
            panic!("short-lived rung-three attempt must initially admit");
        };
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        let effect = admission.into_effect().await;
        let Ok(RegisteredWholeInstancePreparation::Closed(outcome)) = effect else {
            panic!("expired rung-three attempt must close before Core");
        };
        assert!(!outcome.succeeded());
        assert!(outcome.terminal().quarantine_checkpoint().is_empty());
        assert!(!fixture.state.instance_lifecycle_is_held(INSTANCE_ID).await);
        let config = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            fixture.state.config.acquire_mutation(),
        )
        .await
        .expect("config ownership released")
        .expect("config mutation remains available");
        let component = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            fixture.state.sessions.acquire_shared_component_mutation(),
        )
        .await
        .expect("component ownership released")
        .expect("component mutation remains available");
        let journal = fixture
            .journals
            .get(&OperationId::new(
                "whole-instance-expired-before-effect-whole",
            ))
            .expect("before-effect terminal journal");
        assert_eq!(
            journal.failure_point.as_deref(),
            Some(WHOLE_INSTANCE_REMATERIALIZATION_START_STEP)
        );
        drop((component, config));
        cleanup(fixture).await;
    }

    #[tokio::test]
    async fn whole_instance_effect_handoff_rejects_an_active_managed_artifact_writer() {
        let fixture = fixture("whole-instance-active-artifact-writer");
        let admission = whole_instance_admission(
            &fixture,
            "whole-instance-active-artifact-writer",
            GuardianMode::Managed,
        )
        .await;
        let writer = fixture
            .state
            .admit_managed_artifact_mutation()
            .expect("active managed artifact writer");
        assert!(
            !fixture
                .state
                .managed_artifact_mutation_epoch_is_capturable_for_test()
        );

        let Ok(preparation) = admission.into_effect().await else {
            panic!("active writer must close rung-three durably");
        };
        let RegisteredWholeInstancePreparation::Closed(outcome) = preparation else {
            panic!("an active writer must block the rung-three effect handoff");
        };
        assert!(!outcome.succeeded());
        assert_eq!(
            outcome.terminal().outcome(),
            ReconciliationTerminalOutcome::Failed
        );

        drop(writer);
        assert!(
            fixture
                .state
                .managed_artifact_mutation_epoch_is_capturable_for_test()
        );
        cleanup(fixture).await;
    }

    #[tokio::test]
    async fn whole_instance_effect_handoff_rejects_an_earlier_managed_artifact_writer() {
        let fixture = fixture("whole-instance-earlier-artifact-writer");
        let admission = whole_instance_admission(
            &fixture,
            "whole-instance-earlier-artifact-writer",
            GuardianMode::Managed,
        )
        .await;
        drop(
            fixture
                .state
                .admit_managed_artifact_mutation()
                .expect("earlier managed artifact writer"),
        );
        assert!(
            fixture
                .state
                .managed_artifact_mutation_epoch_is_capturable_for_test()
        );

        let Ok(preparation) = admission.into_effect().await else {
            panic!("earlier writer must close rung-three durably");
        };
        let RegisteredWholeInstancePreparation::Closed(outcome) = preparation else {
            panic!("an earlier writer must stale the rung-three effect handoff");
        };
        assert!(!outcome.succeeded());
        assert_eq!(
            outcome.terminal().outcome(),
            ReconciliationTerminalOutcome::Failed
        );

        assert!(
            fixture
                .state
                .managed_artifact_mutation_epoch_is_capturable_for_test()
        );
        cleanup(fixture).await;
    }

    #[tokio::test]
    async fn whole_instance_plan_failure_retries_exactly_before_exposing_core_request() {
        let backend = Arc::new(ControlledWriteBackend::default());
        let fixture = fixture_with_backends(
            "whole-instance-plan-persistence",
            Some(backend.clone()),
            None,
        );
        let admission = whole_instance_admission(
            &fixture,
            "whole-instance-plan-persistence",
            GuardianMode::Managed,
        )
        .await;
        let operation_id = admission.attempt().operation_id().clone();
        let gated_attempt = backend.next_attempt();
        backend.gate_attempt(gated_attempt);
        backend.fail_attempt(gated_attempt);
        let calls = Arc::new(AtomicUsize::new(0));
        let driver_calls = calls.clone();
        let execution = tokio::spawn(async move {
            crate::guardian::execute_whole_instance_rematerialization_with(
                admission,
                move |_, _, _| async move {
                    driver_calls.fetch_add(1, Ordering::SeqCst);
                    Err(axial_minecraft::ManagedWholeInstanceRebuildError::Preparation)
                },
            )
            .await
        });

        backend.wait_for_attempt(gated_attempt).await;
        assert!(fixture.journals.get(&operation_id).is_none());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_whole_instance_ownership_is_held(&fixture).await;
        backend.release();

        let outcome = tokio::time::timeout(Duration::from_secs(2), execution)
            .await
            .expect("plan reconciliation completes")
            .expect("plan reconciliation task")
            .expect("plan reconciliation remains structurally valid");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            outcome.status(),
            crate::guardian::GuardianWholeInstanceRematerializationStatus::Failed
        );
        assert!(!fixture.state.instance_lifecycle_is_held(INSTANCE_ID).await);
        cleanup(fixture).await;
    }

    #[tokio::test]
    async fn whole_instance_user_config_is_durable_before_core_and_failure_has_no_offer() {
        let fixture = fixture("whole-instance-user-config-order");
        let options_path = fixture
            .root
            .join("instances")
            .join(INSTANCE_ID)
            .join("options.txt");
        let exact = b"guiScale:3\nresourcePacks:[\"vanilla\"]\n";
        fs::write(&options_path, exact).expect("seed user options");
        let admission = whole_instance_admission(
            &fixture,
            "whole-instance-user-config-order",
            GuardianMode::Managed,
        )
        .await;
        let snapshot_path = fixture
            .root
            .join("config")
            .join("guardian-user-config-snapshots.json");
        let observed_durable = Arc::new(AtomicBool::new(false));
        let driver_observed = observed_durable.clone();
        let driver_snapshot_path = snapshot_path.clone();

        let outcome = crate::guardian::execute_whole_instance_rematerialization_with(
            admission,
            move |_, _, _| async move {
                let snapshot = serde_json::from_slice::<serde_json::Value>(
                    &fs::read(driver_snapshot_path).expect("snapshot visible before Core"),
                )
                .expect("strict snapshot JSON");
                driver_observed.store(
                    snapshot["snapshots"]
                        .as_array()
                        .is_some_and(|snapshots| snapshots.len() == 1),
                    Ordering::SeqCst,
                );
                Err(axial_minecraft::ManagedWholeInstanceRebuildError::Preparation)
            },
        )
        .await
        .expect("failed Core settles canonically");

        assert!(observed_durable.load(Ordering::SeqCst));
        assert_eq!(
            fs::read(&options_path).expect("read unchanged options"),
            exact
        );
        assert_eq!(
            outcome.status(),
            crate::guardian::GuardianWholeInstanceRematerializationStatus::Failed
        );
        assert!(outcome.into_restore_offer().is_none());
        let settled = serde_json::from_slice::<serde_json::Value>(
            &fs::read(snapshot_path).expect("read settled snapshot store"),
        )
        .expect("settled snapshot JSON");
        assert!(settled["snapshots"].as_array().is_some_and(Vec::is_empty));
        cleanup(fixture).await;
    }

    #[tokio::test]
    async fn whole_instance_capture_and_persist_failures_never_call_core() {
        let capture_fixture = fixture("whole-instance-user-config-capture-failure");
        fs::write(
            capture_fixture
                .root
                .join("instances")
                .join(INSTANCE_ID)
                .join("options.txt"),
            [0xff],
        )
        .expect("seed invalid UTF-8 options");
        let capture_admission = whole_instance_admission(
            &capture_fixture,
            "whole-instance-user-config-capture-failure",
            GuardianMode::Managed,
        )
        .await;
        let capture_calls = Arc::new(AtomicUsize::new(0));
        let called = capture_calls.clone();
        let capture_outcome = crate::guardian::execute_whole_instance_rematerialization_with(
            capture_admission,
            move |_, _, _| async move {
                called.fetch_add(1, Ordering::SeqCst);
                Err(axial_minecraft::ManagedWholeInstanceRebuildError::Preparation)
            },
        )
        .await
        .expect("capture refusal settles before Core");
        assert_eq!(capture_calls.load(Ordering::SeqCst), 0);
        assert!(capture_outcome.into_restore_offer().is_none());
        cleanup(capture_fixture).await;

        let persist_fixture = fixture("whole-instance-user-config-persist-failure");
        let snapshot_path = persist_fixture
            .root
            .join("config")
            .join("guardian-user-config-snapshots.json");
        let blocked_temp = crate::execution::file::atomic_temp_path_for(&snapshot_path);
        fs::create_dir_all(&blocked_temp).expect("block atomic snapshot temp file");
        let persist_admission = whole_instance_admission(
            &persist_fixture,
            "whole-instance-user-config-persist-failure",
            GuardianMode::Managed,
        )
        .await;
        let persist_calls = Arc::new(AtomicUsize::new(0));
        let called = persist_calls.clone();
        let persist_outcome = tokio::time::timeout(
            Duration::from_secs(2),
            crate::guardian::execute_whole_instance_rematerialization_with(
                persist_admission,
                move |_, _, _| async move {
                    called.fetch_add(1, Ordering::SeqCst);
                    Err(axial_minecraft::ManagedWholeInstanceRebuildError::Preparation)
                },
            ),
        )
        .await
        .expect("persistence refusal returns promptly")
        .expect("persistence refusal settles before Core");
        assert_eq!(persist_calls.load(Ordering::SeqCst), 0);
        assert!(persist_outcome.into_restore_offer().is_none());
        fs::remove_dir_all(blocked_temp).expect("unblock snapshot retry");
        cleanup(persist_fixture).await;
    }

    #[tokio::test]
    async fn startup_prunes_capture_without_exact_success_journal_and_memory() {
        let fixture = fixture("whole-instance-user-config-crash-residue");
        let instance = fixture
            .state
            .instances
            .get(INSTANCE_ID)
            .expect("registered crash-residue instance");
        let current = fixture
            .state
            .current_reconciliation_incarnation(INSTANCE_ID)
            .expect("current crash-residue incarnation");
        let game_dir = fixture.state.instances.game_dir(INSTANCE_ID);
        fs::write(game_dir.join("options.txt"), b"exact\n").expect("seed crash-residue options");
        let capture = crate::execution::user_owned_state::capture_user_config(game_dir)
            .await
            .expect("capture crash residue");
        let receipt = fixture
            .state
            .user_config_snapshots
            .persist_before_core(
                crate::state::user_config_snapshots::UserConfigSnapshotIdentity {
                    instance_id: instance.id.clone(),
                    instance_created_at: instance.created_at.clone(),
                    incarnation_fingerprint: current.fingerprint,
                    inventory_fingerprint: current.inventory_fingerprint,
                    r3_operation_id: OperationId::new("crash-before-core-settlement"),
                    captured_at: chrono::Utc::now().fixed_offset(),
                },
                capture,
            )
            .await
            .expect("persist simulated crash residue");
        drop(receipt);
        let snapshot_path = fixture
            .root
            .join("config")
            .join("guardian-user-config-snapshots.json");
        let before = serde_json::from_slice::<serde_json::Value>(
            &fs::read(&snapshot_path).expect("read crash residue"),
        )
        .expect("crash residue JSON");
        assert!(
            before["snapshots"]
                .as_array()
                .is_some_and(|items| items.len() == 1)
        );

        fixture
            .state
            .reconcile_user_config_snapshot_startup()
            .await
            .expect("startup removes non-success residue");
        let after = serde_json::from_slice::<serde_json::Value>(
            &fs::read(snapshot_path).expect("read reconciled residue"),
        )
        .expect("reconciled residue JSON");
        assert!(after["snapshots"].as_array().is_some_and(Vec::is_empty));
        cleanup(fixture).await;
    }

    #[tokio::test]
    async fn whole_instance_terminal_journal_retry_precedes_memory_and_retains_authority() {
        let backend = Arc::new(ControlledWriteBackend::default());
        let fixture = fixture_with_backends(
            "whole-instance-terminal-persistence",
            Some(backend.clone()),
            None,
        );
        let admission = whole_instance_admission(
            &fixture,
            "whole-instance-terminal-persistence",
            GuardianMode::Managed,
        )
        .await;
        let key = reconciliation_attempt_key(admission.attempt());
        let operation_id = admission.attempt().operation_id().clone();
        let calls = Arc::new(AtomicUsize::new(0));
        let driver_calls = calls.clone();
        let (driver_entered_tx, driver_entered_rx) = tokio::sync::oneshot::channel();
        let (driver_release_tx, driver_release_rx) = tokio::sync::oneshot::channel();
        let execution = tokio::spawn(async move {
            crate::guardian::execute_whole_instance_rematerialization_with(
                admission,
                move |_, _, _| async move {
                    driver_calls.fetch_add(1, Ordering::SeqCst);
                    let _ = driver_entered_tx.send(());
                    let _ = driver_release_rx.await;
                    Err(axial_minecraft::ManagedWholeInstanceRebuildError::Preparation)
                },
            )
            .await
        });
        driver_entered_rx.await.expect("Core driver reached once");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let gated_attempt = backend.next_attempt();
        backend.gate_attempt(gated_attempt);
        backend.fail_attempt(gated_attempt);
        let _ = driver_release_tx.send(());

        backend.wait_for_attempt(gated_attempt).await;
        assert_eq!(
            fixture
                .journals
                .get(&operation_id)
                .expect("visible whole-instance plan")
                .status,
            OperationStatus::Planned
        );
        assert!(fixture.failure_memory.get(&key).is_none());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_whole_instance_ownership_is_held(&fixture).await;
        backend.release();

        let outcome = tokio::time::timeout(Duration::from_secs(2), execution)
            .await
            .expect("terminal journal reconciliation completes")
            .expect("terminal journal reconciliation task")
            .expect("terminal settlement remains structurally valid");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            outcome.status(),
            crate::guardian::GuardianWholeInstanceRematerializationStatus::Failed
        );
        let journal = fixture
            .journals
            .get(&operation_id)
            .expect("durable whole-instance terminal");
        let terminal = journal
            .reconciliation_terminal()
            .expect("whole-instance terminal evidence");
        assert_eq!(
            fixture
                .failure_memory
                .get(&key)
                .and_then(|entry| entry.reconciliation_terminal().cloned()),
            Some(terminal.clone())
        );
        cleanup(fixture).await;
    }

    #[tokio::test]
    async fn whole_instance_memory_retry_retains_authority_after_terminal_journal() {
        let backend = Arc::new(ControlledWriteBackend::default());
        let fixture = fixture_with_backends(
            "whole-instance-memory-persistence",
            None,
            Some(backend.clone()),
        );
        let admission = whole_instance_admission(
            &fixture,
            "whole-instance-memory-persistence",
            GuardianMode::Managed,
        )
        .await;
        let key = reconciliation_attempt_key(admission.attempt());
        let operation_id = admission.attempt().operation_id().clone();
        let calls = Arc::new(AtomicUsize::new(0));
        let driver_calls = calls.clone();
        let (driver_entered_tx, driver_entered_rx) = tokio::sync::oneshot::channel();
        let (driver_release_tx, driver_release_rx) = tokio::sync::oneshot::channel();
        let execution = tokio::spawn(async move {
            crate::guardian::execute_whole_instance_rematerialization_with(
                admission,
                move |_, _, _| async move {
                    driver_calls.fetch_add(1, Ordering::SeqCst);
                    let _ = driver_entered_tx.send(());
                    let _ = driver_release_rx.await;
                    Err(axial_minecraft::ManagedWholeInstanceRebuildError::Preparation)
                },
            )
            .await
        });
        driver_entered_rx.await.expect("Core driver reached once");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let gated_attempt = backend.next_attempt();
        backend.gate_attempt(gated_attempt);
        backend.fail_attempt(gated_attempt);
        let _ = driver_release_tx.send(());

        backend.wait_for_attempt(gated_attempt).await;
        let journal = fixture
            .journals
            .get(&operation_id)
            .expect("terminal journal precedes memory");
        assert!(journal.reconciliation_terminal().is_some());
        assert!(fixture.failure_memory.get(&key).is_none());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_whole_instance_ownership_is_held(&fixture).await;
        backend.release();

        let outcome = tokio::time::timeout(Duration::from_secs(2), execution)
            .await
            .expect("memory reconciliation completes")
            .expect("memory reconciliation task")
            .expect("memory settlement remains structurally valid");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            outcome.status(),
            crate::guardian::GuardianWholeInstanceRematerializationStatus::Failed
        );
        let terminal = fixture
            .journals
            .get(&operation_id)
            .and_then(|entry| entry.reconciliation_terminal().cloned())
            .expect("exact whole-instance journal terminal");
        assert_eq!(
            fixture
                .failure_memory
                .get(&key)
                .and_then(|entry| entry.reconciliation_terminal().cloned()),
            Some(terminal)
        );
        cleanup(fixture).await;
    }

    #[tokio::test]
    async fn whole_instance_same_key_nonterminal_is_ambiguous_and_releases_reservation() {
        let fixture = fixture("whole-instance-plan-conflict");
        let admission = whole_instance_admission(
            &fixture,
            "whole-instance-plan-conflict",
            GuardianMode::Managed,
        )
        .await;
        let key = reconciliation_attempt_key(admission.attempt());
        let operation_id = admission.attempt().operation_id().clone();
        let mut conflicting =
            whole_instance_rematerialization_journal_for_attempt(admission.attempt());
        conflicting.journal_id = JournalId::new("journal-conflicting-whole-instance-plan");
        let expected_conflict = conflicting.clone();
        fixture
            .journals
            .create(conflicting)
            .await
            .expect("valid conflicting whole-instance plan");

        let error = admission
            .into_effect()
            .await
            .err()
            .expect("same-key nonterminal must fail structurally");
        assert_eq!(error.class(), "ambiguous_prior_attempt");
        assert_eq!(fixture.journals.get(&operation_id), Some(expected_conflict));
        let reservation = fixture
            .failure_memory
            .reserve_reconciliation_attempt(key)
            .expect("structural plan conflict releases process reservation");
        drop(reservation);
        assert!(!fixture.state.instance_lifecycle_is_held(INSTANCE_ID).await);
        cleanup(fixture).await;
    }

    #[tokio::test]
    async fn guardian_whole_instance_managed_and_custom_call_driver_once_and_fail_canonically() {
        for (label, mode) in [
            ("guardian-whole-managed", GuardianMode::Managed),
            ("guardian-whole-custom", GuardianMode::Custom),
        ] {
            let fixture = fixture(label);
            let admission = whole_instance_admission(&fixture, label, mode).await;
            let key = reconciliation_attempt_key(admission.attempt());
            let operation_id = admission.attempt().operation_id().clone();
            let calls = Arc::new(AtomicUsize::new(0));
            let driver_calls = calls.clone();

            let outcome = crate::guardian::execute_whole_instance_rematerialization_with(
                admission,
                move |_, _, _| async move {
                    driver_calls.fetch_add(1, Ordering::SeqCst);
                    Err(axial_minecraft::ManagedWholeInstanceRebuildError::Preparation)
                },
            )
            .await
            .expect("pre-effect Core error settles canonically");

            assert_eq!(calls.load(Ordering::SeqCst), 1);
            assert_eq!(
                outcome.status(),
                crate::guardian::GuardianWholeInstanceRematerializationStatus::Failed
            );
            assert!(outcome.into_restore_offer().is_none());
            let terminal = fixture
                .journals
                .get(&operation_id)
                .and_then(|journal| journal.reconciliation_terminal().cloned())
                .expect("canonical Guardian whole-instance terminal");
            assert_eq!(terminal.outcome(), ReconciliationTerminalOutcome::Failed);
            assert_eq!(
                fixture
                    .failure_memory
                    .get(&key)
                    .and_then(|entry| entry.reconciliation_terminal().cloned()),
                Some(terminal)
            );
            assert!(!fixture.state.instance_lifecycle_is_held(INSTANCE_ID).await);
            cleanup(fixture).await;
        }
    }

    #[tokio::test]
    async fn guardian_whole_instance_rollback_has_identical_runtime_quarantine_evidence() {
        const RUNTIME_BYTES: &[u8] = b"Guardian whole-instance rollback Runtime";
        let fixture = fixture("guardian-whole-rollback");
        let prior_runtime = fixture
            .state
            .managed_runtime_cache()
            .component_root("java-runtime-delta")
            .expect("fixture Runtime root");
        fs::create_dir_all(&prior_runtime).expect("seed prior Runtime");
        fs::write(prior_runtime.join("prior-runtime"), b"prior Runtime")
            .expect("seed prior Runtime bytes");
        let core_fixture = prepare_core_whole_instance_fixture(&fixture, RUNTIME_BYTES).await;
        fixture.state.activate_known_good_inventory_for_test(
            INSTANCE_ID,
            core_fixture.known_good_inventory(),
        );
        let admission =
            whole_instance_admission(&fixture, "guardian-whole-rollback", GuardianMode::Managed)
                .await;
        let operation_id = admission.attempt().operation_id().clone();
        let key = reconciliation_attempt_key(admission.attempt());

        let outcome = crate::guardian::execute_whole_instance_rematerialization_with(
            admission,
            move |_, runtime_cache, _| async move {
                axial_minecraft::publish_managed_whole_instance_rollback_fixture_for_test(
                    core_fixture,
                    &runtime_cache,
                )
                .await
            },
        )
        .await
        .expect("sealed rollback settles through State");

        assert_eq!(
            outcome.status(),
            crate::guardian::GuardianWholeInstanceRematerializationStatus::Failed
        );
        assert!(outcome.into_restore_offer().is_none());
        let terminal = fixture
            .journals
            .get(&operation_id)
            .and_then(|journal| journal.reconciliation_terminal().cloned())
            .expect("rollback whole-instance terminal");
        assert_eq!(terminal.outcome(), ReconciliationTerminalOutcome::Failed);
        assert_eq!(terminal.quarantine_checkpoint().records().len(), 1);
        let memory_terminal = fixture
            .failure_memory
            .get(&key)
            .and_then(|entry| entry.reconciliation_terminal().cloned())
            .expect("rollback whole-instance memory");
        assert_eq!(memory_terminal, terminal);
        assert_eq!(
            memory_terminal.quarantine_checkpoint(),
            terminal.quarantine_checkpoint()
        );
        cleanup(fixture).await;
    }

    #[tokio::test]
    async fn guardian_whole_instance_managed_and_custom_success_return_one_inert_restore_offer() {
        const RUNTIME_BYTES: &[u8] = b"Guardian whole-instance success Runtime";
        for (label, mode) in [
            (
                "guardian-whole-managed-success-restore-offer",
                GuardianMode::Managed,
            ),
            (
                "guardian-whole-custom-success-restore-offer",
                GuardianMode::Custom,
            ),
        ] {
            let fixture = fixture(label);
            let options_path = fixture
                .root
                .join("instances")
                .join(INSTANCE_ID)
                .join("options.txt");
            let exact = b"guiScale:2\n";
            fs::write(&options_path, exact).expect("seed exact user options");
            let core_fixture = prepare_core_whole_instance_fixture(&fixture, RUNTIME_BYTES).await;
            fixture.state.activate_known_good_inventory_for_test(
                INSTANCE_ID,
                core_fixture.known_good_inventory(),
            );
            let admission = whole_instance_admission(&fixture, label, mode).await;

            let outcome = crate::guardian::execute_whole_instance_rematerialization_with(
                admission,
                move |_, runtime_cache, _| async move {
                    axial_minecraft::publish_managed_whole_instance_fixture_for_test(
                        core_fixture,
                        &runtime_cache,
                    )
                    .await
                },
            )
            .await
            .expect("sealed successful commit settles through State");

            assert_eq!(
                outcome.status(),
                crate::guardian::GuardianWholeInstanceRematerializationStatus::Rematerialized
            );
            assert_eq!(
                fixture
                    .state
                    .whole_instance_rematerialization_eligibility(INSTANCE_ID)
                    .await
                    .err(),
                Some(ReconciliationEvidenceRejection::SuppressedPriorAttempt),
                "the current rung-three window must close eligibility before policy assessment"
            );
            assert!(outcome.into_restore_offer().is_some());
            assert_eq!(fs::read(options_path).expect("read exact options"), exact);
            let snapshot = serde_json::from_slice::<serde_json::Value>(
                &fs::read(
                    fixture
                        .root
                        .join("config")
                        .join("guardian-user-config-snapshots.json"),
                )
                .expect("read successful snapshot"),
            )
            .expect("successful snapshot JSON");
            assert!(
                snapshot["snapshots"]
                    .as_array()
                    .is_some_and(|items| items.len() == 1)
            );
            cleanup(fixture).await;
        }
    }

    #[tokio::test]
    async fn guardian_whole_instance_commit_receipt_fails_closed_after_current_inventory_drift() {
        const RUNTIME_BYTES: &[u8] = b"Guardian whole-instance drift Runtime";
        let fixture = fixture("guardian-whole-current-drift");
        let core_fixture = prepare_core_whole_instance_fixture(&fixture, RUNTIME_BYTES).await;
        fixture.state.activate_known_good_inventory_for_test(
            INSTANCE_ID,
            core_fixture.known_good_inventory(),
        );
        let admission = whole_instance_admission(
            &fixture,
            "guardian-whole-current-drift",
            GuardianMode::Managed,
        )
        .await;
        let state = fixture.state.clone();

        let outcome = crate::guardian::execute_whole_instance_rematerialization_with(
            admission,
            move |_, runtime_cache, _| async move {
                let receipt = axial_minecraft::publish_managed_whole_instance_fixture_for_test(
                    core_fixture,
                    &runtime_cache,
                )
                .await;
                state.activate_known_good_inventory_for_test(INSTANCE_ID, empty_inventory());
                receipt
            },
        )
        .await
        .expect("drifted sealed commit settles as failure");

        assert_eq!(
            outcome.status(),
            crate::guardian::GuardianWholeInstanceRematerializationStatus::Failed
        );
        assert!(outcome.into_restore_offer().is_none());
        cleanup(fixture).await;
    }

    #[tokio::test]
    async fn guardian_whole_instance_commit_rejects_an_intervening_artifact_epoch() {
        const RUNTIME_BYTES: &[u8] = b"Guardian whole-instance intervening epoch Runtime";
        let fixture = fixture("guardian-whole-intervening-epoch");
        let core_fixture = prepare_core_whole_instance_fixture(&fixture, RUNTIME_BYTES).await;
        fixture.state.activate_known_good_inventory_for_test(
            INSTANCE_ID,
            core_fixture.known_good_inventory(),
        );
        let admission = whole_instance_admission(
            &fixture,
            "guardian-whole-intervening-epoch",
            GuardianMode::Managed,
        )
        .await;
        let expected_inventory = fixture
            .state
            .current_reconciliation_incarnation(INSTANCE_ID)
            .expect("current reconciliation incarnation")
            .inventory;
        let state = fixture.state.clone();

        let outcome = crate::guardian::execute_whole_instance_rematerialization_with(
            admission,
            move |_, runtime_cache, _| async move {
                let receipt = axial_minecraft::publish_managed_whole_instance_fixture_for_test(
                    core_fixture,
                    &runtime_cache,
                )
                .await?;
                drop(
                    state
                        .admit_managed_artifact_mutation()
                        .expect("intervening managed artifact writer"),
                );
                Ok(receipt)
            },
        )
        .await
        .expect("stale sealed commit settles canonically");

        assert_eq!(
            outcome.status(),
            crate::guardian::GuardianWholeInstanceRematerializationStatus::Failed
        );
        assert!(outcome.into_restore_offer().is_none());
        let current_inventory = fixture
            .state
            .current_reconciliation_incarnation(INSTANCE_ID)
            .expect("current reconciliation incarnation after stale commit")
            .inventory;
        assert!(Arc::ptr_eq(&expected_inventory, &current_inventory));
        assert!(
            fixture
                .state
                .managed_artifact_mutation_epoch_is_capturable_for_test()
        );
        cleanup(fixture).await;
    }

    #[tokio::test]
    async fn application_whole_instance_owner_rejects_a_foreign_state_handoff_before_admission() {
        let owner = fixture("application-whole-owner-foreign-handoff");
        let foreign = fixture("application-whole-owner-foreign-handoff-source");
        persist_runtime_component_ladder(
            &owner,
            "application-whole-owner-foreign-handoff",
            "java-runtime-delta",
            GuardianMode::Managed,
        )
        .await;
        let offer = whole_instance_offer(&owner, GuardianMode::Managed).await;
        let operation_id = OperationId::new("application-whole-owner-foreign-handoff-whole");
        let foreign_request = foreign.state.try_admit_request().expect("foreign request");
        let executor_calls = Arc::new(AtomicUsize::new(0));
        let executed = executor_calls.clone();
        let core_calls = Arc::new(AtomicUsize::new(0));
        let driven = core_calls.clone();

        let result = crate::application::spawn_explicit_whole_instance_rematerialization_with(
            owner.state.clone(),
            foreign_request.producer_handoff(),
            offer,
            operation_id.clone(),
            chrono::Duration::minutes(30),
            move |admission| {
                executed.fetch_add(1, Ordering::SeqCst);
                crate::guardian::execute_whole_instance_rematerialization_with(
                    admission,
                    move |_, _, _| async move {
                        driven.fetch_add(1, Ordering::SeqCst);
                        Err(axial_minecraft::ManagedWholeInstanceRebuildError::Preparation)
                    },
                )
            },
        );

        assert!(result.is_err());
        assert_eq!(executor_calls.load(Ordering::SeqCst), 0);
        assert_eq!(core_calls.load(Ordering::SeqCst), 0);
        assert!(owner.journals.get(&operation_id).is_none());
        assert!(!owner.state.instance_lifecycle_is_held(INSTANCE_ID).await);
        drop(foreign_request);
        cleanup(owner).await;
        cleanup(foreign).await;
    }

    #[tokio::test]
    async fn application_whole_instance_owner_survives_waiter_cancellation_and_blocks_shutdown() {
        let fixture = fixture("application-whole-owner-cancellation");
        persist_runtime_component_ladder(
            &fixture,
            "application-whole-owner-cancellation",
            "java-runtime-delta",
            GuardianMode::Managed,
        )
        .await;
        let offer = whole_instance_offer(&fixture, GuardianMode::Managed).await;
        let epoch_before_effect = fixture
            .state
            .managed_artifact_mutation_epoch()
            .expect("managed artifact epoch before rung-three effect");
        let operation_id = OperationId::new("application-whole-owner-cancellation-whole");
        let request = fixture.state.try_admit_request().expect("admit request");
        let handoff = request.producer_handoff();
        let shutdown_state = fixture.state.clone();
        let mut quiesce = tokio::spawn(async move { shutdown_state.quiesce().await });
        tokio::time::timeout(Duration::from_secs(1), async {
            while fixture.state.lifecycle_phase()
                != crate::state::AppLifecyclePhase::DrainingRequests
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("request drain begins");

        let calls = Arc::new(AtomicUsize::new(0));
        let driver_calls = calls.clone();
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let result = crate::application::spawn_explicit_whole_instance_rematerialization_with(
            fixture.state.clone(),
            handoff,
            offer,
            operation_id.clone(),
            chrono::Duration::minutes(30),
            move |admission| {
                crate::guardian::execute_whole_instance_rematerialization_with(
                    admission,
                    move |_, _, _| async move {
                        driver_calls.fetch_add(1, Ordering::SeqCst);
                        let _ = entered_tx.send(());
                        let _ = release_rx.await;
                        Err(axial_minecraft::ManagedWholeInstanceRebuildError::Preparation)
                    },
                )
            },
        )
        .expect("DrainingRequests handoff claims a producer");
        drop(request);
        entered_rx.await.expect("Core driver blocks after plan");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            fixture
                .journals
                .get(&operation_id)
                .expect("plan visible before blocked Core driver")
                .status,
            OperationStatus::Planned
        );
        assert_whole_instance_ownership_is_held(&fixture).await;
        let effect_epoch = fixture
            .state
            .managed_artifact_mutation_epoch()
            .expect("managed artifact epoch while rung-three effect is active");
        assert!(effect_epoch > epoch_before_effect);
        assert!(
            !fixture
                .state
                .managed_artifact_mutation_epoch_is_capturable_for_test()
        );
        drop(result);
        tokio::time::timeout(Duration::from_secs(1), async {
            while fixture.state.lifecycle_phase()
                != crate::state::AppLifecyclePhase::QuiescingProducers
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("shutdown reaches producer drain");
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut quiesce)
                .await
                .is_err(),
            "shutdown must wait for detached whole-instance owner"
        );
        assert_eq!(
            fixture.state.managed_artifact_mutation_epoch(),
            Ok(effect_epoch)
        );
        assert!(
            !fixture
                .state
                .managed_artifact_mutation_epoch_is_capturable_for_test()
        );

        let _ = release_tx.send(());
        quiesce
            .await
            .expect("quiesce task")
            .expect("quiesce after whole-instance settlement");
        let journal = fixture
            .journals
            .get(&operation_id)
            .expect("cancelled waiter cannot orphan terminal");
        let terminal = journal
            .reconciliation_terminal()
            .cloned()
            .expect("whole-instance terminal after waiter cancellation");
        let key = reconciliation_attempt_key(terminal.attempt());
        assert_eq!(
            fixture
                .failure_memory
                .get(&key)
                .and_then(|entry| entry.reconciliation_terminal().cloned()),
            Some(terminal)
        );
        assert!(!fixture.state.instance_lifecycle_is_held(INSTANCE_ID).await);
        assert_eq!(
            fixture.state.managed_artifact_mutation_epoch(),
            Ok(effect_epoch)
        );
        assert!(
            fixture
                .state
                .managed_artifact_mutation_epoch_is_capturable_for_test()
        );
        cleanup(fixture).await;
    }

    #[tokio::test]
    async fn application_custom_whole_instance_owner_survives_waiter_cancellation() {
        let fixture = fixture("application-custom-whole-owner-cancellation");
        persist_runtime_component_ladder(
            &fixture,
            "application-custom-whole-owner-cancellation",
            "java-runtime-delta",
            GuardianMode::Custom,
        )
        .await;
        set_guardian_mode(&fixture, GuardianMode::Custom);
        let offer = whole_instance_offer(&fixture, GuardianMode::Custom).await;
        let operation_id = OperationId::new("application-custom-whole-owner-cancellation-whole");
        let request = fixture.state.try_admit_request().expect("admit request");
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let result = crate::application::spawn_explicit_whole_instance_rematerialization_with(
            fixture.state.clone(),
            request.producer_handoff(),
            offer,
            operation_id.clone(),
            chrono::Duration::minutes(30),
            move |admission| {
                crate::guardian::execute_whole_instance_rematerialization_with(
                    admission,
                    move |_, _, _| async move {
                        let _ = entered_tx.send(());
                        let _ = release_rx.await;
                        Err(axial_minecraft::ManagedWholeInstanceRebuildError::Preparation)
                    },
                )
            },
        )
        .expect("Custom request hands off whole-instance owner");
        drop(request);
        entered_rx.await.expect("Custom Core driver reached");
        drop(result);
        let _ = release_tx.send(());

        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if fixture
                    .journals
                    .get(&operation_id)
                    .is_some_and(|journal| journal.reconciliation_terminal().is_some())
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("Custom detached owner settles terminal");
        let terminal = fixture
            .journals
            .get(&operation_id)
            .and_then(|journal| journal.reconciliation_terminal().cloned())
            .expect("Custom cancellation terminal");
        let key = reconciliation_attempt_key(terminal.attempt());
        tokio::time::timeout(Duration::from_secs(2), async {
            while fixture.failure_memory.get(&key).is_none() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("Custom cancellation memory");
        assert_eq!(
            fixture
                .failure_memory
                .get(&key)
                .and_then(|entry| entry.reconciliation_terminal().cloned()),
            Some(terminal)
        );
        assert!(!fixture.state.instance_lifecycle_is_held(INSTANCE_ID).await);
        cleanup(fixture).await;
    }

    #[tokio::test]
    async fn exact_shaped_forged_whole_terminal_fails_restart_and_suppression_closed() {
        let fixture = fixture("whole-instance-forged-terminal");
        persist_runtime_component_ladder(
            &fixture,
            "whole-instance-forged-terminal",
            "java-runtime-delta",
            GuardianMode::Managed,
        )
        .await;
        let lifecycle = fixture.state.acquire_instance_lifecycle(INSTANCE_ID).await;
        let observed_at = chrono::Utc::now().fixed_offset();
        let forged_attempt = fixture
            .state
            .registered_reconciliation_attempt_at(
                &lifecycle,
                OperationId::new("whole-instance-forged-terminal-whole"),
                DIAGNOSIS_ID,
                GuardianDomain::Runtime,
                ReconciliationRung::RematerializeInstance,
                ReconciliationComponent::WholeInstance,
                reconciliation_instance_target(INSTANCE_ID),
                GuardianMode::Managed,
                observed_at,
                observed_at + chrono::Duration::minutes(30),
                ReconciliationLineage::Predecessor {
                    operation_id: OperationId::new("whole-instance-missing-predecessor"),
                },
            )
            .expect("structurally valid forged rung-three attempt");
        drop(lifecycle);
        let (forged_journal, forged_terminal) = whole_instance_terminal_journal_for_shape(
            &forged_attempt,
            ReconciliationTerminalOutcome::Failed,
            ReconciliationQuarantineCheckpoint::default(),
            WHOLE_INSTANCE_REMATERIALIZATION_STEP,
        );
        assert!(whole_instance_terminal_matches(
            &forged_journal,
            &forged_terminal
        ));
        assert!(
            !fixture
                .state
                .whole_instance_terminal_is_semantically_canonical(
                    &forged_journal,
                    &forged_terminal,
                )
        );
        fixture
            .journals
            .create(forged_journal)
            .await
            .expect("persist exact-shaped forged rung-three journal");

        let restarted_journals = Arc::new(OperationJournalStore::new());
        restarted_journals
            .load_snapshot(
                fixture
                    .journals
                    .snapshot()
                    .expect("forged journal snapshot"),
            )
            .expect("reload forged journal snapshot");
        let restarted_memory = Arc::new(GuardianFailureMemoryStore::new());
        restarted_memory
            .load_snapshot(
                fixture
                    .failure_memory
                    .snapshot()
                    .expect("predecessor memory snapshot"),
            )
            .expect("reload predecessor memory snapshot");
        let restarted_state = fixture
            .state
            .clone()
            .with_reconciliation_stores(restarted_journals, restarted_memory.clone());
        assert_eq!(
            restarted_state
                .reconcile_reconciliation_startup()
                .await
                .expect_err("forged rung three must fail restart")
                .kind(),
            io::ErrorKind::InvalidData
        );
        assert!(
            restarted_memory
                .get(&reconciliation_attempt_key(&forged_attempt))
                .is_none()
        );

        let reservation = reserve_reconciliation_attempt(
            fixture.failure_memory.as_ref(),
            fixture.journals.as_ref(),
            reconciliation_attempt_key(&forged_attempt),
        )
        .expect("reserve forged memory for suppression parity test");
        commit_reconciliation_memory(
            fixture.failure_memory.as_ref(),
            reconciliation_memory_entry(forged_terminal).expect("forged canonical memory shape"),
            &reservation,
        )
        .await
        .expect("persist forged memory parity");
        drop(reservation);
        assert_eq!(
            fixture
                .state
                .whole_instance_rematerialization_eligibility(INSTANCE_ID)
                .await
                .err(),
            Some(ReconciliationEvidenceRejection::JournalMismatch)
        );
        drop(restarted_state);
        cleanup(fixture).await;
    }

    #[tokio::test]
    async fn whole_instance_terminal_shapes_reject_impossible_outcomes_and_checkpoints() {
        let fixture = fixture("whole-instance-terminal-shapes");
        persist_runtime_component_ladder(
            &fixture,
            "whole-instance-terminal-shapes",
            "java-runtime-delta",
            GuardianMode::Managed,
        )
        .await;
        let offer = whole_instance_offer(&fixture, GuardianMode::Managed).await;
        let admission = fixture
            .state
            .admit_whole_instance_rematerialization(
                offer.into_authorization(),
                OperationId::new("whole-instance-terminal-shapes-whole"),
                chrono::Duration::minutes(30),
            )
            .await;
        let Ok(admission) = admission else {
            panic!("canonical rung-three shape fixture must admit");
        };
        let attempt = admission.attempt().clone();
        let runtime = || ReconciliationQuarantineRecord::runtime("java-runtime-delta");

        let (success, success_terminal) = whole_instance_terminal_journal_for_shape(
            &attempt,
            ReconciliationTerminalOutcome::Succeeded,
            ReconciliationQuarantineCheckpoint::default(),
            WHOLE_INSTANCE_REMATERIALIZATION_STEP,
        );
        assert!(whole_instance_terminal_matches(&success, &success_terminal));
        let (start_success, start_success_terminal) = whole_instance_terminal_journal_for_shape(
            &attempt,
            ReconciliationTerminalOutcome::Succeeded,
            ReconciliationQuarantineCheckpoint::default(),
            WHOLE_INSTANCE_REMATERIALIZATION_START_STEP,
        );
        assert!(!whole_instance_terminal_matches(
            &start_success,
            &start_success_terminal
        ));
        let (quarantined_success, quarantined_success_terminal) =
            whole_instance_terminal_journal_for_shape(
                &attempt,
                ReconciliationTerminalOutcome::Succeeded,
                ReconciliationQuarantineCheckpoint::new(vec![runtime()]),
                WHOLE_INSTANCE_REMATERIALIZATION_STEP,
            );
        assert!(!whole_instance_terminal_matches(
            &quarantined_success,
            &quarantined_success_terminal,
        ));
        let (duplicate, duplicate_terminal) = whole_instance_terminal_journal_for_shape(
            &attempt,
            ReconciliationTerminalOutcome::Failed,
            ReconciliationQuarantineCheckpoint::new(vec![runtime(), runtime()]),
            WHOLE_INSTANCE_REMATERIALIZATION_STEP,
        );
        assert!(!whole_instance_terminal_matches(
            &duplicate,
            &duplicate_terminal
        ));
        let (foreign, foreign_terminal) = whole_instance_terminal_journal_for_shape(
            &attempt,
            ReconciliationTerminalOutcome::Failed,
            ReconciliationQuarantineCheckpoint::new(vec![runtime()]),
            WHOLE_INSTANCE_REMATERIALIZATION_STEP,
        );
        assert!(whole_instance_terminal_matches(&foreign, &foreign_terminal));
        assert!(
            !fixture
                .state
                .whole_instance_terminal_is_semantically_canonical(&foreign, &foreign_terminal)
        );
        drop(admission);
        cleanup(fixture).await;
    }

    #[tokio::test]
    async fn whole_instance_stale_policy_mode_rejects_before_r3_and_fresh_assessment_preserves_predecessor_mode()
     {
        let fixture = fixture("whole-instance-policy-mode-change");
        persist_runtime_component_ladder(
            &fixture,
            "whole-instance-policy-mode-change",
            "java-runtime-delta",
            GuardianMode::Managed,
        )
        .await;
        set_guardian_mode(&fixture, GuardianMode::Managed);
        let stale_offer = whole_instance_offer(&fixture, GuardianMode::Managed).await;
        set_guardian_mode(&fixture, GuardianMode::Custom);
        let stale_operation_id = OperationId::new("whole-instance-stale-policy-mode");
        assert_eq!(
            fixture
                .state
                .admit_whole_instance_rematerialization(
                    stale_offer.into_authorization(),
                    stale_operation_id.clone(),
                    chrono::Duration::minutes(30),
                )
                .await
                .err(),
            Some(ReconciliationEvidenceRejection::PolicyModeChanged)
        );
        assert!(fixture.journals.get(&stale_operation_id).is_none());
        assert!(!fixture.state.instance_lifecycle_is_held(INSTANCE_ID).await);

        let fresh_offer = whole_instance_offer(&fixture, GuardianMode::Custom).await;
        let admission = fixture
            .state
            .admit_whole_instance_rematerialization(
                fresh_offer.into_authorization(),
                OperationId::new("whole-instance-fresh-policy-mode"),
                chrono::Duration::minutes(30),
            )
            .await
            .expect("fresh Custom assessment admits the Managed predecessor");
        assert_eq!(admission.attempt().mode(), GuardianMode::Managed);
        drop(admission);
        cleanup(fixture).await;
    }

    #[tokio::test]
    async fn guardian_whole_instance_assessment_offers_managed_and_custom_and_witnesses_disabled() {
        for (label, predecessor_mode) in [
            ("whole-instance-policy-managed", GuardianMode::Managed),
            ("whole-instance-policy-custom", GuardianMode::Custom),
        ] {
            let fixture = fixture(label);
            persist_runtime_component_ladder(
                &fixture,
                label,
                "java-runtime-delta",
                predecessor_mode,
            )
            .await;
            let eligibility = fixture
                .state
                .whole_instance_rematerialization_eligibility(INSTANCE_ID)
                .await
                .expect("exact failed rung two is eligible");
            let (assessment, evaluations) =
                crate::guardian::with_guardian_policy_evaluation_count(async move {
                    assess_whole_instance_rematerialization(eligibility, predecessor_mode)
                })
                .await;
            let GuardianWholeInstanceRematerializationDisposition::Offered(_offer) =
                assessment.expect("Managed and Custom assessment")
            else {
                panic!("Managed and Custom must produce an internal offer");
            };
            assert_eq!(evaluations, 1);
            assert!(!fixture.state.instance_lifecycle_is_held(INSTANCE_ID).await);
            cleanup(fixture).await;
        }

        let disabled_fixture = fixture("whole-instance-policy-disabled");
        persist_runtime_component_ladder(
            &disabled_fixture,
            "whole-instance-policy-disabled",
            "java-runtime-delta",
            GuardianMode::Managed,
        )
        .await;
        let eligibility = disabled_fixture
            .state
            .whole_instance_rematerialization_eligibility(INSTANCE_ID)
            .await
            .expect("prior Managed failure remains factual evidence");
        let (assessment, evaluations) =
            crate::guardian::with_guardian_policy_evaluation_count(async move {
                assess_whole_instance_rematerialization(eligibility, GuardianMode::Disabled)
            })
            .await;
        let GuardianWholeInstanceRematerializationDisposition::WitnessOnly { decision } =
            assessment.expect("Disabled assessment")
        else {
            panic!("Disabled must remain witness-only");
        };
        assert_eq!(decision.kind(), GuardianActionKind::RecordOnly);
        assert_eq!(evaluations, 1);
        assert!(
            !disabled_fixture
                .state
                .instance_lifecycle_is_held(INSTANCE_ID)
                .await
        );
        cleanup(disabled_fixture).await;

        let missing = fixture("whole-instance-policy-missing");
        let (missing_eligibility, evaluations) =
            crate::guardian::with_guardian_policy_evaluation_count(async {
                missing
                    .state
                    .whole_instance_rematerialization_eligibility(INSTANCE_ID)
                    .await
            })
            .await;
        assert_eq!(
            missing_eligibility.err(),
            Some(ReconciliationEvidenceRejection::MemoryMissing)
        );
        assert_eq!(evaluations, 0);
        cleanup(missing).await;
    }

    #[tokio::test]
    async fn whole_instance_eligibility_rejects_root_drift_and_cross_mode_ambiguity() {
        let drift = fixture("whole-instance-offer-root-drift");
        persist_runtime_component_ladder(
            &drift,
            "whole-instance-offer-root-drift",
            "java-runtime-delta",
            GuardianMode::Managed,
        )
        .await;
        let offer = whole_instance_offer(&drift, GuardianMode::Managed).await;
        let replacement = drift.root.join("whole-instance-replacement-library");
        fs::create_dir_all(&replacement).expect("replacement root");
        drift
            .state
            .set_library_dir_for_test(replacement.to_string_lossy().into_owned());
        assert_eq!(
            drift
                .state
                .admit_whole_instance_rematerialization(
                    offer.into_authorization(),
                    OperationId::new("whole-instance-root-drift-rematerialize"),
                    chrono::Duration::minutes(30),
                )
                .await
                .err(),
            Some(ReconciliationEvidenceRejection::IncarnationMismatch)
        );
        cleanup(drift).await;

        let ambiguous = fixture("whole-instance-offer-ambiguous");
        persist_runtime_component_ladder(
            &ambiguous,
            "whole-instance-offer-ambiguous-first",
            "java-runtime-delta",
            GuardianMode::Managed,
        )
        .await;
        persist_runtime_component_ladder(
            &ambiguous,
            "whole-instance-offer-ambiguous-second",
            "jre-legacy",
            GuardianMode::Custom,
        )
        .await;
        let (ambiguous_eligibility, evaluations) =
            crate::guardian::with_guardian_policy_evaluation_count(async {
                ambiguous
                    .state
                    .whole_instance_rematerialization_eligibility(INSTANCE_ID)
                    .await
            })
            .await;
        assert_eq!(
            ambiguous_eligibility.err(),
            Some(ReconciliationEvidenceRejection::JournalMismatch)
        );
        assert_eq!(evaluations, 0);
        cleanup(ambiguous).await;
    }
}
