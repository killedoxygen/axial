use super::contracts::{
    OperationId, OwnershipClass, ReconciliationAttempt, ReconciliationComponent,
    ReconciliationQuarantineCheckpoint, ReconciliationTerminal, ReconciliationTerminalOutcome,
    StabilizationSystem, TargetDescriptor, TargetKind,
};
use super::failure_memory::FailureMemoryStoreError;
use super::{
    AppState, KnownGoodVerificationLease, KnownGoodVerificationUnavailable,
    OperationJournalStoreError, ReconciliationAttemptReservation, ReconciliationEvidenceRejection,
    RegisteredReconciliationAuthority, commit_reconciliation_memory, reconciliation_memory_entry,
};
use crate::execution::registered_artifact::{
    RegisteredArtifactMutationCapability, RegisteredArtifactPhysicalState,
};
use crate::guardian::{
    DiagnosisId, GuardianActionKind, GuardianDecision, GuardianDomain, GuardianMode,
};
use axial_minecraft::known_good::{
    KnownGoodArtifactKind, KnownGoodIntegrity, KnownGoodRoot, known_good_entry_path,
};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

const REGISTERED_ARTIFACT_TARGET_DOMAIN: &[u8] = b"axial.guardian.registered-artifact-target.v2";
const REGISTERED_ARTIFACT_MEMORY_RETRY_INITIAL_DELAY: Duration = Duration::from_millis(20);
const REGISTERED_ARTIFACT_MEMORY_RETRY_MAX_DELAY: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum RegisteredArtifactCondition {
    Missing,
    Corrupt,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct RegisteredArtifactObservation {
    inventory_ordinal: usize,
    condition: RegisteredArtifactCondition,
}

impl RegisteredArtifactObservation {
    pub(crate) const fn new(
        inventory_ordinal: usize,
        condition: RegisteredArtifactCondition,
    ) -> Self {
        Self {
            inventory_ordinal,
            condition,
        }
    }

    #[cfg(test)]
    pub(crate) const fn inventory_ordinal(self) -> usize {
        self.inventory_ordinal
    }

    #[cfg(test)]
    pub(crate) const fn condition(self) -> RegisteredArtifactCondition {
        self.condition
    }
}

struct RegisteredArtifactFinding {
    observation: RegisteredArtifactObservation,
    target: TargetDescriptor,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RegisteredArtifactRepairCandidate<'a> {
    target: &'a TargetDescriptor,
    domain: GuardianDomain,
}

impl<'a> RegisteredArtifactRepairCandidate<'a> {
    pub(crate) const fn target(self) -> &'a TargetDescriptor {
        self.target
    }

    pub(crate) const fn domain(self) -> GuardianDomain {
        self.domain
    }

    #[cfg(test)]
    pub(crate) const fn for_test(
        target: &'a TargetDescriptor,
        domain: GuardianDomain,
    ) -> RegisteredArtifactRepairCandidate<'a> {
        RegisteredArtifactRepairCandidate { target, domain }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RegisteredArtifactSourceScope {
    VersionBundle,
    Libraries,
    Assets,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct RegisteredArtifactProvenance {
    inventory_ordinal: usize,
    scope: RegisteredArtifactSourceScope,
}

pub(super) struct RegisteredArtifactProvenanceContext<'a> {
    instance_id: &'a str,
    version_id: &'a str,
    created_at: &'a str,
    library_root: &'a Path,
    runtime_root: &'a Path,
    inventory: &'a axial_minecraft::known_good::KnownGoodInventory,
}

impl<'a> RegisteredArtifactProvenanceContext<'a> {
    pub(super) const fn new(
        instance_id: &'a str,
        version_id: &'a str,
        created_at: &'a str,
        library_root: &'a Path,
        runtime_root: &'a Path,
        inventory: &'a axial_minecraft::known_good::KnownGoodInventory,
    ) -> Self {
        Self {
            instance_id,
            version_id,
            created_at,
            library_root,
            runtime_root,
            inventory,
        }
    }
}

impl RegisteredArtifactProvenance {
    pub(super) const fn inventory_ordinal(self) -> usize {
        self.inventory_ordinal
    }

    pub(super) const fn component(self) -> ReconciliationComponent {
        self.scope.component()
    }
}

impl RegisteredArtifactSourceScope {
    fn from_entry(root: &KnownGoodRoot, kind: KnownGoodArtifactKind) -> Option<Self> {
        match (root, kind) {
            (
                KnownGoodRoot::Versions,
                KnownGoodArtifactKind::VersionMetadata | KnownGoodArtifactKind::ClientJar,
            )
            | (KnownGoodRoot::Assets, KnownGoodArtifactKind::LogConfig) => {
                Some(Self::VersionBundle)
            }
            (
                KnownGoodRoot::Libraries,
                KnownGoodArtifactKind::Library | KnownGoodArtifactKind::NativeLibrary,
            ) => Some(Self::Libraries),
            (
                KnownGoodRoot::Assets,
                KnownGoodArtifactKind::AssetIndex | KnownGoodArtifactKind::AssetObject,
            ) => Some(Self::Assets),
            _ => None,
        }
    }

    const fn domain(self) -> GuardianDomain {
        match self {
            Self::VersionBundle => GuardianDomain::Launch,
            Self::Libraries => GuardianDomain::Library,
            Self::Assets => GuardianDomain::Download,
        }
    }

    const fn component(self) -> ReconciliationComponent {
        match self {
            Self::VersionBundle => ReconciliationComponent::VersionBundle,
            Self::Libraries => ReconciliationComponent::Libraries,
            Self::Assets => ReconciliationComponent::Assets,
        }
    }

    const fn effect(
        self,
        condition: RegisteredArtifactCondition,
    ) -> RegisteredArtifactRepairEffect {
        match (self, condition) {
            (Self::VersionBundle, _) => RegisteredArtifactRepairEffect::ComponentRebuildRequired,
            (_, RegisteredArtifactCondition::Missing) => {
                RegisteredArtifactRepairEffect::DownloadMissing
            }
            (Self::Libraries, RegisteredArtifactCondition::Corrupt) => {
                RegisteredArtifactRepairEffect::QuarantineRedownload
            }
            (Self::Assets, RegisteredArtifactCondition::Corrupt) => {
                RegisteredArtifactRepairEffect::ComponentRebuildRequired
            }
        }
    }
}

/// Exact registered-instance integrity evidence. The retained verification lease makes this
/// move-only and keeps its foreground-or-sweep owner plus instance lifecycle alive.
pub(crate) struct RegisteredArtifactFindings {
    state: AppState,
    authority: KnownGoodVerificationLease,
    findings: Vec<RegisteredArtifactFinding>,
}

/// Move-only Guardian authority for one exact repairable finding.
pub(crate) struct RegisteredArtifactRepairAuthorization {
    findings: RegisteredArtifactFindings,
    observation: RegisteredArtifactObservation,
    diagnosis_id: DiagnosisId,
    action: GuardianActionKind,
    target: TargetDescriptor,
}

/// State-owned admission retained by Guardian until the journal and failure memory settle.
pub(crate) struct RegisteredArtifactRepairAdmission {
    authority: RegisteredReconciliationAuthority,
    findings: RegisteredArtifactFindings,
    attempt: super::contracts::ReconciliationAttempt,
    observation: RegisteredArtifactObservation,
    inventory: Arc<axial_minecraft::known_good::KnownGoodInventory>,
    mutation: RegisteredArtifactMutationCapability,
    plan: RegisteredArtifactRepairPlan,
    _component_mutation: super::sessions::SharedComponentMutationLease,
    _config_mutation: tokio::sync::OwnedMutexGuard<()>,
    #[cfg(test)]
    lifetime: Arc<()>,
}

enum RegisteredArtifactRepairPlan {
    DownloadMissing {
        provider_url: String,
        expected_sha1: String,
        expected_size: u64,
    },
    QuarantineRedownload {
        provider_url: String,
        expected_sha1: String,
        expected_size: u64,
    },
    ComponentRebuild {
        expected_sha1: String,
        expected_size: u64,
    },
}

#[derive(Clone, Copy)]
pub(crate) enum RegisteredArtifactRepairPlanRef<'a> {
    Download(RegisteredArtifactDownloadPlan<'a>),
    ComponentRebuildRequired,
}

#[derive(Clone, Copy)]
pub(crate) struct RegisteredArtifactDownloadPlan<'a> {
    provider_url: &'a str,
    expected_sha1: &'a str,
    expected_size: u64,
}

