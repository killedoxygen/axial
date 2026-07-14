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
use crate::state::{
    MAX_OPERATION_JOURNAL_STEP_FACTS, OperationJournalStoreError, ReconciliationAttemptReservation,
    RegisteredComponentRebuildAdmission, commit_reconciliation_memory,
    operation_journal_completed_step_is_visible, operation_journal_plan_is_visible,
    reconciliation_attempt_key, reconciliation_instance_target, reconciliation_journal_attempt,
    reconciliation_memory_entry, reserve_reconciliation_attempt, settle_reconciliation_memory,
};
use axial_minecraft::runtime::{
    ManagedRuntimeCommitReceipt, ManagedRuntimeFailureReceipt, RuntimeId,
    is_known_runtime_component,
};
use std::future::Future;
use std::sync::Arc;

const COMPONENT_REBUILD_START_STEP: &str = "journal_component_rebuild_start";
const COMPONENT_QUARANTINE_STEP: &str = "quarantine_launcher_managed_target";
const RUNTIME_COMPONENT_REBUILD_STEP: &str = "rebuild_managed_runtime_component";

pub(crate) struct ManagedRuntimeComponentRebuildEffect {
    admission: RegisteredComponentRebuildAdmission,
    reservation: ReconciliationAttemptReservation,
    identity: Arc<()>,
}

