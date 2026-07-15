use super::reconciliation_journal::{
    GuardianJournalReconciliation, reconcile_guardian_journal_error,
    record_reconciliation_terminal_reconciled, repair_step, repair_step_with_rollback,
};
use super::{GuardianDomain, GuardianMode};
use crate::observability::{RedactionAudience, sanitize_evidence_token};
use crate::state::contracts::{
    CommandKind, JournalId, OperationId, OperationJournalEntry, OperationStatus,
    OperationStepResult, OwnershipClass, ReconciliationComponent, ReconciliationRung,
    ReconciliationScope, ReconciliationTerminal, RollbackState, StabilizationSystem, TargetKind,
};
use crate::state::failure_memory::{FailureMemoryStoreError, GuardianFailureMemoryEntry};
use crate::state::{
    MAX_OPERATION_JOURNAL_STEP_FACTS, OperationJournalStoreError, ReconciliationAttemptReservation,
    RegisteredAssetsComponentRebuildEffect, RegisteredComponentRebuildAdmission,
    RegisteredLibrariesComponentRebuildEffect, commit_reconciliation_memory,
    operation_journal_completed_step_is_visible, operation_journal_plan_is_visible,
    reconciliation_attempt_key, reconciliation_instance_target, reconciliation_journal_attempt,
    reconciliation_memory_entry, reserve_reconciliation_attempt, settle_reconciliation_memory,
    validate_reconciliation_memory,
};
use axial_minecraft::runtime::{
    ManagedRuntimeCommitReceipt, ManagedRuntimeFailureReceipt, RuntimeId,
    is_known_runtime_component,
};
use axial_minecraft::{
    ManagedAssetsCommitReceipt, ManagedAssetsRollbackReceipt, ManagedLibrariesCommitReceipt,
    ManagedLibrariesRollbackReceipt,
};
use std::future::Future;
use std::sync::Arc;

const COMPONENT_REBUILD_START_STEP: &str = "journal_component_rebuild_start";
const COMPONENT_QUARANTINE_STEP: &str = "quarantine_launcher_managed_target";
const RUNTIME_COMPONENT_REBUILD_STEP: &str = "rebuild_managed_runtime_component";
const LIBRARIES_COMPONENT_REBUILD_STEP: &str = "rebuild_managed_libraries_component";
const ASSETS_COMPONENT_REBUILD_STEP: &str = "rebuild_managed_assets_component";
const COMPONENT_MEMORY_RETRY_INITIAL_DELAY: std::time::Duration =
    std::time::Duration::from_millis(20);
const COMPONENT_MEMORY_RETRY_MAX_DELAY: std::time::Duration = std::time::Duration::from_secs(1);

pub(crate) struct ManagedRuntimeComponentRebuildEffect {
    admission: RegisteredComponentRebuildAdmission,
    reservation: ReconciliationAttemptReservation,
    identity: Arc<()>,
}

pub(crate) struct RuntimeComponentRebuildEffectResult {
    inner: RuntimeComponentRebuildEffectResultInner,
}

pub(crate) struct ManagedLibrariesComponentRebuildEffect {
    admission: RegisteredComponentRebuildAdmission,
    reservation: ReconciliationAttemptReservation,
    request: RegisteredLibrariesComponentRebuildEffect,
    identity: Arc<()>,
}

pub(crate) struct LibrariesComponentRebuildEffectResult {
    inner: LibrariesComponentRebuildEffectResultInner,
}

struct ManagedAssetsComponentRebuildEffect {
    admission: RegisteredComponentRebuildAdmission,
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

enum RuntimeComponentRebuildEffectResultInner {
    Succeeded {
        effect: ManagedRuntimeComponentRebuildEffect,
        receipt: ManagedRuntimeCommitReceipt,
        facts: Vec<String>,
    },
    FailedBeforeEffect {
        effect: ManagedRuntimeComponentRebuildEffect,
        facts: Vec<String>,
    },
    FailedAfterEffect {
        effect: ManagedRuntimeComponentRebuildEffect,
        receipt: ManagedRuntimeFailureReceipt,
        facts: Vec<String>,
    },
}

impl ManagedRuntimeComponentRebuildEffect {
    fn new(
        admission: RegisteredComponentRebuildAdmission,
        reservation: ReconciliationAttemptReservation,
    ) -> (Self, Arc<()>) {
        let identity = Arc::new(());
        (
            Self {
                admission,
                reservation,
                identity: identity.clone(),
            },
            identity,
        )
    }

