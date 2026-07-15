use super::contracts::{
    CommandKind, OperationId, OperationJournalEntry, OperationJournalStep, OperationOutcome,
    OperationStatus, OwnershipClass, ReconciliationAttempt, ReconciliationComponent,
    ReconciliationIncarnationFingerprint, ReconciliationRung, ReconciliationScope,
    ReconciliationTerminal, ReconciliationTerminalOutcome, StabilizationSystem, TargetDescriptor,
    TargetKind,
};
use super::failure_memory::{
    FailureMemoryActionOutcome, FailureMemoryKey, FailureMemoryStoreError,
    GuardianFailureMemoryEntry, GuardianFailureMemoryStore,
    ReconciliationAttemptReservation as StoreReconciliationAttemptReservation,
    ReconciliationAttemptReserveError,
};
use super::sessions::SharedComponentMutationLease;
use super::{AppState, InstanceLifecycleLease, OperationJournalStore, OperationJournalStoreError};
use crate::guardian::{DiagnosisId, GuardianActionKind, GuardianDomain, GuardianMode};
use axial_config::is_canonical_instance_id;
use axial_minecraft::runtime::{
    ManagedRuntimeCommitReceipt, ManagedRuntimeFailureReceipt, ManagedRuntimeQuarantineObligation,
    RuntimeId, is_known_runtime_component,
};
use axial_minecraft::{ManagedLibrariesCommitReceipt, ManagedLibrariesRollbackReceipt};
use sha2::{Digest, Sha256};
use std::io;
use std::path::{Path, PathBuf};

const RECONCILIATION_FINGERPRINT_DOMAIN: &[u8] = b"axial.guardian.reconciliation.incarnation.v1";

pub(crate) struct RecordedArtifactRepairFailure {
    evidence: RecordedReconciliationFailure,
}

pub(crate) struct RegisteredComponentRebuildAdmission {
    authority: RegisteredReconciliationAuthority,
    attempt: ReconciliationAttempt,
    _predecessor: ReconciliationTerminal,
    known_good: RegisteredKnownGoodInventory,
    component_state: RegisteredComponentRebuildState,
    _component_mutation: SharedComponentMutationLease,
    _library_root_mutation: tokio::sync::OwnedMutexGuard<()>,
}

enum RegisteredComponentRebuildState {
    Runtime {
        postcondition_failure_inventory:
            std::sync::OnceLock<std::sync::Arc<axial_minecraft::known_good::KnownGoodInventory>>,
    },
    Libraries,
}

pub(crate) struct RegisteredLibrariesComponentRebuildEffect {
    library_root: PathBuf,
    version_id: String,
}

impl RegisteredLibrariesComponentRebuildEffect {
    pub(crate) fn core_request(&self) -> (&Path, &str) {
        (&self.library_root, &self.version_id)
    }
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
}

#[derive(Eq, PartialEq)]
struct ReconciliationRoots {
    instance: PathBuf,
    library: PathBuf,
    runtime: PathBuf,
}

struct CurrentReconciliationIncarnation {
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
    SuppressedPriorAttempt,
}

impl RecordedArtifactRepairFailure {
    pub(crate) fn diagnosis_id(&self) -> DiagnosisId {
        self.evidence.terminal.diagnosis_id()
    }