pub(crate) struct RuntimeComponentRebuildEffectResult {
    inner: RuntimeComponentRebuildEffectResultInner,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GuardianRuntimeComponentRebuildStatus {
    Rebuilt,
    Failed,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct GuardianRuntimeComponentRebuildOutcome {
    pub(crate) operation_id: OperationId,
    pub(crate) status: GuardianRuntimeComponentRebuildStatus,
    pub(crate) facts: Vec<String>,
}

pub(crate) async fn execute_managed_runtime_component_rebuild<Effect, EffectFuture>(
    admission: RegisteredComponentRebuildAdmission,
    effect: Effect,
) -> Result<GuardianRuntimeComponentRebuildOutcome, OperationJournalStoreError>
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

enum ComponentRebuildPublicationLease {
    Commit(ManagedRuntimeCommitReceipt),
    Failure(ManagedRuntimeFailureReceipt),
}

impl ComponentRebuildPublicationLease {
    fn release(self) {
        match self {
            Self::Commit(receipt) => drop(receipt),
            Self::Failure(receipt) => drop(receipt),
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
        || !matches!(
            attempt.scope(),
            ReconciliationScope::RegisteredInstance { .. }
        )
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
    if let ReconciliationScope::RegisteredInstance { instance_id, .. } = attempt.scope() {
        entry
            .targets
            .push(reconciliation_instance_target(instance_id));
    }
    entry.planned_steps.push(repair_step(
        COMPONENT_REBUILD_START_STEP,
        OperationStepResult::Planned,
        Some(target.clone()),
        Vec::new(),
    ));
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
    entry.guardian_diagnosis_ids.push(attempt.diagnosis_id());
    reconciliation_journal_attempt(entry, attempt.clone())
}

async fn record_component_quarantine_checkpoint(
    admission: &RegisteredComponentRebuildAdmission,
) -> Result<Option<OperationJournalStoreError>, OperationJournalStoreError> {
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
            Ok(()) => return Ok(None),
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

async fn terminalize_component_rebuild(
    effect: ManagedRuntimeComponentRebuildEffect,
    terminal: ComponentRebuildTerminal,
) -> Result<GuardianRuntimeComponentRebuildOutcome, OperationJournalStoreError> {
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
                        GuardianRuntimeComponentRebuildStatus::Rebuilt,
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
                        GuardianRuntimeComponentRebuildStatus::Failed,
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
                Some(ComponentRebuildPublicationLease::Commit(receipt)),
            )
        }
        ComponentRebuildTerminal::FailedBeforeEffect { facts, step_id } => (
            admission.failed_terminal().map_err(|_| {
                invalid_component_rebuild_error(
                    std::io::ErrorKind::InvalidData,
                    "runtime component rebuild failure terminal is invalid",
                )
            })?,
            GuardianRuntimeComponentRebuildStatus::Failed,
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
                GuardianRuntimeComponentRebuildStatus::Failed,
                facts,
                RUNTIME_COMPONENT_REBUILD_STEP,
                OperationStepResult::Failed,
                Some(RUNTIME_COMPONENT_REBUILD_STEP),
                rollback,
                Some(ComponentRebuildPublicationLease::Failure(receipt)),
            )
        }
    };
    let checkpoint_error = if typed_terminal.quarantined_target().is_some() {
        record_component_quarantine_checkpoint(&admission).await?
    } else {
        None
    };
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
            checkpoint_error,
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
    status: GuardianRuntimeComponentRebuildStatus,
    facts: Vec<String>,
    checkpoint_error: Option<OperationJournalStoreError>,
    publication_lease: Option<ComponentRebuildPublicationLease>,
}

async fn persist_component_rebuild_terminal(
    admission: &RegisteredComponentRebuildAdmission,
    reservation: &ReconciliationAttemptReservation,
    record: ComponentRebuildTerminalRecord,
) -> Result<GuardianRuntimeComponentRebuildOutcome, OperationJournalStoreError> {
    let ComponentRebuildTerminalRecord {
        terminal,
        step_id,
        step_result,
        failure_point,
        rollback,
        status,
        facts,
        checkpoint_error,
        publication_lease,
    } = record;
    let attempt = admission.attempt();
    let operation_id = attempt.operation_id().clone();
    let journal_error = record_reconciliation_terminal_reconciled(
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

    let memory = reconciliation_memory_entry(terminal).map_err(|_| {
        invalid_component_rebuild_error(
            std::io::ErrorKind::InvalidData,
            "runtime component rebuild memory terminal is invalid",
        )
    })?;
    commit_reconciliation_memory(admission.failure_memory(), memory, reservation)
        .await
        .map_err(component_rebuild_memory_error)?;
    if let Some(error) = journal_error.or(checkpoint_error) {
        return Err(error);
    }

    if let Some(publication_lease) = publication_lease {
        publication_lease.release();
    }

    Ok(GuardianRuntimeComponentRebuildOutcome {
        operation_id,
        status,
        facts,
    })
}

fn bounded_fact_ids(facts: impl IntoIterator<Item = String>) -> Vec<String> {
    facts
        .into_iter()
        .filter_map(|fact| sanitize_evidence_token(&fact, RedactionAudience::UserVisible, 96))
        .take(MAX_OPERATION_JOURNAL_STEP_FACTS)
        .collect()
}

fn component_rebuild_memory_error(error: impl std::fmt::Display) -> OperationJournalStoreError {
    invalid_component_rebuild_error(
        std::io::ErrorKind::Other,
        format!("runtime component rebuild memory failed: {error}"),
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
        GuardianRuntimeComponentRebuildStatus, bounded_fact_ids, component_rebuild_plan,
        execute_managed_runtime_component_rebuild,
    };
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
        reserve_reconciliation_attempt,
    };
    use axial_config::{AppPaths, InstanceRegistrySnapshot};
    use std::fs;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    const INSTANCE_ID: &str = "0000000000000001";
    const RUNTIME_COMPONENT: &str = "java-runtime-gamma";
    const DIAGNOSIS_ID: DiagnosisId = DiagnosisId::LauncherManagedArtifactCorrupt;

    struct Fixture {
        state: AppState,
        journals: Arc<OperationJournalStore>,
        failure_memory: Arc<GuardianFailureMemoryStore>,
        root: PathBuf,
    }

    fn fixture(label: &str) -> Fixture {
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
        let journals = Arc::new(OperationJournalStore::new());
        let failure_memory = Arc::new(GuardianFailureMemoryStore::new());
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
        Fixture {
            state,
            journals,
            failure_memory,
            root,
        }
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
        if let ReconciliationScope::RegisteredInstance { instance_id, .. } = attempt.scope() {
            entry
                .targets
                .push(reconciliation_instance_target(instance_id));
        }
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
        mode: GuardianMode,
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
                mode,
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
            .recorded_artifact_repair_failure(&lifecycle, &artifact_operation)
            .expect("exact artifact failure proof");
        let rebuild_operation = OperationId::new(format!("component-{operation_suffix}"));
        let admission = fixture
            .state
            .admit_component_rebuild(evidence, rebuild_operation, chrono::Duration::minutes(30))
            .await
            .expect("component rebuild admission");
        drop(lifecycle);
        (admission, artifact_operation)
    }

    async fn component_readmission_is_refused(
        fixture: &Fixture,
        artifact_operation: &OperationId,
        operation_suffix: &str,
    ) -> bool {
        let lifecycle = fixture.state.acquire_instance_lifecycle(INSTANCE_ID).await;
        let evidence = fixture
            .state
            .recorded_artifact_repair_failure(&lifecycle, artifact_operation)
            .expect("artifact failure remains exact");
        let refused = fixture
            .state
            .admit_component_rebuild(
                evidence,
                OperationId::new(format!("component-{operation_suffix}")),
                chrono::Duration::minutes(30),
            )
            .await
            .is_err();
        drop(lifecycle);
        refused
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
        let (admission, _) =
            component_admission(&fixture, GuardianMode::Managed, "managed-failure").await;
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

        assert_eq!(
            outcome.status,
            GuardianRuntimeComponentRebuildStatus::Failed
        );
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
    async fn custom_admission_never_receives_an_effect_capability() {
        let fixture = fixture("custom-refusal");
        let (admission, _) =
            component_admission(&fixture, GuardianMode::Custom, "custom-refusal").await;
        let operation_id = admission.attempt().operation_id().clone();
        let effect_called = Arc::new(AtomicBool::new(false));
        let effect_called_in_closure = effect_called.clone();

        let error = execute_managed_runtime_component_rebuild(admission, move |effect| {
            effect_called_in_closure.store(true, Ordering::Release);
            async move { effect.failed_before_effect(Vec::new()) }
        })
        .await
        .expect_err("Custom mode must refuse managed component execution");

        assert!(!effect_called.load(Ordering::Acquire));
        assert!(fixture.journals.get(&operation_id).is_none());
        assert!(matches!(
            error,
            crate::state::OperationJournalStoreError::Persistence(ref error)
                if error.kind() == std::io::ErrorKind::PermissionDenied
        ));

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
        let (admission, _) =
            component_admission(&fixture, GuardianMode::Managed, "ambiguous-replay").await;
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
        let (admission, artifact_operation) =
            component_admission(&fixture, GuardianMode::Managed, "window-first").await;
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