#[must_use]
pub(crate) struct RegisteredArtifactRepairMemoryReceipt {
    terminal: ReconciliationTerminal,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RegisteredArtifactRepairEffect {
    DownloadMissing,
    QuarantineRedownload,
    ComponentRebuildRequired,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RegisteredArtifactRepairAuthorizationRejection {
    LiveAuthorityUnavailable,
    NonManagedRepair,
    InvalidRepairPlan,
    AmbiguousFinding,
    RepairAuthorityUnavailable,
}

impl RegisteredArtifactFindings {
    pub(crate) fn len(&self) -> usize {
        self.findings.len()
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.findings.is_empty()
    }

    pub(crate) fn repair_candidate(&self) -> Option<RegisteredArtifactRepairCandidate<'_>> {
        let finding = self.selected_repair_finding()?;
        let scope = registered_artifact_scope(
            &self.authority.inventory,
            finding.observation.inventory_ordinal,
        )?;
        Some(RegisteredArtifactRepairCandidate {
            target: &finding.target,
            domain: scope.domain(),
        })
    }

    pub(crate) fn target_for(
        &self,
        observation: RegisteredArtifactObservation,
    ) -> Option<&TargetDescriptor> {
        self.findings
            .iter()
            .find(|finding| finding.observation == observation)
            .map(|finding| &finding.target)
    }

    pub(crate) fn authorize_repair(
        self,
        decision: &GuardianDecision,
    ) -> Result<RegisteredArtifactRepairAuthorization, RegisteredArtifactRepairAuthorizationRejection>
    {
        if !self.state.registered_artifact_findings_can_admit(&self) {
            return Err(RegisteredArtifactRepairAuthorizationRejection::LiveAuthorityUnavailable);
        }
        if decision.mode() != GuardianMode::Managed || decision.kind() != GuardianActionKind::Repair
        {
            return Err(RegisteredArtifactRepairAuthorizationRejection::NonManagedRepair);
        }
        let plan = decision
            .action_plan()
            .filter(|plan| {
                plan.owner == StabilizationSystem::Guardian
                    && plan.prerequisite.diagnosis_id == DiagnosisId::LauncherManagedArtifactCorrupt
                    && plan.prerequisite.ownership == OwnershipClass::LauncherManaged
                    && plan
                        .prerequisite
                        .candidate_actions
                        .contains(&GuardianActionKind::Repair)
                    && decision
                        .diagnoses()
                        .contains(&DiagnosisId::LauncherManagedArtifactCorrupt)
            })
            .ok_or(RegisteredArtifactRepairAuthorizationRejection::InvalidRepairPlan)?;

        let finding = self
            .selected_repair_finding()
            .ok_or(RegisteredArtifactRepairAuthorizationRejection::RepairAuthorityUnavailable)?;
        if !plan.actions.iter().any(|action| {
            action.kind == GuardianActionKind::Repair
                && action.reason == DiagnosisId::LauncherManagedArtifactCorrupt
                && action.target.as_ref() == Some(&finding.target)
                && plan.prerequisite.affected_targets.contains(&finding.target)
        }) {
            return Err(RegisteredArtifactRepairAuthorizationRejection::AmbiguousFinding);
        }
        registered_artifact_scope(
            &self.authority.inventory,
            finding.observation.inventory_ordinal,
        )
        .ok_or(RegisteredArtifactRepairAuthorizationRejection::RepairAuthorityUnavailable)?;
        let observation = finding.observation;
        let target = finding.target.clone();
        Ok(RegisteredArtifactRepairAuthorization {
            findings: self,
            observation,
            diagnosis_id: plan.prerequisite.diagnosis_id,
            action: GuardianActionKind::Repair,
            target,
        })
    }

    fn selected_repair_finding(&self) -> Option<&RegisteredArtifactFinding> {
        self.findings
            .iter()
            .filter(|finding| {
                registered_artifact_scope(
                    &self.authority.inventory,
                    finding.observation.inventory_ordinal,
                )
                .is_some()
            })
            .min_by_key(|finding| finding.observation.inventory_ordinal)
    }

    #[cfg(test)]
    pub(crate) fn observations_for_test(
        &self,
    ) -> impl Iterator<
        Item = (
            RegisteredArtifactObservation,
            &TargetDescriptor,
            GuardianDomain,
            ReconciliationComponent,
        ),
    > {
        self.findings.iter().map(|finding| {
            let scope = registered_artifact_scope(
                &self.authority.inventory,
                finding.observation.inventory_ordinal,
            )
            .expect("sealed registered artifact finding retains its exact scope");
            (
                finding.observation,
                &finding.target,
                scope.domain(),
                scope.component(),
            )
        })
    }
}

impl RegisteredArtifactRepairAuthorization {
    pub(super) fn exact_recovery_identity(
        &self,
        state: &AppState,
    ) -> Result<
        (
            &KnownGoodVerificationLease,
            usize,
            ReconciliationComponent,
            RegisteredArtifactRepairEffect,
        ),
        ReconciliationEvidenceRejection,
    > {
        if self.diagnosis_id != DiagnosisId::LauncherManagedArtifactCorrupt
            || self.action != GuardianActionKind::Repair
            || self.findings.target_for(self.observation) != Some(&self.target)
        {
            return Err(ReconciliationEvidenceRejection::ScopeMismatch);
        }
        if !state.registered_artifact_findings_can_admit(&self.findings) {
            return Err(ReconciliationEvidenceRejection::IncarnationMismatch);
        }
        let inventory_ordinal = self.observation.inventory_ordinal;
        let scope =
            registered_artifact_scope(&self.findings.authority.inventory, inventory_ordinal)
                .ok_or(ReconciliationEvidenceRejection::RootAuthorityUnavailable)?;
        if registered_artifact_target(&self.findings.authority, inventory_ordinal).as_ref()
            != Some(&self.target)
        {
            return Err(ReconciliationEvidenceRejection::ScopeMismatch);
        }
        Ok((
            &self.findings.authority,
            inventory_ordinal,
            scope.component(),
            scope.effect(self.observation.condition),
        ))
    }

