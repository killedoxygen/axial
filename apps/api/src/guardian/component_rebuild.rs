use super::reconciliation_journal::{
    GuardianJournalReconciliation, reconcile_guardian_journal_error,
    record_reconciliation_terminal_reconciled, repair_step_with_rollback,
};
use super::{GuardianDomain, GuardianMode};
use crate::observability::{RedactionAudience, sanitize_evidence_token};
use crate::state::contracts::{
    CommandKind, OperationId, OperationStatus, OperationStepResult, OwnershipClass,
    ReconciliationComponent, ReconciliationRung, ReconciliationTerminal, RollbackState,
    StabilizationSystem, TargetKind,
};
use crate::state::failure_memory::{
    FailureMemoryStoreError, GuardianFailureMemoryEntry, GuardianFailureMemoryStore,
};
use crate::state::{
    ASSETS_COMPONENT_REBUILD_STEP, COMPONENT_QUARANTINE_STEP, COMPONENT_REBUILD_START_STEP,
    LIBRARIES_COMPONENT_REBUILD_STEP, MAX_OPERATION_JOURNAL_STEP_FACTS,
    ManagedArtifactMutationAdmission, ManagedArtifactMutationEpochUnavailable,
    OperationJournalStoreError, ProducerLease, RUNTIME_COMPONENT_REBUILD_STEP,
    ReconciliationAttemptReservation, RegisteredAssetsComponentRebuildEffect,
    RegisteredComponentRebuildAdmission, RegisteredLibrariesComponentRebuildEffect,
    RegisteredManagedArtifactCommitPostcheck, RegisteredManagedArtifactComponentCompletion,
    RegisteredManagedArtifactComponentEffectAdmission,
    RegisteredManagedArtifactComponentSettlement, RegisteredVersionBundleComponentRebuildEffect,
    VERSION_BUNDLE_COMPONENT_REBUILD_STEP, commit_reconciliation_memory, component_rebuild_journal,
    operation_journal_completed_step_is_visible, operation_journal_plan_is_visible,
    reconciliation_attempt_key, reconciliation_memory_entry, reserve_reconciliation_attempt,
    settle_reconciliation_memory, validate_reconciliation_memory,
};
use axial_minecraft::runtime::{
    ManagedRuntimeCommitReceipt, ManagedRuntimeFailureReceipt, RuntimeId,
    is_known_runtime_component,
};
use axial_minecraft::{
    ManagedAssetsCommitReceipt, ManagedAssetsRollbackReceipt, ManagedLibrariesCommitReceipt,
    ManagedLibrariesRollbackReceipt, ManagedRuntimeCache, ManagedVersionBundleCommitReceipt,
    ManagedVersionBundleRollbackReceipt,
};
use std::future::Future;
use std::sync::Arc;

const COMPONENT_MEMORY_RETRY_INITIAL_DELAY: std::time::Duration =
    std::time::Duration::from_millis(20);
const COMPONENT_MEMORY_RETRY_MAX_DELAY: std::time::Duration = std::time::Duration::from_secs(1);

#[cfg(test)]
tokio::task_local! {
    static REGISTERED_ARTIFACT_EXACT_PROOF_LIFETIME:
        Arc<std::sync::Mutex<Option<std::sync::Weak<()>>>>;
}

pub(crate) struct ManagedRuntimeComponentRebuildEffect {
    admission: RegisteredComponentRebuildAdmission,
    mutation: Option<ManagedArtifactMutationAdmission>,
    reservation: ReconciliationAttemptReservation,
    identity: Arc<()>,
}

pub(crate) struct RuntimeComponentRebuildEffectResult {
    inner: RuntimeComponentRebuildEffectResultInner,
}

pub(crate) struct ManagedLibrariesComponentRebuildEffect {
    completion: RegisteredManagedArtifactComponentCompletion,
    mutation: ManagedArtifactMutationAdmission,
    reservation: ReconciliationAttemptReservation,
    request: RegisteredLibrariesComponentRebuildEffect,
    identity: Arc<()>,
}

pub(crate) struct ManagedVersionBundleComponentRebuildEffect {
    completion: RegisteredManagedArtifactComponentCompletion,
    mutation: ManagedArtifactMutationAdmission,
    reservation: ReconciliationAttemptReservation,
    request: RegisteredVersionBundleComponentRebuildEffect,
    identity: Arc<()>,
}

pub(crate) struct VersionBundleComponentRebuildEffectResult {
    inner: VersionBundleComponentRebuildEffectResultInner,
}

pub(crate) struct LibrariesComponentRebuildEffectResult {
    inner: LibrariesComponentRebuildEffectResultInner,
}

struct ManagedAssetsComponentRebuildEffect {
    completion: RegisteredManagedArtifactComponentCompletion,
    mutation: ManagedArtifactMutationAdmission,
    reservation: ReconciliationAttemptReservation,
    request: RegisteredAssetsComponentRebuildEffect,
    identity: Arc<()>,
}

struct AssetsComponentRebuildEffectResult {
    inner: AssetsComponentRebuildEffectResultInner,
}

enum AssetsComponentRebuildEffectResultInner {
    Committed {
        effect: ManagedAssetsComponentRebuildEffect,
        receipt: ManagedAssetsCommitReceipt,
        facts: Vec<String>,
    },
    FailedBeforeEffect {
        effect: ManagedAssetsComponentRebuildEffect,
        facts: Vec<String>,
    },
    RolledBack {
        effect: ManagedAssetsComponentRebuildEffect,
        receipt: ManagedAssetsRollbackReceipt,
        facts: Vec<String>,
    },
}

enum LibrariesComponentRebuildEffectResultInner {
    Committed {
        effect: ManagedLibrariesComponentRebuildEffect,
        receipt: ManagedLibrariesCommitReceipt,
        facts: Vec<String>,
    },
    FailedBeforeEffect {
        effect: ManagedLibrariesComponentRebuildEffect,
        facts: Vec<String>,
    },
    RolledBack {
        effect: ManagedLibrariesComponentRebuildEffect,
        receipt: ManagedLibrariesRollbackReceipt,
        facts: Vec<String>,
    },
}

enum VersionBundleComponentRebuildEffectResultInner {
    Committed {
        effect: ManagedVersionBundleComponentRebuildEffect,
        receipt: ManagedVersionBundleCommitReceipt,
        facts: Vec<String>,
    },
    FailedBeforeEffect {
        effect: ManagedVersionBundleComponentRebuildEffect,
        facts: Vec<String>,
    },
    RolledBack {
        effect: ManagedVersionBundleComponentRebuildEffect,
        receipt: ManagedVersionBundleRollbackReceipt,
        facts: Vec<String>,
    },
}

enum RuntimeComponentRebuildEffectResultInner {
    Succeeded {
        effect: ManagedRuntimeComponentRebuildEffect,
        receipt: Box<ManagedRuntimeCommitReceipt>,
        facts: Vec<String>,
    },
    FailedBeforeEffect {
        effect: ManagedRuntimeComponentRebuildEffect,
        facts: Vec<String>,
    },
    FailedAfterEffect {
        effect: ManagedRuntimeComponentRebuildEffect,
        receipt: Box<ManagedRuntimeFailureReceipt>,
        facts: Vec<String>,
    },
}

impl ManagedRuntimeComponentRebuildEffect {
    fn new(
        admission: RegisteredComponentRebuildAdmission,
        mutation: Option<ManagedArtifactMutationAdmission>,
        reservation: ReconciliationAttemptReservation,
    ) -> (Self, Arc<()>) {
        let identity = Arc::new(());
        (
            Self {
                admission,
                mutation,
                reservation,
                identity: identity.clone(),
            },
            identity,
        )
    }

    fn matches_identity(&self, expected: &Arc<()>) -> bool {
        Arc::ptr_eq(&self.identity, expected)
    }

    pub(crate) fn core_request(&self) -> (ManagedRuntimeCache, RuntimeId) {
        self.admission
            .runtime_core_request()
            .expect("Guardian validated the State-owned Runtime effect request")
    }

    pub(crate) fn succeeded(
        self,
        receipt: ManagedRuntimeCommitReceipt,
        facts: impl IntoIterator<Item = String>,
    ) -> RuntimeComponentRebuildEffectResult {
        RuntimeComponentRebuildEffectResult {
            inner: RuntimeComponentRebuildEffectResultInner::Succeeded {
                effect: self,
                receipt: Box::new(receipt),
                facts: bounded_fact_ids(facts),
            },
        }
    }

    pub(crate) fn failed_before_effect(
        self,
        facts: impl IntoIterator<Item = String>,
    ) -> RuntimeComponentRebuildEffectResult {
        RuntimeComponentRebuildEffectResult {
            inner: RuntimeComponentRebuildEffectResultInner::FailedBeforeEffect {
                effect: self,
                facts: bounded_fact_ids(facts),
            },
        }
    }

    pub(crate) fn failed_after_effect(
        self,
        receipt: Box<ManagedRuntimeFailureReceipt>,
        facts: impl IntoIterator<Item = String>,
    ) -> RuntimeComponentRebuildEffectResult {
        RuntimeComponentRebuildEffectResult {
            inner: RuntimeComponentRebuildEffectResultInner::FailedAfterEffect {
                effect: self,
                receipt,
                facts: bounded_fact_ids(facts),
            },
        }
    }
}

impl ManagedLibrariesComponentRebuildEffect {
    fn new(
        completion: RegisteredManagedArtifactComponentCompletion,
        mutation: ManagedArtifactMutationAdmission,
        reservation: ReconciliationAttemptReservation,
        request: RegisteredLibrariesComponentRebuildEffect,
    ) -> (Self, Arc<()>) {
        let identity = Arc::new(());
        (
            Self {
                completion,
                mutation,
                reservation,
                request,
                identity: identity.clone(),
            },
            identity,
        )
    }

    fn matches_identity(&self, expected: &Arc<()>) -> bool {
        Arc::ptr_eq(&self.identity, expected)
    }

    pub(crate) fn core_request(&self) -> (&std::path::Path, &str) {
        self.request.core_request()
    }

    pub(crate) fn committed(
        self,
        receipt: ManagedLibrariesCommitReceipt,
        facts: impl IntoIterator<Item = String>,
    ) -> LibrariesComponentRebuildEffectResult {
        LibrariesComponentRebuildEffectResult {
            inner: LibrariesComponentRebuildEffectResultInner::Committed {
                effect: self,
                receipt,
                facts: bounded_fact_ids(facts),
            },
        }
    }

    pub(crate) fn failed_before_effect(
        self,
        facts: impl IntoIterator<Item = String>,
    ) -> LibrariesComponentRebuildEffectResult {
        LibrariesComponentRebuildEffectResult {
            inner: LibrariesComponentRebuildEffectResultInner::FailedBeforeEffect {
                effect: self,
                facts: bounded_fact_ids(facts),
            },
        }
    }

    pub(crate) fn rolled_back(
        self,
        receipt: ManagedLibrariesRollbackReceipt,
        facts: impl IntoIterator<Item = String>,
    ) -> LibrariesComponentRebuildEffectResult {
        LibrariesComponentRebuildEffectResult {
            inner: LibrariesComponentRebuildEffectResultInner::RolledBack {
                effect: self,
                receipt,
                facts: bounded_fact_ids(facts),
            },
        }
    }
}

impl ManagedVersionBundleComponentRebuildEffect {
    fn new(
        completion: RegisteredManagedArtifactComponentCompletion,
        mutation: ManagedArtifactMutationAdmission,
        reservation: ReconciliationAttemptReservation,
        request: RegisteredVersionBundleComponentRebuildEffect,
    ) -> (Self, Arc<()>) {
        let identity = Arc::new(());
        (
            Self {
                completion,
                mutation,
                reservation,
                request,
                identity: identity.clone(),
            },
            identity,
        )
    }

    fn matches_identity(&self, expected: &Arc<()>) -> bool {
        Arc::ptr_eq(&self.identity, expected)
    }

    pub(crate) fn core_request(
        &self,
    ) -> (
        &std::path::Path,
        &str,
        &Arc<axial_minecraft::known_good::KnownGoodInventory>,
    ) {
        self.request.core_request()
    }

    pub(crate) fn committed(
        self,
        receipt: ManagedVersionBundleCommitReceipt,
        facts: impl IntoIterator<Item = String>,
    ) -> VersionBundleComponentRebuildEffectResult {
        VersionBundleComponentRebuildEffectResult {
            inner: VersionBundleComponentRebuildEffectResultInner::Committed {
                effect: self,
                receipt,
                facts: bounded_fact_ids(facts),
            },
        }
    }

    pub(crate) fn failed_before_effect(
        self,
        facts: impl IntoIterator<Item = String>,
    ) -> VersionBundleComponentRebuildEffectResult {
        VersionBundleComponentRebuildEffectResult {
            inner: VersionBundleComponentRebuildEffectResultInner::FailedBeforeEffect {
                effect: self,
                facts: bounded_fact_ids(facts),
            },
        }
    }

    pub(crate) fn rolled_back(
        self,
        receipt: ManagedVersionBundleRollbackReceipt,
        facts: impl IntoIterator<Item = String>,
    ) -> VersionBundleComponentRebuildEffectResult {
        VersionBundleComponentRebuildEffectResult {
            inner: VersionBundleComponentRebuildEffectResultInner::RolledBack {
                effect: self,
                receipt,
                facts: bounded_fact_ids(facts),
            },
        }
    }
}

impl ManagedAssetsComponentRebuildEffect {
    fn new(
        completion: RegisteredManagedArtifactComponentCompletion,
        mutation: ManagedArtifactMutationAdmission,
        reservation: ReconciliationAttemptReservation,
        request: RegisteredAssetsComponentRebuildEffect,
    ) -> (Self, Arc<()>) {
        let identity = Arc::new(());
        (
            Self {
                completion,
                mutation,
                reservation,
                request,
                identity: identity.clone(),
            },
            identity,
        )
    }

    fn matches_identity(&self, expected: &Arc<()>) -> bool {
        Arc::ptr_eq(&self.identity, expected)
    }

    fn core_request(&self) -> (&std::path::Path, &str) {
        self.request.core_request()
    }

    fn committed(
        self,
        receipt: ManagedAssetsCommitReceipt,
        facts: impl IntoIterator<Item = String>,
    ) -> AssetsComponentRebuildEffectResult {
        AssetsComponentRebuildEffectResult {
            inner: AssetsComponentRebuildEffectResultInner::Committed {
                effect: self,
                receipt,
                facts: bounded_fact_ids(facts),
            },
        }
    }

    fn failed_before_effect(
        self,
        facts: impl IntoIterator<Item = String>,
    ) -> AssetsComponentRebuildEffectResult {
        AssetsComponentRebuildEffectResult {
            inner: AssetsComponentRebuildEffectResultInner::FailedBeforeEffect {
                effect: self,
                facts: bounded_fact_ids(facts),
            },
        }
    }

