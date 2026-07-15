use super::contracts::{
    OperationId, OwnershipClass, ReconciliationAttempt, ReconciliationComponent,
    StabilizationSystem, TargetDescriptor, TargetKind,
};
use super::{
    AppState, KnownGoodVerificationLease, KnownGoodVerificationUnavailable,
    ReconciliationEvidenceRejection, RegisteredReconciliationAuthority,
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

const REGISTERED_ARTIFACT_TARGET_DOMAIN: &[u8] = b"axial.guardian.registered-artifact-target.v2";

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
pub(crate) struct RegisteredArtifactRepairCandidate<'a> {
    target: &'a TargetDescriptor,
    domain: GuardianDomain,
}

impl RegisteredArtifactRepairCandidate<'_> {
    pub(crate) const fn target(&self) -> &TargetDescriptor {
        self.target
    }

    pub(crate) const fn domain(&self) -> GuardianDomain {
        self.domain
    }

    #[cfg(test)]
    pub(crate) const fn for_test(
        target: &TargetDescriptor,
        domain: GuardianDomain,
    ) -> RegisteredArtifactRepairCandidate<'_> {
        RegisteredArtifactRepairCandidate { target, domain }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RegisteredArtifactSourceScope {
    Libraries,
    Assets,
}

impl RegisteredArtifactSourceScope {
    fn from_source(root: &KnownGoodRoot, kind: KnownGoodArtifactKind) -> Option<Self> {
        match (root, kind) {
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
            Self::Libraries => GuardianDomain::Library,
            Self::Assets => GuardianDomain::Download,
        }
    }

    const fn component(self) -> ReconciliationComponent {
        match self {
            Self::Libraries => ReconciliationComponent::Libraries,
            Self::Assets => ReconciliationComponent::Assets,
        }
    }

    const fn corrupt_effect(self) -> RegisteredArtifactRepairEffect {
        match self {
            Self::Libraries => RegisteredArtifactRepairEffect::QuarantineRedownload,
            Self::Assets => RegisteredArtifactRepairEffect::ComponentRebuildRequired,
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
    effect: RegisteredArtifactRepairEffect,
    provider_url: String,
    expected_sha1: String,
    expected_size: u64,
    _component_mutation: super::sessions::SharedComponentMutationLease,
    _config_mutation: tokio::sync::OwnedMutexGuard<()>,
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
    RepairSourceUnavailable,
}

impl RegisteredArtifactFindings {
    pub(crate) fn len(&self) -> usize {
        self.findings.len()
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.findings.is_empty()
    }

    pub(crate) fn repair_target(&self) -> Option<&TargetDescriptor> {
        self.selected_repair_finding()
            .map(|finding| &finding.target)
    }

    pub(crate) fn repair_candidate(&self) -> Option<RegisteredArtifactRepairCandidate<'_>> {
        let finding = self.selected_repair_finding()?;
        let source = self
            .authority
            .inventory
            .bind_standalone_leaf_repair_source(finding.observation.inventory_ordinal)
            .ok()?;
        let scope = RegisteredArtifactSourceScope::from_source(source.root(), source.kind())?;
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
            .ok_or(RegisteredArtifactRepairAuthorizationRejection::RepairSourceUnavailable)?;
        if !plan.actions.iter().any(|action| {
            action.kind == GuardianActionKind::Repair
                && action.reason == DiagnosisId::LauncherManagedArtifactCorrupt
                && action.target.as_ref() == Some(&finding.target)
                && plan.prerequisite.affected_targets.contains(&finding.target)
        }) {
            return Err(RegisteredArtifactRepairAuthorizationRejection::AmbiguousFinding);
        }
        self.authority
            .inventory
            .bind_standalone_leaf_repair_source(finding.observation.inventory_ordinal)
            .map_err(|_| RegisteredArtifactRepairAuthorizationRejection::RepairSourceUnavailable)?;
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
                self.authority
                    .inventory
                    .bind_standalone_leaf_repair_source(finding.observation.inventory_ordinal)
                    .is_ok()
            })
            .min_by_key(|finding| finding.observation.inventory_ordinal)
    }

    #[cfg(test)]
    pub(crate) fn observations_for_test(
        &self,
    ) -> impl Iterator<Item = (RegisteredArtifactObservation, &TargetDescriptor)> {
        self.findings
            .iter()
            .map(|finding| (finding.observation, &finding.target))
    }
}