    fn into_parts(
        self,
    ) -> (
        RegisteredArtifactFindings,
        RegisteredArtifactObservation,
        DiagnosisId,
        GuardianActionKind,
        TargetDescriptor,
    ) {
        (
            self.findings,
            self.observation,
            self.diagnosis_id,
            self.action,
            self.target,
        )
    }
}

impl RegisteredArtifactRepairAdmission {
    pub(crate) fn authority(&self) -> &RegisteredReconciliationAuthority {
        &self.authority
    }

    pub(crate) fn attempt(&self) -> &super::contracts::ReconciliationAttempt {
        &self.attempt
    }

    pub(crate) const fn effect(&self) -> RegisteredArtifactRepairEffect {
        match &self.plan {
            RegisteredArtifactRepairPlan::DownloadMissing { .. } => {
                RegisteredArtifactRepairEffect::DownloadMissing
            }
            RegisteredArtifactRepairPlan::QuarantineRedownload { .. } => {
                RegisteredArtifactRepairEffect::QuarantineRedownload
            }
            RegisteredArtifactRepairPlan::ComponentRebuild { .. } => {
                RegisteredArtifactRepairEffect::ComponentRebuildRequired
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn lifetime_for_test(&self) -> std::sync::Weak<()> {
        Arc::downgrade(&self.lifetime)
    }

    pub(crate) fn mutation(&self) -> &RegisteredArtifactMutationCapability {
        &self.mutation
    }

    pub(crate) fn admit_managed_artifact_mutation(
        &self,
    ) -> Result<
        super::ManagedArtifactMutationAdmission,
        super::ManagedArtifactMutationEpochUnavailable,
    > {
        self.findings
            .state
            .admit_managed_artifact_mutation_for_verification(&self.findings.authority)
    }

    pub(crate) fn plan(&self) -> RegisteredArtifactRepairPlanRef<'_> {
        match &self.plan {
            RegisteredArtifactRepairPlan::DownloadMissing {
                provider_url,
                expected_sha1,
                expected_size,
            }
            | RegisteredArtifactRepairPlan::QuarantineRedownload {
                provider_url,
                expected_sha1,
                expected_size,
            } => RegisteredArtifactRepairPlanRef::Download(RegisteredArtifactDownloadPlan {
                provider_url,
                expected_sha1,
                expected_size: *expected_size,
            }),
            RegisteredArtifactRepairPlan::ComponentRebuild { .. } => {
                RegisteredArtifactRepairPlanRef::ComponentRebuildRequired
            }
        }
    }

    pub(crate) fn evidence_is_live(&self) -> bool {
        self.authority
            .registered_artifact_findings_are_live(&self.findings)
            && Arc::ptr_eq(&self.inventory, &self.findings.authority.inventory)
            && registered_artifact_scope(&self.inventory, self.observation.inventory_ordinal)
                .is_some()
            && self.authority.attempt_is_current(&self.attempt)
            && self.mutation.is_current()
    }

    pub(crate) async fn physical_state(&self) -> Option<RegisteredArtifactPhysicalState> {
        let (expected_sha1, expected_size) = self.expected_integrity();
        self.mutation.classify(expected_sha1, expected_size).await
    }

    fn expected_integrity(&self) -> (&str, u64) {
        match &self.plan {
            RegisteredArtifactRepairPlan::DownloadMissing {
                expected_sha1,
                expected_size,
                ..
            }
            | RegisteredArtifactRepairPlan::QuarantineRedownload {
                expected_sha1,
                expected_size,
                ..
            }
            | RegisteredArtifactRepairPlan::ComponentRebuild {
                expected_sha1,
                expected_size,
            } => (expected_sha1, *expected_size),
        }
    }

    pub(crate) const fn expected_physical_state(&self) -> RegisteredArtifactPhysicalState {
        match self.observation.condition {
            RegisteredArtifactCondition::Missing => RegisteredArtifactPhysicalState::Missing,
            RegisteredArtifactCondition::Corrupt => RegisteredArtifactPhysicalState::Corrupt,
        }
    }

    pub(crate) fn terminal(
        &self,
        attempt: super::contracts::ReconciliationAttempt,
        outcome: super::contracts::ReconciliationTerminalOutcome,
        quarantine_checkpoint: ReconciliationQuarantineCheckpoint,
    ) -> Result<super::contracts::ReconciliationTerminal, ReconciliationEvidenceRejection> {
        if outcome == super::contracts::ReconciliationTerminalOutcome::Succeeded
            && !self.evidence_is_live()
        {
            return Err(ReconciliationEvidenceRejection::IncarnationMismatch);
        }
        self.authority
            .artifact_terminal(attempt, outcome, quarantine_checkpoint)
    }

    pub(crate) async fn commit_terminal_memory(
        &self,
        terminal: ReconciliationTerminal,
        reservation: &ReconciliationAttemptReservation,
    ) -> Result<RegisteredArtifactRepairMemoryReceipt, OperationJournalStoreError> {
        if terminal.attempt() != &self.attempt
            || self
                .authority
                .journals()
                .get(terminal.operation_id())
                .and_then(|journal| journal.reconciliation_terminal().cloned())
                .as_ref()
                != Some(&terminal)
        {
            return Err(invalid_registered_artifact_memory_terminal());
        }
        let memory = reconciliation_memory_entry(terminal.clone())
            .map_err(|_| invalid_registered_artifact_memory_terminal())?;
        let mut delay = REGISTERED_ARTIFACT_MEMORY_RETRY_INITIAL_DELAY;
        loop {
            if self.authority.failure_memory().get(&memory.key).as_ref() == Some(&memory) {
                break;
            }
            match commit_reconciliation_memory(
                self.authority.failure_memory(),
                memory.clone(),
                reservation,
            )
            .await
            {
                Ok(()) => {
                    if self.authority.failure_memory().get(&memory.key).as_ref() == Some(&memory) {
                        break;
                    }
                    return Err(invalid_registered_artifact_memory_terminal());
                }
                Err(FailureMemoryStoreError::Persistence(_)) => {
                    tokio::time::sleep(delay).await;
                    delay = delay
                        .saturating_mul(2)
                        .min(REGISTERED_ARTIFACT_MEMORY_RETRY_MAX_DELAY);
                }
                Err(error) => {
                    return Err(OperationJournalStoreError::Persistence(
                        std::io::Error::other(format!(
                            "Guardian artifact repair memory commit failed: {}",
                            error.class()
                        )),
                    ));
                }
            }
        }
        Ok(RegisteredArtifactRepairMemoryReceipt { terminal })
    }

    pub(crate) fn into_failed_continuation(
        self,
        receipt: RegisteredArtifactRepairMemoryReceipt,
    ) -> Result<super::RegisteredArtifactFailedRepair, ReconciliationEvidenceRejection> {
        if !receipt.matches_failed_attempt(&self.attempt) {
            return Err(ReconciliationEvidenceRejection::JournalMismatch);
        }
        self.authority
            .into_registered_artifact_failed_repair(&self.attempt)
    }
}

impl<'a> RegisteredArtifactDownloadPlan<'a> {
    pub(crate) const fn provider_url(self) -> &'a str {
        self.provider_url
    }

    pub(crate) const fn expected_sha1(self) -> &'a str {
        self.expected_sha1
    }