    fn rolled_back(
        self,
        receipt: ManagedAssetsRollbackReceipt,
        facts: impl IntoIterator<Item = String>,
    ) -> AssetsComponentRebuildEffectResult {
        AssetsComponentRebuildEffectResult {
            inner: AssetsComponentRebuildEffectResultInner::RolledBack {
                effect: self,
                receipt,
                facts: bounded_fact_ids(facts),
            },
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GuardianComponentRebuildStatus {
    Rebuilt,
    Failed,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct GuardianComponentRebuildOutcome {
    pub(crate) operation_id: OperationId,
    pub(crate) status: GuardianComponentRebuildStatus,
    pub(crate) facts: Vec<String>,
}

async fn await_component_rebuild_owner<Owner>(
    producer: ProducerLease,
    owner: Owner,
) -> Result<GuardianComponentRebuildOutcome, OperationJournalStoreError>
where
    Owner: Future<Output = Result<GuardianComponentRebuildOutcome, OperationJournalStoreError>>
        + Send
        + 'static,
{
    #[cfg(test)]
    let exact_proof_lifetime = REGISTERED_ARTIFACT_EXACT_PROOF_LIFETIME
        .try_with(Arc::clone)
        .ok();
    #[cfg(test)]
    let owner = producer.spawn_joinable(async move {
        match exact_proof_lifetime {
            Some(exact_proof_lifetime) => {
                REGISTERED_ARTIFACT_EXACT_PROOF_LIFETIME
                    .scope(exact_proof_lifetime, owner)
                    .await
            }
            None => owner.await,
        }
    });
    #[cfg(not(test))]
    let owner = producer.spawn_joinable(owner);
    owner.await.map_err(|_| {
        invalid_component_rebuild_error(
            std::io::ErrorKind::Other,
            "component rebuild effect owner stopped unexpectedly",
        )
    })?
}

pub(crate) async fn execute_managed_runtime_component_rebuild<Effect, EffectFuture>(
    producer: ProducerLease,
    admission: RegisteredComponentRebuildAdmission,
    effect: Effect,
) -> Result<GuardianComponentRebuildOutcome, OperationJournalStoreError>
where
    Effect: FnOnce(ManagedRuntimeComponentRebuildEffect) -> EffectFuture + Send + 'static,
    EffectFuture: Future<Output = RuntimeComponentRebuildEffectResult> + Send + 'static,
{
    validate_managed_runtime_admission(&admission)?;
    await_component_rebuild_owner(producer, async move {
        settle_reconciliation_memory(admission.failure_memory())
            .await
            .map_err(component_rebuild_memory_error)?;
        let reservation = reserve_reconciliation_attempt(
            admission.failure_memory(),
            admission.journals(),
            reconciliation_attempt_key(admission.attempt()),
        )
        .map_err(|_| {
            invalid_component_rebuild_error(
                std::io::ErrorKind::WouldBlock,
                "runtime component rebuild attempt is already active or ambiguous",
            )
        })?;

        if let Some(plan_error) = create_component_rebuild_plan(&admission).await? {
            let (effect, _) =
                ManagedRuntimeComponentRebuildEffect::new(admission, None, reservation);
            terminalize_component_rebuild(
                effect,
                ComponentRebuildTerminal::FailedBeforeEffect {
                    facts: Vec::new(),
                    step_id: COMPONENT_REBUILD_START_STEP,
                },
            )
            .await?;
            return Err(plan_error);
        }

        let mutation = match admission.admit_managed_artifact_mutation() {
            Ok(mutation) => mutation,
            Err(error) => {
                let (effect, _) =
                    ManagedRuntimeComponentRebuildEffect::new(admission, None, reservation);
                return terminalize_component_rebuild(
                    effect,
                    ComponentRebuildTerminal::FailedBeforeEffect {
                        facts: vec![managed_artifact_mutation_admission_fact(error)],
                        step_id: RUNTIME_COMPONENT_REBUILD_STEP,
                    },
                )
                .await;
            }
        };

        let (effect_capability, effect_identity) =
            ManagedRuntimeComponentRebuildEffect::new(admission, Some(mutation), reservation);
        match effect(effect_capability).await.inner {
            RuntimeComponentRebuildEffectResultInner::Succeeded {
                effect,
                receipt,
                facts,
            } => {
                validate_effect_identity(&effect, &effect_identity)?;
                terminalize_component_rebuild(
                    effect,
                    ComponentRebuildTerminal::Succeeded { receipt, facts },
                )
                .await
            }
            RuntimeComponentRebuildEffectResultInner::FailedBeforeEffect { effect, facts } => {
                validate_effect_identity(&effect, &effect_identity)?;
                terminalize_component_rebuild(
                    effect,
                    ComponentRebuildTerminal::FailedBeforeEffect {
                        facts,
                        step_id: RUNTIME_COMPONENT_REBUILD_STEP,
                    },
                )
                .await
            }
            RuntimeComponentRebuildEffectResultInner::FailedAfterEffect {
                effect,
                receipt,
                facts,
            } => {
                validate_effect_identity(&effect, &effect_identity)?;
                terminalize_component_rebuild(
                    effect,
                    ComponentRebuildTerminal::FailedAfterEffect { receipt, facts },
                )
                .await
            }
        }
    })
    .await
}

pub(crate) async fn execute_managed_libraries_component_rebuild<Effect, EffectFuture>(
    producer: ProducerLease,
    admission: RegisteredComponentRebuildAdmission,
    effect: Effect,
) -> Result<GuardianComponentRebuildOutcome, OperationJournalStoreError>
where
    Effect: FnOnce(ManagedLibrariesComponentRebuildEffect) -> EffectFuture + Send + 'static,
    EffectFuture: Future<Output = LibrariesComponentRebuildEffectResult> + Send + 'static,
{
    validate_managed_artifact_admission(&admission, ManagedArtifactGuardianComponent::Libraries)?;
    await_component_rebuild_owner(producer, async move {
        settle_reconciliation_memory(admission.failure_memory())
            .await
            .map_err(component_rebuild_memory_error)?;
        let reservation = reserve_reconciliation_attempt(
            admission.failure_memory(),
            admission.journals(),
            reconciliation_attempt_key(admission.attempt()),
        )
        .map_err(|_| {
            invalid_component_rebuild_error(
                std::io::ErrorKind::WouldBlock,
                "Libraries component rebuild attempt is already active or ambiguous",
            )
        })?;

        if let Some(plan_error) = create_component_rebuild_plan(&admission).await? {
            let settlement = match admission.into_libraries_effect() {
                RegisteredManagedArtifactComponentEffectAdmission::Admitted {
                    completion, ..
                } => (*completion).into_failed_settlement(),
                RegisteredManagedArtifactComponentEffectAdmission::Refused(settlement) => {
                    *settlement
                }
            };
            persist_managed_artifact_component_terminal(
                &settlement,
                reservation,
                Vec::new(),
                COMPONENT_REBUILD_START_STEP,
                RollbackState::NotApplicable,
            )
            .await?;
            return Err(plan_error);
        }

        let mutation = match admission.admit_managed_artifact_mutation() {
            Ok(mutation) => mutation,
            Err(error) => {
                let settlement = match admission.into_libraries_effect() {
                    RegisteredManagedArtifactComponentEffectAdmission::Admitted {
                        completion,
                        ..
                    } => (*completion).into_failed_settlement(),
                    RegisteredManagedArtifactComponentEffectAdmission::Refused(settlement) => {
                        *settlement
                    }
                };
                return persist_managed_artifact_component_terminal(
                    &settlement,
                    reservation,
                    vec![managed_artifact_mutation_admission_fact(error)],
                    LIBRARIES_COMPONENT_REBUILD_STEP,
                    RollbackState::NotApplicable,
                )
                .await;
            }
        };

        let (request, completion) = match admission.into_libraries_effect() {
            RegisteredManagedArtifactComponentEffectAdmission::Admitted {
                request,
                completion,
            } => (request, *completion),
            RegisteredManagedArtifactComponentEffectAdmission::Refused(settlement) => {
                return persist_managed_artifact_component_terminal(
                    &settlement,
                    reservation,
                    vec!["libraries_component_authority_changed".to_string()],
                    LIBRARIES_COMPONENT_REBUILD_STEP,
                    RollbackState::NotApplicable,
                )
                .await;
            }
        };
        let (effect_capability, effect_identity) =
            ManagedLibrariesComponentRebuildEffect::new(completion, mutation, reservation, request);
        match effect(effect_capability).await.inner {
            LibrariesComponentRebuildEffectResultInner::Committed {
                effect,
                receipt,
                facts,
            } => {
                validate_libraries_effect_identity(&effect, &effect_identity)?;
                terminalize_libraries_component_rebuild(
                    effect,
                    LibrariesComponentRebuildTerminal::Committed { receipt, facts },
                )
                .await
            }
            LibrariesComponentRebuildEffectResultInner::FailedBeforeEffect { effect, facts } => {
                validate_libraries_effect_identity(&effect, &effect_identity)?;
                terminalize_libraries_component_rebuild(
                    effect,
                    LibrariesComponentRebuildTerminal::FailedBeforeEffect { facts },
                )
                .await
            }
            LibrariesComponentRebuildEffectResultInner::RolledBack {
                effect,
                receipt,
                facts,
            } => {
                validate_libraries_effect_identity(&effect, &effect_identity)?;
                terminalize_libraries_component_rebuild(
                    effect,
                    LibrariesComponentRebuildTerminal::RolledBack { receipt, facts },
                )
                .await
            }
        }
    })
    .await
}

pub(crate) async fn execute_managed_version_bundle_component_rebuild<Effect, EffectFuture>(
    producer: ProducerLease,
    admission: RegisteredComponentRebuildAdmission,
    effect: Effect,
) -> Result<GuardianComponentRebuildOutcome, OperationJournalStoreError>
where
    Effect: FnOnce(ManagedVersionBundleComponentRebuildEffect) -> EffectFuture + Send + 'static,
    EffectFuture: Future<Output = VersionBundleComponentRebuildEffectResult> + Send + 'static,
{
    validate_managed_artifact_admission(
        &admission,
        ManagedArtifactGuardianComponent::VersionBundle,
    )?;
    await_component_rebuild_owner(producer, async move {
        settle_reconciliation_memory(admission.failure_memory())
            .await
            .map_err(component_rebuild_memory_error)?;
        let reservation = reserve_reconciliation_attempt(
            admission.failure_memory(),
            admission.journals(),
            reconciliation_attempt_key(admission.attempt()),
        )
        .map_err(|_| {
            invalid_component_rebuild_error(
                std::io::ErrorKind::WouldBlock,
                "VersionBundle component rebuild attempt is already active or ambiguous",
            )
        })?;

        if let Some(plan_error) = create_component_rebuild_plan(&admission).await? {
            let settlement = match admission.into_version_bundle_effect() {
                RegisteredManagedArtifactComponentEffectAdmission::Admitted {
                    completion, ..
                } => (*completion).into_failed_settlement(),
                RegisteredManagedArtifactComponentEffectAdmission::Refused(settlement) => {
                    *settlement
                }
            };
            persist_managed_artifact_component_terminal(
                &settlement,
                reservation,
                Vec::new(),
                COMPONENT_REBUILD_START_STEP,
                RollbackState::NotApplicable,
            )
            .await?;
            return Err(plan_error);
        }

        let mutation = match admission.admit_managed_artifact_mutation() {
            Ok(mutation) => mutation,
            Err(error) => {
                let settlement = match admission.into_version_bundle_effect() {
                    RegisteredManagedArtifactComponentEffectAdmission::Admitted {
                        completion,
                        ..
                    } => (*completion).into_failed_settlement(),
                    RegisteredManagedArtifactComponentEffectAdmission::Refused(settlement) => {
                        *settlement
                    }
                };
                return persist_managed_artifact_component_terminal(
                    &settlement,
                    reservation,
                    vec![managed_artifact_mutation_admission_fact(error)],
                    VERSION_BUNDLE_COMPONENT_REBUILD_STEP,
                    RollbackState::NotApplicable,
                )
                .await;
            }
        };

        let (request, completion) = match admission.into_version_bundle_effect() {
            RegisteredManagedArtifactComponentEffectAdmission::Admitted {
                request,
                completion,
            } => (request, *completion),
            RegisteredManagedArtifactComponentEffectAdmission::Refused(settlement) => {
                return persist_managed_artifact_component_terminal(
                    &settlement,
                    reservation,
                    vec!["version_bundle_component_authority_changed".to_string()],
                    VERSION_BUNDLE_COMPONENT_REBUILD_STEP,
                    RollbackState::NotApplicable,
                )
                .await;
            }
        };
        let (effect_capability, effect_identity) = ManagedVersionBundleComponentRebuildEffect::new(
            completion,
            mutation,
            reservation,
            request,
        );
        match effect(effect_capability).await.inner {
            VersionBundleComponentRebuildEffectResultInner::Committed {
                effect,
                receipt,
                facts,
            } => {
                validate_version_bundle_effect_identity(&effect, &effect_identity)?;
                terminalize_version_bundle_component_rebuild(
                    effect,
                    VersionBundleComponentRebuildTerminal::Committed { receipt, facts },
                )
                .await
            }
            VersionBundleComponentRebuildEffectResultInner::FailedBeforeEffect {
                effect,
                facts,
            } => {
                validate_version_bundle_effect_identity(&effect, &effect_identity)?;
                terminalize_version_bundle_component_rebuild(
                    effect,
                    VersionBundleComponentRebuildTerminal::FailedBeforeEffect { facts },
                )
                .await
            }
            VersionBundleComponentRebuildEffectResultInner::RolledBack {
                effect,
                receipt,
                facts,
            } => {
                validate_version_bundle_effect_identity(&effect, &effect_identity)?;
                terminalize_version_bundle_component_rebuild(
                    effect,
                    VersionBundleComponentRebuildTerminal::RolledBack { receipt, facts },
                )
                .await
            }
        }
    })
    .await
}

pub(crate) async fn execute_managed_assets_component_rebuild(
    producer: ProducerLease,
    admission: RegisteredComponentRebuildAdmission,
) -> Result<GuardianComponentRebuildOutcome, OperationJournalStoreError> {
    execute_managed_assets_component_rebuild_with_driver(producer, admission, |effect| async move {
        let (managed_root, version_id) = effect.core_request();
        let managed_root = managed_root.to_path_buf();
        let version_id = version_id.to_string();
        match axial_minecraft::rebuild_managed_assets(managed_root, &version_id).await {
            Ok(receipt) => effect.committed(receipt, ["assets_component_rebuilt".to_string()]),
            Err(
                axial_minecraft::ManagedAssetsRebuildError::Reconstruction(_)
                | axial_minecraft::ManagedAssetsRebuildError::Preparation,
            ) => effect.failed_before_effect(["assets_component_rebuild_failed".to_string()]),
            Err(axial_minecraft::ManagedAssetsRebuildError::RolledBack(receipt)) => {
                effect.rolled_back(receipt, ["assets_component_rolled_back".to_string()])
            }
        }
    })
    .await
}

async fn execute_managed_assets_component_rebuild_with_driver<Driver, DriverFuture>(
    producer: ProducerLease,
    admission: RegisteredComponentRebuildAdmission,
    driver: Driver,
) -> Result<GuardianComponentRebuildOutcome, OperationJournalStoreError>
where
    Driver: FnOnce(ManagedAssetsComponentRebuildEffect) -> DriverFuture + Send + 'static,
    DriverFuture: Future<Output = AssetsComponentRebuildEffectResult> + Send + 'static,
{
    validate_managed_artifact_admission(&admission, ManagedArtifactGuardianComponent::Assets)?;
    await_component_rebuild_owner(producer, async move {
        settle_reconciliation_memory(admission.failure_memory())
            .await
            .map_err(component_rebuild_memory_error)?;
        let reservation = reserve_reconciliation_attempt(
            admission.failure_memory(),
            admission.journals(),
            reconciliation_attempt_key(admission.attempt()),
        )
        .map_err(|_| {
            invalid_component_rebuild_error(
                std::io::ErrorKind::WouldBlock,
                "Assets component rebuild attempt is already active or ambiguous",
            )
        })?;

        if let Some(plan_error) = create_component_rebuild_plan(&admission).await? {
            let settlement = match admission.into_assets_effect() {
                RegisteredManagedArtifactComponentEffectAdmission::Admitted {
                    completion, ..
                } => (*completion).into_failed_settlement(),
                RegisteredManagedArtifactComponentEffectAdmission::Refused(settlement) => {
                    *settlement
                }
            };
            persist_managed_artifact_component_terminal(
                &settlement,
                reservation,
                Vec::new(),
                COMPONENT_REBUILD_START_STEP,
                RollbackState::NotApplicable,
            )
            .await?;
            return Err(plan_error);
        }

        let mutation = match admission.admit_managed_artifact_mutation() {
            Ok(mutation) => mutation,
            Err(error) => {
                let settlement = match admission.into_assets_effect() {
                    RegisteredManagedArtifactComponentEffectAdmission::Admitted {
                        completion,
                        ..
                    } => (*completion).into_failed_settlement(),
                    RegisteredManagedArtifactComponentEffectAdmission::Refused(settlement) => {
                        *settlement
                    }
                };
                return persist_managed_artifact_component_terminal(
                    &settlement,
                    reservation,
                    vec![managed_artifact_mutation_admission_fact(error)],
                    ASSETS_COMPONENT_REBUILD_STEP,
                    RollbackState::NotApplicable,
                )
                .await;
            }
        };

        let (request, completion) = match admission.into_assets_effect() {
            RegisteredManagedArtifactComponentEffectAdmission::Admitted {
                request,
                completion,
            } => (request, *completion),
            RegisteredManagedArtifactComponentEffectAdmission::Refused(settlement) => {
                return persist_managed_artifact_component_terminal(
                    &settlement,
                    reservation,
                    vec!["assets_component_authority_changed".to_string()],
                    ASSETS_COMPONENT_REBUILD_STEP,
                    RollbackState::NotApplicable,
                )
                .await;
            }
        };
        let (effect_capability, effect_identity) =
            ManagedAssetsComponentRebuildEffect::new(completion, mutation, reservation, request);
        match driver(effect_capability).await.inner {
            AssetsComponentRebuildEffectResultInner::Committed {
                effect,
                receipt,
                facts,
            } => {
                validate_assets_effect_identity(&effect, &effect_identity)?;
                terminalize_assets_component_rebuild(
                    effect,
                    AssetsComponentRebuildTerminal::Committed { receipt, facts },
                )
                .await
            }
            AssetsComponentRebuildEffectResultInner::FailedBeforeEffect { effect, facts } => {
                validate_assets_effect_identity(&effect, &effect_identity)?;
                terminalize_assets_component_rebuild(
                    effect,
                    AssetsComponentRebuildTerminal::FailedBeforeEffect { facts },
                )
                .await
            }
            AssetsComponentRebuildEffectResultInner::RolledBack {
                effect,
                receipt,
                facts,
            } => {
                validate_assets_effect_identity(&effect, &effect_identity)?;
                terminalize_assets_component_rebuild(
                    effect,
                    AssetsComponentRebuildTerminal::RolledBack { receipt, facts },
                )
                .await
            }
        }
    })
    .await
}

#[cfg(test)]
pub(crate) async fn execute_failed_managed_assets_component_rebuild_for_test(
    producer: ProducerLease,
    admission: RegisteredComponentRebuildAdmission,
) -> Result<GuardianComponentRebuildOutcome, OperationJournalStoreError> {
    execute_managed_assets_component_rebuild_with_driver(producer, admission, |effect| async move {
        effect.failed_before_effect(["assets_component_fixture_failure".to_string()])
    })
    .await
}

#[cfg(test)]
pub(crate) async fn execute_managed_assets_component_rebuild_fixture_for_test(
    producer: ProducerLease,
    admission: RegisteredComponentRebuildAdmission,
) -> Result<GuardianComponentRebuildOutcome, OperationJournalStoreError> {
    execute_managed_assets_component_rebuild_with_driver(producer, admission, |effect| async move {
        let (managed_root, version_id) = effect.core_request();
        let managed_root = managed_root.to_path_buf();
        let version_id = version_id.to_string();
        match axial_minecraft::rebuild_managed_assets_fixture_for_test(managed_root, &version_id)
            .await
        {
            Ok(receipt) => effect.committed(receipt, ["assets_component_rebuilt".to_string()]),
            Err(
                axial_minecraft::ManagedAssetsRebuildError::Reconstruction(_)
                | axial_minecraft::ManagedAssetsRebuildError::Preparation,
            ) => effect.failed_before_effect(["assets_component_rebuild_failed".to_string()]),
            Err(axial_minecraft::ManagedAssetsRebuildError::RolledBack(receipt)) => {
                effect.rolled_back(receipt, ["assets_component_rolled_back".to_string()])
            }
        }
    })
    .await
}

enum ComponentRebuildTerminal {
    Succeeded {
        receipt: Box<ManagedRuntimeCommitReceipt>,
        facts: Vec<String>,
    },
    FailedBeforeEffect {
        facts: Vec<String>,
        step_id: &'static str,
    },
    FailedAfterEffect {
        receipt: Box<ManagedRuntimeFailureReceipt>,
        facts: Vec<String>,
    },
}

enum LibrariesComponentRebuildTerminal {
    Committed {
        receipt: ManagedLibrariesCommitReceipt,
        facts: Vec<String>,
    },
    FailedBeforeEffect {
        facts: Vec<String>,
    },
    RolledBack {
        receipt: ManagedLibrariesRollbackReceipt,
        facts: Vec<String>,
    },
}

enum VersionBundleComponentRebuildTerminal {
    Committed {
        receipt: ManagedVersionBundleCommitReceipt,
        facts: Vec<String>,
    },
    FailedBeforeEffect {
        facts: Vec<String>,
    },
    RolledBack {
        receipt: ManagedVersionBundleRollbackReceipt,
        facts: Vec<String>,
    },
}

enum AssetsComponentRebuildTerminal {
    Committed {
        receipt: ManagedAssetsCommitReceipt,
        facts: Vec<String>,
    },
    FailedBeforeEffect {
        facts: Vec<String>,
    },
    RolledBack {
        receipt: ManagedAssetsRollbackReceipt,
        facts: Vec<String>,
    },
}

enum ComponentRebuildPublicationLease {
    RuntimeCommit(Box<ManagedRuntimeCommitReceipt>),
    RuntimeFailure(Box<ManagedRuntimeFailureReceipt>),
}

impl ComponentRebuildPublicationLease {
    fn release(self) {
        match self {
            Self::RuntimeCommit(receipt) => drop(receipt),
            Self::RuntimeFailure(receipt) => drop(receipt),
        }
    }
}

fn validate_managed_runtime_admission(
    admission: &RegisteredComponentRebuildAdmission,
) -> Result<(), OperationJournalStoreError> {
    let attempt = admission.attempt();
    if attempt.mode() != GuardianMode::Managed
        || attempt.domain() != GuardianDomain::Runtime
        || attempt.rung() != ReconciliationRung::RebuildComponent
        || attempt.component() != ReconciliationComponent::Runtime
        || attempt.ownership() != OwnershipClass::LauncherManaged
        || attempt.target().ownership != OwnershipClass::LauncherManaged
        || attempt.target().system != StabilizationSystem::Execution
        || attempt.target().kind != TargetKind::Runtime
        || !is_known_runtime_component(&attempt.target().id)
        || admission.runtime_core_request().is_err()
    {
        return Err(invalid_component_rebuild_error(
            std::io::ErrorKind::PermissionDenied,
            "Guardian refused a non-managed or non-runtime component rebuild admission",
        ));
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum ManagedArtifactGuardianComponent {
    VersionBundle,
    Libraries,
    Assets,
}

impl ManagedArtifactGuardianComponent {
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

    fn label(self) -> &'static str {
        match self {
            Self::VersionBundle => "VersionBundle",
            Self::Libraries => "Libraries",
            Self::Assets => "Assets",
        }
    }
}

fn validate_managed_artifact_admission(
    admission: &RegisteredComponentRebuildAdmission,
    component: ManagedArtifactGuardianComponent,
) -> Result<(), OperationJournalStoreError> {
    let attempt = admission.attempt();
    if attempt.mode() != GuardianMode::Managed
        || attempt.domain() != component.domain()
        || attempt.rung() != ReconciliationRung::RebuildComponent
        || attempt.component() != component.reconciliation_component()
        || attempt.ownership() != OwnershipClass::LauncherManaged
        || attempt.target().ownership != OwnershipClass::LauncherManaged
        || attempt.target().system != StabilizationSystem::Execution
        || attempt.target().kind != TargetKind::Artifact
    {
        return Err(invalid_component_rebuild_error(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "Guardian refused a non-managed or non-{} component rebuild admission",
                component.label()
            ),
        ));
    }
    Ok(())
}

fn validate_effect_identity(
    effect: &ManagedRuntimeComponentRebuildEffect,
    expected: &Arc<()>,
) -> Result<(), OperationJournalStoreError> {
    if !effect.matches_identity(expected) {
        return Err(invalid_component_rebuild_error(
            std::io::ErrorKind::InvalidData,
            "runtime component rebuild returned a foreign effect capability",
        ));
    }
    Ok(())
}

fn validate_libraries_effect_identity(
    effect: &ManagedLibrariesComponentRebuildEffect,
    expected: &Arc<()>,
) -> Result<(), OperationJournalStoreError> {
    if !effect.matches_identity(expected) {
        return Err(invalid_component_rebuild_error(
            std::io::ErrorKind::InvalidData,
            "Libraries component rebuild returned a foreign effect capability",
        ));
    }
    Ok(())
}

fn validate_version_bundle_effect_identity(
    effect: &ManagedVersionBundleComponentRebuildEffect,
    expected: &Arc<()>,
) -> Result<(), OperationJournalStoreError> {
    if !effect.matches_identity(expected) {
        return Err(invalid_component_rebuild_error(
            std::io::ErrorKind::InvalidData,
            "VersionBundle component rebuild returned a foreign effect capability",
        ));
    }
    Ok(())
}

fn validate_assets_effect_identity(
    effect: &ManagedAssetsComponentRebuildEffect,
    expected: &Arc<()>,
) -> Result<(), OperationJournalStoreError> {
    if !effect.matches_identity(expected) {
        return Err(invalid_component_rebuild_error(
            std::io::ErrorKind::InvalidData,
            "Assets component rebuild returned a foreign effect capability",
        ));
    }
    Ok(())
}

async fn create_component_rebuild_plan(
    admission: &RegisteredComponentRebuildAdmission,
) -> Result<Option<OperationJournalStoreError>, OperationJournalStoreError> {
    let journals = admission.journals();
    let operation_id = admission.attempt().operation_id();
    let expected = component_rebuild_journal(admission);
    loop {
        match journals.create_fresh(expected.clone()).await {
            Ok(()) => return Ok(None),
            Err(OperationJournalStoreError::AlreadyExists) => {
                return Err(OperationJournalStoreError::AlreadyExists);
            }
            Err(OperationJournalStoreError::RetryRequired) => {
                journals.retry().await?;
            }
            Err(error) => {
                match reconcile_guardian_journal_error(journals, operation_id, error, |entry| {
                    operation_journal_plan_is_visible(entry, &expected)
                })
                .await?
                {
                    GuardianJournalReconciliation::MutationCommitted => return Ok(None),
                    GuardianJournalReconciliation::AcceptedFailure(error) => {
                        return Ok(Some(error));
                    }
                    GuardianJournalReconciliation::RetryMutation => {}
                }
            }
        }
    }
}

async fn record_component_quarantine_checkpoint(
    admission: &RegisteredComponentRebuildAdmission,
) -> Result<(), OperationJournalStoreError> {
    let journals = admission.journals();
    let attempt = admission.attempt();
    let operation_id = attempt.operation_id();
    let checkpoint = repair_step_with_rollback(
        COMPONENT_QUARANTINE_STEP,
        OperationStepResult::Completed,
        Some(attempt.target().clone()),
        Vec::new(),
        RollbackState::Available,
    );
    loop {
        match journals
            .record_checkpoint(operation_id, checkpoint.clone())
            .await
        {
            Ok(()) => return Ok(()),
            Err(error) => {
                match reconcile_guardian_journal_error(journals, operation_id, error, |entry| {
                    &entry.operation_id == operation_id
                        && entry.command == CommandKind::RepairInstance
                        && entry.owner == StabilizationSystem::Guardian
                        && entry.status == OperationStatus::Running
                        && entry.reconciliation_attempt() == Some(attempt)
                        && entry.reconciliation_terminal().is_none()
                        && operation_journal_completed_step_is_visible(entry, &checkpoint)
                })
                .await?
                {
                    GuardianJournalReconciliation::MutationCommitted
                    | GuardianJournalReconciliation::AcceptedFailure(_) => return Ok(()),
                    GuardianJournalReconciliation::RetryMutation => {}
                }
            }
        }
    }
}

async fn terminalize_component_rebuild(
    effect: ManagedRuntimeComponentRebuildEffect,
    terminal: ComponentRebuildTerminal,
) -> Result<GuardianComponentRebuildOutcome, OperationJournalStoreError> {
    let ManagedRuntimeComponentRebuildEffect {
        admission,
        mutation: _mutation,
        reservation,
        identity: _,
    } = effect;
    let (
        typed_terminal,
        status,
        facts,
        step_id,
        step_result,
        failure_point,
        rollback,
        publication_lease,
    ) = match terminal {
        ComponentRebuildTerminal::Succeeded { receipt, facts } => {
            let rollback = if receipt.quarantine_obligation().is_some() {
                RollbackState::Available
            } else {
                RollbackState::NotApplicable
            };
            let (terminal, status, facts, step_result, failure_point) =
                match admission.succeeded_terminal(&receipt).await {
                    Ok(terminal) => (
                        terminal,
                        GuardianComponentRebuildStatus::Rebuilt,
                        facts,
                        OperationStepResult::Completed,
                        None,
                    ),
                    Err(_) => (
                        admission
                            .failed_postcondition_terminal(&receipt)
                            .map_err(|_| {
                                invalid_component_rebuild_error(
                                    std::io::ErrorKind::InvalidData,
                                    "runtime component rebuild postcondition terminal is invalid",
                                )
                            })?,
                        GuardianComponentRebuildStatus::Failed,
                        vec!["runtime_component_postcondition_failed".to_string()],
                        OperationStepResult::Failed,
                        Some(RUNTIME_COMPONENT_REBUILD_STEP),
                    ),
                };
            (
                terminal,
                status,
                facts,
                RUNTIME_COMPONENT_REBUILD_STEP,
                step_result,
                failure_point,
                rollback,
                Some(ComponentRebuildPublicationLease::RuntimeCommit(receipt)),
            )
        }
        ComponentRebuildTerminal::FailedBeforeEffect { facts, step_id } => (
            admission.failed_terminal().map_err(|_| {
                invalid_component_rebuild_error(
                    std::io::ErrorKind::InvalidData,
                    "runtime component rebuild failure terminal is invalid",
                )
            })?,
            GuardianComponentRebuildStatus::Failed,
            facts,
            step_id,
            OperationStepResult::Failed,
            Some(step_id),
            RollbackState::NotApplicable,
            None,
        ),
        ComponentRebuildTerminal::FailedAfterEffect { receipt, facts } => {
            let rollback = if receipt.quarantine_obligation().is_some() {
                RollbackState::Available
            } else {
                RollbackState::Applied
            };
            let terminal = admission.failed_effect_terminal(&receipt).map_err(|_| {
                invalid_component_rebuild_error(
                    std::io::ErrorKind::InvalidData,
                    "runtime component rebuild effect receipt is invalid or ambiguous",
                )
            })?;
            (
                terminal,
                GuardianComponentRebuildStatus::Failed,
                facts,
                RUNTIME_COMPONENT_REBUILD_STEP,
                OperationStepResult::Failed,
                Some(RUNTIME_COMPONENT_REBUILD_STEP),
                rollback,
                Some(ComponentRebuildPublicationLease::RuntimeFailure(receipt)),
            )
        }
    };
    if !typed_terminal.quarantine_checkpoint().is_empty() {
        record_component_quarantine_checkpoint(&admission).await?;
    }
    persist_component_rebuild_terminal(
        &admission,
        &reservation,
        ComponentRebuildTerminalRecord {
            terminal: typed_terminal,
            step_id,
            step_result,
            failure_point,
            rollback,
            status,
            facts,
            publication_lease,
        },
    )
    .await
}

async fn terminalize_libraries_component_rebuild(
    effect: ManagedLibrariesComponentRebuildEffect,
    terminal: LibrariesComponentRebuildTerminal,
) -> Result<GuardianComponentRebuildOutcome, OperationJournalStoreError> {
    let ManagedLibrariesComponentRebuildEffect {
        completion,
        mutation: _mutation,
        reservation,
        request: _,
        identity: _,
    } = effect;
    let committed = matches!(
        terminal,
        LibrariesComponentRebuildTerminal::Committed { .. }
    );
    let (settlement, facts, rollback) = match terminal {
        LibrariesComponentRebuildTerminal::Committed { receipt, facts } => {
            let settlement = match completion.begin_libraries_commit(receipt).await {
                RegisteredManagedArtifactCommitPostcheck::Verify { pending, verifier } => {
                    pending.settle(verifier.verify().await.ok()).await
                }
                RegisteredManagedArtifactCommitPostcheck::Failed(settlement) => settlement,
            };
            (settlement, facts, RollbackState::NotApplicable)
        }
        LibrariesComponentRebuildTerminal::FailedBeforeEffect { facts } => (
            completion.into_failed_settlement(),
            facts,
            RollbackState::NotApplicable,
        ),
        LibrariesComponentRebuildTerminal::RolledBack { receipt, facts } => {
            let (settlement, applied) = completion.settle_libraries_rollback(receipt).await;
            (
                settlement,
                facts,
                if applied {
                    RollbackState::Applied
                } else {
                    RollbackState::Unavailable
                },
            )
        }
    };
    let facts = if settlement.succeeded() {
        facts
    } else if committed {
        vec!["libraries_component_postcondition_failed".to_string()]
    } else {
        facts
    };
    persist_managed_artifact_component_terminal(
        &settlement,
        reservation,
        facts,
        LIBRARIES_COMPONENT_REBUILD_STEP,
        rollback,
    )
    .await
}

async fn terminalize_version_bundle_component_rebuild(
    effect: ManagedVersionBundleComponentRebuildEffect,
    terminal: VersionBundleComponentRebuildTerminal,
) -> Result<GuardianComponentRebuildOutcome, OperationJournalStoreError> {
    let ManagedVersionBundleComponentRebuildEffect {
        completion,
        mutation: _mutation,
        reservation,
        request: _,
        identity: _,
    } = effect;
    let committed = matches!(
        terminal,
        VersionBundleComponentRebuildTerminal::Committed { .. }
    );
    let (settlement, facts, rollback) = match terminal {
        VersionBundleComponentRebuildTerminal::Committed { receipt, facts } => {
            let settlement = match completion.begin_version_bundle_commit(receipt).await {
                RegisteredManagedArtifactCommitPostcheck::Verify { pending, verifier } => {
                    let proof = verifier.verify().await.ok();
                    #[cfg(test)]
                    if let Some(proof) = proof.as_ref() {
                        let lifetime = proof.lifetime_for_test();
                        let _ = REGISTERED_ARTIFACT_EXACT_PROOF_LIFETIME.try_with(|slot| {
                            *slot.lock().expect("exact-proof observer lock") = Some(lifetime);
                        });
                    }
                    pending.settle(proof).await
                }
                RegisteredManagedArtifactCommitPostcheck::Failed(settlement) => settlement,
            };
            (settlement, facts, RollbackState::NotApplicable)
        }
        VersionBundleComponentRebuildTerminal::FailedBeforeEffect { facts } => (
            completion.into_failed_settlement(),
            facts,
            RollbackState::NotApplicable,
        ),
        VersionBundleComponentRebuildTerminal::RolledBack { receipt, facts } => {
            let (settlement, applied) = completion.settle_version_bundle_rollback(receipt).await;
            (
                settlement,
                facts,
                if applied {
                    RollbackState::Applied
                } else {
                    RollbackState::Unavailable
                },
            )
        }
    };
    let facts = if settlement.succeeded() {
        facts
    } else if committed {
        vec!["version_bundle_component_postcondition_failed".to_string()]
    } else {
        facts
    };
    persist_managed_artifact_component_terminal(
        &settlement,
        reservation,
        facts,
        VERSION_BUNDLE_COMPONENT_REBUILD_STEP,
        rollback,
    )
    .await
}

async fn terminalize_assets_component_rebuild(
    effect: ManagedAssetsComponentRebuildEffect,
    terminal: AssetsComponentRebuildTerminal,
) -> Result<GuardianComponentRebuildOutcome, OperationJournalStoreError> {
    let ManagedAssetsComponentRebuildEffect {
        completion,
        mutation: _mutation,
        reservation,
        request: _,
        identity: _,
    } = effect;
    let committed = matches!(terminal, AssetsComponentRebuildTerminal::Committed { .. });
    let (settlement, facts, rollback) = match terminal {
        AssetsComponentRebuildTerminal::Committed { receipt, facts } => {
            let settlement = match completion.begin_assets_commit(receipt).await {
                RegisteredManagedArtifactCommitPostcheck::Verify { pending, verifier } => {
                    let proof = verifier.verify().await.ok();
                    #[cfg(test)]
                    if let Some(proof) = proof.as_ref() {
                        let lifetime = proof.lifetime_for_test();
                        let _ = REGISTERED_ARTIFACT_EXACT_PROOF_LIFETIME.try_with(|slot| {
                            *slot.lock().expect("exact-proof observer lock") = Some(lifetime);
                        });
                    }
                    pending.settle(proof).await
                }
                RegisteredManagedArtifactCommitPostcheck::Failed(settlement) => settlement,
            };
            (settlement, facts, RollbackState::NotApplicable)
        }
        AssetsComponentRebuildTerminal::FailedBeforeEffect { facts } => (
            completion.into_failed_settlement(),
            facts,
            RollbackState::NotApplicable,
        ),
        AssetsComponentRebuildTerminal::RolledBack { receipt, facts } => {
            let (settlement, applied) = completion.settle_assets_rollback(receipt).await;
            (
                settlement,
                facts,
                if applied {
                    RollbackState::Applied
                } else {
                    RollbackState::Unavailable
                },
            )
        }
    };
    let facts = if settlement.succeeded() {
        facts
    } else if committed {
        vec!["assets_component_postcondition_failed".to_string()]
    } else {
        facts
    };
    persist_managed_artifact_component_terminal(
        &settlement,
        reservation,
        facts,
        ASSETS_COMPONENT_REBUILD_STEP,
        rollback,
    )
    .await
}

async fn persist_managed_artifact_component_terminal(
    settlement: &RegisteredManagedArtifactComponentSettlement,
    reservation: ReconciliationAttemptReservation,
    facts: Vec<String>,
    step_id: &'static str,
    rollback: RollbackState,
) -> Result<GuardianComponentRebuildOutcome, OperationJournalStoreError> {
    let attempt = settlement.attempt();
    let terminal = settlement.terminal().clone();
    let operation_id = attempt.operation_id().clone();
    let succeeded = settlement.succeeded();
    let memory = reconciliation_memory_entry(terminal.clone()).map_err(|_| {
        invalid_component_rebuild_error(
            std::io::ErrorKind::InvalidData,
            "managed artifact component rebuild memory terminal is invalid",
        )
    })?;
    validate_reconciliation_memory(settlement.failure_memory(), &memory, &reservation)
        .map_err(component_rebuild_memory_error)?;
    let step_result = if succeeded {
        OperationStepResult::Completed
    } else {
        OperationStepResult::Failed
    };
    let _journal_persistence_error = record_reconciliation_terminal_reconciled(
        settlement.journals(),
        &operation_id,
        repair_step_with_rollback(
            step_id,
            step_result,
            Some(attempt.target().clone()),
            facts.clone(),
            rollback,
        ),
        (!succeeded).then_some(step_id),
        &terminal,
        None,
    )
    .await?;
    persist_exact_component_rebuild_memory(settlement.failure_memory(), &reservation, &memory)
        .await?;

    Ok(GuardianComponentRebuildOutcome {
        operation_id,
        status: if succeeded {
            GuardianComponentRebuildStatus::Rebuilt
        } else {
            GuardianComponentRebuildStatus::Failed
        },
        facts,
    })
}

struct ComponentRebuildTerminalRecord {
    terminal: ReconciliationTerminal,
    step_id: &'static str,
    step_result: OperationStepResult,
    failure_point: Option<&'static str>,
    rollback: RollbackState,
    status: GuardianComponentRebuildStatus,
    facts: Vec<String>,
    publication_lease: Option<ComponentRebuildPublicationLease>,
}

async fn persist_component_rebuild_terminal(
    admission: &RegisteredComponentRebuildAdmission,
    reservation: &ReconciliationAttemptReservation,
    record: ComponentRebuildTerminalRecord,
) -> Result<GuardianComponentRebuildOutcome, OperationJournalStoreError> {
    let ComponentRebuildTerminalRecord {
        terminal,
        step_id,
        step_result,
        failure_point,
        rollback,
        status,
        facts,
        publication_lease,
    } = record;
    let attempt = admission.attempt();
    let operation_id = attempt.operation_id().clone();
    let memory = reconciliation_memory_entry(terminal.clone()).map_err(|_| {
        invalid_component_rebuild_error(
            std::io::ErrorKind::InvalidData,
            "component rebuild memory terminal is invalid",
        )
    })?;
    validate_reconciliation_memory(admission.failure_memory(), &memory, reservation)
        .map_err(component_rebuild_memory_error)?;

    let _journal_persistence_error = record_reconciliation_terminal_reconciled(
        admission.journals(),
        &operation_id,
        repair_step_with_rollback(
            step_id,
            step_result,
            Some(attempt.target().clone()),
            facts.clone(),
            rollback,
        ),
        failure_point,
        &terminal,
        None,
    )
    .await?;
    persist_exact_component_rebuild_memory(admission.failure_memory(), reservation, &memory)
        .await?;

    if let Some(publication_lease) = publication_lease {
        publication_lease.release();
    }

    Ok(GuardianComponentRebuildOutcome {
        operation_id,
        status,
        facts,
    })
}

async fn persist_exact_component_rebuild_memory(
    failure_memory: &GuardianFailureMemoryStore,
    reservation: &ReconciliationAttemptReservation,
    expected: &GuardianFailureMemoryEntry,
) -> Result<(), OperationJournalStoreError> {
    let mut delay = COMPONENT_MEMORY_RETRY_INITIAL_DELAY;
    loop {
        if failure_memory.get(&expected.key).as_ref() == Some(expected) {
            return Ok(());
        }
        match commit_reconciliation_memory(failure_memory, expected.clone(), reservation).await {
            Ok(()) => {
                if failure_memory.get(&expected.key).as_ref() == Some(expected) {
                    return Ok(());
                }
                return Err(invalid_component_rebuild_error(
                    std::io::ErrorKind::InvalidData,
                    "component rebuild memory commit did not publish the exact terminal",
                ));
            }
            Err(FailureMemoryStoreError::Persistence(_)) => {
                tokio::time::sleep(delay).await;
                delay = delay
                    .saturating_mul(2)
                    .min(COMPONENT_MEMORY_RETRY_MAX_DELAY);
            }
            Err(error) => return Err(component_rebuild_memory_error(error)),
        }
    }
}

fn bounded_fact_ids(facts: impl IntoIterator<Item = String>) -> Vec<String> {
    facts
        .into_iter()
        .filter_map(|fact| sanitize_evidence_token(&fact, RedactionAudience::UserVisible, 96))
        .take(MAX_OPERATION_JOURNAL_STEP_FACTS)
        .collect()
}

fn managed_artifact_mutation_admission_fact(
    error: ManagedArtifactMutationEpochUnavailable,
) -> String {
    match error {
        ManagedArtifactMutationEpochUnavailable::MutationInFlight => {
            "managed_artifact_mutation_in_flight"
        }
        ManagedArtifactMutationEpochUnavailable::EpochChanged => {
            "managed_artifact_mutation_epoch_changed"
        }
        ManagedArtifactMutationEpochUnavailable::Exhausted(_) => {
            "managed_artifact_mutation_epoch_exhausted"
        }
    }
    .to_string()
}

fn component_rebuild_memory_error(error: FailureMemoryStoreError) -> OperationJournalStoreError {
    invalid_component_rebuild_error(
        std::io::ErrorKind::Other,
        format!("component rebuild memory failed: {error}"),
    )
}

fn invalid_component_rebuild_error(
    kind: std::io::ErrorKind,
    message: impl Into<String>,
) -> OperationJournalStoreError {
    OperationJournalStoreError::Persistence(std::io::Error::new(kind, message.into()))
}

#[cfg(test)]
mod tests {
    use super::{
        ASSETS_COMPONENT_REBUILD_STEP, COMPONENT_QUARANTINE_STEP, GuardianComponentRebuildStatus,
        VERSION_BUNDLE_COMPONENT_REBUILD_STEP, bounded_fact_ids,
        execute_managed_assets_component_rebuild_with_driver,
        execute_managed_runtime_component_rebuild,
        execute_managed_version_bundle_component_rebuild,
    };
    use crate::execution::persistence::{AtomicWriteBackend, PersistenceCoordinator};
    use crate::guardian::{DiagnosisId, GuardianDomain};
    use crate::state::contracts::{
        CommandKind, JournalId, OperationId, OperationJournalEntry, OperationJournalStep,
        OperationPhase, OperationStatus, OperationStepResult, OwnershipClass,
        ReconciliationComponent, ReconciliationScope, ReconciliationTerminalOutcome, RollbackState,
        StabilizationSystem, TargetDescriptor, TargetKind,
    };
    use crate::state::failure_memory::GuardianFailureMemoryStore;
    use crate::state::{
        AppState, AppStateInit, InstallStore, MAX_OPERATION_JOURNAL_STEP_FACTS,
        OperationJournalStore, ProducerLease, RegisteredComponentRebuildAdmission, SessionStore,
        commit_reconciliation_memory, component_rebuild_journal, new_instance,
        reconciliation_attempt_key, reconciliation_instance_target, reconciliation_journal_attempt,
        reconciliation_memory_entry, record_reconciliation_journal_failure,
        registered_artifact_target_for_test, reserve_reconciliation_attempt,
    };
    #[cfg(unix)]
    use crate::state::{
        RegisteredManagedArtifactCommitPostcheck,
        RegisteredManagedArtifactComponentEffectAdmission, settle_reconciliation_memory,
    };
    use axial_config::{AppPaths, InstanceRegistrySnapshot};
    use axial_minecraft::known_good::{
        KnownGoodArtifactKind, KnownGoodInventory, TestKnownGoodEntry, TestKnownGoodIntegrity,
        TestKnownGoodRoot,
    };
    use axial_minecraft::{ManagedRuntimeCache, RuntimeId};
    use sha1::{Digest as _, Sha1};
    use std::fs;
    use std::io;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

    const INSTANCE_ID: &str = "0000000000000001";
    const RUNTIME_COMPONENT: &str = "java-runtime-gamma";
    const DIAGNOSIS_ID: DiagnosisId = DiagnosisId::LauncherManagedArtifactCorrupt;

    fn test_component_owner(state: &AppState) -> ProducerLease {
        state
            .try_claim_producer()
            .expect("claim component rebuild test owner")
    }

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
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                while self.attempts.load(Ordering::SeqCst) < expected {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("component rebuild persistence attempt");
        }

        async fn wait_for_gate_armed(&self) -> usize {
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                loop {
                    let attempt = self.gated_attempt.load(Ordering::SeqCst);
                    if attempt != 0 {
                        return attempt;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("component rebuild persistence retry gate")
        }
    }

    impl AtomicWriteBackend for ControlledWriteBackend {
        fn write(
            &self,
            destination: &crate::execution::anchored_record::AnchoredRecordTarget,
            effects: &axial_fs::EffectOwner,
            contents: &[u8],
        ) -> io::Result<()> {
            let attempt = self.attempts.fetch_add(1, Ordering::SeqCst) + 1;
            if self.gated_attempt.load(Ordering::SeqCst) == attempt {
                while !self.release_gate.load(Ordering::SeqCst) {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
            }
            if self.failed_attempt.load(Ordering::SeqCst) == attempt {
                return Err(io::Error::other(
                    "injected component rebuild persistence failure",
                ));
            }
            destination.write(effects, contents)
        }
    }

    struct Fixture {
        state: AppState,
        journals: Arc<OperationJournalStore>,
        failure_memory: Arc<GuardianFailureMemoryStore>,
        root: PathBuf,
    }

    fn fixture(label: &str) -> Fixture {
        fixture_with_backends(label, None, None)
    }

    fn fixture_with_backends(
        label: &str,
        journal_backend: Option<Arc<ControlledWriteBackend>>,
        memory_backend: Option<Arc<ControlledWriteBackend>>,
    ) -> Fixture {
        fixture_with_backends_and_version(label, journal_backend, memory_backend, "1.21.1")
    }

    fn fixture_with_backends_and_version(
        label: &str,
        journal_backend: Option<Arc<ControlledWriteBackend>>,
        memory_backend: Option<Arc<ControlledWriteBackend>>,
        version_id: &str,
    ) -> Fixture {
        static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(1);

        let root = std::env::temp_dir().join(format!(
            "axial-component-rebuild-{label}-{}-{}",
            std::process::id(),
            NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&root);
        let paths = AppPaths::from_root(root.to_path_buf()).expect("absolute test app root");
        fs::create_dir_all(paths.instances_dir().join(INSTANCE_ID)).expect("instance root");
        fs::create_dir_all(paths.library_dir()).expect("library root");
        let root_session = crate::state::test_root_session(&paths);
        let config = Arc::new(
            axial_config::ConfigStore::load_from(paths.clone(), Arc::clone(&root_session))
                .expect("test config store"),
        );
        let instances = Arc::new(
            axial_config::InstanceStore::from_snapshot(
                paths.clone(),
                root_session,
                InstanceRegistrySnapshot::new(
                    vec![new_instance(
                        INSTANCE_ID.to_string(),
                        "Component Rebuild Test".to_string(),
                        version_id.to_string(),
                        String::new(),
                        String::new(),
                    )],
                    INSTANCE_ID.to_string(),
                    Vec::new(),
                )
                .expect("instance registry snapshot"),
            )
            .expect("test instance store"),
        );
        let journals = Arc::new(match journal_backend {
            Some(backend) => OperationJournalStore::try_load_from_paths_with_coordinator(
                &paths,
                PersistenceCoordinator::for_test(
                    backend,
                    std::time::Duration::from_millis(1),
                    std::time::Duration::from_millis(5),
                ),
            )
            .expect("persistent component rebuild journals"),
            None => OperationJournalStore::new(),
        });
        let failure_memory = Arc::new(match memory_backend {
            Some(backend) => GuardianFailureMemoryStore::try_load_from_paths_with_coordinator(
                &paths,
                PersistenceCoordinator::for_test(
                    backend,
                    std::time::Duration::from_millis(1),
                    std::time::Duration::from_millis(5),
                ),
            )
            .expect("persistent component rebuild memory"),
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
                axial_performance::PerformanceManager::load_for_startup(paths.performance_dir())
                    .expect("test performance state"),
            ),
            startup_warnings: Vec::new(),
        })
        .with_reconciliation_stores(journals.clone(), failure_memory.clone());
        state.set_library_dir_for_test(paths.library_dir().to_string_lossy().into_owned());
        state.activate_known_good_inventory_for_test(
            INSTANCE_ID,
            KnownGoodInventory::from_test_entries(Vec::<TestKnownGoodEntry>::new())
                .expect("empty component rebuild inventory"),
        );
        Fixture {
            state,
            journals,
            failure_memory,
            root,
        }
    }

    fn assets_fixture_inventory() -> KnownGoodInventory {
        const OBJECT_BYTES: &[u8] = b"axial managed Assets fixture";
        let object_digest = format!("{:x}", Sha1::digest(OBJECT_BYTES));
        let empty_digest = format!("{:x}", Sha1::digest([]));
        let index_bytes = serde_json::to_vec(&serde_json::json!({
            "objects": {
                "fixture/object": {
                    "hash": object_digest.as_str(),
                    "size": OBJECT_BYTES.len()
                },
                "fixture/empty": {
                    "hash": empty_digest.as_str(),
                    "size": 0
                }
            }
        }))
        .expect("Assets fixture index");
        KnownGoodInventory::from_test_entries([
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
                    size: OBJECT_BYTES.len() as u64,
                },
            },
            TestKnownGoodEntry {
                root: TestKnownGoodRoot::Assets,
                path: format!("objects/{}/{}", &empty_digest[..2], empty_digest),
                kind: KnownGoodArtifactKind::AssetObject,
                integrity: TestKnownGoodIntegrity::Sha1 {
                    digest: empty_digest,
                    size: 0,
                },
            },
        ])
        .expect("Assets fixture inventory")
        .with_test_standalone_leaf_repair_source(0, "https://example.invalid/fixture-assets.json")
        .expect("Assets fixture index source")
        .with_test_standalone_leaf_repair_source(
            1,
            &format!(
                "https://resources.download.minecraft.net/{}/{}",
                &object_digest[..2],
                object_digest
            ),
        )
        .expect("Assets fixture object source")
    }

    fn activate_assets_fixture_inventory(fixture: &Fixture) -> TargetDescriptor {
        let inventory = assets_fixture_inventory();
        let instance = fixture
            .state
            .instances()
            .get(INSTANCE_ID)
            .expect("registered Assets fixture instance");
        let library_dir = fixture
            .state
            .library_dir()
            .expect("configured Assets fixture library root");
        let library_root = fs::canonicalize(library_dir).expect("canonical Assets library root");
        fs::create_dir_all(fixture.state.managed_runtime_cache().root())
            .expect("Assets fixture runtime root");
        let runtime_root = fs::canonicalize(fixture.state.managed_runtime_cache().root())
            .expect("canonical Assets runtime root");
        let target = registered_artifact_target_for_test(
            &instance.id,
            &instance.version_id,
            &instance.created_at,
            &library_root,
            &runtime_root,
            &inventory,
            1,
        )
        .expect("exact source-backed Assets target");
        fixture
            .state
            .activate_known_good_inventory_for_test(INSTANCE_ID, inventory);
        target
    }

    fn activate_version_bundle_fixture_inventory(fixture: &Fixture) -> TargetDescriptor {
        const CLIENT_BYTES: &[u8] = b"axial managed VersionBundle client fixture";
        const LOG_BYTES: &[u8] = b"<Configuration/>";
        let instance = fixture
            .state
            .instances()
            .get(INSTANCE_ID)
            .expect("registered VersionBundle fixture instance");
        let version_json = serde_json::to_vec(&serde_json::json!({
            "id": instance.version_id.as_str(),
            "type": "release",
            "mainClass": "org.axial.GuardianFixture"
        }))
        .expect("VersionBundle fixture metadata");
        let version_id = instance.version_id.as_str();
        let inventory = KnownGoodInventory::from_test_entries([
            TestKnownGoodEntry {
                root: TestKnownGoodRoot::Versions,
                path: format!("{version_id}/{version_id}.json"),
                kind: KnownGoodArtifactKind::VersionMetadata,
                integrity: TestKnownGoodIntegrity::Sha1 {
                    digest: format!("{:x}", Sha1::digest(&version_json)),
                    size: version_json.len() as u64,
                },
            },
            TestKnownGoodEntry {
                root: TestKnownGoodRoot::Versions,
                path: format!("{version_id}/{version_id}.jar"),
                kind: KnownGoodArtifactKind::ClientJar,
                integrity: TestKnownGoodIntegrity::Sha1 {
                    digest: format!("{:x}", Sha1::digest(CLIENT_BYTES)),
                    size: CLIENT_BYTES.len() as u64,
                },
            },
            TestKnownGoodEntry {
                root: TestKnownGoodRoot::Assets,
                path: "log_configs/guardian-version-bundle.xml".to_string(),
                kind: KnownGoodArtifactKind::LogConfig,
                integrity: TestKnownGoodIntegrity::Sha1 {
                    digest: format!("{:x}", Sha1::digest(LOG_BYTES)),
                    size: LOG_BYTES.len() as u64,
                },
            },
        ])
        .expect("VersionBundle fixture inventory");
        let library_dir = fixture
            .state
            .library_dir()
            .expect("configured VersionBundle fixture library root");
        let library_root =
            fs::canonicalize(library_dir).expect("canonical VersionBundle fixture library root");
        fs::create_dir_all(fixture.state.managed_runtime_cache().root())
            .expect("VersionBundle fixture runtime root");
        let runtime_root = fs::canonicalize(fixture.state.managed_runtime_cache().root())
            .expect("canonical VersionBundle fixture runtime root");
        let target = registered_artifact_target_for_test(
            &instance.id,
            &instance.version_id,
            &instance.created_at,
            &library_root,
            &runtime_root,
            &inventory,
            1,
        )
        .expect("exact VersionBundle client target");
        fixture
            .state
            .activate_known_good_inventory_for_test(INSTANCE_ID, inventory);
        target
    }

    async fn cleanup(fixture: Fixture) {
        fixture
            .state
            .close_known_good_inventories()
            .await
            .expect("close known-good stores");
        fixture
            .state
            .close_instance_registry()
            .await
            .expect("close instance registry");
        fixture
            .journals
            .close()
            .await
            .expect("close component rebuild journals");
        fixture
            .failure_memory
            .close()
            .await
            .expect("close component rebuild memory");
        let Fixture {
            state,
            journals,
            failure_memory,
            root,
        } = fixture;
        drop((state, journals, failure_memory));
        let _ = fs::remove_dir_all(root);
    }

    fn runtime_target() -> TargetDescriptor {
        TargetDescriptor::new(
            StabilizationSystem::Execution,
            TargetKind::Runtime,
            RUNTIME_COMPONENT,
            OwnershipClass::LauncherManaged,
        )
    }

    fn artifact_repair_plan(
        attempt: &crate::state::contracts::ReconciliationAttempt,
    ) -> OperationJournalEntry {
        let mut entry = OperationJournalEntry::new(
            JournalId::new(format!("journal-{}", attempt.operation_id())),
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
        let mut step = OperationJournalStep::new("repair_runtime", OperationPhase::Repairing);
        step.result = OperationStepResult::Planned;
        step.changed_target = Some(attempt.target().clone());
        entry.planned_steps.push(step);
        entry.guardian_diagnosis_ids.push(attempt.diagnosis_id());
        reconciliation_journal_attempt(entry, attempt.clone())
    }

    fn artifact_repair_failed_step(target: &TargetDescriptor) -> OperationJournalStep {
        let mut step = OperationJournalStep::new("repair_runtime", OperationPhase::Repairing);
        step.result = OperationStepResult::Failed;
        step.changed_target = Some(target.clone());
        step.rollback = RollbackState::Available;
        step
    }

    async fn component_admission(
        fixture: &Fixture,
        operation_suffix: &str,
    ) -> (RegisteredComponentRebuildAdmission, OperationId) {
        let lifecycle = fixture.state.acquire_instance_lifecycle(INSTANCE_ID).await;
        let authority = fixture
            .state
            .registered_reconciliation_authority(&lifecycle)
            .expect("registered reconciliation authority");
        let artifact_operation = OperationId::deterministic_test(format!("artifact-{operation_suffix}"));
        let attempt = authority
            .repair_artifact_attempt(
                artifact_operation.clone(),
                DIAGNOSIS_ID,
                GuardianDomain::Runtime,
                ReconciliationComponent::Runtime,
                runtime_target(),
                chrono::Duration::minutes(30),
            )
            .expect("runtime artifact attempt");
        let terminal = authority
            .terminal(attempt.clone(), ReconciliationTerminalOutcome::Failed)
            .expect("runtime artifact terminal");
        let reservation = reserve_reconciliation_attempt(
            fixture.failure_memory.as_ref(),
            fixture.journals.as_ref(),
            reconciliation_attempt_key(&attempt),
        )
        .expect("artifact attempt reservation");
        fixture
            .journals
            .create(artifact_repair_plan(&attempt))
            .await
            .expect("artifact repair plan");
        record_reconciliation_journal_failure(
            fixture.journals.as_ref(),
            &artifact_operation,
            artifact_repair_failed_step(attempt.target()),
            "repair_runtime",
            terminal.clone(),
        )
        .await
        .expect("artifact repair failure");
        commit_reconciliation_memory(
            fixture.failure_memory.as_ref(),
            reconciliation_memory_entry(terminal).expect("artifact failure memory"),
            &reservation,
        )
        .await
        .expect("artifact failure memory commit");
        drop((reservation, authority));

        let evidence = fixture
            .state
            .recorded_runtime_artifact_repair_failure(&lifecycle, &artifact_operation)
            .expect("exact artifact failure proof");
        let rebuild_operation = OperationId::deterministic_test(format!("component-{operation_suffix}"));
        let admission = fixture
            .state
            .admit_runtime_component_rebuild(
                evidence,
                rebuild_operation,
                chrono::Duration::minutes(30),
            )
            .await
            .expect("component rebuild admission");
        drop(lifecycle);
        (admission, artifact_operation)
    }

    async fn assets_component_admission(
        fixture: &Fixture,
        operation_suffix: &str,
    ) -> RegisteredComponentRebuildAdmission {
        let target = activate_assets_fixture_inventory(fixture);
        managed_artifact_component_admission(
            fixture,
            operation_suffix,
            target,
            GuardianDomain::Download,
            ReconciliationComponent::Assets,
        )
        .await
    }

    async fn version_bundle_component_admission(
        fixture: &Fixture,
        operation_suffix: &str,
    ) -> RegisteredComponentRebuildAdmission {
        let target = activate_version_bundle_fixture_inventory(fixture);
        managed_artifact_component_admission(
            fixture,
            operation_suffix,
            target,
            GuardianDomain::Launch,
            ReconciliationComponent::VersionBundle,
        )
        .await
    }

    async fn managed_artifact_component_admission(
        fixture: &Fixture,
        operation_suffix: &str,
        target: TargetDescriptor,
        domain: GuardianDomain,
        component: ReconciliationComponent,
    ) -> RegisteredComponentRebuildAdmission {
        let lifecycle = fixture.state.acquire_instance_lifecycle(INSTANCE_ID).await;
        let foreground = fixture
            .state
            .register_integrity_foreground()
            .expect("register managed artifact predecessor foreground")
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
                        .expect("managed artifact predecessor library root"),
                ),
            )
            .expect("mint managed artifact predecessor verification");
        let authority = fixture
            .state
            .registered_reconciliation_authority(&lifecycle)
            .expect("registered managed artifact reconciliation authority");
        let artifact_operation = OperationId::deterministic_test(format!("artifact-{operation_suffix}"));
        let attempt = authority
            .repair_artifact_attempt(
                artifact_operation.clone(),
                DIAGNOSIS_ID,
                domain,
                component,
                target,
                chrono::Duration::minutes(30),
            )
            .expect("managed artifact attempt");
        let terminal = authority
            .artifact_terminal(
                attempt.clone(),
                ReconciliationTerminalOutcome::Failed,
                Default::default(),
            )
            .expect("managed artifact terminal");
        let reservation = reserve_reconciliation_attempt(
            fixture.failure_memory.as_ref(),
            fixture.journals.as_ref(),
            reconciliation_attempt_key(&attempt),
        )
        .expect("managed artifact attempt reservation");
        fixture
            .journals
            .create(artifact_repair_plan(&attempt))
            .await
            .expect("managed artifact repair plan");
        record_reconciliation_journal_failure(
            fixture.journals.as_ref(),
            &artifact_operation,
            artifact_repair_failed_step(attempt.target()),
            "repair_managed_artifact",
            terminal.clone(),
        )
        .await
        .expect("managed artifact repair failure");
        commit_reconciliation_memory(
            fixture.failure_memory.as_ref(),
            reconciliation_memory_entry(terminal).expect("managed artifact failure memory"),
            &reservation,
        )
        .await
        .expect("managed artifact failure memory commit");
        drop((reservation, authority));

        let continuation = fixture
            .state
            .recorded_verified_registered_artifact_failure_for_test(verification, &attempt)
            .expect("closed verified managed artifact component predecessor proof");
        let admission = fixture
            .state
            .admit_registered_artifact_component_rebuild(
                continuation,
                OperationId::deterministic_test(format!("component-{operation_suffix}")),
                chrono::Duration::minutes(30),
            )
            .await
            .expect("managed artifact component rebuild admission");
        drop((foreground, lifecycle));
        admission
    }

    async fn component_readmission_is_refused(
        fixture: &Fixture,
        artifact_operation: &OperationId,
        operation_suffix: &str,
    ) -> bool {
        let lifecycle = fixture.state.acquire_instance_lifecycle(INSTANCE_ID).await;
        let evidence = fixture
            .state
            .recorded_runtime_artifact_repair_failure(&lifecycle, artifact_operation)
            .expect("artifact failure remains exact");
        let refused = fixture
            .state
            .admit_runtime_component_rebuild(
                evidence,
                OperationId::deterministic_test(format!("component-{operation_suffix}")),
                chrono::Duration::minutes(30),
            )
            .await
            .is_err();
        drop(lifecycle);
        refused
    }

    async fn wait_for_component_owner_settlement(fixture: &Fixture, operation_id: &OperationId) {
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if fixture
                    .journals
                    .get(operation_id)
                    .is_some_and(|journal| journal.reconciliation_terminal().is_some())
                    && fixture
                        .state
                        .managed_artifact_mutation_epoch_is_capturable_for_test()
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("detached component owner settlement");
    }

    async fn assert_receipt_is_retained_until_persistence_retry(
        fixture: &Fixture,
        backend: Arc<ControlledWriteBackend>,
        operation_suffix: &str,
        terminal_visible_while_retrying: bool,
    ) {
        let (admission, _) = component_admission(fixture, operation_suffix).await;
        let operation_id = admission.attempt().operation_id().clone();
        let memory_key = reconciliation_attempt_key(admission.attempt());
        let runtime_cache = fixture.state.managed_runtime_cache().clone();
        let expected_effect_cache = runtime_cache.clone();
        let effect_backend = backend.clone();
        let rebuild = execute_managed_runtime_component_rebuild(
            test_component_owner(&fixture.state),
            admission,
            move |effect| {
                let (effect_cache, component) = effect.core_request();
                assert!(
                    effect_cache.shares_identity_with(&expected_effect_cache),
                    "Guardian effect must retain the exact State-owned Runtime cache"
                );
                async move {
                    let receipt = axial_minecraft::rebuild_managed_runtime_fixture_for_test(
                        &effect_cache,
                        component,
                    )
                    .await
                    .expect("sealed managed Runtime fixture receipt");
                    let failed_attempt = effect_backend.next_attempt();
                    effect_backend.fail_attempt(failed_attempt);
                    effect_backend.gate_attempt(failed_attempt + 1);
                    effect.succeeded(receipt, vec!["runtime_component_rebuilt".to_string()])
                }
            },
        );
        let settlement_complete = Arc::new(AtomicBool::new(false));
        let rebuild_complete = settlement_complete.clone();
        let rebuild = async move {
            let outcome = rebuild.await;
            rebuild_complete.store(true, Ordering::Release);
            outcome
        };
        let control = async {
            let gated_attempt = backend.wait_for_gate_armed().await;
            backend.wait_for_attempt(gated_attempt).await;
            assert!(
                !settlement_complete.load(Ordering::Acquire),
                "component rebuild future must remain pending during persistence retry"
            );
            assert_eq!(
                fixture
                    .journals
                    .get(&operation_id)
                    .and_then(|entry| entry.reconciliation_terminal().cloned())
                    .is_some(),
                terminal_visible_while_retrying
            );
            assert!(fixture.failure_memory.get(&memory_key).is_none());

            let mut competing_rebuild =
                Box::pin(axial_minecraft::rebuild_managed_runtime_fixture_for_test(
                    &runtime_cache,
                    RuntimeId::from(RUNTIME_COMPONENT),
                ));
            assert!(
                tokio::time::timeout(
                    std::time::Duration::from_millis(100),
                    &mut competing_rebuild,
                )
                .await
                .is_err(),
                "publication receipt must retain Runtime exclusion during persistence retry"
            );

            backend.release();
            let competing_receipt =
                tokio::time::timeout(std::time::Duration::from_secs(2), competing_rebuild)
                    .await
                    .expect("competing Runtime rebuild resumes after settlement")
                    .expect("competing Runtime rebuild receipt");
            drop(competing_receipt);
        };
        let (outcome, ()) = tokio::join!(rebuild, control);
        let outcome = outcome.expect("component rebuild settles after persistence retry");
        assert!(settlement_complete.load(Ordering::Acquire));

        assert_eq!(outcome.status, GuardianComponentRebuildStatus::Rebuilt);
        let terminal = fixture
            .journals
            .get(&operation_id)
            .and_then(|entry| entry.reconciliation_terminal().cloned())
            .expect("exact component terminal journal");
        assert_eq!(
            fixture
                .failure_memory
                .get(&memory_key)
                .and_then(|entry| entry.reconciliation_terminal().cloned()),
            Some(terminal)
        );
    }

    #[tokio::test]
    async fn p00_b09_contract_cross_owner_managed_version_bundle_commit_settles_exact_terminal_memory()
     {
        let fixture = fixture("version-bundle-commit");
        let admission = version_bundle_component_admission(&fixture, "version-bundle-commit").await;
        let operation_id = admission.attempt().operation_id().clone();
        let memory_key = reconciliation_attempt_key(admission.attempt());
        let journals = fixture.journals.clone();
        let root = PathBuf::from(fixture.state.library_dir().expect("library root"));
        let expected_root = root.clone();
        let state = fixture.state.clone();

        let outcome = execute_managed_version_bundle_component_rebuild(
            test_component_owner(&fixture.state),
            admission,
            move |effect| {
                let plan = journals
                    .get(&operation_id)
                    .expect("VersionBundle plan is visible before effect");
                assert_eq!(plan.status, OperationStatus::Planned);
                assert!(
                    plan.planned_steps
                        .iter()
                        .all(|step| step.step_id != COMPONENT_QUARANTINE_STEP)
                );
                let (request_root, request_version_id, inventory) = effect.core_request();
                assert_eq!(request_root, expected_root.as_path());
                assert_eq!(request_version_id, "1.21.1");
                assert_eq!(
                    inventory
                        .managed_component_projection(
                            axial_minecraft::known_good::ManagedKnownGoodComponent::VersionBundle,
                        )
                        .expect("admitted VersionBundle projection")
                        .entry_count(),
                    3
                );
                async move {
                    assert!(
                        !state.instance_lifecycle_is_held(INSTANCE_ID).await,
                        "managed VersionBundle Core I/O must not retain the old lifecycle"
                    );
                    let receipt = axial_minecraft::rebuild_managed_version_bundle_fixture_for_test(
                        expected_root,
                        "1.21.1",
                    )
                    .await
                    .expect("sealed VersionBundle fixture receipt");
                    effect.committed(receipt, ["version_bundle_component_rebuilt".to_string()])
                }
            },
        )
        .await
        .expect("VersionBundle rebuild terminal settlement");

        assert_eq!(outcome.status, GuardianComponentRebuildStatus::Rebuilt);
        assert_eq!(outcome.facts, vec!["version_bundle_component_rebuilt"]);
        let journal = fixture
            .journals
            .get(&outcome.operation_id)
            .expect("VersionBundle component terminal journal");
        assert_eq!(journal.status, OperationStatus::Succeeded);
        let terminal = journal
            .reconciliation_terminal()
            .expect("typed VersionBundle component terminal");
        assert_eq!(terminal.outcome(), ReconciliationTerminalOutcome::Succeeded);
        assert!(terminal.quarantine_checkpoint().is_empty());
        assert_eq!(
            fixture
                .failure_memory
                .get(&memory_key)
                .and_then(|entry| entry.reconciliation_terminal().cloned()),
            Some(terminal.clone())
        );

        cleanup(fixture).await;
    }

    #[tokio::test]
    async fn managed_version_bundle_commit_rejects_an_intervening_artifact_epoch() {
        let fixture = fixture("version-bundle-intervening-epoch");
        let mut admission =
            version_bundle_component_admission(&fixture, "version-bundle-intervening-epoch").await;
        admission.bind_managed_artifact_epoch_for_test();
        let root = PathBuf::from(fixture.state.library_dir().expect("library root"));
        let state = fixture.state.clone();

        let outcome = execute_managed_version_bundle_component_rebuild(
            test_component_owner(&fixture.state),
            admission,
            move |effect| async move {
                let receipt = axial_minecraft::rebuild_managed_version_bundle_fixture_for_test(
                    root, "1.21.1",
                )
                .await
                .expect("sealed VersionBundle fixture receipt");
                drop(
                    state
                        .admit_managed_artifact_mutation()
                        .expect("intervening managed artifact writer"),
                );
                effect.committed(receipt, ["version_bundle_component_rebuilt".to_string()])
            },
        )
        .await
        .expect("stale VersionBundle rebuild settles durably");

        assert_eq!(outcome.status, GuardianComponentRebuildStatus::Failed);
        assert_eq!(
            outcome.facts,
            vec!["version_bundle_component_postcondition_failed"]
        );
        cleanup(fixture).await;
    }

    #[tokio::test]
    async fn managed_version_bundle_effect_rollback_is_failed_and_applied() {
        const VERSION_ID: &str = "guardian-version-bundle-rollback";
        let fixture =
            fixture_with_backends_and_version("version-bundle-rollback", None, None, VERSION_ID);
        let admission =
            version_bundle_component_admission(&fixture, "version-bundle-rollback").await;
        let memory_key = reconciliation_attempt_key(admission.attempt());
        let operation_id = admission.attempt().operation_id().clone();
        let journals = fixture.journals.clone();
        let root = PathBuf::from(fixture.state.library_dir().expect("library root"));
        let outcome = execute_managed_version_bundle_component_rebuild(
            test_component_owner(&fixture.state),
            admission,
            move |effect| async move {
                let plan = journals
                    .get(&operation_id)
                    .expect("VersionBundle rollback plan is visible before Core mutation");
                assert_eq!(plan.status, OperationStatus::Planned);
                let (request_root, request_version_id, inventory) = effect.core_request();
                assert_eq!(request_root, root.as_path());
                assert_eq!(request_version_id, VERSION_ID);
                assert_eq!(
                    inventory
                        .managed_component_projection(
                            axial_minecraft::known_good::ManagedKnownGoodComponent::VersionBundle,
                        )
                        .expect("admitted VersionBundle projection")
                        .entry_count(),
                    3
                );
                let rollback_receipt =
                    match axial_minecraft::rebuild_managed_version_bundle_rollback_fixture_for_test(
                        root, VERSION_ID,
                    )
                    .await
                    {
                        Err(axial_minecraft::ManagedVersionBundleRebuildError::RolledBack(
                            receipt,
                        )) => receipt,
                        Err(error) => {
                            panic!("VersionBundle effect did not reach rollback: {error}")
                        }
                        Ok(receipt) => {
                            drop(receipt);
                            panic!("injected VersionBundle rollback unexpectedly committed")
                        }
                    };
                effect.rolled_back(
                    rollback_receipt,
                    ["version_bundle_component_rolled_back".to_string()],
                )
            },
        )
        .await
        .expect("VersionBundle rollback terminal settlement");

        assert_eq!(outcome.status, GuardianComponentRebuildStatus::Failed);
        let journal = fixture
            .journals
            .get(&outcome.operation_id)
            .expect("VersionBundle rollback journal");
        let step = journal
            .completed_steps
            .iter()
            .find(|step| step.step_id == VERSION_BUNDLE_COMPONENT_REBUILD_STEP)
            .expect("VersionBundle rollback step");
        assert_eq!(step.result, OperationStepResult::Failed);
        assert_eq!(step.rollback, RollbackState::Applied);
        let terminal = journal
            .reconciliation_terminal()
            .expect("VersionBundle rollback terminal");
        assert_eq!(terminal.outcome(), ReconciliationTerminalOutcome::Failed);
        assert!(terminal.quarantine_checkpoint().is_empty());
        assert_eq!(
            fixture
                .failure_memory
                .get(&memory_key)
                .and_then(|entry| entry.reconciliation_terminal().cloned()),
            Some(terminal.clone())
        );

        cleanup(fixture).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn selected_version_bundle_leaf_replacement_is_a_durable_postcheck_failure() {
        let fixture = fixture("version-bundle-selected-postcheck-corrupt");
        let admission = version_bundle_component_admission(
            &fixture,
            "version-bundle-selected-postcheck-corrupt",
        )
        .await;
        let operation_id = admission.attempt().operation_id().clone();
        let memory_key = reconciliation_attempt_key(admission.attempt());
        settle_reconciliation_memory(admission.failure_memory())
            .await
            .expect("settle prior VersionBundle memory persistence");
        let reservation = reserve_reconciliation_attempt(
            admission.failure_memory(),
            admission.journals(),
            reconciliation_attempt_key(admission.attempt()),
        )
        .expect("reserve selected VersionBundle component attempt");
        assert!(
            super::create_component_rebuild_plan(&admission)
                .await
                .expect("create selected VersionBundle component plan")
                .is_none()
        );
        let completion = match admission.into_version_bundle_effect() {
            RegisteredManagedArtifactComponentEffectAdmission::Admitted { completion, .. } => {
                *completion
            }
            RegisteredManagedArtifactComponentEffectAdmission::Refused(_) => {
                panic!("selected VersionBundle component effect must remain admitted")
            }
        };
        let root = PathBuf::from(fixture.state.library_dir().expect("library root"));
        let receipt =
            axial_minecraft::rebuild_managed_version_bundle_fixture_for_test(&root, "1.21.1")
                .await
                .expect("sealed VersionBundle fixture receipt");
        let selected = root.join("versions/1.21.1/1.21.1.jar");
        let postcheck = completion.begin_version_bundle_commit(receipt).await;
        let settlement = match postcheck {
            RegisteredManagedArtifactCommitPostcheck::Verify { pending, verifier } => {
                let proof = verifier
                    .verify()
                    .await
                    .expect("fresh selected VersionBundle leaf proof");
                let replacement = selected.with_extension("replacement");
                fs::write(
                    &replacement,
                    vec![b'x'; b"axial managed VersionBundle client fixture".len()],
                )
                .expect("write VersionBundle replacement");
                fs::rename(replacement, selected)
                    .expect("replace VersionBundle leaf after exact verification");
                pending.settle(Some(proof)).await
            }
            RegisteredManagedArtifactCommitPostcheck::Failed(_) => {
                panic!("typed VersionBundle receipt must reach selected-leaf verifier")
            }
        };
        let outcome = super::persist_managed_artifact_component_terminal(
            &settlement,
            reservation,
            vec!["version_bundle_component_postcondition_failed".to_string()],
            VERSION_BUNDLE_COMPONENT_REBUILD_STEP,
            RollbackState::NotApplicable,
        )
        .await
        .expect("persist selected VersionBundle postcheck failure");

        assert_eq!(outcome.status, GuardianComponentRebuildStatus::Failed);
        let terminal = fixture
            .journals
            .get(&operation_id)
            .and_then(|entry| entry.reconciliation_terminal().cloned())
            .expect("selected VersionBundle failed terminal");
        assert_eq!(terminal.outcome(), ReconciliationTerminalOutcome::Failed);
        assert_eq!(
            fixture
                .failure_memory
                .get(&memory_key)
                .and_then(|entry| entry.reconciliation_terminal().cloned()),
            Some(terminal)
        );

        drop(settlement);
        cleanup(fixture).await;
    }

    #[tokio::test]
    async fn managed_assets_commit_releases_old_lifecycle_and_settles_exact_terminal_memory() {
        let fixture = fixture("assets-commit");
        let admission = assets_component_admission(&fixture, "commit").await;
        let operation_id = admission.attempt().operation_id().clone();
        let memory_key = reconciliation_attempt_key(admission.attempt());
        let journals = fixture.journals.clone();
        let root = PathBuf::from(fixture.state.library_dir().expect("library root"));
        let expected_root = root.clone();
        let state = fixture.state.clone();

        let outcome = execute_managed_assets_component_rebuild_with_driver(
            test_component_owner(&fixture.state),
            admission,
            move |effect| {
                let plan = journals
                    .get(&operation_id)
                    .expect("Assets plan is visible before effect");
                assert_eq!(plan.status, OperationStatus::Planned);
                assert!(
                    plan.planned_steps
                        .iter()
                        .all(|step| step.step_id != COMPONENT_QUARANTINE_STEP)
                );
                assert_eq!(effect.core_request(), (expected_root.as_path(), "1.21.1"));
                async move {
                    assert!(
                        !state.instance_lifecycle_is_held(INSTANCE_ID).await,
                        "managed Assets Core I/O must not retain the old lifecycle"
                    );
                    let receipt = axial_minecraft::rebuild_managed_assets_fixture_for_test(
                        expected_root,
                        "1.21.1",
                    )
                    .await
                    .expect("sealed Assets fixture receipt");
                    effect.committed(receipt, vec!["assets_component_rebuilt".to_string()])
                }
            },
        )
        .await
        .expect("Assets rebuild terminal settlement");

        assert_eq!(outcome.status, GuardianComponentRebuildStatus::Rebuilt);
        assert_eq!(outcome.facts, vec!["assets_component_rebuilt"]);
        let journal = fixture
            .journals
            .get(&outcome.operation_id)
            .expect("Assets component terminal journal");
        assert_eq!(journal.status, OperationStatus::Succeeded);
        assert!(
            journal
                .completed_steps
                .iter()
                .all(|step| step.step_id != COMPONENT_QUARANTINE_STEP)
        );
        let terminal = journal
            .reconciliation_terminal()
            .expect("typed Assets component terminal");
        assert_eq!(terminal.outcome(), ReconciliationTerminalOutcome::Succeeded);
        assert!(terminal.quarantine_checkpoint().is_empty());
        assert_eq!(
            fixture
                .failure_memory
                .get(&memory_key)
                .and_then(|entry| entry.reconciliation_terminal().cloned()),
            Some(terminal.clone())
        );

        cleanup(fixture).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn selected_nonfirst_asset_postcheck_replacement_is_a_durable_failure() {
        const OBJECT_BYTES: &[u8] = b"axial managed Assets fixture";

        let fixture = fixture("assets-selected-postcheck-corrupt");
        let admission = assets_component_admission(&fixture, "selected-postcheck-corrupt").await;
        let operation_id = admission.attempt().operation_id().clone();
        let memory_key = reconciliation_attempt_key(admission.attempt());
        settle_reconciliation_memory(admission.failure_memory())
            .await
            .expect("settle prior Assets memory persistence");
        let reservation = reserve_reconciliation_attempt(
            admission.failure_memory(),
            admission.journals(),
            reconciliation_attempt_key(admission.attempt()),
        )
        .expect("reserve selected Assets component attempt");
        assert!(
            super::create_component_rebuild_plan(&admission)
                .await
                .expect("create selected Assets component plan")
                .is_none()
        );
        let completion = match admission.into_assets_effect() {
            RegisteredManagedArtifactComponentEffectAdmission::Admitted { completion, .. } => {
                *completion
            }
            RegisteredManagedArtifactComponentEffectAdmission::Refused(_) => {
                panic!("selected Assets component effect must remain admitted")
            }
        };
        let root = PathBuf::from(fixture.state.library_dir().expect("library root"));
        let receipt = axial_minecraft::rebuild_managed_assets_fixture_for_test(&root, "1.21.1")
            .await
            .expect("sealed Assets fixture receipt");
        let object_digest = format!("{:x}", Sha1::digest(OBJECT_BYTES));
        let selected = root
            .join("assets/objects")
            .join(&object_digest[..2])
            .join(&object_digest);
        let postcheck = completion.begin_assets_commit(receipt).await;
        let settlement = match postcheck {
            RegisteredManagedArtifactCommitPostcheck::Verify { pending, verifier } => {
                let proof = verifier
                    .verify()
                    .await
                    .expect("fresh selected-leaf proof before replacement");
                let replacement = selected.with_extension("replacement");
                fs::write(&replacement, vec![b'x'; OBJECT_BYTES.len()])
                    .expect("write selected non-first Assets replacement");
                fs::rename(replacement, selected)
                    .expect("replace selected non-first Assets leaf after exact verification");
                pending.settle(Some(proof)).await
            }
            RegisteredManagedArtifactCommitPostcheck::Failed(_) => {
                panic!("typed receipt must reach the selected-leaf verifier")
            }
        };
        let outcome = super::persist_managed_artifact_component_terminal(
            &settlement,
            reservation,
            vec!["assets_component_postcondition_failed".to_string()],
            ASSETS_COMPONENT_REBUILD_STEP,
            RollbackState::NotApplicable,
        )
        .await
        .expect("persist selected Assets postcheck failure");

        assert_eq!(outcome.status, GuardianComponentRebuildStatus::Failed);
        let terminal = fixture
            .journals
            .get(&operation_id)
            .and_then(|entry| entry.reconciliation_terminal().cloned())
            .expect("selected Assets failed terminal");
        assert_eq!(terminal.outcome(), ReconciliationTerminalOutcome::Failed);
        assert_eq!(
            fixture
                .failure_memory
                .get(&memory_key)
                .and_then(|entry| entry.reconciliation_terminal().cloned()),
            Some(terminal)
        );

        drop(settlement);
        cleanup(fixture).await;
    }

    #[tokio::test]
    async fn selected_asset_postcheck_lifecycle_contention_fails_without_waiting() {
        let fixture = fixture("assets-postcheck-lifecycle-contention");
        let admission = assets_component_admission(&fixture, "lifecycle-contention").await;
        let state = fixture.state.clone();
        let root = PathBuf::from(fixture.state.library_dir().expect("library root"));
        let held_lifecycle = Arc::new(tokio::sync::Mutex::new(None));
        let effect_hold = held_lifecycle.clone();
        let (armed_tx, armed_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let rebuild = tokio::spawn(execute_managed_assets_component_rebuild_with_driver(
            test_component_owner(&fixture.state),
            admission,
            move |effect| async move {
                let receipt =
                    axial_minecraft::rebuild_managed_assets_fixture_for_test(&root, "1.21.1")
                        .await
                        .expect("sealed Assets fixture receipt");
                let lifecycle = state.acquire_instance_lifecycle(INSTANCE_ID).await;
                *effect_hold.lock().await = Some(lifecycle);
                let _ = armed_tx.send(());
                release_rx.await.expect("release Assets postcheck");
                effect.committed(receipt, vec!["assets_component_rebuilt".to_string()])
            },
        ));
        tokio::time::timeout(std::time::Duration::from_secs(10), armed_rx)
            .await
            .expect("Assets postcheck fixture setup must finish")
            .expect("Assets postcheck driver must arm");
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            release_tx.send(()).expect("release Assets postcheck");
            rebuild.await.expect("join Assets postcheck rebuild")
        })
        .await
        .expect("postcheck contention must not wait in reverse lock order")
        .expect("postcheck contention settles durably");

        assert_eq!(outcome.status, GuardianComponentRebuildStatus::Failed);
        assert_eq!(
            fixture
                .journals
                .get(&outcome.operation_id)
                .and_then(|entry| entry.reconciliation_terminal().cloned())
                .map(|terminal| terminal.outcome()),
            Some(ReconciliationTerminalOutcome::Failed)
        );
        drop(held_lifecycle.lock().await.take());
        cleanup(fixture).await;
    }

    #[tokio::test]
    async fn managed_assets_preeffect_failure_is_failed_and_not_applicable() {
        let fixture = fixture("assets-preeffect");
        let admission = assets_component_admission(&fixture, "preeffect").await;
        let memory_key = reconciliation_attempt_key(admission.attempt());

        let outcome = execute_managed_assets_component_rebuild_with_driver(
            test_component_owner(&fixture.state),
            admission,
            |effect| async move {
                effect.failed_before_effect(vec!["assets_source_unavailable".to_string()])
            },
        )
        .await
        .expect("Assets preeffect failure settlement");

        assert_eq!(outcome.status, GuardianComponentRebuildStatus::Failed);
        let journal = fixture
            .journals
            .get(&outcome.operation_id)
            .expect("Assets failure journal");
        assert_eq!(journal.status, OperationStatus::Failed);
        let step = journal
            .completed_steps
            .iter()
            .find(|step| step.step_id == ASSETS_COMPONENT_REBUILD_STEP)
            .expect("Assets terminal step");
        assert_eq!(step.result, OperationStepResult::Failed);
        assert_eq!(step.rollback, RollbackState::NotApplicable);
        let terminal = journal
            .reconciliation_terminal()
            .expect("Assets failed terminal");
        assert_eq!(terminal.outcome(), ReconciliationTerminalOutcome::Failed);
        assert!(terminal.quarantine_checkpoint().is_empty());
        assert_eq!(
            fixture
                .failure_memory
                .get(&memory_key)
                .and_then(|entry| entry.reconciliation_terminal().cloned()),
            Some(terminal.clone())
        );

        cleanup(fixture).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn managed_assets_effect_rollback_is_failed_and_applied() {
        use std::os::unix::fs::PermissionsExt;

        const OBJECT_BYTES: &[u8] = b"axial managed Assets fixture";
        let fixture = fixture("assets-rollback");
        let admission = assets_component_admission(&fixture, "rollback").await;
        let memory_key = reconciliation_attempt_key(admission.attempt());
        let operation_id = admission.attempt().operation_id().clone();
        let journals = fixture.journals.clone();
        let root = PathBuf::from(fixture.state.library_dir().expect("library root"));
        let outcome = execute_managed_assets_component_rebuild_with_driver(
            test_component_owner(&fixture.state),
            admission,
            move |effect| async move {
                let plan = journals
                    .get(&operation_id)
                    .expect("Assets rollback plan is visible before Core mutation");
                assert_eq!(plan.status, OperationStatus::Planned);
                assert_eq!(effect.core_request(), (root.as_path(), "1.21.1"));
                let object_digest = format!("{:x}", Sha1::digest(OBJECT_BYTES));
                let empty_digest = format!("{:x}", Sha1::digest([]));
                let protected = [&object_digest[..2], &empty_digest[..2]]
                    .into_iter()
                    .map(|prefix| root.join("assets/objects").join(prefix))
                    .collect::<Vec<_>>();
                for path in &protected {
                    fs::create_dir_all(path).expect("protected Assets object parent");
                    fs::set_permissions(path, fs::Permissions::from_mode(0o500))
                        .expect("deny Assets object publication");
                }
                let rebuild =
                    axial_minecraft::rebuild_managed_assets_fixture_for_test(&root, "1.21.1").await;
                for path in &protected {
                    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
                        .expect("restore Assets object parent");
                }
                let rollback_receipt = match rebuild {
                    Err(axial_minecraft::ManagedAssetsRebuildError::RolledBack(receipt)) => receipt,
                    Err(error) => panic!("Assets effect did not reach rollback: {error}"),
                    Ok(receipt) => {
                        drop(receipt);
                        panic!("Assets permission fault unexpectedly committed")
                    }
                };
                effect.rolled_back(
                    rollback_receipt,
                    vec!["assets_component_rolled_back".to_string()],
                )
            },
        )
        .await
        .expect("Assets rollback terminal settlement");

        assert_eq!(outcome.status, GuardianComponentRebuildStatus::Failed);
        let journal = fixture
            .journals
            .get(&outcome.operation_id)
            .expect("Assets rollback journal");
        let step = journal
            .completed_steps
            .iter()
            .find(|step| step.step_id == ASSETS_COMPONENT_REBUILD_STEP)
            .expect("Assets rollback step");
        assert_eq!(step.result, OperationStepResult::Failed);
        assert_eq!(step.rollback, RollbackState::Applied);
        let terminal = journal
            .reconciliation_terminal()
            .expect("Assets rollback terminal");
        assert_eq!(terminal.outcome(), ReconciliationTerminalOutcome::Failed);
        assert!(terminal.quarantine_checkpoint().is_empty());
        assert_eq!(
            fixture
                .failure_memory
                .get(&memory_key)
                .and_then(|entry| entry.reconciliation_terminal().cloned()),
            Some(terminal.clone())
        );

        cleanup(fixture).await;
    }

    #[tokio::test]
    async fn version_bundle_receipt_and_leaf_proof_are_retained_until_exact_memory_is_durable() {
        let backend = Arc::new(ControlledWriteBackend::default());
        let fixture =
            fixture_with_backends("version-bundle-memory-retry", None, Some(backend.clone()));
        let admission =
            version_bundle_component_admission(&fixture, "version-bundle-memory-retry").await;
        let operation_id = admission.attempt().operation_id().clone();
        let memory_key = reconciliation_attempt_key(admission.attempt());
        let root = PathBuf::from(fixture.state.library_dir().expect("library root"));
        let effect_root = root.clone();
        let effect_backend = backend.clone();
        let proof_lifetime = Arc::new(std::sync::Mutex::new(None));
        let rebuild = super::REGISTERED_ARTIFACT_EXACT_PROOF_LIFETIME.scope(
            proof_lifetime.clone(),
            execute_managed_version_bundle_component_rebuild(
                test_component_owner(&fixture.state),
                admission,
                move |effect| async move {
                    let receipt = axial_minecraft::rebuild_managed_version_bundle_fixture_for_test(
                        effect_root,
                        "1.21.1",
                    )
                    .await
                    .expect("sealed VersionBundle fixture receipt");
                    let failed_attempt = effect_backend.next_attempt();
                    effect_backend.fail_attempt(failed_attempt);
                    effect_backend.gate_attempt(failed_attempt + 1);
                    effect.committed(receipt, ["version_bundle_component_rebuilt".to_string()])
                },
            ),
        );
        let settlement_complete = Arc::new(AtomicBool::new(false));
        let rebuild_complete = settlement_complete.clone();
        let rebuild = async move {
            let outcome = rebuild.await;
            rebuild_complete.store(true, Ordering::Release);
            outcome
        };
        let control = async {
            let gated_attempt = backend.wait_for_gate_armed().await;
            backend.wait_for_attempt(gated_attempt).await;
            assert!(!settlement_complete.load(Ordering::Acquire));
            assert!(
                fixture
                    .journals
                    .get(&operation_id)
                    .and_then(|entry| entry.reconciliation_terminal().cloned())
                    .is_some()
            );
            assert!(fixture.failure_memory.get(&memory_key).is_none());
            assert!(
                proof_lifetime
                    .lock()
                    .expect("exact-proof observation lock")
                    .as_ref()
                    .is_some_and(|proof| proof.upgrade().is_some()),
                "exact VersionBundle leaf proof must remain retained during memory retry"
            );

            let mut competing = Box::pin(
                axial_minecraft::rebuild_managed_version_bundle_fixture_for_test(&root, "1.21.1"),
            );
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(100), &mut competing)
                    .await
                    .is_err(),
                "VersionBundle receipt must retain publication exclusion during memory retry"
            );
            backend.release();
            let competing_receipt =
                tokio::time::timeout(std::time::Duration::from_secs(2), competing)
                    .await
                    .expect("competing VersionBundle rebuild resumes")
                    .expect("competing VersionBundle receipt");
            drop(competing_receipt);
        };
        let (outcome, ()) = tokio::join!(rebuild, control);
        let outcome = outcome.expect("VersionBundle memory retry settles");
        assert_eq!(outcome.status, GuardianComponentRebuildStatus::Rebuilt);
        let terminal = fixture
            .journals
            .get(&operation_id)
            .and_then(|entry| entry.reconciliation_terminal().cloned())
            .expect("exact VersionBundle terminal");
        assert_eq!(
            fixture
                .failure_memory
                .get(&memory_key)
                .and_then(|entry| entry.reconciliation_terminal().cloned()),
            Some(terminal)
        );

        cleanup(fixture).await;
    }

    #[tokio::test]
    async fn assets_receipt_is_retained_until_exact_memory_is_durable() {
        let backend = Arc::new(ControlledWriteBackend::default());
        let fixture = fixture_with_backends("assets-memory-retry", None, Some(backend.clone()));
        let admission = assets_component_admission(&fixture, "memory-retry").await;
        let operation_id = admission.attempt().operation_id().clone();
        let memory_key = reconciliation_attempt_key(admission.attempt());
        let root = PathBuf::from(fixture.state.library_dir().expect("library root"));
        let effect_root = root.clone();
        let effect_backend = backend.clone();
        let proof_lifetime = Arc::new(std::sync::Mutex::new(None));
        let rebuild = super::REGISTERED_ARTIFACT_EXACT_PROOF_LIFETIME.scope(
            proof_lifetime.clone(),
            execute_managed_assets_component_rebuild_with_driver(
                test_component_owner(&fixture.state),
                admission,
                move |effect| async move {
                    let receipt = axial_minecraft::rebuild_managed_assets_fixture_for_test(
                        effect_root,
                        "1.21.1",
                    )
                    .await
                    .expect("sealed Assets fixture receipt");
                    let failed_attempt = effect_backend.next_attempt();
                    effect_backend.fail_attempt(failed_attempt);
                    effect_backend.gate_attempt(failed_attempt + 1);
                    effect.committed(receipt, vec!["assets_component_rebuilt".to_string()])
                },
            ),
        );
        let settlement_complete = Arc::new(AtomicBool::new(false));
        let rebuild_complete = settlement_complete.clone();
        let rebuild = async move {
            let outcome = rebuild.await;
            rebuild_complete.store(true, Ordering::Release);
            outcome
        };
        let control = async {
            let gated_attempt = backend.wait_for_gate_armed().await;
            backend.wait_for_attempt(gated_attempt).await;
            assert!(!settlement_complete.load(Ordering::Acquire));
            assert!(
                fixture
                    .journals
                    .get(&operation_id)
                    .and_then(|entry| entry.reconciliation_terminal().cloned())
                    .is_some()
            );
            assert!(fixture.failure_memory.get(&memory_key).is_none());
            assert!(
                proof_lifetime
                    .lock()
                    .expect("exact-proof observation lock")
                    .as_ref()
                    .is_some_and(|proof| proof.upgrade().is_some()),
                "exact selected-leaf proof must remain retained during exact-memory retry"
            );

            let mut competing = Box::pin(axial_minecraft::rebuild_managed_assets_fixture_for_test(
                &root, "1.21.1",
            ));
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(100), &mut competing)
                    .await
                    .is_err(),
                "Assets receipt must retain publication exclusion during memory retry"
            );
            backend.release();
            let competing_receipt =
                tokio::time::timeout(std::time::Duration::from_secs(2), competing)
                    .await
                    .expect("competing Assets rebuild resumes")
                    .expect("competing Assets receipt");
            drop(competing_receipt);
        };
        let (outcome, ()) = tokio::join!(rebuild, control);
        let outcome = outcome.expect("Assets memory retry settles");
        assert_eq!(outcome.status, GuardianComponentRebuildStatus::Rebuilt);
        let terminal = fixture
            .journals
            .get(&operation_id)
            .and_then(|entry| entry.reconciliation_terminal().cloned())
            .expect("exact Assets terminal");
        assert_eq!(
            fixture
                .failure_memory
                .get(&memory_key)
                .and_then(|entry| entry.reconciliation_terminal().cloned()),
            Some(terminal)
        );

        cleanup(fixture).await;
    }

    #[test]
    fn effect_facts_are_redacted_and_bounded_before_journaling() {
        let mut facts = (0..MAX_OPERATION_JOURNAL_STEP_FACTS + 4)
            .map(|index| format!("runtime_rebuild_fact_{index}"))
            .collect::<Vec<_>>();
        facts.insert(0, "/home/player/private/runtime".to_string());

        let bounded = bounded_fact_ids(facts);

        assert_eq!(bounded.len(), MAX_OPERATION_JOURNAL_STEP_FACTS);
        assert!(bounded.iter().all(|fact| !fact.contains("/home/")));
    }

    #[tokio::test]
    async fn managed_runtime_effect_owns_exact_state_cache_after_durable_plan() {
        let fixture = fixture("managed-failure");
        let (admission, _) = component_admission(&fixture, "managed-failure").await;
        let component_operation = admission.attempt().operation_id().clone();
        let component_key = reconciliation_attempt_key(admission.attempt());
        let journals = fixture.journals.clone();
        let expected_runtime_cache = fixture.state.managed_runtime_cache().clone();
        let foreign_runtime_cache =
            ManagedRuntimeCache::isolated_for_test().expect("foreign cache");

        let outcome = execute_managed_runtime_component_rebuild(
            test_component_owner(&fixture.state),
            admission,
            move |effect| {
                let journal = journals
                    .get(&component_operation)
                    .expect("plan must be visible before effect capability");
                assert_eq!(journal.status, OperationStatus::Planned);
                assert!(journal.reconciliation_terminal().is_none());
                let (runtime_cache, component) = effect.core_request();
                assert!(runtime_cache.shares_identity_with(&expected_runtime_cache));
                assert!(!runtime_cache.shares_identity_with(&foreign_runtime_cache));
                assert_eq!(component.as_str(), RUNTIME_COMPONENT);
                async move { effect.failed_before_effect(vec!["runtime_stage_failed".to_string()]) }
            },
        )
        .await
        .expect("failed effect has truthful Guardian terminal");

        assert_eq!(outcome.status, GuardianComponentRebuildStatus::Failed);
        assert_eq!(outcome.facts, vec!["runtime_stage_failed"]);
        let journal = fixture
            .journals
            .get(&outcome.operation_id)
            .expect("component terminal journal");
        assert_eq!(journal.status, OperationStatus::Failed);
        let terminal = journal
            .reconciliation_terminal()
            .expect("typed component terminal");
        assert_eq!(terminal.outcome(), ReconciliationTerminalOutcome::Failed);
        let memory = fixture
            .failure_memory
            .get(&component_key)
            .expect("component memory is immediate");
        assert_eq!(memory.reconciliation_terminal(), Some(terminal));

        cleanup(fixture).await;
    }

    #[tokio::test]
    async fn managed_runtime_success_rejects_an_intervening_artifact_epoch() {
        let fixture = fixture("runtime-intervening-epoch");
        let (mut admission, _) = component_admission(&fixture, "runtime-intervening-epoch").await;
        admission.bind_managed_artifact_epoch_for_test();
        let state = fixture.state.clone();

        let outcome = execute_managed_runtime_component_rebuild(
            test_component_owner(&fixture.state),
            admission,
            move |effect| {
                let (runtime_cache, component) = effect.core_request();
                async move {
                    let receipt = axial_minecraft::rebuild_managed_runtime_fixture_for_test(
                        &runtime_cache,
                        component,
                    )
                    .await
                    .expect("sealed managed Runtime fixture receipt");
                    drop(
                        state
                            .admit_managed_artifact_mutation()
                            .expect("intervening managed artifact writer"),
                    );
                    effect.succeeded(receipt, ["runtime_component_rebuilt".to_string()])
                }
            },
        )
        .await
        .expect("stale Runtime rebuild settles durably");

        assert_eq!(outcome.status, GuardianComponentRebuildStatus::Failed);
        assert_eq!(
            outcome.facts,
            vec!["runtime_component_postcondition_failed"]
        );
        cleanup(fixture).await;
    }

    #[tokio::test]
    async fn journal_persistence_retry_retains_runtime_receipt_until_terminal_is_durable() {
        let backend = Arc::new(ControlledWriteBackend::default());
        let fixture = fixture_with_backends("journal-retry", Some(backend.clone()), None);

        assert_receipt_is_retained_until_persistence_retry(
            &fixture,
            backend,
            "journal-retry",
            false,
        )
        .await;

        cleanup(fixture).await;
    }

    #[tokio::test]
    async fn memory_persistence_retry_retains_runtime_receipt_after_terminal_is_durable() {
        let backend = Arc::new(ControlledWriteBackend::default());
        let fixture = fixture_with_backends("memory-retry", None, Some(backend.clone()));

        assert_receipt_is_retained_until_persistence_retry(&fixture, backend, "memory-retry", true)
            .await;

        cleanup(fixture).await;
    }

    #[tokio::test]
    async fn ambiguous_planned_replay_refuses_effect_ownership() {
        let fixture = fixture("ambiguous-replay");
        let (admission, _) = component_admission(&fixture, "ambiguous-replay").await;
        let operation_id = admission.attempt().operation_id().clone();
        fixture
            .journals
            .create(component_rebuild_journal(&admission))
            .await
            .expect("interrupted component plan");
        let effect_called = Arc::new(AtomicBool::new(false));
        let effect_called_in_closure = effect_called.clone();

        let error = execute_managed_runtime_component_rebuild(
            test_component_owner(&fixture.state),
            admission,
            move |effect| {
                effect_called_in_closure.store(true, Ordering::Release);
                async move { effect.failed_before_effect(Vec::new()) }
            },
        )
        .await
        .expect_err("ambiguous replay must refuse");

        assert!(!effect_called.load(Ordering::Acquire));
        assert_eq!(
            fixture
                .journals
                .get(&operation_id)
                .expect("interrupted plan retained")
                .status,
            OperationStatus::Planned
        );
        assert!(matches!(
            error,
            crate::state::OperationJournalStoreError::Persistence(ref error)
                if error.kind() == std::io::ErrorKind::WouldBlock
        ));

        cleanup(fixture).await;
    }

    #[tokio::test]
    async fn cancelled_runtime_waiter_retains_mutation_epoch_until_owner_settles() {
        let fixture = fixture("runtime-cancelled-waiter");
        let (admission, _) = component_admission(&fixture, "runtime-cancelled-waiter").await;
        let operation_id = admission.attempt().operation_id().clone();
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let rebuild = tokio::spawn(execute_managed_runtime_component_rebuild(
            test_component_owner(&fixture.state),
            admission,
            move |effect| async move {
                let _ = entered_tx.send(());
                release_rx.await.expect("release Runtime driver");
                effect.failed_before_effect(["runtime_driver_cancelled".to_string()])
            },
        ));
        entered_rx.await.expect("Runtime driver entered");
        assert!(
            !fixture
                .state
                .managed_artifact_mutation_epoch_is_capturable_for_test()
        );

        rebuild.abort();
        assert!(matches!(rebuild.await, Err(error) if error.is_cancelled()));
        assert!(
            !fixture
                .state
                .managed_artifact_mutation_epoch_is_capturable_for_test()
        );
        release_tx.send(()).expect("detached Runtime owner");
        wait_for_component_owner_settlement(&fixture, &operation_id).await;
        cleanup(fixture).await;
    }

    #[tokio::test]
    async fn cancelled_version_bundle_waiter_blocks_shutdown_until_durable_terminal() {
        let fixture = fixture("version-bundle-cancelled-waiter-shutdown");
        let admission = version_bundle_component_admission(
            &fixture,
            "version-bundle-cancelled-waiter-shutdown",
        )
        .await;
        let operation_id = admission.attempt().operation_id().clone();
        let owner = fixture
            .state
            .try_claim_producer()
            .expect("claim VersionBundle rebuild owner");
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let rebuild = tokio::spawn(execute_managed_version_bundle_component_rebuild(
            owner,
            admission,
            move |effect| async move {
                let _ = entered_tx.send(());
                release_rx.await.expect("release VersionBundle driver");
                effect.failed_before_effect(["version_bundle_driver_cancelled".to_string()])
            },
        ));
        entered_rx.await.expect("VersionBundle driver entered");
        assert!(
            !fixture
                .state
                .managed_artifact_mutation_epoch_is_capturable_for_test()
        );

        rebuild.abort();
        assert!(matches!(rebuild.await, Err(error) if error.is_cancelled()));
        assert!(
            fixture
                .journals
                .get(&operation_id)
                .is_some_and(|journal| journal.reconciliation_terminal().is_none())
        );

        let shutdown_state = fixture.state.clone();
        let mut quiesce = tokio::spawn(async move { shutdown_state.quiesce().await });
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while fixture.state.lifecycle_phase()
                != crate::state::AppLifecyclePhase::QuiescingProducers
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("shutdown reaches VersionBundle producer drain");
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), &mut quiesce)
                .await
                .is_err(),
            "shutdown must wait for the detached VersionBundle owner"
        );
        assert!(
            !fixture
                .state
                .managed_artifact_mutation_epoch_is_capturable_for_test()
        );

        release_tx
            .send(())
            .expect("release detached VersionBundle owner");
        quiesce
            .await
            .expect("VersionBundle quiesce task")
            .expect("VersionBundle quiesce completes");
        let terminal = fixture
            .journals
            .get(&operation_id)
            .and_then(|journal| journal.reconciliation_terminal().cloned())
            .expect("cancelled VersionBundle waiter cannot orphan its terminal");
        let memory_key = reconciliation_attempt_key(terminal.attempt());
        assert_eq!(
            fixture
                .failure_memory
                .get(&memory_key)
                .and_then(|entry| entry.reconciliation_terminal().cloned()),
            Some(terminal)
        );
        assert!(
            fixture
                .state
                .managed_artifact_mutation_epoch_is_capturable_for_test()
        );
        cleanup(fixture).await;
    }

    #[tokio::test]
    async fn cancelled_assets_waiter_blocks_shutdown_until_durable_terminal() {
        let fixture = fixture("assets-cancelled-waiter-shutdown");
        let admission =
            assets_component_admission(&fixture, "assets-cancelled-waiter-shutdown").await;
        let operation_id = admission.attempt().operation_id().clone();
        let owner = fixture
            .state
            .try_claim_producer()
            .expect("claim Assets rebuild owner");
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let rebuild = tokio::spawn(execute_managed_assets_component_rebuild_with_driver(
            owner,
            admission,
            move |effect| async move {
                let _ = entered_tx.send(());
                release_rx.await.expect("release Assets driver");
                effect.failed_before_effect(["assets_driver_cancelled".to_string()])
            },
        ));
        entered_rx.await.expect("Assets driver entered");
        assert!(
            !fixture
                .state
                .managed_artifact_mutation_epoch_is_capturable_for_test()
        );

        rebuild.abort();
        assert!(matches!(rebuild.await, Err(error) if error.is_cancelled()));
        assert!(
            fixture
                .journals
                .get(&operation_id)
                .is_some_and(|journal| journal.reconciliation_terminal().is_none())
        );

        let shutdown_state = fixture.state.clone();
        let mut quiesce = tokio::spawn(async move { shutdown_state.quiesce().await });
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while fixture.state.lifecycle_phase()
                != crate::state::AppLifecyclePhase::QuiescingProducers
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("shutdown reaches component producer drain");
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), &mut quiesce)
                .await
                .is_err(),
            "shutdown must wait for the detached component owner"
        );
        assert!(
            !fixture
                .state
                .managed_artifact_mutation_epoch_is_capturable_for_test()
        );

        release_tx.send(()).expect("release detached Assets owner");
        quiesce
            .await
            .expect("component quiesce task")
            .expect("component quiesce completes");
        let terminal = fixture
            .journals
            .get(&operation_id)
            .and_then(|journal| journal.reconciliation_terminal().cloned())
            .expect("cancelled Assets waiter cannot orphan its terminal");
        let memory_key = reconciliation_attempt_key(terminal.attempt());
        assert_eq!(
            fixture
                .failure_memory
                .get(&memory_key)
                .and_then(|entry| entry.reconciliation_terminal().cloned()),
            Some(terminal)
        );
        assert!(
            fixture
                .state
                .managed_artifact_mutation_epoch_is_capturable_for_test()
        );
        cleanup(fixture).await;
    }

    #[tokio::test]
    async fn failed_component_attempt_refuses_readmission_in_the_window() {
        let fixture = fixture("window-gate");
        let (admission, artifact_operation) = component_admission(&fixture, "window-first").await;
        execute_managed_runtime_component_rebuild(
            test_component_owner(&fixture.state),
            admission,
            |effect| async move {
                effect.failed_before_effect(vec!["runtime_stage_failed".to_string()])
            },
        )
        .await
        .expect("first component failure settled");
        assert!(
            component_readmission_is_refused(&fixture, &artifact_operation, "window-second").await,
            "active rung-2 suppression must refuse admission before Guardian effect ownership"
        );

        cleanup(fixture).await;
    }
}