impl RegisteredArtifactRepairAuthorization {
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
        self.effect
    }

    pub(crate) fn mutation(&self) -> &RegisteredArtifactMutationCapability {
        &self.mutation
    }

    pub(crate) fn download_contract(&self) -> (&str, &str, u64) {
        (&self.provider_url, &self.expected_sha1, self.expected_size)
    }

    pub(crate) fn evidence_is_live(&self) -> bool {
        self.authority
            .registered_artifact_findings_are_live(&self.findings)
            && Arc::ptr_eq(&self.inventory, &self.findings.authority.inventory)
            && self
                .inventory
                .bind_standalone_leaf_repair_source(self.observation.inventory_ordinal)
                .is_ok()
            && self.authority.attempt_is_current(&self.attempt)
            && self.mutation.is_current()
    }

    pub(crate) async fn physical_state(&self) -> Option<RegisteredArtifactPhysicalState> {
        self.mutation
            .classify(&self.expected_sha1, self.expected_size)
            .await
    }

    pub(crate) fn terminal(
        &self,
        attempt: super::contracts::ReconciliationAttempt,
        outcome: super::contracts::ReconciliationTerminalOutcome,
        quarantined_target: Option<TargetDescriptor>,
    ) -> Result<super::contracts::ReconciliationTerminal, ReconciliationEvidenceRejection> {
        if outcome == super::contracts::ReconciliationTerminalOutcome::Succeeded
            && !self.evidence_is_live()
        {
            return Err(ReconciliationEvidenceRejection::IncarnationMismatch);
        }
        self.authority
            .artifact_terminal(attempt, outcome, quarantined_target)
    }
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
        let source = inventory
            .bind_standalone_leaf_repair_source(observation.inventory_ordinal)
            .map_err(|_| ReconciliationEvidenceRejection::RootAuthorityUnavailable)?;
        let source_scope = RegisteredArtifactSourceScope::from_source(source.root(), source.kind())
            .ok_or(ReconciliationEvidenceRejection::ScopeMismatch)?;

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
        let source = inventory
            .bind_standalone_leaf_repair_source(observation.inventory_ordinal)
            .map_err(|_| ReconciliationEvidenceRejection::RootAuthorityUnavailable)?;
        let current_source_scope =
            RegisteredArtifactSourceScope::from_source(source.root(), source.kind())
                .ok_or(ReconciliationEvidenceRejection::ScopeMismatch)?;
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
        let provider_url = source.provider_url().to_string();
        let expected_sha1 = source.sha1().as_str().to_string();
        let expected_size = source.size();
        let mutation = RegisteredArtifactMutationCapability::mint(physical_path)
            .await
            .map_err(|_| ReconciliationEvidenceRejection::RootAuthorityUnavailable)?;
        let effect = match observation.condition {
            RegisteredArtifactCondition::Missing => RegisteredArtifactRepairEffect::DownloadMissing,
            RegisteredArtifactCondition::Corrupt => source_scope.corrupt_effect(),
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
            effect,
            provider_url,
            expected_sha1,
            expected_size,
            _component_mutation: component_mutation,
            _config_mutation: config_mutation,
        })
    }
}

fn registered_artifact_target(
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
    let source = inventory
        .bind_standalone_leaf_repair_source(inventory_ordinal)
        .ok()?;
    let source_scope = RegisteredArtifactSourceScope::from_source(source.root(), source.kind())?;
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
    update_frame(
        &mut hasher,
        b"repair_provider_url",
        source.provider_url().as_bytes(),
    );

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

pub(super) fn recorded_artifact_target_has_live_provenance(
    instance_id: &str,
    version_id: &str,
    created_at: &str,
    library_root: &Path,
    runtime_root: &Path,
    inventory: &axial_minecraft::known_good::KnownGoodInventory,
    attempt: &ReconciliationAttempt,
) -> bool {
    if attempt.diagnosis_id() != DiagnosisId::LauncherManagedArtifactCorrupt {
        return false;
    }
    inventory
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
        })
        .filter(|(target, scope)| {
            target == attempt.target()
                && scope.domain() == attempt.domain()
                && scope.component() == attempt.component()
        })
        .take(2)
        .count()
        == 1
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