    pub(crate) const fn expected_size(self) -> u64 {
        self.expected_size
    }
}

impl RegisteredArtifactRepairMemoryReceipt {
    fn matches_failed_attempt(&self, attempt: &ReconciliationAttempt) -> bool {
        self.terminal.attempt() == attempt
            && self.terminal.outcome() == ReconciliationTerminalOutcome::Failed
    }
}

fn invalid_registered_artifact_memory_terminal() -> OperationJournalStoreError {
    OperationJournalStoreError::Persistence(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        "Guardian registered artifact memory terminal is not the exact admitted journal terminal",
    ))
}

impl KnownGoodVerificationLease {
    pub(crate) fn registered_artifact_observation(
        &self,
        inventory_ordinal: usize,
        condition: RegisteredArtifactCondition,
    ) -> Option<RegisteredArtifactObservation> {
        registered_artifact_target(self, inventory_ordinal)
            .map(|_| RegisteredArtifactObservation::new(inventory_ordinal, condition))
    }
}

impl AppState {
    pub(crate) fn seal_registered_artifact_findings(
        &self,
        authority: KnownGoodVerificationLease,
        observations: Vec<RegisteredArtifactObservation>,
    ) -> Result<RegisteredArtifactFindings, KnownGoodVerificationUnavailable> {
        if !self.known_good_verification_lease_can_admit(&authority) {
            return Err(KnownGoodVerificationUnavailable::LiveAuthorityUnavailable);
        }

        let mut seen = BTreeSet::new();
        let mut findings = Vec::with_capacity(observations.len());
        for observation in observations {
            if !seen.insert(observation) {
                return Err(KnownGoodVerificationUnavailable::LiveAuthorityUnavailable);
            }
            let target = registered_artifact_target(&authority, observation.inventory_ordinal)
                .ok_or(KnownGoodVerificationUnavailable::LiveAuthorityUnavailable)?;
            findings.push(RegisteredArtifactFinding {
                observation,
                target,
            });
        }

        if !self.known_good_verification_lease_can_admit(&authority) {
            return Err(KnownGoodVerificationUnavailable::LiveAuthorityUnavailable);
        }
        Ok(RegisteredArtifactFindings {
            state: self.clone(),
            authority,
            findings,
        })
    }