    #[cfg(test)]
    pub(crate) fn terminal(&self) -> &ReconciliationTerminal {
        &self.evidence.terminal
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

    pub(crate) fn failed_terminal(
        &self,
    ) -> Result<ReconciliationTerminal, ReconciliationEvidenceRejection> {
        self.authority
            .terminal(self.attempt.clone(), ReconciliationTerminalOutcome::Failed)
    }

    pub(crate) fn libraries_effect(
        &self,
    ) -> Result<RegisteredLibrariesComponentRebuildEffect, ReconciliationEvidenceRejection> {
        self.current_libraries_inventory()?;
        Ok(RegisteredLibrariesComponentRebuildEffect {
            library_root: self.known_good.library_root.clone(),
            version_id: self.known_good.version_id.clone(),
        })
    }

    pub(crate) async fn succeeded_libraries_terminal(
        &self,
        receipt: &ManagedLibrariesCommitReceipt,
    ) -> Result<ReconciliationTerminal, ReconciliationEvidenceRejection> {
        self.validate_libraries_receipt_version(receipt.version_id())?;
        if !receipt.matches_root(&self.known_good.library_root).await
            || !receipt.matches_known_good_inventory(&self.known_good.inventory)
            || !receipt.revalidate().await
        {
            return Err(ReconciliationEvidenceRejection::JournalMismatch);
        }
        self.current_libraries_inventory()?;
        Ok(ReconciliationTerminal::from_attempt(
            self.attempt.clone(),
            ReconciliationTerminalOutcome::Succeeded,
            None,
        ))
    }

    pub(crate) async fn failed_libraries_effect_terminal(
        &self,
        receipt: &ManagedLibrariesRollbackReceipt,
    ) -> Result<ReconciliationTerminal, ReconciliationEvidenceRejection> {
        self.validate_libraries_receipt_version(receipt.version_id())?;
        if !receipt.matches_root(&self.known_good.library_root).await
            || !receipt.matches_known_good_inventory(&self.known_good.inventory)
        {
            return Err(ReconciliationEvidenceRejection::JournalMismatch);
        }
        self.current_libraries_inventory()?;
        Ok(ReconciliationTerminal::from_attempt(
            self.attempt.clone(),
            ReconciliationTerminalOutcome::Failed,
            None,
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
        self.validate_runtime_receipt_identity(receipt)?;
        let refreshed_inventory = std::sync::Arc::new(
            receipt
                .replace_known_good_runtime_projection(&self.known_good.inventory)
                .map_err(|_| ReconciliationEvidenceRejection::JournalMismatch)?,
        );
        if !receipt.matches_known_good_inventory(&refreshed_inventory) {
            return Err(ReconciliationEvidenceRejection::JournalMismatch);
        }
        let quarantined_target =
            self.validated_quarantine_target(receipt.component(), receipt.quarantine_obligation())?;
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
        Ok(ReconciliationTerminal::from_attempt(
            self.attempt.clone(),
            ReconciliationTerminalOutcome::Succeeded,
            quarantined_target,
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
        let quarantined_target =
            self.validated_quarantine_target(receipt.component(), receipt.quarantine_obligation())?;
        Ok(ReconciliationTerminal::from_attempt(
            self.attempt.clone(),
            ReconciliationTerminalOutcome::Failed,
            quarantined_target,
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
            self.validated_quarantine_target(receipt.component(), receipt.quarantine_obligation())?,
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

    fn validate_libraries_receipt_version(
        &self,
        version_id: &str,
    ) -> Result<(), ReconciliationEvidenceRejection> {
        self.validate_libraries_admission()?;
        if version_id != self.known_good.version_id {
            return Err(ReconciliationEvidenceRejection::JournalMismatch);
        }
        Ok(())
    }

    fn current_libraries_inventory(
        &self,
    ) -> Result<
        std::sync::Arc<axial_minecraft::known_good::KnownGoodInventory>,
        ReconciliationEvidenceRejection,
    > {
        self.validate_libraries_admission()?;
        let ReconciliationScope::RegisteredInstance {
            instance_id,
            fingerprint,
        } = self.attempt.scope();
        let current = self
            .authority
            .state
            .current_reconciliation_incarnation(instance_id)?;
        if &current.fingerprint != fingerprint
            || current.roots.library != self.known_good.library_root
        {
            return Err(ReconciliationEvidenceRejection::IncarnationMismatch);
        }
        let instance = self
            .authority
            .state
            .instances
            .get(instance_id)
            .filter(|instance| {
                instance.id == self.known_good.instance_id
                    && instance.version_id == self.known_good.version_id
                    && instance.created_at == self.known_good.created_at
            })
            .ok_or(ReconciliationEvidenceRejection::InstanceNotRegistered)?;
        let inventory = self
            .authority
            .state
            .known_good
            .active_inventory(
                &instance.id,
                &instance.version_id,
                &instance.created_at,
                &current.roots.library,
            )
            .ok_or(ReconciliationEvidenceRejection::RootAuthorityUnavailable)?;
        if !std::sync::Arc::ptr_eq(&inventory, &self.known_good.inventory) {
            return Err(ReconciliationEvidenceRejection::IncarnationMismatch);
        }
        Ok(inventory)
    }

    fn validate_libraries_admission(&self) -> Result<(), ReconciliationEvidenceRejection> {
        if self.attempt.rung() != ReconciliationRung::RebuildComponent
            || self.attempt.component() != ReconciliationComponent::Libraries
            || self.attempt.domain() != GuardianDomain::Library
            || self.attempt.mode() != GuardianMode::Managed
            || self.attempt.ownership() != OwnershipClass::LauncherManaged
            || self.attempt.target().system != StabilizationSystem::Execution
            || self.attempt.target().kind != TargetKind::Artifact
            || self.attempt.target().ownership != OwnershipClass::LauncherManaged
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
        let inventory = self
            .current_runtime_inventory(component, matches_cache)?
            .ok_or(ReconciliationEvidenceRejection::RootAuthorityUnavailable)?;
        if !std::sync::Arc::ptr_eq(&inventory, expected_inventory) {
            return Err(ReconciliationEvidenceRejection::IncarnationMismatch);
        }
        Ok(inventory)
    }

    fn current_runtime_inventory(
        &self,
        component: &RuntimeId,
        matches_cache: bool,
    ) -> Result<
        Option<std::sync::Arc<axial_minecraft::known_good::KnownGoodInventory>>,
        ReconciliationEvidenceRejection,
    > {
        self.validate_runtime_receipt_capability(component, matches_cache)?;
        let ReconciliationScope::RegisteredInstance {
            instance_id,
            fingerprint,
        } = self.attempt.scope();
        let current = self
            .authority
            .state
            .current_reconciliation_incarnation(instance_id)?;
        if &current.fingerprint != fingerprint {
            return Err(ReconciliationEvidenceRejection::IncarnationMismatch);
        }
        let instance = self
            .authority
            .state
            .instances
            .get(instance_id)
            .filter(|instance| instance.id == *instance_id)
            .ok_or(ReconciliationEvidenceRejection::InstanceNotRegistered)?;
        let inventory = self.authority.state.known_good.active_inventory(
            &instance.id,
            &instance.version_id,
            &instance.created_at,
            &current.roots.library,
        );
        Ok(inventory)
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
            } => Ok(postcondition_failure_inventory),
            RegisteredComponentRebuildState::Libraries => {
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

    fn validated_quarantine_target(
        &self,
        component: &RuntimeId,
        quarantine: Option<&ManagedRuntimeQuarantineObligation>,
    ) -> Result<Option<TargetDescriptor>, ReconciliationEvidenceRejection> {
        let Some(quarantine) = quarantine else {
            return Ok(None);
        };
        if quarantine.component() != component
            || !quarantine.matches_cache(&self.authority.state.managed_runtime_cache)
            || !quarantine.is_present()
        {
            return Err(ReconciliationEvidenceRejection::JournalMismatch);
        }
        Ok(Some(TargetDescriptor::new(
            StabilizationSystem::Execution,
            self.attempt.target().kind,
            format!("quarantine-{}", self.attempt.target().id),
            self.attempt.ownership(),
        )))
    }

    #[cfg(test)]
    fn predecessor(&self) -> &ReconciliationTerminal {
        &self._predecessor
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

    pub(crate) fn registered_artifact_findings_are_current(
        &self,
        findings: &super::RegisteredArtifactFindings,
    ) -> bool {
        self.state
            .registered_artifact_findings_are_current(findings)
    }

    pub(crate) fn attempt_is_current(&self, attempt: &ReconciliationAttempt) -> bool {
        self.state
            .current_reconciliation_incarnation(&self.lifecycle.instance_id)
            .is_ok_and(|current| {
                matches!(
                    attempt.scope(),
                    ReconciliationScope::RegisteredInstance {
                        instance_id,
                        fingerprint,
                    } if instance_id == &self.lifecycle.instance_id
                        && fingerprint == &current.fingerprint
                )
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
        )
    }

    pub(crate) fn terminal(
        &self,
        attempt: ReconciliationAttempt,
        outcome: ReconciliationTerminalOutcome,
    ) -> Result<ReconciliationTerminal, ReconciliationEvidenceRejection> {
        self.terminal_with_quarantine(attempt, outcome, None)
    }

    pub(crate) fn artifact_terminal(
        &self,
        attempt: ReconciliationAttempt,
        outcome: ReconciliationTerminalOutcome,
        quarantined_target: Option<TargetDescriptor>,
    ) -> Result<ReconciliationTerminal, ReconciliationEvidenceRejection> {
        if attempt.rung() != ReconciliationRung::RepairArtifact
            || attempt.component() != ReconciliationComponent::Libraries
            || attempt.domain() != GuardianDomain::Library
            || attempt.mode() != GuardianMode::Managed
            || attempt.ownership() != OwnershipClass::LauncherManaged
            || attempt.target().system != StabilizationSystem::Execution
            || attempt.target().kind != TargetKind::Artifact
            || attempt.target().ownership != OwnershipClass::LauncherManaged
        {
            return Err(ReconciliationEvidenceRejection::ScopeMismatch);
        }
        self.terminal_with_quarantine(attempt, outcome, quarantined_target)
    }

    fn terminal_with_quarantine(
        &self,
        attempt: ReconciliationAttempt,
        outcome: ReconciliationTerminalOutcome,
        quarantined_target: Option<TargetDescriptor>,
    ) -> Result<ReconciliationTerminal, ReconciliationEvidenceRejection> {
        let current = self
            .state
            .current_reconciliation_incarnation(&self.lifecycle.instance_id)?;
        let ReconciliationScope::RegisteredInstance {
            instance_id,
            fingerprint,
        } = attempt.scope();
        if instance_id != &self.lifecycle.instance_id || fingerprint != &current.fingerprint {
            return Err(ReconciliationEvidenceRejection::IncarnationMismatch);
        }
        if let Some(quarantined_target) = &quarantined_target {
            let expected = TargetDescriptor::new(
                StabilizationSystem::Execution,
                attempt.target().kind,
                format!("quarantine-{}", attempt.target().id),
                attempt.ownership(),
            );
            if quarantined_target != &expected
                || quarantined_target.ownership != OwnershipClass::LauncherManaged
            {
                return Err(ReconciliationEvidenceRejection::OwnershipMismatch);
            }
        }
        Ok(ReconciliationTerminal::from_attempt(
            attempt,
            outcome,
            quarantined_target,
        ))
    }
}

pub(crate) fn reconciliation_journal_attempt(
    mut entry: OperationJournalEntry,
    attempt: ReconciliationAttempt,
) -> OperationJournalEntry {
    entry.reconciliation_attempt = Some(attempt);
    entry
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
    let quarantined_target = terminal.quarantined_target().cloned();
    let mut entry = GuardianFailureMemoryEntry::observed(
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
    .with_reconciliation_terminal(terminal);
    if let Some(quarantined_target) = quarantined_target {
        entry = entry.with_quarantined_target(quarantined_target);
    }
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
            },
            component,
            target,
            mode,
            OwnershipClass::LauncherManaged,
            observed_at.to_rfc3339(),
            suppression_until.to_rfc3339(),
        );
        attempt
            .validate()
            .map_err(|_| ReconciliationEvidenceRejection::JournalMismatch)?;
        Ok(attempt)
    }

    pub(crate) fn recorded_artifact_repair_failure(
        &self,
        lifecycle: &InstanceLifecycleLease,
        operation_id: &OperationId,
    ) -> Result<RecordedArtifactRepairFailure, ReconciliationEvidenceRejection> {
        self.recorded_reconciliation_failure_at(
            lifecycle,
            operation_id,
            ReconciliationRung::RepairArtifact,
            chrono::Utc::now().fixed_offset(),
        )
        .map(|evidence| RecordedArtifactRepairFailure { evidence })
    }

    pub(crate) fn active_recorded_runtime_artifact_failure(
        &self,
        lifecycle: &InstanceLifecycleLease,
    ) -> Result<RecordedArtifactRepairFailure, ReconciliationEvidenceRejection> {
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
                    } if instance_id == &lifecycle.instance_id
                        && fingerprint == &current.fingerprint
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
        )?;
        Ok(RecordedArtifactRepairFailure { evidence })
    }

    pub(crate) async fn admit_component_rebuild(
        &self,
        evidence: RecordedArtifactRepairFailure,
        operation_id: OperationId,
        suppression_for: chrono::Duration,
    ) -> Result<RegisteredComponentRebuildAdmission, ReconciliationEvidenceRejection> {
        self.admit_component_rebuild_with_config_observer(
            evidence,
            operation_id,
            suppression_for,
            || {},
        )
        .await
    }

    async fn admit_component_rebuild_with_config_observer<AfterConfig>(
        &self,
        evidence: RecordedArtifactRepairFailure,
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
        let predecessor_before_wait = self.recorded_reconciliation_failure_at(
            &evidence.evidence.lifecycle,
            evidence.evidence.terminal.operation_id(),
            ReconciliationRung::RepairArtifact,
            chrono::Utc::now().fixed_offset(),
        )?;
        if predecessor_before_wait.terminal != evidence.evidence.terminal
            || predecessor_before_wait.roots != evidence.evidence.roots
        {
            return Err(ReconciliationEvidenceRejection::JournalMismatch);
        }
        if !std::sync::Arc::ptr_eq(
            &predecessor_before_wait.inventory,
            &evidence.evidence.inventory,
        ) {
            return Err(ReconciliationEvidenceRejection::IncarnationMismatch);
        }
        // Config precedes the shared-component writer: config mutations never acquire
        // session admission, while session admission owns only the component reader.
        let library_root_mutation = self
            .config
            .acquire_mutation()
            .await
            .map_err(|_| ReconciliationEvidenceRejection::RootAuthorityUnavailable)?;
        after_config();
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
        )?;
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
        let authority = self.registered_reconciliation_authority(&predecessor.lifecycle)?;
        let prior = predecessor.terminal;
        let component_state = match prior.component() {
            ReconciliationComponent::Runtime => RegisteredComponentRebuildState::Runtime {
                postcondition_failure_inventory: std::sync::OnceLock::new(),
            },
            ReconciliationComponent::Libraries => RegisteredComponentRebuildState::Libraries,
            ReconciliationComponent::VersionBundle
            | ReconciliationComponent::Assets
            | ReconciliationComponent::WholeInstance => {
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
        )?;
        self.refuse_active_component_rebuild_window(&attempt, observed_at)?;
        Ok(RegisteredComponentRebuildAdmission {
            authority,
            attempt,
            _predecessor: prior,
            known_good,
            component_state,
            _component_mutation: component_mutation,
            _library_root_mutation: library_root_mutation,
        })
    }

    fn refuse_active_component_rebuild_window(
        &self,
        attempt: &ReconciliationAttempt,
        observed_at: chrono::DateTime<chrono::FixedOffset>,
    ) -> Result<(), ReconciliationEvidenceRejection> {
        let matches_suppression = |candidate: &ReconciliationAttempt| {
            candidate.rung() == ReconciliationRung::RebuildComponent
                && if attempt.component() == ReconciliationComponent::Runtime {
                    candidate.component() == ReconciliationComponent::Runtime
                        && candidate.target() == attempt.target()
                } else {
                    reconciliation_attempt_key(candidate) == reconciliation_attempt_key(attempt)
                }
        };
        self.refuse_active_reconciliation_window(observed_at, matches_suppression)
    }

    pub(crate) fn refuse_active_artifact_repair_window(
        &self,
        attempt: &ReconciliationAttempt,
    ) -> Result<(), ReconciliationEvidenceRejection> {
        let key = reconciliation_attempt_key(attempt);
        self.refuse_active_reconciliation_window(
            chrono::Utc::now().fixed_offset(),
            move |candidate| {
                candidate.rung() == ReconciliationRung::RepairArtifact
                    && reconciliation_attempt_key(candidate) == key
            },
        )
    }

    fn refuse_active_reconciliation_window<Matches>(
        &self,
        observed_at: chrono::DateTime<chrono::FixedOffset>,
        matches_suppression: Matches,
    ) -> Result<(), ReconciliationEvidenceRejection>
    where
        Matches: Fn(&ReconciliationAttempt) -> bool,
    {
        let journals = self.journals.list();
        if journals.iter().any(|journal| {
            matches!(
                journal.status,
                OperationStatus::Planned | OperationStatus::Running
            ) && journal.reconciliation_terminal().is_none()
                && journal
                    .reconciliation_attempt()
                    .is_some_and(&matches_suppression)
        }) {
            return Err(ReconciliationEvidenceRejection::JournalMismatch);
        }
        let mut active_journals = Vec::new();
        for journal in journals {
            let Some(terminal) = journal.reconciliation_terminal().cloned() else {
                continue;
            };
            if !matches_suppression(terminal.attempt())
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
            if !matches_suppression(terminal.attempt())
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
        if active_journals.is_empty() {
            Ok(())
        } else {
            Err(ReconciliationEvidenceRejection::SuppressedPriorAttempt)
        }
    }

    fn recorded_reconciliation_failure_at(
        &self,
        lifecycle: &InstanceLifecycleLease,
        operation_id: &OperationId,
        expected_rung: ReconciliationRung,
        observed_at: chrono::DateTime<chrono::FixedOffset>,
    ) -> Result<RecordedReconciliationFailure, ReconciliationEvidenceRejection> {
        if !self.instance_lifecycle_gates.owns(&lifecycle.owner) {
            return Err(ReconciliationEvidenceRejection::ScopeMismatch);
        }
        let instance_id = lifecycle.instance_id.as_str();
        if !is_canonical_instance_id(instance_id) {
            return Err(ReconciliationEvidenceRejection::InvalidInstanceIdentity);
        }
        let before = self.current_reconciliation_incarnation(instance_id)?;
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
        if memory.reconciliation_terminal() != Some(&terminal) {
            return Err(ReconciliationEvidenceRejection::MemoryNotFailed);
        }
        if terminal.rung() != expected_rung {
            return Err(ReconciliationEvidenceRejection::NonAdjacentRung);
        }
        if terminal.outcome() != ReconciliationTerminalOutcome::Failed
            || memory.last_action_kind != Some(GuardianActionKind::Repair)
            || memory.last_action_outcome != Some(FailureMemoryActionOutcome::Failed)
            || memory.repair_attempt_count == 0
        {
            return Err(ReconciliationEvidenceRejection::MemoryNotFailed);
        }
        let expected_key = FailureMemoryKey::for_reconciliation(
            memory.domain,
            &memory.diagnosis_id,
            &memory.target,
            &terminal,
        );
        if expected_key != key {
            return Err(ReconciliationEvidenceRejection::MemoryNotFailed);
        }
        if memory.ownership != OwnershipClass::LauncherManaged
            || memory.target.ownership != OwnershipClass::LauncherManaged
            || terminal.ownership() != OwnershipClass::LauncherManaged
        {
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
        } = terminal.scope();
        if terminal_instance_id != instance_id || fingerprint != &before.fingerprint {
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
        let after = self.current_reconciliation_incarnation(instance_id)?;
        if before.fingerprint != after.fingerprint || before.roots != after.roots {
            return Err(ReconciliationEvidenceRejection::IncarnationMismatch);
        }
        let instance = self
            .instances
            .get(instance_id)
            .filter(|instance| instance.id == instance_id)
            .ok_or(ReconciliationEvidenceRejection::InstanceNotRegistered)?;
        let inventory = self
            .known_good
            .active_inventory(
                &instance.id,
                &instance.version_id,
                &instance.created_at,
                &after.roots.library,
            )
            .ok_or(ReconciliationEvidenceRejection::RootAuthorityUnavailable)?;
        Ok(RecordedReconciliationFailure {
            terminal,
            lifecycle: lifecycle.retained(),
            roots: after.roots,
            inventory,
        })
    }

    fn current_reconciliation_incarnation(
        &self,
        instance_id: &str,
    ) -> Result<CurrentReconciliationIncarnation, ReconciliationEvidenceRejection> {
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
        Ok(CurrentReconciliationIncarnation { fingerprint, roots })
    }
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
    use crate::state::contracts::{JournalId, OperationPhase, OperationStepResult, RollbackState};
    use crate::state::failure_memory::FailureMemorySnapshot;
    use crate::state::{AppStateInit, InstallStore, SessionStore, new_instance};
    use axial_config::{AppPaths, InstanceRegistrySnapshot};
    use axial_minecraft::known_good::{
        KnownGoodArtifactKind, KnownGoodInventory, TestKnownGoodEntry, TestKnownGoodIntegrity,
        TestKnownGoodRoot,
    };
    use std::fs;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    const INSTANCE_ID: &str = "0000000000000001";
    const DIAGNOSIS_ID: DiagnosisId = DiagnosisId::LauncherManagedArtifactCorrupt;

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

    fn libraries_fixture_inventory() -> KnownGoodInventory {
        KnownGoodInventory::from_test_entries([TestKnownGoodEntry {
            root: TestKnownGoodRoot::Libraries,
            path: "org/axial/fixture/1.0.0/fixture-1.0.0.jar".to_string(),
            kind: KnownGoodArtifactKind::Library,
            integrity: TestKnownGoodIntegrity::Sha1 {
                digest: "d5eff5a05903f96145d60e61ffb9cd9159a745ac".to_string(),
                size: b"axial managed Libraries fixture".len() as u64,
            },
        }])
        .expect("Libraries fixture inventory")
    }

    fn activate_libraries_fixture_inventory(
        state: &AppState,
        instance_id: &str,
    ) -> Arc<KnownGoodInventory> {
        state.activate_known_good_inventory_for_test(instance_id, libraries_fixture_inventory());
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
            .expect("active Libraries fixture inventory")
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

    async fn cleanup(fixture: Fixture) {
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

    fn failed_step(target: &TargetDescriptor) -> OperationJournalStep {
        let mut step = OperationJournalStep::new("repair_artifact", OperationPhase::Failed);
        step.result = OperationStepResult::Failed;
        step.changed_target = Some(target.clone());
        step
    }

    async fn persist_failed_journal(
        fixture: &Fixture,
        attempt: &ReconciliationAttempt,
        terminal: ReconciliationTerminal,
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
            "repair_failed",
            terminal,
        )
        .await
        .expect("persist failed reconciliation");
    }

    async fn recorded_runtime_artifact_failure(
        fixture: &Fixture,
        instance_id: &str,
        operation_id: &str,
    ) -> (RecordedArtifactRepairFailure, ReconciliationAttempt) {
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
            .recorded_artifact_repair_failure(&lifecycle, attempt.operation_id())
            .expect("recorded Runtime artifact failure");
        drop((authority, lifecycle));
        (evidence, attempt)
    }

    async fn recorded_libraries_artifact_failure(
        fixture: &Fixture,
        operation_id: &str,
    ) -> (RecordedArtifactRepairFailure, ReconciliationAttempt) {
        let lifecycle = fixture.state.acquire_instance_lifecycle(INSTANCE_ID).await;
        let authority = fixture
            .state
            .registered_reconciliation_authority(&lifecycle)
            .expect("registered Libraries authority");
        let attempt = authority
            .repair_artifact_attempt(
                OperationId::new(operation_id),
                DIAGNOSIS_ID,
                GuardianDomain::Library,
                ReconciliationComponent::Libraries,
                TargetDescriptor::new(
                    StabilizationSystem::Execution,
                    TargetKind::Artifact,
                    "fixture-library",
                    OwnershipClass::LauncherManaged,
                ),
                GuardianMode::Managed,
                chrono::Duration::minutes(30),
            )
            .expect("Libraries artifact attempt");
        let terminal = authority
            .artifact_terminal(attempt.clone(), ReconciliationTerminalOutcome::Failed, None)
            .expect("Libraries artifact failure");
        let reservation = reserve_reconciliation_attempt(
            fixture.failure_memory.as_ref(),
            fixture.journals.as_ref(),
            reconciliation_attempt_key(&attempt),
        )
        .expect("reserve Libraries artifact attempt");
        persist_failed_journal(fixture, &attempt, terminal.clone()).await;
        commit_reconciliation_memory(
            fixture.failure_memory.as_ref(),
            reconciliation_memory_entry(terminal).expect("Libraries artifact memory"),
            &reservation,
        )
        .await
        .expect("commit Libraries artifact memory");
        drop(reservation);
        let evidence = fixture
            .state
            .recorded_artifact_repair_failure(&lifecycle, attempt.operation_id())
            .expect("recorded Libraries artifact failure");
        drop((authority, lifecycle));
        (evidence, attempt)
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
    async fn artifact_failure_proof_requires_exact_current_adjacent_evidence() {
        let fixture = fixture("artifact-proof");
        let (attempt, terminal) = registered_attempt(
            &fixture,
            "artifact-proof",
            ReconciliationComponent::VersionBundle,
        )
        .await;
        let key = reconciliation_attempt_key(&attempt);
        let reservation = reserve_reconciliation_attempt(
            fixture.failure_memory.as_ref(),
            fixture.journals.as_ref(),
            key.clone(),
        )
        .expect("reserve before the effect");
        persist_failed_journal(&fixture, &attempt, terminal.clone()).await;
        commit_reconciliation_memory(
            fixture.failure_memory.as_ref(),
            reconciliation_memory_entry(terminal.clone()).expect("typed failed memory"),
            &reservation,
        )
        .await
        .expect("commit failed memory");
        drop(reservation);

        let lifecycle = fixture.state.acquire_instance_lifecycle(INSTANCE_ID).await;

        let evidence = fixture
            .state
            .recorded_artifact_repair_failure(&lifecycle, attempt.operation_id())
            .expect("exact current evidence mints proof");
        assert_eq!(evidence.terminal(), &terminal);
        drop(evidence);

        let missing_memory = fixture.state.clone().with_reconciliation_stores(
            fixture.journals.clone(),
            Arc::new(GuardianFailureMemoryStore::new()),
        );
        assert_eq!(
            missing_memory
                .recorded_artifact_repair_failure(&lifecycle, attempt.operation_id())
                .err(),
            Some(ReconciliationEvidenceRejection::MemoryMissing)
        );
        drop(missing_memory);

        let missing_journal = fixture.state.clone().with_reconciliation_stores(
            Arc::new(OperationJournalStore::new()),
            fixture.failure_memory.clone(),
        );
        assert_eq!(
            missing_journal
                .recorded_artifact_repair_failure(&lifecycle, attempt.operation_id())
                .err(),
            Some(ReconciliationEvidenceRejection::JournalMissing)
        );
        drop(missing_journal);

        assert_eq!(
            fixture
                .state
                .recorded_artifact_repair_failure(
                    &lifecycle,
                    &OperationId::new("foreign-operation"),
                )
                .err(),
            Some(ReconciliationEvidenceRejection::JournalMissing)
        );
        let foreign = self::fixture("artifact-proof-foreign-lifecycle");
        let foreign_lifecycle = foreign.state.acquire_instance_lifecycle(INSTANCE_ID).await;
        assert_eq!(
            fixture
                .state
                .recorded_artifact_repair_failure(&foreign_lifecycle, attempt.operation_id())
                .err(),
            Some(ReconciliationEvidenceRejection::ScopeMismatch)
        );
        drop(foreign_lifecycle);
        cleanup(foreign).await;

        let stale_at = chrono::DateTime::parse_from_rfc3339(terminal.suppression_until())
            .expect("suppression timestamp");
        assert_eq!(
            fixture
                .state
                .recorded_reconciliation_failure_at(
                    &lifecycle,
                    attempt.operation_id(),
                    ReconciliationRung::RepairArtifact,
                    stale_at,
                )
                .err(),
            Some(ReconciliationEvidenceRejection::MemoryWindowInactive)
        );

        let wrong_journals = Arc::new(OperationJournalStore::new());
        let mut wrong_journal = OperationJournalEntry::new(
            JournalId::new("journal-artifact-proof-wrong"),
            attempt.operation_id().clone(),
            CommandKind::InstallVersion,
            StabilizationSystem::Application,
            OwnershipClass::LauncherManaged,
            RollbackState::NotApplicable,
        );
        wrong_journal.targets.push(attempt.target().clone());
        wrong_journals
            .create(wrong_journal)
            .await
            .expect("create mismatched journal");
        wrong_journals
            .record_failure(
                attempt.operation_id(),
                failed_step(attempt.target()),
                "install_failed",
                OperationOutcome::Failed,
            )
            .await
            .expect("fail mismatched journal");
        let mismatched_journal = fixture
            .state
            .clone()
            .with_reconciliation_stores(wrong_journals, fixture.failure_memory.clone());
        assert_eq!(
            mismatched_journal
                .recorded_artifact_repair_failure(&lifecycle, attempt.operation_id())
                .err(),
            Some(ReconciliationEvidenceRejection::JournalMismatch)
        );
        drop(mismatched_journal);

        let changed_library = fixture.root.join("changed-library");
        fs::create_dir_all(&changed_library).expect("changed library root");
        fixture
            .state
            .set_library_dir_for_test(changed_library.to_string_lossy().into_owned());
        assert_eq!(
            fixture
                .state
                .recorded_artifact_repair_failure(&lifecycle, attempt.operation_id())
                .err(),
            Some(ReconciliationEvidenceRejection::IncarnationMismatch)
        );

        drop(lifecycle);
        cleanup(fixture).await;
    }

    #[tokio::test]
    async fn libraries_component_commit_keeps_exact_root_projection_and_inventory_arc() {
        let fixture = fixture("libraries-component-commit");
        let admitted_inventory = activate_libraries_fixture_inventory(&fixture.state, INSTANCE_ID);
        let (evidence, artifact_attempt) =
            recorded_libraries_artifact_failure(&fixture, "libraries-component-commit-artifact")
                .await;
        let component_operation = OperationId::new("component-admission-rebuild");
        let admission = fixture
            .state
            .admit_component_rebuild(
                evidence,
                component_operation.clone(),
                chrono::Duration::minutes(30),
            )
            .await
            .expect("component rebuild admission");
        let component_attempt = admission.attempt().clone();
        let artifact_terminal = fixture
            .journals
            .get(artifact_attempt.operation_id())
            .and_then(|journal| journal.reconciliation_terminal().cloned())
            .expect("exact Libraries predecessor terminal");

        assert_eq!(admission.predecessor(), &artifact_terminal);
        assert_eq!(component_attempt.operation_id(), &component_operation);
        assert_eq!(
            component_attempt.rung(),
            ReconciliationRung::RebuildComponent
        );
        assert_eq!(
            component_attempt.component(),
            ReconciliationComponent::Libraries
        );
        assert_eq!(component_attempt.target(), artifact_terminal.target());
        assert_eq!(component_attempt.scope(), artifact_terminal.scope());
        assert_eq!(
            component_attempt.diagnosis_id(),
            artifact_terminal.diagnosis_id()
        );
        assert_eq!(component_attempt.domain(), artifact_terminal.domain());
        assert_eq!(component_attempt.mode(), artifact_terminal.mode());
        assert_eq!(component_attempt.ownership(), artifact_terminal.ownership());
        assert!(std::ptr::eq(
            admission.journals(),
            fixture.journals.as_ref()
        ));
        assert!(std::ptr::eq(
            admission.failure_memory(),
            fixture.failure_memory.as_ref()
        ));
        let root = PathBuf::from(fixture.state.library_dir().expect("library root"));
        let request = admission
            .libraries_effect()
            .expect("State-authored Libraries effect");
        assert_eq!(request.core_request(), (root.as_path(), "1.21.1"));

        let wrong_root = fixture.root.join("wrong-libraries-root");
        fs::create_dir_all(&wrong_root).expect("wrong Libraries root");
        let wrong_receipt =
            axial_minecraft::rebuild_managed_libraries_fixture_for_test(&wrong_root, "1.21.1")
                .await
                .expect("wrong-root Libraries receipt");
        assert_eq!(
            admission
                .succeeded_libraries_terminal(&wrong_receipt)
                .await
                .err(),
            Some(ReconciliationEvidenceRejection::JournalMismatch)
        );
        drop(wrong_receipt);

        let receipt = axial_minecraft::rebuild_managed_libraries_fixture_for_test(&root, "1.21.1")
            .await
            .expect("sealed Libraries rebuild receipt");
        let terminal = admission
            .succeeded_libraries_terminal(&receipt)
            .await
            .expect("truthful Libraries success terminal");
        assert_eq!(terminal.attempt(), &component_attempt);
        assert_eq!(terminal.outcome(), ReconciliationTerminalOutcome::Succeeded);
        let instance = fixture.state.instances().get(INSTANCE_ID).unwrap();
        let active = fixture
            .state
            .known_good
            .active_inventory(
                &instance.id,
                &instance.version_id,
                &instance.created_at,
                &root,
            )
            .expect("same inventory remains active");
        assert!(Arc::ptr_eq(&active, &admitted_inventory));

        let canonical = root.join("libraries/org/axial/fixture/1.0.0/fixture-1.0.0.jar");
        let mut corrupted = fs::read(&canonical).expect("read fixture JAR");
        corrupted[0] ^= 0xff;
        fs::write(&canonical, corrupted).expect("corrupt fixture JAR");
        assert_eq!(
            admission.succeeded_libraries_terminal(&receipt).await.err(),
            Some(ReconciliationEvidenceRejection::JournalMismatch)
        );

        drop((receipt, admission));
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
            .admit_component_rebuild(
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
            .admit_component_rebuild(
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
            .admit_component_rebuild(
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
            .admit_component_rebuild(
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
    async fn component_rebuild_admission_retains_library_root_mutation_until_drop() {
        let fixture = fixture("component-admission-config-retention");
        let (evidence, _) = recorded_runtime_artifact_failure(
            &fixture,
            INSTANCE_ID,
            "component-admission-config-retention-artifact",
        )
        .await;
        let admission = fixture
            .state
            .admit_component_rebuild(
                evidence,
                OperationId::new("component-admission-config-retention-rebuild"),
                chrono::Duration::minutes(30),
            )
            .await
            .expect("component admission");
        let replacement_library = fixture.root.join("replacement-library-after-admission");
        fs::create_dir_all(&replacement_library).expect("replacement library root");
        let replacement_library = replacement_library.to_string_lossy().into_owned();
        let (mutation_entered_tx, mut mutation_entered_rx) = tokio::sync::oneshot::channel();
        let state = fixture.state.clone();
        let mutation = tokio::spawn(async move {
            state
                .mutate_config(move |config| {
                    let _ = mutation_entered_tx.send(());
                    config.library_dir = replacement_library;
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
            .admit_component_rebuild(
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
                .admit_component_rebuild(
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
    async fn component_rebuild_admission_suppresses_active_terminal_after_store_reload() {
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
            .recorded_artifact_repair_failure(&lifecycle, artifact_attempt.operation_id())
            .expect("recorded artifact failure");
        let admission = fixture
            .state
            .admit_component_rebuild(
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
        drop((component_reservation, admission));

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
        let restarted_evidence = restarted_state
            .recorded_artifact_repair_failure(&lifecycle, artifact_attempt.operation_id())
            .expect("reloaded artifact failure");

        assert_eq!(
            restarted_state
                .admit_component_rebuild(
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
            .recorded_artifact_repair_failure(&lifecycle, artifact_attempt.operation_id())
            .expect("artifact failure remains available");
        assert_eq!(
            disagreed_state
                .admit_component_rebuild(
                    disagreed_evidence,
                    OperationId::new("component-restart-disagreed"),
                    chrono::Duration::minutes(30),
                )
                .await
                .err(),
            Some(ReconciliationEvidenceRejection::JournalMismatch)
        );

        drop((disagreed_state, restarted_state, authority, lifecycle));
        cleanup(fixture).await;
    }
}