    fn matches_identity(&self, expected: &Arc<()>) -> bool {
        Arc::ptr_eq(&self.identity, expected)
    }

    pub(crate) fn component(&self) -> RuntimeId {
        RuntimeId::from(self.admission.attempt().target().id.clone())
    }

    pub(crate) fn succeeded(
        self,
        receipt: ManagedRuntimeCommitReceipt,
        facts: impl IntoIterator<Item = String>,
    ) -> RuntimeComponentRebuildEffectResult {
        RuntimeComponentRebuildEffectResult {
            inner: RuntimeComponentRebuildEffectResultInner::Succeeded {
                effect: self,
                receipt,
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
        receipt: ManagedRuntimeFailureReceipt,
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
        admission: RegisteredComponentRebuildAdmission,
        reservation: ReconciliationAttemptReservation,
        request: RegisteredLibrariesComponentRebuildEffect,
    ) -> (Self, Arc<()>) {
        let identity = Arc::new(());
        (
            Self {
                admission,
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

impl ManagedAssetsComponentRebuildEffect {
    fn new(
        admission: RegisteredComponentRebuildAdmission,
        reservation: ReconciliationAttemptReservation,
        request: RegisteredAssetsComponentRebuildEffect,
    ) -> (Self, Arc<()>) {
        let identity = Arc::new(());
        (
            Self {
                admission,
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

pub(crate) async fn execute_managed_runtime_component_rebuild<Effect, EffectFuture>(
    admission: RegisteredComponentRebuildAdmission,
    effect: Effect,
) -> Result<GuardianComponentRebuildOutcome, OperationJournalStoreError>
where
    Effect: FnOnce(ManagedRuntimeComponentRebuildEffect) -> EffectFuture + Send,
    EffectFuture: Future<Output = RuntimeComponentRebuildEffectResult> + Send,
{
    validate_managed_runtime_admission(&admission)?;
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
        let (effect, _) = ManagedRuntimeComponentRebuildEffect::new(admission, reservation);
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

    let (effect_capability, effect_identity) =
        ManagedRuntimeComponentRebuildEffect::new(admission, reservation);
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
}

pub(crate) async fn execute_managed_libraries_component_rebuild<Effect, EffectFuture>(
    admission: RegisteredComponentRebuildAdmission,
    effect: Effect,
) -> Result<GuardianComponentRebuildOutcome, OperationJournalStoreError>
where
    Effect: FnOnce(ManagedLibrariesComponentRebuildEffect) -> EffectFuture + Send,
    EffectFuture: Future<Output = LibrariesComponentRebuildEffectResult> + Send,
{
    validate_managed_artifact_admission(&admission, ManagedArtifactGuardianComponent::Libraries)?;
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
        terminalize_managed_artifact_before_effect(
            admission,
            reservation,
            Vec::new(),
            COMPONENT_REBUILD_START_STEP,
        )
        .await?;
        return Err(plan_error);
    }

    let request = match admission.libraries_effect() {
        Ok(request) => request,
        Err(_) => {
            return terminalize_managed_artifact_before_effect(
                admission,
                reservation,
                vec!["libraries_component_authority_changed".to_string()],
                LIBRARIES_COMPONENT_REBUILD_STEP,
            )
            .await;
        }
    };
    let (effect_capability, effect_identity) =
        ManagedLibrariesComponentRebuildEffect::new(admission, reservation, request);
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
}

pub(crate) async fn execute_managed_assets_component_rebuild(
    admission: RegisteredComponentRebuildAdmission,
) -> Result<GuardianComponentRebuildOutcome, OperationJournalStoreError> {
    execute_managed_assets_component_rebuild_with_driver(admission, |effect| async move {
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
    admission: RegisteredComponentRebuildAdmission,
    driver: Driver,
) -> Result<GuardianComponentRebuildOutcome, OperationJournalStoreError>
where
    Driver: FnOnce(ManagedAssetsComponentRebuildEffect) -> DriverFuture + Send,
    DriverFuture: Future<Output = AssetsComponentRebuildEffectResult> + Send,
{
    validate_managed_artifact_admission(&admission, ManagedArtifactGuardianComponent::Assets)?;
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
        terminalize_managed_artifact_before_effect(
            admission,
            reservation,
            Vec::new(),
            COMPONENT_REBUILD_START_STEP,
        )
        .await?;
        return Err(plan_error);
    }

    let request = match admission.assets_effect() {
        Ok(request) => request,
        Err(_) => {
            return terminalize_managed_artifact_before_effect(
                admission,
                reservation,
                vec!["assets_component_authority_changed".to_string()],
                ASSETS_COMPONENT_REBUILD_STEP,
            )
            .await;
        }
    };
    let (effect_capability, effect_identity) =
        ManagedAssetsComponentRebuildEffect::new(admission, reservation, request);
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
}

#[cfg(test)]
pub(crate) async fn execute_failed_managed_assets_component_rebuild_for_test(
    admission: RegisteredComponentRebuildAdmission,
) -> Result<GuardianComponentRebuildOutcome, OperationJournalStoreError> {
    execute_managed_assets_component_rebuild_with_driver(admission, |effect| async move {
        effect.failed_before_effect(["assets_component_fixture_failure".to_string()])
    })
    .await
}

enum ComponentRebuildTerminal {
    Succeeded {
        receipt: ManagedRuntimeCommitReceipt,
        facts: Vec<String>,
    },
    FailedBeforeEffect {
        facts: Vec<String>,
        step_id: &'static str,
    },
    FailedAfterEffect {
        receipt: ManagedRuntimeFailureReceipt,
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
    RuntimeCommit(ManagedRuntimeCommitReceipt),
    RuntimeFailure(ManagedRuntimeFailureReceipt),
    LibrariesCommit(ManagedLibrariesCommitReceipt),
    LibrariesRollback(ManagedLibrariesRollbackReceipt),
    AssetsCommit(ManagedAssetsCommitReceipt),
    AssetsRollback(ManagedAssetsRollbackReceipt),
}

impl ComponentRebuildPublicationLease {
    fn release(self) {
        match self {
            Self::RuntimeCommit(receipt) => drop(receipt),
            Self::RuntimeFailure(receipt) => drop(receipt),
            Self::LibrariesCommit(receipt) => drop(receipt),
            Self::LibrariesRollback(receipt) => drop(receipt),
            Self::AssetsCommit(receipt) => drop(receipt),
            Self::AssetsRollback(receipt) => drop(receipt),
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
    Libraries,
    Assets,
}

impl ManagedArtifactGuardianComponent {
    fn reconciliation_component(self) -> ReconciliationComponent {
        match self {
            Self::Libraries => ReconciliationComponent::Libraries,
            Self::Assets => ReconciliationComponent::Assets,
        }
    }

    fn domain(self) -> GuardianDomain {
        match self {
            Self::Libraries => GuardianDomain::Library,
            Self::Assets => GuardianDomain::Download,
        }
    }

    fn label(self) -> &'static str {
        match self {
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
    let expected = component_rebuild_plan(admission);
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

fn component_rebuild_plan(
    admission: &RegisteredComponentRebuildAdmission,
) -> OperationJournalEntry {
    let attempt = admission.attempt();
    let target = attempt.target();
    let mut entry = OperationJournalEntry::new(
        JournalId::new(format!("journal-{}", attempt.operation_id().as_str())),
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
    entry.planned_steps.push(repair_step(
        COMPONENT_REBUILD_START_STEP,
        OperationStepResult::Planned,
        Some(target.clone()),
        Vec::new(),
    ));
    match attempt.component() {
        ReconciliationComponent::Runtime => {
            entry.planned_steps.push(repair_step_with_rollback(
                COMPONENT_QUARANTINE_STEP,
                OperationStepResult::Planned,
                Some(target.clone()),
                Vec::new(),
                RollbackState::Available,
            ));
            entry.planned_steps.push(repair_step(
                RUNTIME_COMPONENT_REBUILD_STEP,
                OperationStepResult::Planned,
                Some(target.clone()),
                Vec::new(),
            ));
        }
        ReconciliationComponent::Libraries | ReconciliationComponent::Assets => {
            let step_id = match attempt.component() {
                ReconciliationComponent::Libraries => LIBRARIES_COMPONENT_REBUILD_STEP,
                ReconciliationComponent::Assets => ASSETS_COMPONENT_REBUILD_STEP,
                _ => unreachable!(),
            };
            entry.planned_steps.push(repair_step(
                step_id,
                OperationStepResult::Planned,
                Some(target.clone()),
                Vec::new(),
            ));
        }
        _ => {}
    }
    entry.guardian_diagnosis_ids.push(attempt.diagnosis_id());
    reconciliation_journal_attempt(entry, attempt.clone())
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
                    entry.operation_id == *operation_id
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
    if typed_terminal.quarantined_target().is_some() {
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

async fn terminalize_managed_artifact_before_effect(
    admission: RegisteredComponentRebuildAdmission,
    reservation: ReconciliationAttemptReservation,
    facts: Vec<String>,
    step_id: &'static str,
) -> Result<GuardianComponentRebuildOutcome, OperationJournalStoreError> {
    let terminal = admission.failed_terminal().map_err(|_| {
        invalid_component_rebuild_error(
            std::io::ErrorKind::InvalidData,
            "managed artifact component rebuild pre-effect terminal is invalid",
        )
    })?;
    persist_component_rebuild_terminal(
        &admission,
        &reservation,
        ComponentRebuildTerminalRecord {
            terminal,
            step_id,
            step_result: OperationStepResult::Failed,
            failure_point: Some(step_id),
            rollback: RollbackState::NotApplicable,
            status: GuardianComponentRebuildStatus::Failed,
            facts,
            publication_lease: None,
        },
    )
    .await
}

async fn terminalize_libraries_component_rebuild(
    effect: ManagedLibrariesComponentRebuildEffect,
    terminal: LibrariesComponentRebuildTerminal,
) -> Result<GuardianComponentRebuildOutcome, OperationJournalStoreError> {
    let ManagedLibrariesComponentRebuildEffect {
        admission,
        reservation,
        request: _,
        identity: _,
    } = effect;
    let (terminal, status, facts, step_result, failure_point, rollback, publication_lease) =
        match terminal {
            LibrariesComponentRebuildTerminal::Committed { receipt, facts } => {
                let (terminal, status, facts, step_result, failure_point) =
                    match admission.succeeded_libraries_terminal(&receipt).await {
                        Ok(terminal) => (
                            terminal,
                            GuardianComponentRebuildStatus::Rebuilt,
                            facts,
                            OperationStepResult::Completed,
                            None,
                        ),
                        Err(_) => (
                            admission.failed_terminal().map_err(|_| {
                                invalid_component_rebuild_error(
                                    std::io::ErrorKind::InvalidData,
                                    "Libraries component rebuild postcondition terminal is invalid",
                                )
                            })?,
                            GuardianComponentRebuildStatus::Failed,
                            vec!["libraries_component_postcondition_failed".to_string()],
                            OperationStepResult::Failed,
                            Some(LIBRARIES_COMPONENT_REBUILD_STEP),
                        ),
                    };
                (
                    terminal,
                    status,
                    facts,
                    step_result,
                    failure_point,
                    RollbackState::NotApplicable,
                    Some(ComponentRebuildPublicationLease::LibrariesCommit(receipt)),
                )
            }
            LibrariesComponentRebuildTerminal::FailedBeforeEffect { facts } => (
                admission.failed_terminal().map_err(|_| {
                    invalid_component_rebuild_error(
                        std::io::ErrorKind::InvalidData,
                        "Libraries component rebuild failure terminal is invalid",
                    )
                })?,
                GuardianComponentRebuildStatus::Failed,
                facts,
                OperationStepResult::Failed,
                Some(LIBRARIES_COMPONENT_REBUILD_STEP),
                RollbackState::NotApplicable,
                None,
            ),
            LibrariesComponentRebuildTerminal::RolledBack { receipt, facts } => {
                let terminal = admission
                    .failed_libraries_effect_terminal(&receipt)
                    .await
                    .map_err(|_| {
                        invalid_component_rebuild_error(
                            std::io::ErrorKind::InvalidData,
                            "Libraries component rollback receipt is invalid or ambiguous",
                        )
                    })?;
                (
                    terminal,
                    GuardianComponentRebuildStatus::Failed,
                    facts,
                    OperationStepResult::Failed,
                    Some(LIBRARIES_COMPONENT_REBUILD_STEP),
                    RollbackState::Applied,
                    Some(ComponentRebuildPublicationLease::LibrariesRollback(receipt)),
                )
            }
        };
    persist_component_rebuild_terminal(
        &admission,
        &reservation,
        ComponentRebuildTerminalRecord {
            terminal,
            step_id: LIBRARIES_COMPONENT_REBUILD_STEP,
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

async fn terminalize_assets_component_rebuild(
    effect: ManagedAssetsComponentRebuildEffect,
    terminal: AssetsComponentRebuildTerminal,
) -> Result<GuardianComponentRebuildOutcome, OperationJournalStoreError> {
    let ManagedAssetsComponentRebuildEffect {
        admission,
        reservation,
        request: _,
        identity: _,
    } = effect;
    let (terminal, status, facts, step_result, failure_point, rollback, publication_lease) =
        match terminal {
            AssetsComponentRebuildTerminal::Committed { receipt, facts } => {
                let (terminal, status, facts, step_result, failure_point) =
                    match admission.succeeded_assets_terminal(&receipt).await {
                        Ok(terminal) => (
                            terminal,
                            GuardianComponentRebuildStatus::Rebuilt,
                            facts,
                            OperationStepResult::Completed,
                            None,
                        ),
                        Err(_) => (
                            admission.failed_terminal().map_err(|_| {
                                invalid_component_rebuild_error(
                                    std::io::ErrorKind::InvalidData,
                                    "Assets component rebuild postcondition terminal is invalid",
                                )
                            })?,
                            GuardianComponentRebuildStatus::Failed,
                            vec!["assets_component_postcondition_failed".to_string()],
                            OperationStepResult::Failed,
                            Some(ASSETS_COMPONENT_REBUILD_STEP),
                        ),
                    };
                (
                    terminal,
                    status,
                    facts,
                    step_result,
                    failure_point,
                    RollbackState::NotApplicable,
                    Some(ComponentRebuildPublicationLease::AssetsCommit(receipt)),
                )
            }
            AssetsComponentRebuildTerminal::FailedBeforeEffect { facts } => (
                admission.failed_terminal().map_err(|_| {
                    invalid_component_rebuild_error(
                        std::io::ErrorKind::InvalidData,
                        "Assets component rebuild failure terminal is invalid",
                    )
                })?,
                GuardianComponentRebuildStatus::Failed,
                facts,
                OperationStepResult::Failed,
                Some(ASSETS_COMPONENT_REBUILD_STEP),
                RollbackState::NotApplicable,
                None,
            ),
            AssetsComponentRebuildTerminal::RolledBack { receipt, facts } => {
                let terminal = admission
                    .failed_assets_effect_terminal(&receipt)
                    .await
                    .map_err(|_| {
                        invalid_component_rebuild_error(
                            std::io::ErrorKind::InvalidData,
                            "Assets component rollback receipt is invalid or ambiguous",
                        )
                    })?;
                (
                    terminal,
                    GuardianComponentRebuildStatus::Failed,
                    facts,
                    OperationStepResult::Failed,
                    Some(ASSETS_COMPONENT_REBUILD_STEP),
                    RollbackState::Applied,
                    Some(ComponentRebuildPublicationLease::AssetsRollback(receipt)),
                )
            }
        };
    persist_component_rebuild_terminal(
        &admission,
        &reservation,
        ComponentRebuildTerminalRecord {
            terminal,
            step_id: ASSETS_COMPONENT_REBUILD_STEP,
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
    persist_exact_component_rebuild_memory(admission, reservation, &memory).await?;

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
    admission: &RegisteredComponentRebuildAdmission,
    reservation: &ReconciliationAttemptReservation,
    expected: &GuardianFailureMemoryEntry,
) -> Result<(), OperationJournalStoreError> {
    let mut delay = COMPONENT_MEMORY_RETRY_INITIAL_DELAY;
    loop {
        if admission.failure_memory().get(&expected.key).as_ref() == Some(expected) {
            return Ok(());
        }
        match commit_reconciliation_memory(
            admission.failure_memory(),
            expected.clone(),
            reservation,
        )
        .await
        {
            Ok(()) => {
                if admission.failure_memory().get(&expected.key).as_ref() == Some(expected) {
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
        bounded_fact_ids, component_rebuild_plan,
        execute_managed_assets_component_rebuild_with_driver,
        execute_managed_runtime_component_rebuild,
    };
    use crate::execution::persistence::{AtomicWriteBackend, PersistenceCoordinator};
    use crate::guardian::{DiagnosisId, GuardianDomain, GuardianMode};
    use crate::state::contracts::{
        CommandKind, JournalId, OperationId, OperationJournalEntry, OperationJournalStep,
        OperationPhase, OperationStatus, OperationStepResult, OwnershipClass,
        ReconciliationComponent, ReconciliationScope, ReconciliationTerminalOutcome, RollbackState,
        StabilizationSystem, TargetDescriptor, TargetKind,
    };
    use crate::state::failure_memory::GuardianFailureMemoryStore;
    use crate::state::{
        AppState, AppStateInit, InstallStore, MAX_OPERATION_JOURNAL_STEP_FACTS,
        OperationJournalStore, RegisteredComponentRebuildAdmission, SessionStore,
        commit_reconciliation_memory, new_instance, reconciliation_attempt_key,
        reconciliation_instance_target, reconciliation_journal_attempt,
        reconciliation_memory_entry, record_reconciliation_journal_failure,
        registered_artifact_target_for_test, reserve_reconciliation_attempt,
    };
    use axial_config::{AppPaths, InstanceRegistrySnapshot};
    use axial_minecraft::RuntimeId;
    use axial_minecraft::known_good::{
        KnownGoodArtifactKind, KnownGoodInventory, TestKnownGoodEntry, TestKnownGoodIntegrity,
        TestKnownGoodRoot,
    };
    use sha1::{Digest as _, Sha1};
    use std::fs;
    use std::io;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

    const INSTANCE_ID: &str = "0000000000000001";
    const RUNTIME_COMPONENT: &str = "java-runtime-gamma";
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
            target: &TargetDescriptor,
            destination: &Path,
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
        static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(1);

        let root = std::env::temp_dir().join(format!(
            "axial-component-rebuild-{label}-{}-{}",
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
        fs::create_dir_all(paths.instances_dir.join(INSTANCE_ID)).expect("instance root");
        fs::create_dir_all(&paths.library_dir).expect("library root");
        let config = Arc::new(
            axial_config::ConfigStore::load_from(paths.clone()).expect("test config store"),
        );
        let instances = Arc::new(
            axial_config::InstanceStore::from_snapshot(
                paths.clone(),
                InstanceRegistrySnapshot::new(
                    vec![new_instance(
                        INSTANCE_ID.to_string(),
                        "Component Rebuild Test".to_string(),
                        "1.21.1".to_string(),
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
                axial_performance::PerformanceManager::load_for_startup(&paths.config_dir)
                    .expect("test performance state"),
            ),
            startup_warnings: Vec::new(),
            frontend_dir: root.join("frontend"),
        })
        .with_reconciliation_stores(journals.clone(), failure_memory.clone());
        state.set_library_dir_for_test(paths.library_dir.to_string_lossy().into_owned());
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
                    digest: object_digest,
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
            0,
        )
        .expect("exact source-backed Assets target");
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
        let artifact_operation = OperationId::new(format!("artifact-{operation_suffix}"));
        let attempt = authority
            .repair_artifact_attempt(
                artifact_operation.clone(),
                DIAGNOSIS_ID,
                GuardianDomain::Runtime,
                ReconciliationComponent::Runtime,
                runtime_target(),
                GuardianMode::Managed,
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
        let rebuild_operation = OperationId::new(format!("component-{operation_suffix}"));
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
        let lifecycle = fixture.state.acquire_instance_lifecycle(INSTANCE_ID).await;
        let foreground = fixture
            .state
            .register_integrity_foreground()
            .expect("register Assets predecessor foreground")
            .wait_for_settlement()
            .await;
        let verification = fixture
            .state
            .mint_current_known_good_verification_lease(&foreground, &lifecycle)
            .expect("mint Assets predecessor verification");
        let authority = fixture
            .state
            .registered_reconciliation_authority(&lifecycle)
            .expect("registered Assets reconciliation authority");
        let artifact_operation = OperationId::new(format!("asset-artifact-{operation_suffix}"));
        let attempt = authority
            .repair_artifact_attempt(
                artifact_operation.clone(),
                DIAGNOSIS_ID,
                GuardianDomain::Download,
                ReconciliationComponent::Assets,
                target,
                GuardianMode::Managed,
                chrono::Duration::minutes(30),
            )
            .expect("Assets artifact attempt");
        let terminal = authority
            .artifact_terminal(attempt.clone(), ReconciliationTerminalOutcome::Failed, None)
            .expect("Assets artifact terminal");
        let reservation = reserve_reconciliation_attempt(
            fixture.failure_memory.as_ref(),
            fixture.journals.as_ref(),
            reconciliation_attempt_key(&attempt),
        )
        .expect("Assets artifact attempt reservation");
        fixture
            .journals
            .create(artifact_repair_plan(&attempt))
            .await
            .expect("Assets artifact repair plan");
        record_reconciliation_journal_failure(
            fixture.journals.as_ref(),
            &artifact_operation,
            artifact_repair_failed_step(attempt.target()),
            "repair_asset_artifact",
            terminal.clone(),
        )
        .await
        .expect("Assets artifact repair failure");
        commit_reconciliation_memory(
            fixture.failure_memory.as_ref(),
            reconciliation_memory_entry(terminal).expect("Assets artifact failure memory"),
            &reservation,
        )
        .await
        .expect("Assets artifact failure memory commit");
        drop((reservation, authority));

        let continuation = fixture
            .state
            .recorded_verified_registered_artifact_failure_for_test(verification, &attempt)
            .expect("closed verified Assets component predecessor proof");
        let admission = fixture
            .state
            .admit_registered_artifact_component_rebuild(
                continuation,
                OperationId::new(format!("asset-component-{operation_suffix}")),
                chrono::Duration::minutes(30),
            )
            .await
            .expect("Assets component rebuild admission");
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
                OperationId::new(format!("component-{operation_suffix}")),
                chrono::Duration::minutes(30),
            )
            .await
            .is_err();
        drop(lifecycle);
        refused
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
        let effect_cache = runtime_cache.clone();
        let effect_backend = backend.clone();
        let rebuild = execute_managed_runtime_component_rebuild(admission, move |effect| {
            let component = effect.component();
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
        });
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
    async fn managed_assets_commit_has_no_quarantine_and_settles_exact_terminal_memory() {
        let fixture = fixture("assets-commit");
        let admission = assets_component_admission(&fixture, "commit").await;
        let operation_id = admission.attempt().operation_id().clone();
        let memory_key = reconciliation_attempt_key(admission.attempt());
        let journals = fixture.journals.clone();
        let root = PathBuf::from(fixture.state.library_dir().expect("library root"));
        let expected_root = root.clone();

        let outcome =
            execute_managed_assets_component_rebuild_with_driver(admission, move |effect| {
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
                    let receipt = axial_minecraft::rebuild_managed_assets_fixture_for_test(
                        expected_root,
                        "1.21.1",
                    )
                    .await
                    .expect("sealed Assets fixture receipt");
                    effect.committed(receipt, vec!["assets_component_rebuilt".to_string()])
                }
            })
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
        assert!(terminal.quarantined_target().is_none());
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
    async fn managed_assets_preeffect_failure_is_failed_and_not_applicable() {
        let fixture = fixture("assets-preeffect");
        let admission = assets_component_admission(&fixture, "preeffect").await;
        let memory_key = reconciliation_attempt_key(admission.attempt());

        let outcome =
            execute_managed_assets_component_rebuild_with_driver(admission, |effect| async move {
                effect.failed_before_effect(vec!["assets_source_unavailable".to_string()])
            })
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
        assert!(terminal.quarantined_target().is_none());
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
        assert!(terminal.quarantined_target().is_none());
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
    async fn assets_receipt_is_retained_until_exact_memory_is_durable() {
        let backend = Arc::new(ControlledWriteBackend::default());
        let fixture = fixture_with_backends("assets-memory-retry", None, Some(backend.clone()));
        let admission = assets_component_admission(&fixture, "memory-retry").await;
        let operation_id = admission.attempt().operation_id().clone();
        let memory_key = reconciliation_attempt_key(admission.attempt());
        let root = PathBuf::from(fixture.state.library_dir().expect("library root"));
        let effect_root = root.clone();
        let effect_backend = backend.clone();
        let rebuild = execute_managed_assets_component_rebuild_with_driver(
            admission,
            move |effect| async move {
                let receipt =
                    axial_minecraft::rebuild_managed_assets_fixture_for_test(effect_root, "1.21.1")
                        .await
                        .expect("sealed Assets fixture receipt");
                let failed_attempt = effect_backend.next_attempt();
                effect_backend.fail_attempt(failed_attempt);
                effect_backend.gate_attempt(failed_attempt + 1);
                effect.committed(receipt, vec!["assets_component_rebuilt".to_string()])
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
            assert!(!settlement_complete.load(Ordering::Acquire));
            assert!(
                fixture
                    .journals
                    .get(&operation_id)
                    .and_then(|entry| entry.reconciliation_terminal().cloned())
                    .is_some()
            );
            assert!(fixture.failure_memory.get(&memory_key).is_none());

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
    async fn managed_runtime_failure_is_planned_before_effect_and_settled_immediately() {
        let fixture = fixture("managed-failure");
        let (admission, _) = component_admission(&fixture, "managed-failure").await;
        let component_operation = admission.attempt().operation_id().clone();
        let component_key = reconciliation_attempt_key(admission.attempt());
        let journals = fixture.journals.clone();

        let outcome = execute_managed_runtime_component_rebuild(admission, move |effect| {
            let journal = journals
                .get(&component_operation)
                .expect("plan must be visible before effect capability");
            assert_eq!(journal.status, OperationStatus::Planned);
            assert!(journal.reconciliation_terminal().is_none());
            assert_eq!(effect.component().as_str(), RUNTIME_COMPONENT);
            async move { effect.failed_before_effect(vec!["runtime_stage_failed".to_string()]) }
        })
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
    async fn disabled_mode_cannot_mint_the_lower_attempt_needed_for_effect_admission() {
        let fixture = fixture("disabled-refusal");
        let lifecycle = fixture.state.acquire_instance_lifecycle(INSTANCE_ID).await;
        let authority = fixture
            .state
            .registered_reconciliation_authority(&lifecycle)
            .expect("registered reconciliation authority");

        assert!(
            authority
                .repair_artifact_attempt(
                    OperationId::new("artifact-disabled-refusal"),
                    DIAGNOSIS_ID,
                    GuardianDomain::Runtime,
                    ReconciliationComponent::Runtime,
                    runtime_target(),
                    GuardianMode::Disabled,
                    chrono::Duration::minutes(30),
                )
                .is_err(),
            "Disabled mode must not reach a lower-rung attempt or component admission"
        );

        drop((authority, lifecycle));
        cleanup(fixture).await;
    }

    #[tokio::test]
    async fn ambiguous_planned_replay_refuses_effect_ownership() {
        let fixture = fixture("ambiguous-replay");
        let (admission, _) = component_admission(&fixture, "ambiguous-replay").await;
        let operation_id = admission.attempt().operation_id().clone();
        fixture
            .journals
            .create(component_rebuild_plan(&admission))
            .await
            .expect("interrupted component plan");
        let effect_called = Arc::new(AtomicBool::new(false));
        let effect_called_in_closure = effect_called.clone();

        let error = execute_managed_runtime_component_rebuild(admission, move |effect| {
            effect_called_in_closure.store(true, Ordering::Release);
            async move { effect.failed_before_effect(Vec::new()) }
        })
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
    async fn failed_component_attempt_refuses_readmission_in_the_window() {
        let fixture = fixture("window-gate");
        let (admission, artifact_operation) = component_admission(&fixture, "window-first").await;
        execute_managed_runtime_component_rebuild(admission, |effect| async move {
            effect.failed_before_effect(vec!["runtime_stage_failed".to_string()])
        })
        .await
        .expect("first component failure settled");
        assert!(
            component_readmission_is_refused(&fixture, &artifact_operation, "window-second").await,
            "active rung-2 suppression must refuse admission before Guardian effect ownership"
        );

        cleanup(fixture).await;
    }
}