    pub(crate) fn registered_artifact_findings_can_admit(
        &self,
        findings: &RegisteredArtifactFindings,
    ) -> bool {
        self.known_good_verification_lease_can_admit(&findings.authority)
    }

    pub(crate) fn registered_artifact_findings_are_live(
        &self,
        findings: &RegisteredArtifactFindings,
    ) -> bool {
        self.known_good_verification_lease_is_live(&findings.authority)
    }

    pub(crate) async fn admit_registered_artifact_repair(
        &self,
        authorization: RegisteredArtifactRepairAuthorization,
        operation_id: OperationId,
        suppression_for: chrono::Duration,
    ) -> Result<RegisteredArtifactRepairAdmission, ReconciliationEvidenceRejection> {
        let (findings, observation, diagnosis_id, action, target) = authorization.into_parts();
        if action != GuardianActionKind::Repair
            || findings.target_for(observation) != Some(&target)
            || !self.registered_artifact_findings_can_admit(&findings)
        {
            return Err(ReconciliationEvidenceRejection::IncarnationMismatch);
        }
        let inventory = findings.authority.inventory.clone();
        let source_scope = registered_artifact_scope(&inventory, observation.inventory_ordinal)
            .ok_or(ReconciliationEvidenceRejection::RootAuthorityUnavailable)?;

        // Config precedes the shared component writer, matching component rebuild admission.
        let config_mutation = self
            .config
            .acquire_mutation()
            .await
            .map_err(|_| ReconciliationEvidenceRejection::RootAuthorityUnavailable)?;
        let component_mutation = self
            .sessions
            .acquire_shared_component_mutation()
            .await
            .ok_or(ReconciliationEvidenceRejection::ActiveSession)?;

        if !self.registered_artifact_findings_can_admit(&findings)
            || !Arc::ptr_eq(&inventory, &findings.authority.inventory)
        {
            return Err(ReconciliationEvidenceRejection::IncarnationMismatch);
        }
        let current_source_scope =
            registered_artifact_scope(&inventory, observation.inventory_ordinal)
                .ok_or(ReconciliationEvidenceRejection::RootAuthorityUnavailable)?;
        if current_source_scope != source_scope {
            return Err(ReconciliationEvidenceRejection::IncarnationMismatch);
        }
        let entry = inventory
            .entries()
            .get(observation.inventory_ordinal)
            .ok_or(ReconciliationEvidenceRejection::RootAuthorityUnavailable)?;
        let physical_path = known_good_entry_path(
            &findings.authority.library_root,
            &findings.authority.managed_runtime_cache,
            entry,
        );
        let (expected_sha1, expected_size) = match entry.integrity() {
            KnownGoodIntegrity::Sha1 { digest, size }
            | KnownGoodIntegrity::ExactBytes { digest, size } => {
                (digest.as_str().to_string(), *size)
            }
            KnownGoodIntegrity::Directory | KnownGoodIntegrity::LinkTarget(_) => {
                return Err(ReconciliationEvidenceRejection::ScopeMismatch);
            }
        };
        let mutation = RegisteredArtifactMutationCapability::mint(physical_path)
            .await
            .map_err(|_| ReconciliationEvidenceRejection::RootAuthorityUnavailable)?;
        let effect = source_scope.effect(observation.condition);
        let plan = match effect {
            RegisteredArtifactRepairEffect::DownloadMissing => {
                let source = inventory
                    .bind_standalone_leaf_repair_source(observation.inventory_ordinal)
                    .map_err(|_| ReconciliationEvidenceRejection::RootAuthorityUnavailable)?;
                RegisteredArtifactRepairPlan::DownloadMissing {
                    provider_url: source.provider_url().to_string(),
                    expected_sha1,
                    expected_size,
                }
            }
            RegisteredArtifactRepairEffect::QuarantineRedownload => {
                let source = inventory
                    .bind_standalone_leaf_repair_source(observation.inventory_ordinal)
                    .map_err(|_| ReconciliationEvidenceRejection::RootAuthorityUnavailable)?;
                RegisteredArtifactRepairPlan::QuarantineRedownload {
                    provider_url: source.provider_url().to_string(),
                    expected_sha1,
                    expected_size,
                }
            }
            RegisteredArtifactRepairEffect::ComponentRebuildRequired => {
                RegisteredArtifactRepairPlan::ComponentRebuild {
                    expected_sha1,
                    expected_size,
                }
            }
        };
        if !self.registered_artifact_findings_can_admit(&findings)
            || !Arc::ptr_eq(&inventory, &findings.authority.inventory)
            || !mutation.is_current()
        {
            return Err(ReconciliationEvidenceRejection::IncarnationMismatch);
        }
        let authority =
            self.registered_reconciliation_authority_for_verification(&findings.authority)?;
        let attempt = authority.repair_artifact_attempt(
            operation_id,
            diagnosis_id,
            source_scope.domain(),
            source_scope.component(),
            target,
            GuardianMode::Managed,
            suppression_for,
        )?;
        self.refuse_active_artifact_repair_window(&attempt)?;

        Ok(RegisteredArtifactRepairAdmission {
            authority,
            findings,
            attempt,
            observation,
            inventory,
            mutation,
            plan,
            _component_mutation: component_mutation,
            _config_mutation: config_mutation,
            #[cfg(test)]
            lifetime: Arc::new(()),
        })
    }
}

pub(super) fn registered_artifact_target(
    authority: &KnownGoodVerificationLease,
    inventory_ordinal: usize,
) -> Option<TargetDescriptor> {
    let (instance_id, version_id, created_at, library_root, runtime_cache, inventory) =
        authority.execution_parts();
    registered_artifact_target_from_inventory(
        instance_id,
        version_id,
        created_at,
        library_root,
        runtime_cache.root(),
        inventory,
        inventory_ordinal,
    )
    .map(|(target, _)| target)
}

fn registered_artifact_target_from_inventory(
    instance_id: &str,
    version_id: &str,
    created_at: &str,
    library_root: &Path,
    runtime_root: &Path,
    inventory: &axial_minecraft::known_good::KnownGoodInventory,
    inventory_ordinal: usize,
) -> Option<(TargetDescriptor, RegisteredArtifactSourceScope)> {
    let entry = inventory.entries().get(inventory_ordinal)?;
    let source_scope = registered_artifact_scope(inventory, inventory_ordinal)?;
    let source = inventory
        .bind_standalone_leaf_repair_source(inventory_ordinal)
        .ok();
    let inventory_ordinal = u64::try_from(inventory_ordinal).ok()?;

    let mut hasher = Sha256::new();
    update_frame(&mut hasher, b"domain", REGISTERED_ARTIFACT_TARGET_DOMAIN);
    update_frame(&mut hasher, b"instance_id", instance_id.as_bytes());
    update_frame(&mut hasher, b"version_id", version_id.as_bytes());
    update_frame(&mut hasher, b"created_at", created_at.as_bytes());
    update_path_frame(&mut hasher, b"library_root", library_root);
    update_path_frame(&mut hasher, b"runtime_root", runtime_root);
    update_frame(
        &mut hasher,
        b"inventory_ordinal",
        &inventory_ordinal.to_le_bytes(),
    );
    update_frame(
        &mut hasher,
        b"entry_root",
        entry.root().stable_id().as_bytes(),
    );
    update_frame(
        &mut hasher,
        b"entry_scope",
        entry.root().scope_id().as_bytes(),
    );
    update_frame(&mut hasher, b"entry_path", entry.path().as_str().as_bytes());
    update_frame(
        &mut hasher,
        b"entry_kind",
        entry.kind().stable_id().as_bytes(),
    );
    match entry.integrity() {
        KnownGoodIntegrity::Sha1 { digest, size } => {
            update_frame(&mut hasher, b"integrity_kind", b"sha1");
            update_frame(&mut hasher, b"integrity_digest", digest.as_str().as_bytes());
            update_frame(&mut hasher, b"integrity_size", &size.to_le_bytes());
        }
        KnownGoodIntegrity::ExactBytes { digest, size } => {
            update_frame(&mut hasher, b"integrity_kind", b"exact_bytes");
            update_frame(&mut hasher, b"integrity_digest", digest.as_str().as_bytes());
            update_frame(&mut hasher, b"integrity_size", &size.to_le_bytes());
        }
        KnownGoodIntegrity::Directory | KnownGoodIntegrity::LinkTarget(_) => return None,
    }
    if let Some(source) = source {
        update_frame(
            &mut hasher,
            b"repair_provider_url",
            source.provider_url().as_bytes(),
        );
    } else {
        update_frame(&mut hasher, b"repair_authority", b"component_rebuild");
    }

    let hex = format!("{:x}", hasher.finalize());
    let dotted = hex
        .as_bytes()
        .chunks_exact(8)
        .map(|chunk| std::str::from_utf8(chunk).expect("SHA-256 hex is ASCII"))
        .collect::<Vec<_>>()
        .join(".");
    Some((
        TargetDescriptor::new(
            StabilizationSystem::Execution,
            TargetKind::Artifact,
            format!("leaf-v2.{dotted}"),
            OwnershipClass::LauncherManaged,
        ),
        source_scope,
    ))
}

fn registered_artifact_scope(
    inventory: &axial_minecraft::known_good::KnownGoodInventory,
    inventory_ordinal: usize,
) -> Option<RegisteredArtifactSourceScope> {
    let entry = inventory.entries().get(inventory_ordinal)?;
    let scope = RegisteredArtifactSourceScope::from_entry(entry.root(), entry.kind())?;
    match scope {
        RegisteredArtifactSourceScope::VersionBundle => inventory
            .bind_standalone_leaf_repair_source(inventory_ordinal)
            .is_err()
            .then_some(scope),
        RegisteredArtifactSourceScope::Libraries | RegisteredArtifactSourceScope::Assets => {
            let source = inventory
                .bind_standalone_leaf_repair_source(inventory_ordinal)
                .ok()?;
            (source.root() == entry.root() && source.kind() == entry.kind()).then_some(scope)
        }
    }
}

#[cfg(test)]
pub(crate) fn registered_artifact_target_for_test(
    instance_id: &str,
    version_id: &str,
    created_at: &str,
    library_root: &Path,
    runtime_root: &Path,
    inventory: &axial_minecraft::known_good::KnownGoodInventory,
    inventory_ordinal: usize,
) -> Option<TargetDescriptor> {
    registered_artifact_target_from_inventory(
        instance_id,
        version_id,
        created_at,
        library_root,
        runtime_root,
        inventory,
        inventory_ordinal,
    )
    .map(|(target, _)| target)
}

pub(super) fn resolve_recorded_artifact_provenance(
    instance_id: &str,
    version_id: &str,
    created_at: &str,
    library_root: &Path,
    runtime_root: &Path,
    inventory: &axial_minecraft::known_good::KnownGoodInventory,
    attempt: &ReconciliationAttempt,
) -> Option<RegisteredArtifactProvenance> {
    if attempt.diagnosis_id() != DiagnosisId::LauncherManagedArtifactCorrupt {
        return None;
    }
    let mut matches = inventory
        .entries()
        .iter()
        .enumerate()
        .filter_map(|(inventory_ordinal, _)| {
            registered_artifact_target_from_inventory(
                instance_id,
                version_id,
                created_at,
                library_root,
                runtime_root,
                inventory,
                inventory_ordinal,
            )
            .map(|(target, scope)| (inventory_ordinal, target, scope))
        })
        .filter_map(|(inventory_ordinal, target, scope)| {
            (target == *attempt.target()
                && scope.domain() == attempt.domain()
                && scope.component() == attempt.component())
            .then_some(RegisteredArtifactProvenance {
                inventory_ordinal,
                scope,
            })
        })
        .take(2);
    let provenance = matches.next()?;
    matches.next().is_none().then_some(provenance)
}

pub(super) fn recorded_artifact_provenance_matches(
    context: RegisteredArtifactProvenanceContext<'_>,
    attempt: &ReconciliationAttempt,
    provenance: RegisteredArtifactProvenance,
) -> bool {
    registered_artifact_target_from_inventory(
        context.instance_id,
        context.version_id,
        context.created_at,
        context.library_root,
        context.runtime_root,
        context.inventory,
        provenance.inventory_ordinal,
    )
    .is_some_and(|(target, scope)| {
        target == *attempt.target()
            && scope == provenance.scope
            && scope.domain() == attempt.domain()
            && scope.component() == attempt.component()
    })
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
    use axial_minecraft::known_good::{
        KnownGoodInventory, TestKnownGoodEntry, TestKnownGoodIntegrity, TestKnownGoodRoot,
    };

    const ZERO_SHA1: &str = "0000000000000000000000000000000000000000";

    #[test]
    fn version_bundle_kinds_and_conditions_select_provider_free_component_rebuild() {
        let cases = [
            (
                TestKnownGoodRoot::Versions,
                "1.21.1/1.21.1.json",
                KnownGoodArtifactKind::VersionMetadata,
            ),
            (
                TestKnownGoodRoot::Versions,
                "1.21.1/1.21.1.jar",
                KnownGoodArtifactKind::ClientJar,
            ),
            (
                TestKnownGoodRoot::Assets,
                "log_configs/guardian.xml",
                KnownGoodArtifactKind::LogConfig,
            ),
        ];

        for (root, path, kind) in cases {
            let inventory = KnownGoodInventory::from_test_entries([TestKnownGoodEntry {
                root,
                path: path.to_string(),
                kind,
                integrity: TestKnownGoodIntegrity::Sha1 {
                    digest: ZERO_SHA1.to_string(),
                    size: 7,
                },
            }])
            .expect("single VersionBundle entry inventory");
            assert!(
                inventory.bind_standalone_leaf_repair_source(0).is_err(),
                "VersionBundle leaves must not carry standalone providers"
            );
            let scope = registered_artifact_scope(&inventory, 0)
                .expect("provider-free VersionBundle scope");
            assert_eq!(scope, RegisteredArtifactSourceScope::VersionBundle);
            assert_eq!(scope.domain(), GuardianDomain::Launch);
            assert_eq!(scope.component(), ReconciliationComponent::VersionBundle);
            for condition in [
                RegisteredArtifactCondition::Missing,
                RegisteredArtifactCondition::Corrupt,
            ] {
                assert_eq!(
                    scope.effect(condition),
                    RegisteredArtifactRepairEffect::ComponentRebuildRequired,
                    "unexpected repair effect for {kind:?} {condition:?}"
                );
            }
        }
    }
}
