use crate::download::{
    AuthenticatedVersionBundleSource, Downloader, ManagedInstallActivationContractId,
    ManagedInstallPublicationCandidates, ManagedInstallPublicationEvidenceId,
    ManagedReconstructionContext, RegisteredVersionBundleSourceError,
};
use crate::known_good::{
    KnownGoodInventory, KnownGoodReconstructionReceipt, ManagedAssetsReconstruction,
    ManagedKnownGoodComponent, ManagedLibrariesReconstruction, ManagedVersionBundleReconstruction,
    RetainedKnownGoodReconstruction, VersionBundleProjectionAuthority,
};
use crate::managed_component_lifecycle::{
    ManagedComponentCommittedReceipt, ManagedComponentLifecycleOutcome,
    ManagedComponentRolledBackReceipt, publish_managed_component_effect,
};
use crate::managed_component_publication::ComponentRollbackEffect;
use crate::managed_component_table::ManagedComponentKind;
use crate::managed_fs::{ManagedDir, ManagedLibraryOperation};
use crate::managed_publication::ManagedRootPublicationLease;
#[cfg(feature = "test-support")]
use crate::managed_publication::run_publication_blocking;
use crate::version_bundle_publication::{
    DurableVersionBundleAcknowledgementOutcome, DurableVersionBundleEvidence,
    DurableVersionBundleOutcome, VersionBundlePublicationPurpose, VersionBundleTransactionEffect,
    VersionBundleTransactionError, VersionBundleTransactionRecovery,
    VersionBundleTransactionSettledOutcome, acknowledge_durable_version_bundle,
    classify_durable_version_bundle_candidates, durable_version_bundle_root_binding,
    publish_version_bundle, revalidate_settled_version_bundle, settle_version_bundle_publication,
    settled_version_bundle_matches_managed_library,
};
use std::path::Path;
#[cfg(feature = "test-support")]
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum KnownGoodReconstructionError {
    #[error("vanilla known-good reconstruction failed")]
    Vanilla,
    #[error("loader known-good reconstruction failed")]
    Loader,
    #[error("managed root admission failed")]
    ManagedRoot,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReconstructionKind {
    Vanilla,
    Loader,
}

pub struct ManagedLibrariesCommitReceipt {
    authority: Box<CommittedComponentRebuildAuthority>,
}

pub struct ManagedLibrariesRollbackReceipt {
    authority: Box<RolledBackComponentRebuildAuthority>,
}

pub struct ManagedAssetsCommitReceipt {
    authority: Box<CommittedComponentRebuildAuthority>,
}

pub struct ManagedAssetsRollbackReceipt {
    authority: Box<RolledBackComponentRebuildAuthority>,
}

pub struct ManagedVersionBundleCommitReceipt {
    authority: Box<SettledVersionBundleRebuildAuthority>,
}

pub struct ManagedVersionBundleRollbackReceipt {
    authority: Box<SettledVersionBundleRebuildAuthority>,
}

#[must_use = "the discovered Guardian VersionBundle settlement must be acknowledged or dropped"]
pub struct ManagedVersionBundleOrphanSettlement {
    lease: ManagedRootPublicationLease,
    evidence: DurableVersionBundleEvidence,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManagedVersionBundleSettlementOutcome {
    Committed,
    RolledBack {
        effect: ManagedVersionBundleRollbackEffect,
    },
}

#[must_use = "the Guardian VersionBundle orphan outcome must be handled"]
pub enum ManagedVersionBundleOrphanOutcome {
    NoSettlement,
    Settled(ManagedVersionBundleOrphanSettlement),
    Mismatch,
    Indeterminate(ManagedVersionBundleOrphanRecovery),
}

#[must_use = "dropping orphan recovery releases the lease but leaves the settlement intact"]
pub struct ManagedVersionBundleOrphanRecovery {
    state: ManagedVersionBundleOrphanRecoveryState,
}

enum ManagedVersionBundleOrphanRecoveryState {
    Acquire {
        managed_root: ManagedLibraryOperation,
        expected: ExpectedVersionBundleProjection,
    },
    Classify {
        lease: ManagedRootPublicationLease,
        expected: ExpectedVersionBundleProjection,
    },
}

#[must_use = "the durable VersionBundle settlement must be acknowledged or retried"]
pub enum ManagedVersionBundleAcknowledgementOutcome {
    Acknowledged,
    NoSettlement,
    Mismatch,
    Indeterminate(ManagedVersionBundleAcknowledgementRecovery),
}

#[must_use = "dropping acknowledgement recovery leaves the durable settlement intact"]
pub struct ManagedVersionBundleAcknowledgementRecovery {
    state: ManagedVersionBundleAcknowledgementState,
}

enum ManagedVersionBundleAcknowledgementState {
    Acquire {
        managed_root: ManagedLibraryOperation,
        expected: ExpectedVersionBundleSettlement,
    },
    Classify {
        lease: ManagedRootPublicationLease,
        expected: ExpectedVersionBundleSettlement,
    },
    Acknowledge {
        lease: ManagedRootPublicationLease,
        evidence: DurableVersionBundleEvidence,
    },
}

struct ExpectedVersionBundleSettlement {
    projection: ExpectedVersionBundleProjection,
    evidence_id: ManagedInstallPublicationEvidenceId,
    settlement: ManagedVersionBundleExpectedSettlement,
}

struct ExpectedVersionBundleProjection {
    version_id: String,
    inventory: Arc<KnownGoodInventory>,
    activation_contract_id: ManagedInstallActivationContractId,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManagedVersionBundleExpectedSettlement {
    Committed,
    RolledBack,
}

#[must_use = "dropping recovery releases the exact VersionBundle rebuild authority"]
pub struct ManagedVersionBundleRebuildRecovery {
    state: ManagedVersionBundleRebuildRecoveryState,
}

enum ManagedVersionBundleRebuildRecoveryState {
    Restart(VersionBundleRebuildSeed),
    Publication {
        projection: VersionBundleProjectionAuthority,
        publication: VersionBundleTransactionRecovery,
    },
}

struct VersionBundleRebuildSeed {
    managed_root: ManagedDir,
    projection: VersionBundleProjectionAuthority,
    source: AuthenticatedVersionBundleSource,
}

struct VersionBundleRebuildSeedGuard {
    holder: Arc<Mutex<Option<VersionBundleRebuildSeed>>>,
    seed: Option<VersionBundleRebuildSeed>,
}

impl Drop for VersionBundleRebuildSeedGuard {
    fn drop(&mut self) {
        let Some(seed) = self.seed.take() else {
            return;
        };
        *self
            .holder
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(seed);
    }
}

impl VersionBundleRebuildSeedGuard {
    fn take(
        holder: Arc<Mutex<Option<VersionBundleRebuildSeed>>>,
    ) -> Result<Self, ManagedVersionBundleRebuildError> {
        let seed = holder
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .ok_or(ManagedVersionBundleRebuildError::Preparation)?;
        Ok(Self {
            holder,
            seed: Some(seed),
        })
    }

    fn seed(&self) -> &VersionBundleRebuildSeed {
        self.seed.as_ref().expect("live rebuild seed")
    }

    fn take_projection(&mut self) -> VersionBundleProjectionAuthority {
        self.seed.take().expect("live rebuild seed").projection
    }
}

impl std::fmt::Debug for ManagedVersionBundleRebuildRecovery {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = match &self.state {
            ManagedVersionBundleRebuildRecoveryState::Restart(_) => "restart",
            ManagedVersionBundleRebuildRecoveryState::Publication { .. } => "publication",
        };
        formatter
            .debug_struct("ManagedVersionBundleRebuildRecovery")
            .field("state", &state)
            .finish_non_exhaustive()
    }
}

struct SettledVersionBundleRebuildAuthority {
    projection: VersionBundleProjectionAuthority,
    lease: ManagedRootPublicationLease,
    evidence: DurableVersionBundleEvidence,
}

struct CommittedComponentRebuildAuthority {
    projection: KnownGoodReconstructionReceipt,
    terminal: ManagedComponentCommittedReceipt,
}

struct RolledBackComponentRebuildAuthority {
    projection: KnownGoodReconstructionReceipt,
    terminal: ManagedComponentRolledBackReceipt,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManagedLibrariesRollbackEffect {
    None,
    Execution,
    Reconciliation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManagedAssetsRollbackEffect {
    None,
    Execution,
    Reconciliation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManagedVersionBundleRollbackEffect {
    Promotion,
    Postcheck,
    Rollback,
}

pub enum ManagedLibrariesRebuildError {
    Reconstruction(KnownGoodReconstructionError),
    Preparation,
    Indeterminate,
    RolledBack(ManagedLibrariesRollbackReceipt),
}

pub enum ManagedAssetsRebuildError {
    Reconstruction(KnownGoodReconstructionError),
    Preparation,
    Indeterminate,
    RolledBack(ManagedAssetsRollbackReceipt),
}

pub enum ManagedVersionBundleRebuildError {
    Reconstruction(KnownGoodReconstructionError),
    Source,
    Authority,
    LocalPreparation,
    Preparation,
    Interrupted,
    Unsettled,
    Indeterminate(Box<ManagedVersionBundleRebuildRecovery>),
    RolledBack(ManagedVersionBundleRollbackReceipt),
}

impl std::fmt::Debug for ManagedLibrariesCommitReceipt {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ManagedLibrariesCommitReceipt { .. }")
    }
}

impl std::fmt::Debug for ManagedLibrariesRollbackReceipt {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ManagedLibrariesRollbackReceipt { .. }")
    }
}

impl std::fmt::Debug for ManagedAssetsCommitReceipt {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ManagedAssetsCommitReceipt { .. }")
    }
}

impl std::fmt::Debug for ManagedAssetsRollbackReceipt {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ManagedAssetsRollbackReceipt { .. }")
    }
}

impl std::fmt::Debug for ManagedVersionBundleCommitReceipt {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ManagedVersionBundleCommitReceipt { .. }")
    }
}

impl std::fmt::Debug for ManagedVersionBundleRollbackReceipt {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ManagedVersionBundleRollbackReceipt { .. }")
    }
}

impl std::fmt::Debug for ManagedLibrariesRebuildError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Reconstruction(_) => "ManagedLibrariesRebuildError::Reconstruction(..)",
            Self::Preparation => "ManagedLibrariesRebuildError::Preparation",
            Self::Indeterminate => "ManagedLibrariesRebuildError::Indeterminate",
            Self::RolledBack(_) => "ManagedLibrariesRebuildError::RolledBack(..)",
        })
    }
}

impl std::fmt::Debug for ManagedAssetsRebuildError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Reconstruction(_) => "ManagedAssetsRebuildError::Reconstruction(..)",
            Self::Preparation => "ManagedAssetsRebuildError::Preparation",
            Self::Indeterminate => "ManagedAssetsRebuildError::Indeterminate",
            Self::RolledBack(_) => "ManagedAssetsRebuildError::RolledBack(..)",
        })
    }
}

impl std::fmt::Debug for ManagedVersionBundleRebuildError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Reconstruction(_) => "ManagedVersionBundleRebuildError::Reconstruction(..)",
            Self::Source => "ManagedVersionBundleRebuildError::Source",
            Self::Authority => "ManagedVersionBundleRebuildError::Authority",
            Self::LocalPreparation => "ManagedVersionBundleRebuildError::LocalPreparation",
            Self::Preparation => "ManagedVersionBundleRebuildError::Preparation",
            Self::Interrupted => "ManagedVersionBundleRebuildError::Interrupted",
            Self::Unsettled => "ManagedVersionBundleRebuildError::Unsettled",
            Self::Indeterminate(_) => "ManagedVersionBundleRebuildError::Indeterminate(..)",
            Self::RolledBack(_) => "ManagedVersionBundleRebuildError::RolledBack(..)",
        })
    }
}

impl std::fmt::Display for ManagedLibrariesRebuildError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Reconstruction(_) => "managed Libraries reconstruction failed",
            Self::Preparation => "managed Libraries rebuild failed before its canonical effect",
            Self::Indeterminate => "managed Libraries rebuild outcome is indeterminate",
            Self::RolledBack(_) => "managed Libraries rebuild rolled back",
        })
    }
}

impl std::error::Error for ManagedLibrariesRebuildError {}

impl std::fmt::Display for ManagedAssetsRebuildError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Reconstruction(_) => "managed Assets reconstruction failed",
            Self::Preparation => "managed Assets rebuild failed before its canonical effect",
            Self::Indeterminate => "managed Assets rebuild outcome is indeterminate",
            Self::RolledBack(_) => "managed Assets rebuild rolled back",
        })
    }
}

impl std::error::Error for ManagedAssetsRebuildError {}

impl std::fmt::Display for ManagedVersionBundleRebuildError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Reconstruction(_) => "managed VersionBundle reconstruction failed",
            Self::Source => "managed VersionBundle source acquisition failed",
            Self::Authority => "managed VersionBundle authority was rejected",
            Self::LocalPreparation => "managed VersionBundle local preparation failed",
            Self::Preparation => "managed VersionBundle rebuild failed before its canonical effect",
            Self::Interrupted => "managed VersionBundle rebuild was interrupted",
            Self::Unsettled => "managed VersionBundle rebuild could not settle its effect",
            Self::Indeterminate(_) => "managed VersionBundle rebuild outcome is indeterminate",
            Self::RolledBack(_) => "managed VersionBundle rebuild rolled back",
        })
    }
}

impl std::error::Error for ManagedVersionBundleRebuildError {}

impl ManagedLibrariesCommitReceipt {
    pub fn version_id(&self) -> &str {
        self.authority.projection.version_id()
    }

    pub async fn matches_root(&self, expected: &Path) -> bool {
        self.authority.terminal.matches_root(expected).await
    }

    pub fn matches_managed_library(&self, expected: &ManagedLibraryOperation) -> bool {
        self.authority.terminal.matches_managed_library(expected)
    }

    pub fn matches_known_good_inventory(&self, expected: &KnownGoodInventory) -> bool {
        expected
            .managed_component_projection(ManagedKnownGoodComponent::Libraries)
            .is_ok_and(|projection| self.authority.terminal.matches_projection(&projection))
    }

    pub async fn revalidate(&self) -> bool {
        self.authority.terminal.revalidate().await
    }
}

impl ManagedLibrariesRollbackReceipt {
    pub fn version_id(&self) -> &str {
        self.authority.projection.version_id()
    }

    pub async fn matches_root(&self, expected: &Path) -> bool {
        self.authority.terminal.matches_root(expected).await
    }

    pub fn matches_managed_library(&self, expected: &ManagedLibraryOperation) -> bool {
        self.authority.terminal.matches_managed_library(expected)
    }

    pub fn matches_known_good_inventory(&self, expected: &KnownGoodInventory) -> bool {
        expected
            .managed_component_projection(ManagedKnownGoodComponent::Libraries)
            .is_ok_and(|projection| self.authority.terminal.matches_projection(&projection))
    }

    pub fn effect(&self) -> ManagedLibrariesRollbackEffect {
        match self.authority.terminal.rollback_effect() {
            ComponentRollbackEffect::None => ManagedLibrariesRollbackEffect::None,
            ComponentRollbackEffect::Execution => ManagedLibrariesRollbackEffect::Execution,
            ComponentRollbackEffect::Reconciliation => {
                ManagedLibrariesRollbackEffect::Reconciliation
            }
        }
    }
}

impl ManagedAssetsCommitReceipt {
    pub fn version_id(&self) -> &str {
        self.authority.projection.version_id()
    }

    pub async fn matches_root(&self, expected: &Path) -> bool {
        self.authority.terminal.matches_root(expected).await
    }

    pub fn matches_managed_library(&self, expected: &ManagedLibraryOperation) -> bool {
        self.authority.terminal.matches_managed_library(expected)
    }

    pub fn matches_known_good_inventory(&self, expected: &KnownGoodInventory) -> bool {
        expected
            .managed_component_projection(ManagedKnownGoodComponent::Assets)
            .is_ok_and(|projection| self.authority.terminal.matches_projection(&projection))
    }

    pub async fn revalidate(&self) -> bool {
        self.authority.terminal.revalidate().await
    }
}

impl ManagedAssetsRollbackReceipt {
    pub fn version_id(&self) -> &str {
        self.authority.projection.version_id()
    }

    pub async fn matches_root(&self, expected: &Path) -> bool {
        self.authority.terminal.matches_root(expected).await
    }

    pub fn matches_managed_library(&self, expected: &ManagedLibraryOperation) -> bool {
        self.authority.terminal.matches_managed_library(expected)
    }

    pub fn matches_known_good_inventory(&self, expected: &KnownGoodInventory) -> bool {
        expected
            .managed_component_projection(ManagedKnownGoodComponent::Assets)
            .is_ok_and(|projection| self.authority.terminal.matches_projection(&projection))
    }

    pub fn effect(&self) -> ManagedAssetsRollbackEffect {
        match self.authority.terminal.rollback_effect() {
            ComponentRollbackEffect::None => ManagedAssetsRollbackEffect::None,
            ComponentRollbackEffect::Execution => ManagedAssetsRollbackEffect::Execution,
            ComponentRollbackEffect::Reconciliation => ManagedAssetsRollbackEffect::Reconciliation,
        }
    }
}

impl ManagedVersionBundleCommitReceipt {
    pub fn version_id(&self) -> &str {
        self.authority.projection.version_id()
    }

    pub fn matches_managed_library(&self, expected: &ManagedLibraryOperation) -> bool {
        settled_version_bundle_matches_managed_library(&self.authority.lease, expected)
    }

    pub fn matches_known_good_inventory(&self, expected: &KnownGoodInventory) -> bool {
        self.authority
            .projection
            .matches_known_good_inventory(expected)
    }

    pub fn matches_activation_contract(
        &self,
        expected: &ManagedInstallActivationContractId,
    ) -> bool {
        self.authority
            .evidence
            .matches_activation_contract(expected)
    }

    pub async fn revalidate(&self) -> bool {
        let Ok(projection) = self.authority.projection.component_projection() else {
            return false;
        };
        revalidate_settled_version_bundle(&self.authority.lease, projection).await
    }

    pub fn evidence_id(&self) -> ManagedInstallPublicationEvidenceId {
        self.authority.evidence.evidence_id()
    }

    pub async fn acknowledge(self) -> ManagedVersionBundleAcknowledgementOutcome {
        acknowledge_settled_version_bundle_rebuild(*self.authority).await
    }
}

impl ManagedVersionBundleRollbackReceipt {
    pub fn version_id(&self) -> &str {
        self.authority.projection.version_id()
    }

    pub fn matches_managed_library(&self, expected: &ManagedLibraryOperation) -> bool {
        settled_version_bundle_matches_managed_library(&self.authority.lease, expected)
    }

    pub fn matches_known_good_inventory(&self, expected: &KnownGoodInventory) -> bool {
        self.authority
            .projection
            .matches_known_good_inventory(expected)
    }

    pub fn matches_activation_contract(
        &self,
        expected: &ManagedInstallActivationContractId,
    ) -> bool {
        self.authority
            .evidence
            .matches_activation_contract(expected)
    }

    pub fn effect(&self) -> ManagedVersionBundleRollbackEffect {
        managed_version_bundle_rollback_effect(
            self.authority
                .evidence
                .rollback_effect()
                .expect("rollback receipt retains rollback evidence"),
        )
    }

    pub fn evidence_id(&self) -> ManagedInstallPublicationEvidenceId {
        self.authority.evidence.evidence_id()
    }

    pub async fn acknowledge(self) -> ManagedVersionBundleAcknowledgementOutcome {
        acknowledge_settled_version_bundle_rebuild(*self.authority).await
    }
}

impl ManagedVersionBundleOrphanSettlement {
    pub fn evidence_id(&self) -> ManagedInstallPublicationEvidenceId {
        self.evidence.evidence_id()
    }

    pub fn outcome(&self) -> ManagedVersionBundleSettlementOutcome {
        match self.evidence.rollback_effect() {
            None => ManagedVersionBundleSettlementOutcome::Committed,
            Some(effect) => ManagedVersionBundleSettlementOutcome::RolledBack {
                effect: managed_version_bundle_rollback_effect(effect),
            },
        }
    }

    pub async fn acknowledge(self) -> ManagedVersionBundleAcknowledgementOutcome {
        acknowledge_version_bundle_rebuild_state(
            ManagedVersionBundleAcknowledgementState::Acknowledge {
                lease: self.lease,
                evidence: self.evidence,
            },
        )
        .await
    }
}

impl ManagedVersionBundleOrphanRecovery {
    pub async fn retry(self) -> ManagedVersionBundleOrphanOutcome {
        recover_guardian_version_bundle_orphan_state(self.state).await
    }
}

impl ManagedVersionBundleAcknowledgementRecovery {
    pub async fn retry(self) -> ManagedVersionBundleAcknowledgementOutcome {
        acknowledge_version_bundle_rebuild_state(self.state).await
    }
}

impl ExpectedVersionBundleProjection {
    fn from_source(source: &crate::known_good::KnownGoodActivationSource) -> Self {
        Self {
            version_id: source.version_id().to_string(),
            inventory: Arc::clone(source.inventory()),
            activation_contract_id: source.activation_contract_id().clone(),
        }
    }

    fn matches_evidence(&self, evidence: &DurableVersionBundleEvidence) -> bool {
        let Ok(projection) = self
            .inventory
            .managed_component_projection(ManagedKnownGoodComponent::VersionBundle)
        else {
            return false;
        };
        evidence.matches_expected_projection(
            &self.version_id,
            &self.activation_contract_id,
            VersionBundlePublicationPurpose::GuardianRebuild,
            &projection,
        )
    }
}

impl ExpectedVersionBundleSettlement {
    fn from_source(
        source: &crate::known_good::KnownGoodActivationSource,
        evidence_id: &ManagedInstallPublicationEvidenceId,
        settlement: ManagedVersionBundleExpectedSettlement,
    ) -> Self {
        Self {
            projection: ExpectedVersionBundleProjection::from_source(source),
            evidence_id: evidence_id.clone(),
            settlement,
        }
    }

    fn matches_evidence(&self, evidence: &DurableVersionBundleEvidence) -> bool {
        evidence.evidence_id() == self.evidence_id
            && self.projection.matches_evidence(evidence)
            && match self.settlement {
                ManagedVersionBundleExpectedSettlement::Committed => evidence.is_committed(),
                ManagedVersionBundleExpectedSettlement::RolledBack => {
                    evidence.rollback_effect().is_some()
                }
            }
    }

    fn evidence_root_matches(&self, root: &ManagedDir) -> bool {
        if !self
            .evidence_id
            .matches_version_id(&self.projection.version_id)
        {
            return false;
        }
        let (transaction_nonce, settlement_generation, expected_root_binding) =
            self.evidence_id.binding_parts();
        durable_version_bundle_root_binding(root, transaction_nonce, settlement_generation)
            .is_some_and(|observed| observed == expected_root_binding)
    }
}

pub async fn recover_guardian_version_bundle_orphan(
    managed_root: ManagedLibraryOperation,
    expected: &crate::known_good::KnownGoodActivationSource,
) -> ManagedVersionBundleOrphanOutcome {
    recover_guardian_version_bundle_orphan_state(ManagedVersionBundleOrphanRecoveryState::Acquire {
        managed_root,
        expected: ExpectedVersionBundleProjection::from_source(expected),
    })
    .await
}

pub async fn recover_managed_version_bundle_acknowledgement(
    managed_root: ManagedLibraryOperation,
    expected: &crate::known_good::KnownGoodActivationSource,
    expected_settlement: ManagedVersionBundleExpectedSettlement,
    expected_evidence_id: &ManagedInstallPublicationEvidenceId,
) -> ManagedVersionBundleAcknowledgementOutcome {
    acknowledge_version_bundle_rebuild_state(ManagedVersionBundleAcknowledgementState::Acquire {
        managed_root,
        expected: ExpectedVersionBundleSettlement::from_source(
            expected,
            expected_evidence_id,
            expected_settlement,
        ),
    })
    .await
}

async fn acquire_version_bundle_publication_lease(
    managed_root: &ManagedLibraryOperation,
) -> Option<ManagedRootPublicationLease> {
    let guarded_root = managed_root.managed_directory().ok()?;
    ManagedRootPublicationLease::try_acquire(guarded_root)
        .await
        .ok()
        .flatten()
}

async fn recover_guardian_version_bundle_orphan_state(
    mut state: ManagedVersionBundleOrphanRecoveryState,
) -> ManagedVersionBundleOrphanOutcome {
    loop {
        state = match state {
            ManagedVersionBundleOrphanRecoveryState::Acquire {
                managed_root,
                expected,
            } => {
                let Some(lease) = acquire_version_bundle_publication_lease(&managed_root).await
                else {
                    return ManagedVersionBundleOrphanOutcome::Indeterminate(
                        ManagedVersionBundleOrphanRecovery {
                            state: ManagedVersionBundleOrphanRecoveryState::Acquire {
                                managed_root,
                                expected,
                            },
                        },
                    );
                };
                ManagedVersionBundleOrphanRecoveryState::Classify { lease, expected }
            }
            ManagedVersionBundleOrphanRecoveryState::Classify { lease, expected } => {
                let candidates =
                    ManagedInstallPublicationCandidates::one_unchecked(expected.version_id.clone());
                match classify_durable_version_bundle_candidates(
                    lease,
                    candidates,
                    VersionBundlePublicationPurpose::GuardianRebuild,
                )
                .await
                {
                    DurableVersionBundleOutcome::NoEffect(lease) => {
                        drop(lease);
                        return ManagedVersionBundleOrphanOutcome::NoSettlement;
                    }
                    DurableVersionBundleOutcome::Mismatch(lease) => {
                        drop(lease);
                        return ManagedVersionBundleOrphanOutcome::Mismatch;
                    }
                    DurableVersionBundleOutcome::Committed { lease, evidence }
                    | DurableVersionBundleOutcome::RolledBack {
                        lease, evidence, ..
                    } => {
                        if !expected.matches_evidence(&evidence) {
                            drop(lease);
                            return ManagedVersionBundleOrphanOutcome::Mismatch;
                        }
                        return ManagedVersionBundleOrphanOutcome::Settled(
                            ManagedVersionBundleOrphanSettlement { lease, evidence },
                        );
                    }
                    DurableVersionBundleOutcome::Indeterminate(lease) => {
                        return ManagedVersionBundleOrphanOutcome::Indeterminate(
                            ManagedVersionBundleOrphanRecovery {
                                state: ManagedVersionBundleOrphanRecoveryState::Classify {
                                    lease,
                                    expected,
                                },
                            },
                        );
                    }
                }
            }
        };
    }
}

async fn acknowledge_settled_version_bundle_rebuild(
    authority: SettledVersionBundleRebuildAuthority,
) -> ManagedVersionBundleAcknowledgementOutcome {
    let SettledVersionBundleRebuildAuthority {
        projection: _,
        lease,
        evidence,
    } = authority;
    acknowledge_version_bundle_rebuild_state(
        ManagedVersionBundleAcknowledgementState::Acknowledge { lease, evidence },
    )
    .await
}

async fn acknowledge_version_bundle_rebuild_state(
    mut state: ManagedVersionBundleAcknowledgementState,
) -> ManagedVersionBundleAcknowledgementOutcome {
    loop {
        state = match state {
            ManagedVersionBundleAcknowledgementState::Acquire {
                managed_root,
                expected,
            } => {
                let Some(lease) = acquire_version_bundle_publication_lease(&managed_root).await
                else {
                    return ManagedVersionBundleAcknowledgementOutcome::Indeterminate(
                        ManagedVersionBundleAcknowledgementRecovery {
                            state: ManagedVersionBundleAcknowledgementState::Acquire {
                                managed_root,
                                expected,
                            },
                        },
                    );
                };
                ManagedVersionBundleAcknowledgementState::Classify { lease, expected }
            }
            ManagedVersionBundleAcknowledgementState::Classify { lease, expected } => {
                let candidates = ManagedInstallPublicationCandidates::one_unchecked(
                    expected.projection.version_id.clone(),
                );
                match classify_durable_version_bundle_candidates(
                    lease,
                    candidates,
                    VersionBundlePublicationPurpose::GuardianRebuild,
                )
                .await
                {
                    DurableVersionBundleOutcome::NoEffect(lease) => {
                        let root_matches = expected.evidence_root_matches(lease.root());
                        drop(lease);
                        return if root_matches {
                            ManagedVersionBundleAcknowledgementOutcome::NoSettlement
                        } else {
                            ManagedVersionBundleAcknowledgementOutcome::Mismatch
                        };
                    }
                    DurableVersionBundleOutcome::Mismatch(lease) => {
                        drop(lease);
                        return ManagedVersionBundleAcknowledgementOutcome::Mismatch;
                    }
                    DurableVersionBundleOutcome::Committed { lease, evidence }
                    | DurableVersionBundleOutcome::RolledBack {
                        lease, evidence, ..
                    } => {
                        if !expected.matches_evidence(&evidence) {
                            drop(lease);
                            return ManagedVersionBundleAcknowledgementOutcome::Mismatch;
                        }
                        ManagedVersionBundleAcknowledgementState::Acknowledge { lease, evidence }
                    }
                    DurableVersionBundleOutcome::Indeterminate(lease) => {
                        return ManagedVersionBundleAcknowledgementOutcome::Indeterminate(
                            ManagedVersionBundleAcknowledgementRecovery {
                                state: ManagedVersionBundleAcknowledgementState::Classify {
                                    lease,
                                    expected,
                                },
                            },
                        );
                    }
                }
            }
            ManagedVersionBundleAcknowledgementState::Acknowledge { lease, evidence } => {
                return match acknowledge_durable_version_bundle(lease, evidence).await {
                    DurableVersionBundleAcknowledgementOutcome::Acknowledged(lease) => {
                        drop(lease);
                        ManagedVersionBundleAcknowledgementOutcome::Acknowledged
                    }
                    DurableVersionBundleAcknowledgementOutcome::Indeterminate {
                        lease,
                        evidence,
                    } => ManagedVersionBundleAcknowledgementOutcome::Indeterminate(
                        ManagedVersionBundleAcknowledgementRecovery {
                            state: ManagedVersionBundleAcknowledgementState::Acknowledge {
                                lease,
                                evidence,
                            },
                        },
                    ),
                };
            }
        };
    }
}

fn managed_version_bundle_rollback_effect(
    effect: VersionBundleTransactionEffect,
) -> ManagedVersionBundleRollbackEffect {
    match effect {
        VersionBundleTransactionEffect::Promotion => ManagedVersionBundleRollbackEffect::Promotion,
        VersionBundleTransactionEffect::Postcheck => ManagedVersionBundleRollbackEffect::Postcheck,
        VersionBundleTransactionEffect::Rollback => ManagedVersionBundleRollbackEffect::Rollback,
    }
}

pub async fn rebuild_managed_libraries(
    managed_root: ManagedLibraryOperation,
    version_id: &str,
) -> Result<ManagedLibrariesCommitReceipt, ManagedLibrariesRebuildError> {
    let managed_root = managed_root.managed_directory().map_err(|_| {
        ManagedLibrariesRebuildError::Reconstruction(KnownGoodReconstructionError::ManagedRoot)
    })?;
    let reconstruction = prepare_managed_libraries_reconstruction(managed_root, version_id)
        .await
        .map_err(ManagedLibrariesRebuildError::Reconstruction)?;
    publish_managed_libraries_reconstruction(reconstruction).await
}

pub async fn rebuild_managed_assets(
    managed_root: ManagedLibraryOperation,
    version_id: &str,
) -> Result<ManagedAssetsCommitReceipt, ManagedAssetsRebuildError> {
    let managed_root = managed_root.managed_directory().map_err(|_| {
        ManagedAssetsRebuildError::Reconstruction(KnownGoodReconstructionError::ManagedRoot)
    })?;
    let reconstruction = prepare_managed_assets_reconstruction(managed_root, version_id)
        .await
        .map_err(ManagedAssetsRebuildError::Reconstruction)?;
    publish_managed_assets_reconstruction(reconstruction).await
}

pub async fn rebuild_managed_version_bundle(
    managed_root: ManagedLibraryOperation,
    authority: &crate::known_good::KnownGoodActivationSource,
) -> Result<ManagedVersionBundleCommitReceipt, ManagedVersionBundleRebuildError> {
    let version_id = authority.version_id().to_string();
    let reconstruction = match reconstruction_kind(&version_id) {
        ReconstructionKind::Vanilla => {
            prepare_registered_managed_version_bundle_reconstruction(managed_root, authority)
                .await?
        }
        ReconstructionKind::Loader => {
            let reconstruction =
                prepare_loader_managed_version_bundle_reconstruction(managed_root, &version_id)
                    .await
                    .map_err(|error| match error {
                        KnownGoodReconstructionError::ManagedRoot => {
                            ManagedVersionBundleRebuildError::LocalPreparation
                        }
                        KnownGoodReconstructionError::Vanilla
                        | KnownGoodReconstructionError::Loader => {
                            ManagedVersionBundleRebuildError::Reconstruction(error)
                        }
                    })?;
            require_loader_version_bundle_projection(reconstruction, authority)?
        }
    };
    publish_managed_version_bundle_reconstruction(reconstruction).await
}

fn require_loader_version_bundle_projection(
    reconstruction: ManagedVersionBundleReconstruction,
    expected: &crate::known_good::KnownGoodActivationSource,
) -> Result<ManagedVersionBundleReconstruction, ManagedVersionBundleRebuildError> {
    if !reconstruction.matches_known_good_inventory(expected.inventory())
        || reconstruction
            .activation_contract_id()
            .map_or(true, |observed| {
                observed != *expected.activation_contract_id()
            })
    {
        return Err(ManagedVersionBundleRebuildError::Authority);
    }
    Ok(reconstruction)
}

#[cfg(feature = "test-support")]
pub async fn rebuild_managed_libraries_fixture_for_test(
    managed_root: impl Into<PathBuf>,
    version_id: &str,
) -> Result<ManagedLibrariesCommitReceipt, ManagedLibrariesRebuildError> {
    let managed_root = managed_root.into();
    let guarded_root = run_publication_blocking(move || ManagedDir::open_root(&managed_root))
        .await
        .map_err(|_| ManagedLibrariesRebuildError::Preparation)?
        .map_err(|_| ManagedLibrariesRebuildError::Preparation)?;
    let reconstruction = crate::known_good::managed_libraries_reconstruction_fixture_for_test(
        guarded_root,
        version_id,
    )
    .map_err(|_| ManagedLibrariesRebuildError::Preparation)?;
    publish_managed_libraries_reconstruction(reconstruction).await
}

#[cfg(feature = "test-support")]
pub async fn rebuild_registered_managed_libraries_fixture_for_test(
    managed_root: ManagedLibraryOperation,
    version_id: &str,
) -> Result<ManagedLibrariesCommitReceipt, ManagedLibrariesRebuildError> {
    let guarded_root = managed_root
        .managed_directory()
        .map_err(|_| ManagedLibrariesRebuildError::Preparation)?;
    let reconstruction = crate::known_good::managed_libraries_reconstruction_fixture_for_test(
        guarded_root,
        version_id,
    )
    .map_err(|_| ManagedLibrariesRebuildError::Preparation)?;
    publish_managed_libraries_reconstruction(reconstruction).await
}

#[cfg(feature = "test-support")]
pub async fn rebuild_managed_assets_fixture_for_test(
    managed_root: impl Into<PathBuf>,
    version_id: &str,
) -> Result<ManagedAssetsCommitReceipt, ManagedAssetsRebuildError> {
    let managed_root = managed_root.into();
    let guarded_root = run_publication_blocking(move || ManagedDir::open_root(&managed_root))
        .await
        .map_err(|_| ManagedAssetsRebuildError::Preparation)?
        .map_err(|_| ManagedAssetsRebuildError::Preparation)?;
    let reconstruction =
        crate::known_good::managed_assets_reconstruction_fixture_for_test(guarded_root, version_id)
            .await
            .map_err(|_| ManagedAssetsRebuildError::Preparation)?;
    publish_managed_assets_reconstruction(reconstruction).await
}

#[cfg(feature = "test-support")]
pub async fn rebuild_registered_managed_assets_fixture_for_test(
    managed_root: ManagedLibraryOperation,
    version_id: &str,
) -> Result<ManagedAssetsCommitReceipt, ManagedAssetsRebuildError> {
    let guarded_root = managed_root
        .managed_directory()
        .map_err(|_| ManagedAssetsRebuildError::Preparation)?;
    let reconstruction =
        crate::known_good::managed_assets_reconstruction_fixture_for_test(guarded_root, version_id)
            .await
            .map_err(|_| ManagedAssetsRebuildError::Preparation)?;
    publish_managed_assets_reconstruction(reconstruction).await
}

#[cfg(any(test, feature = "test-support"))]
pub async fn rebuild_managed_version_bundle_fixture_for_test(
    managed_root: ManagedLibraryOperation,
    version_id: &str,
) -> Result<ManagedVersionBundleCommitReceipt, ManagedVersionBundleRebuildError> {
    let guarded_root = managed_root
        .managed_directory()
        .map_err(|_| ManagedVersionBundleRebuildError::Preparation)?;
    let reconstruction = crate::known_good::managed_version_bundle_reconstruction_fixture_for_test(
        guarded_root,
        version_id,
    )
    .map_err(|_| ManagedVersionBundleRebuildError::Preparation)?;
    publish_managed_version_bundle_reconstruction(reconstruction).await
}

#[cfg(any(test, feature = "test-support"))]
pub async fn rebuild_managed_version_bundle_fixture_for_source_test(
    managed_root: ManagedLibraryOperation,
    expected: &crate::known_good::KnownGoodActivationSource,
) -> Result<ManagedVersionBundleCommitReceipt, ManagedVersionBundleRebuildError> {
    let guarded_root = managed_root
        .managed_directory()
        .map_err(|_| ManagedVersionBundleRebuildError::LocalPreparation)?;
    let reconstruction =
        crate::known_good::managed_version_bundle_reconstruction_fixture_for_source_test(
            guarded_root,
            expected,
        )
        .map_err(|_| ManagedVersionBundleRebuildError::Authority)?;
    publish_managed_version_bundle_reconstruction(reconstruction).await
}

#[cfg(feature = "test-support")]
pub async fn rebuild_managed_version_bundle_rollback_fixture_for_test(
    managed_root: ManagedLibraryOperation,
    version_id: &str,
) -> Result<ManagedVersionBundleCommitReceipt, ManagedVersionBundleRebuildError> {
    crate::version_bundle_publication::fail_after_promotions_for_test(version_id, 1);
    rebuild_managed_version_bundle_fixture_for_test(managed_root, version_id).await
}

#[cfg(any(test, feature = "test-support"))]
pub async fn rebuild_managed_version_bundle_rollback_fixture_for_source_test(
    managed_root: ManagedLibraryOperation,
    expected: &crate::known_good::KnownGoodActivationSource,
) -> Result<ManagedVersionBundleCommitReceipt, ManagedVersionBundleRebuildError> {
    crate::version_bundle_publication::fail_after_promotions_for_test(expected.version_id(), 1);
    rebuild_managed_version_bundle_fixture_for_source_test(managed_root, expected).await
}

async fn publish_managed_libraries_reconstruction(
    reconstruction: ManagedLibrariesReconstruction,
) -> Result<ManagedLibrariesCommitReceipt, ManagedLibrariesRebuildError> {
    tokio::spawn(publish_managed_libraries_reconstruction_owned(
        reconstruction,
    ))
    .await
    .map_err(|_| ManagedLibrariesRebuildError::Indeterminate)?
}

#[cfg(test)]
async fn publish_managed_libraries_reconstruction_with_start_signal(
    reconstruction: ManagedLibrariesReconstruction,
    started: tokio::sync::oneshot::Sender<()>,
) -> Result<ManagedLibrariesCommitReceipt, ManagedLibrariesRebuildError> {
    tokio::spawn(async move {
        let _ = started.send(());
        publish_managed_libraries_reconstruction_owned(reconstruction).await
    })
    .await
    .map_err(|_| ManagedLibrariesRebuildError::Indeterminate)?
}

async fn publish_managed_libraries_reconstruction_owned(
    reconstruction: ManagedLibrariesReconstruction,
) -> Result<ManagedLibrariesCommitReceipt, ManagedLibrariesRebuildError> {
    let (managed_root, projection, sources) = reconstruction.into_effect_parts();
    let lease = ManagedRootPublicationLease::acquire(managed_root)
        .await
        .map_err(|_| ManagedLibrariesRebuildError::Preparation)?;
    let libraries = projection
        .component_projection(ManagedKnownGoodComponent::Libraries)
        .map_err(|_| ManagedLibrariesRebuildError::Preparation)?;
    match publish_managed_component_effect(
        lease,
        libraries,
        ManagedComponentKind::Libraries,
        sources,
    )
    .await
    .map_err(|_| ManagedLibrariesRebuildError::Preparation)?
    {
        ManagedComponentLifecycleOutcome::Committed(terminal) => {
            Ok(ManagedLibrariesCommitReceipt {
                authority: Box::new(CommittedComponentRebuildAuthority {
                    projection,
                    terminal,
                }),
            })
        }
        ManagedComponentLifecycleOutcome::RolledBack(terminal) => Err(
            ManagedLibrariesRebuildError::RolledBack(ManagedLibrariesRollbackReceipt {
                authority: Box::new(RolledBackComponentRebuildAuthority {
                    projection,
                    terminal,
                }),
            }),
        ),
    }
}

async fn publish_managed_assets_reconstruction(
    reconstruction: ManagedAssetsReconstruction,
) -> Result<ManagedAssetsCommitReceipt, ManagedAssetsRebuildError> {
    tokio::spawn(publish_managed_assets_reconstruction_owned(reconstruction))
        .await
        .map_err(|_| ManagedAssetsRebuildError::Indeterminate)?
}

#[cfg(test)]
async fn publish_managed_assets_reconstruction_with_start_signal(
    reconstruction: ManagedAssetsReconstruction,
    started: tokio::sync::oneshot::Sender<()>,
) -> Result<ManagedAssetsCommitReceipt, ManagedAssetsRebuildError> {
    tokio::spawn(async move {
        let _ = started.send(());
        publish_managed_assets_reconstruction_owned(reconstruction).await
    })
    .await
    .map_err(|_| ManagedAssetsRebuildError::Indeterminate)?
}

async fn publish_managed_assets_reconstruction_owned(
    reconstruction: ManagedAssetsReconstruction,
) -> Result<ManagedAssetsCommitReceipt, ManagedAssetsRebuildError> {
    let (managed_root, projection, sources) = reconstruction.into_effect_parts();
    let lease = ManagedRootPublicationLease::acquire(managed_root)
        .await
        .map_err(|_| ManagedAssetsRebuildError::Preparation)?;
    let assets = projection
        .component_projection(ManagedKnownGoodComponent::Assets)
        .map_err(|_| ManagedAssetsRebuildError::Preparation)?;
    match publish_managed_component_effect(lease, assets, ManagedComponentKind::Assets, sources)
        .await
        .map_err(|_| ManagedAssetsRebuildError::Preparation)?
    {
        ManagedComponentLifecycleOutcome::Committed(terminal) => Ok(ManagedAssetsCommitReceipt {
            authority: Box::new(CommittedComponentRebuildAuthority {
                projection,
                terminal,
            }),
        }),
        ManagedComponentLifecycleOutcome::RolledBack(terminal) => Err(
            ManagedAssetsRebuildError::RolledBack(ManagedAssetsRollbackReceipt {
                authority: Box::new(RolledBackComponentRebuildAuthority {
                    projection,
                    terminal,
                }),
            }),
        ),
    }
}

async fn publish_managed_version_bundle_reconstruction(
    reconstruction: ManagedVersionBundleReconstruction,
) -> Result<ManagedVersionBundleCommitReceipt, ManagedVersionBundleRebuildError> {
    let (managed_root, projection, source) = reconstruction.into_effect_parts();
    run_managed_version_bundle_rebuild_seed(VersionBundleRebuildSeed {
        managed_root,
        projection,
        source,
    })
    .await
}

async fn run_managed_version_bundle_rebuild_seed(
    seed: VersionBundleRebuildSeed,
) -> Result<ManagedVersionBundleCommitReceipt, ManagedVersionBundleRebuildError> {
    let holder = Arc::new(Mutex::new(Some(seed)));
    let worker_holder = Arc::clone(&holder);
    match tokio::spawn(async move {
        let guard = VersionBundleRebuildSeedGuard::take(worker_holder)?;
        publish_managed_version_bundle_rebuild_seed_owned(guard).await
    })
    .await
    {
        Ok(result) => result,
        Err(_) => {
            let seed = holder
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take()
                .ok_or(ManagedVersionBundleRebuildError::Interrupted)?;
            Err(ManagedVersionBundleRebuildError::Indeterminate(Box::new(
                ManagedVersionBundleRebuildRecovery {
                    state: ManagedVersionBundleRebuildRecoveryState::Restart(seed),
                },
            )))
        }
    }
}

async fn publish_managed_version_bundle_rebuild_seed_owned(
    mut seed: VersionBundleRebuildSeedGuard,
) -> Result<ManagedVersionBundleCommitReceipt, ManagedVersionBundleRebuildError> {
    let lease = ManagedRootPublicationLease::acquire(seed.seed().managed_root.clone())
        .await
        .map_err(|_| ManagedVersionBundleRebuildError::Preparation)?;
    let publication = {
        let activation_contract_id = seed
            .seed()
            .projection
            .activation_contract_id()
            .map_err(|_| ManagedVersionBundleRebuildError::Preparation)?;
        let version_bundle = seed
            .seed()
            .projection
            .component_projection()
            .map_err(|_| ManagedVersionBundleRebuildError::Preparation)?;
        publish_version_bundle(
            lease,
            seed.seed().source.clone(),
            activation_contract_id,
            VersionBundlePublicationPurpose::GuardianRebuild,
            version_bundle,
        )
        .await
    };
    let settled = match settle_version_bundle_publication(publication).await {
        Ok(settled) => settled,
        Err(VersionBundleTransactionError::Indeterminate(publication)) => {
            let projection = seed.take_projection();
            return Err(ManagedVersionBundleRebuildError::Indeterminate(Box::new(
                ManagedVersionBundleRebuildRecovery {
                    state: ManagedVersionBundleRebuildRecoveryState::Publication {
                        projection,
                        publication,
                    },
                },
            )));
        }
        Err(error) => return Err(classify_version_bundle_settlement_error(error)),
    };
    let projection = seed.take_projection();
    settled_version_bundle_rebuild(projection, settled)
}

fn settled_version_bundle_rebuild(
    projection: VersionBundleProjectionAuthority,
    settled: VersionBundleTransactionSettledOutcome,
) -> Result<ManagedVersionBundleCommitReceipt, ManagedVersionBundleRebuildError> {
    match settled {
        VersionBundleTransactionSettledOutcome::Committed { lease, evidence } => {
            Ok(ManagedVersionBundleCommitReceipt {
                authority: Box::new(SettledVersionBundleRebuildAuthority {
                    projection,
                    lease,
                    evidence,
                }),
            })
        }
        VersionBundleTransactionSettledOutcome::RolledBack { lease, evidence } => Err(
            ManagedVersionBundleRebuildError::RolledBack(ManagedVersionBundleRollbackReceipt {
                authority: Box::new(SettledVersionBundleRebuildAuthority {
                    projection,
                    lease,
                    evidence,
                }),
            }),
        ),
    }
}

impl ManagedVersionBundleRebuildRecovery {
    pub async fn retry(
        self,
    ) -> Result<ManagedVersionBundleCommitReceipt, ManagedVersionBundleRebuildError> {
        self.retry_owned().await
    }

    async fn retry_owned(
        self,
    ) -> Result<ManagedVersionBundleCommitReceipt, ManagedVersionBundleRebuildError> {
        match self.state {
            ManagedVersionBundleRebuildRecoveryState::Restart(seed) => {
                run_managed_version_bundle_rebuild_seed(seed).await
            }
            ManagedVersionBundleRebuildRecoveryState::Publication {
                projection,
                publication,
            } => match publication.retry().await {
                Ok(settled) => settled_version_bundle_rebuild(projection, settled),
                Err(VersionBundleTransactionError::Indeterminate(publication)) => Err(
                    ManagedVersionBundleRebuildError::Indeterminate(Box::new(Self {
                        state: ManagedVersionBundleRebuildRecoveryState::Publication {
                            projection,
                            publication,
                        },
                    })),
                ),
                Err(error) => Err(classify_version_bundle_settlement_error(error)),
            },
        }
    }
}

fn classify_version_bundle_settlement_error(
    error: VersionBundleTransactionError,
) -> ManagedVersionBundleRebuildError {
    match error {
        VersionBundleTransactionError::TaskStopped => ManagedVersionBundleRebuildError::Interrupted,
        VersionBundleTransactionError::UnacknowledgedSettlement
        | VersionBundleTransactionError::RecoveryAmbiguous
        | VersionBundleTransactionError::RecoveryUnsettled => {
            ManagedVersionBundleRebuildError::Unsettled
        }
        VersionBundleTransactionError::Indeterminate(_) => {
            unreachable!("indeterminate publication retains its recovery before classification")
        }
        VersionBundleTransactionError::ProjectionMismatch
        | VersionBundleTransactionError::PortablePathAlias
        | VersionBundleTransactionError::LaneOccupied
        | VersionBundleTransactionError::Preparation
        | VersionBundleTransactionError::Effect(_) => ManagedVersionBundleRebuildError::Preparation,
    }
}

pub async fn reconstruct_known_good(
    version_id: &str,
) -> Result<KnownGoodReconstructionReceipt, KnownGoodReconstructionError> {
    match reconstruction_kind(version_id) {
        ReconstructionKind::Vanilla => Downloader::source_only()
            .reconstruct_version(version_id)
            .await
            .map_err(|_| KnownGoodReconstructionError::Vanilla),
        ReconstructionKind::Loader => crate::loaders::reconstruct_build(version_id)
            .await
            .map_err(|_| KnownGoodReconstructionError::Loader),
    }
}

async fn prepare_managed_libraries_reconstruction(
    managed_root: ManagedDir,
    version_id: &str,
) -> Result<ManagedLibrariesReconstruction, KnownGoodReconstructionError> {
    let (reconstruction, guarded_root, context, kind) =
        prepare_managed_reconstruction(managed_root, version_id, ManagedComponentKind::Libraries)
            .await?;
    reconstruction
        .bind_managed_libraries(guarded_root, context.take_library_cache_proofs())
        .map_err(|_| reconstruction_error_for(kind))
}

async fn prepare_managed_assets_reconstruction(
    managed_root: ManagedDir,
    version_id: &str,
) -> Result<ManagedAssetsReconstruction, KnownGoodReconstructionError> {
    let (reconstruction, guarded_root, context, kind) =
        prepare_managed_reconstruction(managed_root, version_id, ManagedComponentKind::Assets)
            .await?;
    let (sources, cache_proofs) = context
        .take_assets_authority()
        .map_err(|_| reconstruction_error_for(kind))?;
    reconstruction
        .bind_managed_assets(guarded_root, sources, cache_proofs)
        .map_err(|_| reconstruction_error_for(kind))
}

async fn prepare_registered_managed_version_bundle_reconstruction(
    managed_root: ManagedLibraryOperation,
    authority: &crate::known_good::KnownGoodActivationSource,
) -> Result<ManagedVersionBundleReconstruction, ManagedVersionBundleRebuildError> {
    let version_id = authority.version_id().to_string();
    let expected = authority.inventory().clone();
    let activation_contract_id = authority.activation_contract_id().clone();
    let guarded_root = managed_root
        .managed_directory()
        .map_err(|_| ManagedVersionBundleRebuildError::LocalPreparation)?;
    let source = Downloader::source_only()
        .reconstruct_registered_version_bundle_source(guarded_root.clone(), &version_id, &expected)
        .await
        .map_err(|error| match error {
            RegisteredVersionBundleSourceError::Source => ManagedVersionBundleRebuildError::Source,
            RegisteredVersionBundleSourceError::Authority => {
                ManagedVersionBundleRebuildError::Authority
            }
            RegisteredVersionBundleSourceError::LocalPreparation => {
                ManagedVersionBundleRebuildError::LocalPreparation
            }
        })?;
    ManagedVersionBundleReconstruction::from_registered(
        guarded_root,
        &version_id,
        expected,
        activation_contract_id,
        source,
    )
    .map_err(|_| ManagedVersionBundleRebuildError::Authority)
}

async fn prepare_loader_managed_version_bundle_reconstruction(
    managed_root: ManagedLibraryOperation,
    version_id: &str,
) -> Result<ManagedVersionBundleReconstruction, KnownGoodReconstructionError> {
    let kind = ReconstructionKind::Loader;
    let guarded_root = managed_root
        .managed_directory()
        .map_err(|_| KnownGoodReconstructionError::ManagedRoot)?;
    let context = ManagedReconstructionContext::version_bundle();
    let reconstruction = reconstruct_managed_authority(version_id, &context, kind).await?;
    reconstruction
        .bind_managed_version_bundle(guarded_root)
        .map_err(|_| reconstruction_error_for(kind))
}

async fn prepare_managed_reconstruction(
    guarded_root: ManagedDir,
    version_id: &str,
    component: ManagedComponentKind,
) -> Result<
    (
        RetainedKnownGoodReconstruction,
        ManagedDir,
        ManagedReconstructionContext,
        ReconstructionKind,
    ),
    KnownGoodReconstructionError,
> {
    let kind = reconstruction_kind(version_id);
    let context = match component {
        ManagedComponentKind::Libraries => {
            ManagedReconstructionContext::bind_libraries(guarded_root.clone()).await
        }
        ManagedComponentKind::Assets => {
            ManagedReconstructionContext::bind_assets(guarded_root.clone()).await
        }
    }
    .map_err(|_| KnownGoodReconstructionError::ManagedRoot)?;
    let reconstruction = reconstruct_managed_authority(version_id, &context, kind).await?;
    Ok((reconstruction, guarded_root, context, kind))
}

async fn reconstruct_managed_authority(
    version_id: &str,
    context: &ManagedReconstructionContext,
    kind: ReconstructionKind,
) -> Result<RetainedKnownGoodReconstruction, KnownGoodReconstructionError> {
    match kind {
        ReconstructionKind::Vanilla => Downloader::source_only()
            .reconstruct_version_authority(version_id, context)
            .await
            .map_err(|_| KnownGoodReconstructionError::Vanilla),
        ReconstructionKind::Loader => {
            crate::loaders::reconstruct_managed_component(version_id, context)
                .await
                .map_err(|_| KnownGoodReconstructionError::Loader)
        }
    }
}

fn reconstruction_error_for(kind: ReconstructionKind) -> KnownGoodReconstructionError {
    match kind {
        ReconstructionKind::Vanilla => KnownGoodReconstructionError::Vanilla,
        ReconstructionKind::Loader => KnownGoodReconstructionError::Loader,
    }
}

fn reconstruction_kind(version_id: &str) -> ReconstructionKind {
    if crate::loaders::api::is_reserved_installed_loader_id(version_id) {
        ReconstructionKind::Loader
    } else {
        ReconstructionKind::Vanilla
    }
}

#[cfg(test)]
mod tests {
    use super::{
        KnownGoodReconstructionError, ReconstructionKind, reconstruct_known_good,
        reconstruction_kind,
    };
    use crate::managed_fs::ManagedLibraryTestAuthority;
    use sha1::{Digest as _, Sha1};
    use std::fs;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    fn registered_authority(
        version_id: &str,
        inventory: Arc<crate::known_good::KnownGoodInventory>,
        contract_byte: u8,
    ) -> crate::known_good::KnownGoodActivationSource {
        crate::known_good::KnownGoodActivationSource::from_registered_snapshot(
            version_id,
            inventory,
            crate::ManagedInstallActivationContractId::from_digest([contract_byte; 32]),
        )
        .expect("registered authority fixture")
    }

    fn version_bundle_fixture_activation_source(
        version_id: &str,
    ) -> crate::known_good::KnownGoodActivationSource {
        crate::known_good::managed_install_reconstruction_receipt_fixture_for_test(version_id)
            .expect("VersionBundle activation fixture")
            .into_activation_source()
    }

    async fn checkpoint_and_ack_version_bundle(
        managed_root: crate::managed_fs::ManagedLibraryOperation,
        version_id: &str,
    ) {
        let source = version_bundle_fixture_activation_source(version_id);
        tokio::time::timeout(std::time::Duration::from_secs(60), async {
            let mut outcome =
                super::recover_guardian_version_bundle_orphan(managed_root, &source).await;
            loop {
                match outcome {
                    super::ManagedVersionBundleOrphanOutcome::Settled(settlement) => {
                        acknowledge_version_bundle(settlement.acknowledge().await).await;
                        return;
                    }
                    super::ManagedVersionBundleOrphanOutcome::Indeterminate(recovery) => {
                        outcome = recovery.retry().await;
                    }
                    super::ManagedVersionBundleOrphanOutcome::NoSettlement
                    | super::ManagedVersionBundleOrphanOutcome::Mismatch => {
                        panic!("checkpointed Guardian publication has no exact witness");
                    }
                }
            }
        })
        .await
        .expect("checkpointed publication acknowledgement should settle");
    }

    async fn acknowledge_version_bundle(
        mut outcome: super::ManagedVersionBundleAcknowledgementOutcome,
    ) {
        loop {
            match outcome {
                super::ManagedVersionBundleAcknowledgementOutcome::Acknowledged => return,
                super::ManagedVersionBundleAcknowledgementOutcome::Indeterminate(recovery) => {
                    outcome = recovery.retry().await;
                }
                super::ManagedVersionBundleAcknowledgementOutcome::NoSettlement
                | super::ManagedVersionBundleAcknowledgementOutcome::Mismatch => {
                    panic!("exact retained settlement was not acknowledged");
                }
            }
        }
    }

    async fn component_owner_is_terminal(
        root: &std::path::Path,
        lane_name: &str,
        canonical: &std::path::Path,
    ) -> bool {
        if !canonical.is_file() {
            return false;
        }
        let lane = root.join(".axial-publication").join(lane_name);
        let lane_settled = fs::read_dir(&lane).is_ok_and(|entries| {
            let mut names = entries
                .filter_map(Result::ok)
                .filter_map(|entry| entry.file_name().into_string().ok())
                .collect::<Vec<_>>();
            names.sort();
            names == ["ancestors", "quarantine", "staging", "table"]
                && ["quarantine", "staging", "table"]
                    .into_iter()
                    .all(|directory| {
                        fs::read_dir(lane.join(directory))
                            .is_ok_and(|mut entries| entries.next().is_none())
                    })
                && ["records", "staging"].into_iter().all(|directory| {
                    fs::read_dir(lane.join("ancestors").join(directory))
                        .is_ok_and(|mut entries| entries.next().is_none())
                })
        });
        if !lane_settled {
            return false;
        }
        let root = root.to_path_buf();
        matches!(
            crate::managed_publication::run_publication_blocking(move || {
                let Ok(root) = crate::managed_fs::ManagedDir::open_root(&root) else {
                    return false;
                };
                crate::managed_publication::ManagedRootPublicationReadLease::acquire(root).is_ok()
            })
            .await,
            Ok(true)
        )
    }

    async fn assert_version_bundle_intent_retry(version_id: &str, inject: impl FnOnce(&str)) {
        let managed = tempfile::tempdir().expect("managed root");
        let authority =
            ManagedLibraryTestAuthority::open(managed.path()).expect("guard intent retry root");
        let guarded_root = authority
            .managed_directory()
            .expect("project intent retry root");
        let reconstruction =
            crate::known_good::managed_version_bundle_reconstruction_fixture_for_test(
                guarded_root,
                version_id,
            )
            .expect("intent retry reconstruction");
        inject(version_id);

        let receipt = super::publish_managed_version_bundle_reconstruction(reconstruction)
            .await
            .expect("intent-boundary retry should commit");

        assert!(receipt.revalidate().await);
        drop(receipt);
        checkpoint_and_ack_version_bundle(authority.operation().clone(), version_id).await;
        drop(authority);
        let lane = managed.path().join(".axial-publication/version-bundle");
        let mut names = fs::read_dir(&lane)
            .expect("intent retry lane")
            .map(|entry| {
                entry
                    .expect("intent retry entry")
                    .file_name()
                    .into_string()
                    .expect("portable intent retry entry")
            })
            .collect::<Vec<_>>();
        names.sort();
        assert_eq!(names, ["quarantine", "staging"]);
    }

    #[test]
    fn exact_loader_namespace_is_reserved_without_fallback() {
        assert_eq!(reconstruction_kind("1.21.5"), ReconstructionKind::Vanilla);
        assert_eq!(
            reconstruction_kind(" loader-v2-invalid "),
            ReconstructionKind::Vanilla
        );
        assert_eq!(
            reconstruction_kind("loader-v2-"),
            ReconstructionKind::Loader
        );
        assert_eq!(
            reconstruction_kind("loader-v2-invalid"),
            ReconstructionKind::Loader
        );
    }

    #[tokio::test]
    async fn registered_vanilla_version_bundle_all_exact_requires_no_provider() {
        const VERSION_ID: &str = "registered-bundle-all-exact";
        const CLIENT_BYTES: &[u8] = b"expected-client";
        const LOG_ID: &str = "registered-log.xml";
        const LOG_BYTES: &[u8] = b"<Configuration/>";
        let managed = tempfile::tempdir().expect("managed root");
        let authority =
            ManagedLibraryTestAuthority::open(managed.path()).expect("guard exact bundle root");
        let version_json = registered_version_bundle_metadata(
            VERSION_ID,
            "http://127.0.0.1:9/client",
            CLIENT_BYTES,
            LOG_ID,
            "http://127.0.0.1:9/log",
            LOG_BYTES,
            None,
        );
        seed_registered_version_bundle(
            managed.path(),
            VERSION_ID,
            &version_json,
            CLIENT_BYTES,
            LOG_ID,
            LOG_BYTES,
        );
        let inventory = Arc::new(
            crate::known_good::KnownGoodInventory::version_bundle_for_test(
                VERSION_ID,
                &version_json,
                CLIENT_BYTES,
                Some((LOG_ID, LOG_BYTES)),
            ),
        );

        let receipt = super::rebuild_managed_version_bundle(
            authority.operation().clone(),
            &registered_authority(VERSION_ID, inventory.clone(), 11),
        )
        .await
        .expect("exact local VersionBundle rebuild");

        assert_eq!(receipt.version_id(), VERSION_ID);
        assert!(receipt.matches_managed_library(authority.operation()));
        assert!(receipt.matches_known_good_inventory(&inventory));
        assert!(receipt.revalidate().await);
    }

    #[tokio::test]
    async fn cancelling_standalone_publication_retains_owner_through_settlement() {
        const VERSION_ID: &str = "standalone-publication-cancellation";
        let managed = tempfile::tempdir().expect("managed root");
        let authority =
            ManagedLibraryTestAuthority::open(managed.path()).expect("guard standalone root");
        let guarded_root = authority
            .managed_directory()
            .expect("project standalone root");
        let reconstruction =
            crate::known_good::managed_version_bundle_reconstruction_fixture_for_test(
                guarded_root,
                VERSION_ID,
            )
            .expect("standalone reconstruction");
        let (reached, release) =
            crate::version_bundle_publication::pause_after_promotions_for_test(VERSION_ID, 1);

        let caller = tokio::spawn(super::publish_managed_version_bundle_reconstruction(
            reconstruction,
        ));
        tokio::time::timeout(std::time::Duration::from_secs(60), reached)
            .await
            .expect("standalone publication should reach its first promotion")
            .expect("standalone publication pause signal");
        caller.abort();
        assert!(
            caller
                .await
                .expect_err("standalone caller should be cancelled")
                .is_cancelled()
        );
        release
            .send(())
            .expect("release retained standalone publication owner");

        let root = managed.path().to_path_buf();
        tokio::time::timeout(std::time::Duration::from_secs(60), async {
            loop {
                let lane = root.join(".axial-publication/version-bundle");
                let lane_settled = fs::read_dir(&lane).is_ok_and(|entries| {
                    let mut names = entries
                        .filter_map(Result::ok)
                        .filter_map(|entry| entry.file_name().into_string().ok())
                        .collect::<Vec<_>>();
                    names.sort();
                    names == ["quarantine", "settlement.json", "staging"]
                        && ["quarantine", "staging"].into_iter().all(|directory| {
                            fs::read_dir(lane.join(directory))
                                .is_ok_and(|mut entries| entries.next().is_none())
                        })
                });
                if lane_settled
                    && root
                        .join(format!("versions/{VERSION_ID}/{VERSION_ID}.json"))
                        .is_file()
                    && root
                        .join(format!("versions/{VERSION_ID}/{VERSION_ID}.jar"))
                        .is_file()
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("retained standalone publication should settle");
        checkpoint_and_ack_version_bundle(authority.operation().clone(), VERSION_ID).await;
        let candidate = authority
            .managed_directory()
            .expect("project quiescent standalone root");
        assert!(
            crate::managed_publication::run_publication_blocking(move || {
                crate::managed_publication::ManagedRootPublicationReadLease::acquire(candidate)
                    .is_ok()
            })
            .await
            .expect("standalone root read probe task"),
            "standalone root should be quiescent"
        );

        assert!(
            managed
                .path()
                .join(format!("versions/{VERSION_ID}/{VERSION_ID}.json"))
                .is_file()
        );
        assert!(
            managed
                .path()
                .join(format!("versions/{VERSION_ID}/{VERSION_ID}.jar"))
                .is_file()
        );
        let lane = managed.path().join(".axial-publication/version-bundle");
        let mut lane_names = fs::read_dir(&lane)
            .expect("settled standalone lane")
            .map(|entry| {
                entry
                    .expect("settled standalone entry")
                    .file_name()
                    .into_string()
                    .expect("portable standalone entry")
            })
            .collect::<Vec<_>>();
        lane_names.sort();
        assert_eq!(lane_names, ["quarantine", "staging"]);
        for directory in ["quarantine", "staging"] {
            assert!(
                fs::read_dir(lane.join(directory))
                    .expect("settled standalone bucket")
                    .next()
                    .is_none()
            );
        }
    }

    #[tokio::test]
    async fn attempted_intent_write_reenters_owned_version_bundle_recovery() {
        assert_version_bundle_intent_retry(
            "version-bundle-intent-write-retry",
            crate::version_bundle_publication::fail_intent_write_after_promotion_for_test,
        )
        .await;
    }

    #[tokio::test]
    async fn post_intent_failure_reenters_owned_version_bundle_recovery() {
        assert_version_bundle_intent_retry(
            "version-bundle-post-intent-retry",
            crate::version_bundle_publication::fail_after_intent_for_test,
        )
        .await;
    }

    #[tokio::test]
    async fn cancelling_standalone_libraries_rebuild_retains_complete_owner() {
        const VERSION_ID: &str = "standalone-libraries-cancellation";
        let managed = tempfile::tempdir().expect("managed root");
        let guarded_root = crate::managed_fs::ManagedDir::open_root(managed.path())
            .expect("guard standalone Libraries root");
        let reconstruction = crate::known_good::managed_libraries_reconstruction_fixture_for_test(
            guarded_root.clone(),
            VERSION_ID,
        )
        .expect("standalone Libraries reconstruction");
        let held =
            crate::managed_publication::ManagedRootPublicationLease::acquire(guarded_root.clone())
                .await
                .expect("hold standalone Libraries lease");
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let caller = tokio::spawn(
            super::publish_managed_libraries_reconstruction_with_start_signal(
                reconstruction,
                started_tx,
            ),
        );
        started_rx.await.expect("Libraries owner started");
        caller.abort();
        assert!(
            caller
                .await
                .expect_err("Libraries caller should be cancelled")
                .is_cancelled()
        );
        drop(held);

        let canonical = managed
            .path()
            .join("libraries/org/axial/fixture/1.0.0/fixture-1.0.0.jar");
        tokio::time::timeout(std::time::Duration::from_secs(60), async {
            while !component_owner_is_terminal(managed.path(), "libraries", &canonical).await {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("retained Libraries owner should settle");
        assert_eq!(
            fs::read(canonical).expect("published Libraries fixture"),
            b"axial managed Libraries fixture"
        );
    }

    #[tokio::test]
    async fn cancelling_standalone_assets_rebuild_retains_complete_owner() {
        const VERSION_ID: &str = "standalone-assets-cancellation";
        let managed = tempfile::tempdir().expect("managed root");
        let guarded_root = crate::managed_fs::ManagedDir::open_root(managed.path())
            .expect("guard standalone Assets root");
        let reconstruction = crate::known_good::managed_assets_reconstruction_fixture_for_test(
            guarded_root.clone(),
            VERSION_ID,
        )
        .await
        .expect("standalone Assets reconstruction");
        let held =
            crate::managed_publication::ManagedRootPublicationLease::acquire(guarded_root.clone())
                .await
                .expect("hold standalone Assets lease");
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let caller = tokio::spawn(
            super::publish_managed_assets_reconstruction_with_start_signal(
                reconstruction,
                started_tx,
            ),
        );
        started_rx.await.expect("Assets owner started");
        caller.abort();
        assert!(
            caller
                .await
                .expect_err("Assets caller should be cancelled")
                .is_cancelled()
        );
        drop(held);

        let canonical = managed.path().join("assets/indexes/fixture-assets.json");
        tokio::time::timeout(std::time::Duration::from_secs(60), async {
            while !component_owner_is_terminal(managed.path(), "assets", &canonical).await {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("retained Assets owner should settle");
    }

    #[tokio::test]
    async fn registered_vanilla_version_bundle_fetches_only_corrupt_member() {
        const VERSION_ID: &str = "registered-bundle-corrupt-client";
        const CLIENT_BYTES: &[u8] = b"expected-client";
        const CORRUPT_CLIENT_BYTES: &[u8] = b"tampered-client";
        const LOG_ID: &str = "registered-log.xml";
        const LOG_BYTES: &[u8] = b"<Configuration/>";
        let managed = tempfile::tempdir().expect("managed root");
        let authority =
            ManagedLibraryTestAuthority::open(managed.path()).expect("guard corrupt bundle root");
        let (client_url, requested_path) = serve_single_version_bundle_member(CLIENT_BYTES).await;
        let version_json = registered_version_bundle_metadata(
            VERSION_ID,
            &client_url,
            CLIENT_BYTES,
            LOG_ID,
            "http://127.0.0.1:9/log",
            LOG_BYTES,
            None,
        );
        seed_registered_version_bundle(
            managed.path(),
            VERSION_ID,
            &version_json,
            CORRUPT_CLIENT_BYTES,
            LOG_ID,
            LOG_BYTES,
        );
        let inventory = Arc::new(
            crate::known_good::KnownGoodInventory::version_bundle_for_test(
                VERSION_ID,
                &version_json,
                CLIENT_BYTES,
                Some((LOG_ID, LOG_BYTES)),
            ),
        );

        let receipt = super::rebuild_managed_version_bundle(
            authority.operation().clone(),
            &registered_authority(VERSION_ID, inventory.clone(), 12),
        )
        .await
        .expect("corrupt client-only VersionBundle rebuild");

        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(2), requested_path)
                .await
                .expect("selected client request")
                .expect("selected client request path"),
            "/member"
        );
        assert_eq!(
            fs::read(
                managed
                    .path()
                    .join(format!("versions/{VERSION_ID}/{VERSION_ID}.jar")),
            )
            .expect("repaired client"),
            CLIENT_BYTES
        );
        assert!(receipt.matches_known_good_inventory(&inventory));
        assert!(receipt.revalidate().await);
    }

    #[tokio::test]
    async fn registered_vanilla_version_bundle_rejects_metadata_contract_drift_before_effect() {
        const VERSION_ID: &str = "registered-bundle-contract-drift";
        const CLIENT_BYTES: &[u8] = b"expected-client";
        const OTHER_CLIENT_BYTES: &[u8] = b"different-client";
        const LOG_ID: &str = "registered-log.xml";
        const LOG_BYTES: &[u8] = b"<Configuration/>";
        let managed = tempfile::tempdir().expect("managed root");
        let authority =
            ManagedLibraryTestAuthority::open(managed.path()).expect("guard drift bundle root");
        let drifted_client_sha1 = format!("{:x}", Sha1::digest(OTHER_CLIENT_BYTES));
        let version_json = registered_version_bundle_metadata(
            VERSION_ID,
            "http://127.0.0.1:9/client",
            CLIENT_BYTES,
            LOG_ID,
            "http://127.0.0.1:9/log",
            LOG_BYTES,
            Some(&drifted_client_sha1),
        );
        seed_registered_version_bundle(
            managed.path(),
            VERSION_ID,
            &version_json,
            CLIENT_BYTES,
            LOG_ID,
            LOG_BYTES,
        );
        let inventory = Arc::new(
            crate::known_good::KnownGoodInventory::version_bundle_for_test(
                VERSION_ID,
                &version_json,
                CLIENT_BYTES,
                Some((LOG_ID, LOG_BYTES)),
            ),
        );

        let error = super::rebuild_managed_version_bundle(
            authority.operation().clone(),
            &registered_authority(VERSION_ID, inventory, 13),
        )
        .await
        .expect_err("metadata contract drift must be rejected");

        assert!(matches!(
            error,
            super::ManagedVersionBundleRebuildError::Authority
        ));
        assert_eq!(
            fs::read(
                managed
                    .path()
                    .join(format!("versions/{VERSION_ID}/{VERSION_ID}.jar")),
            )
            .expect("unmodified client"),
            CLIENT_BYTES
        );
    }

    #[tokio::test]
    async fn loader_version_bundle_projection_gate_publishes_only_exact_pinned_authority() {
        const CLIENT_BYTES: &[u8] = b"axial managed VersionBundle client fixture";
        const LOG_ID: &str = "guardian-version-bundle.xml";
        const LOG_BYTES: &[u8] = b"<Configuration/>";
        let version_id = crate::loaders::installed_version_id_for(
            crate::loaders::LoaderComponentId::Fabric,
            "1.21.5",
            "0.16.14",
        )
        .expect("canonical loader version id");
        assert_eq!(
            super::reconstruction_kind(&version_id),
            super::ReconstructionKind::Loader
        );
        let client_sha1 = format!("{:x}", Sha1::digest(CLIENT_BYTES));
        let version_json = serde_json::to_vec(&serde_json::json!({
            "id": version_id.as_str(),
            "type": "release",
            "mainClass": "org.axial.GuardianFixture",
            "downloads": {
                "client": {
                    "sha1": client_sha1,
                    "size": CLIENT_BYTES.len(),
                    "url": "https://example.invalid/managed-version-bundle-client"
                }
            }
        }))
        .expect("loader projection metadata");
        let expected = Arc::new(
            crate::known_good::KnownGoodInventory::version_bundle_for_test(
                &version_id,
                &version_json,
                CLIENT_BYTES,
                Some((LOG_ID, LOG_BYTES)),
            ),
        );

        let matching_root = tempfile::tempdir().expect("matching loader root");
        let matching_guard = crate::managed_fs::ManagedDir::open_root(matching_root.path())
            .expect("matching loader root guard");
        let matching = crate::known_good::managed_version_bundle_reconstruction_fixture_for_test(
            matching_guard,
            &version_id,
        )
        .expect("matching loader reconstruction");
        let matching_contract = matching
            .activation_contract_id()
            .expect("matching loader activation contract");
        let matching_authority =
            crate::known_good::KnownGoodActivationSource::from_registered_snapshot(
                &version_id,
                expected.clone(),
                matching_contract,
            )
            .expect("matching loader registered authority");
        let matching =
            super::require_loader_version_bundle_projection(matching, &matching_authority)
                .expect("matching pinned loader projection");
        let receipt = super::publish_managed_version_bundle_reconstruction(matching)
            .await
            .expect("matching loader projection publication");
        assert!(receipt.matches_known_good_inventory(&expected));
        assert!(receipt.revalidate().await);

        let mismatched_root = tempfile::tempdir().expect("mismatched loader root");
        let mismatched_guard = crate::managed_fs::ManagedDir::open_root(mismatched_root.path())
            .expect("mismatched loader root guard");
        let mismatched = crate::known_good::managed_version_bundle_reconstruction_fixture_for_test(
            mismatched_guard,
            &version_id,
        )
        .expect("mismatched loader reconstruction");
        let mismatched_contract = mismatched
            .activation_contract_id()
            .expect("mismatched loader activation contract");
        let mismatch = crate::known_good::KnownGoodInventory::version_bundle_for_test(
            &version_id,
            &version_json,
            b"different pinned client",
            Some((LOG_ID, LOG_BYTES)),
        );
        let mismatched_authority =
            crate::known_good::KnownGoodActivationSource::from_registered_snapshot(
                &version_id,
                Arc::new(mismatch),
                mismatched_contract,
            )
            .expect("mismatched loader registered authority");
        assert!(matches!(
            super::require_loader_version_bundle_projection(mismatched, &mismatched_authority,),
            Err(super::ManagedVersionBundleRebuildError::Authority)
        ));
        assert!(!mismatched_root.path().join("versions").exists());
        assert!(!mismatched_root.path().join("assets").exists());
    }

    fn registered_version_bundle_metadata(
        version_id: &str,
        client_url: &str,
        client_bytes: &[u8],
        log_id: &str,
        log_url: &str,
        log_bytes: &[u8],
        client_sha1_override: Option<&str>,
    ) -> Vec<u8> {
        let client_sha1 = client_sha1_override
            .map(str::to_string)
            .unwrap_or_else(|| format!("{:x}", Sha1::digest(client_bytes)));
        serde_json::to_vec(&serde_json::json!({
            "id": version_id,
            "type": "release",
            "mainClass": "org.axial.RegisteredBundleFixture",
            "assetIndex": {
                "id": "unrequested-assets",
                "sha1": format!("{:x}", Sha1::digest(b"unrequested-assets")),
                "size": 18,
                "totalSize": 18,
                "url": "http://127.0.0.1:9/asset-index"
            },
            "downloads": {
                "client": {
                    "sha1": client_sha1,
                    "size": client_bytes.len(),
                    "url": client_url
                }
            },
            "javaVersion": {
                "component": "unrequested-runtime",
                "majorVersion": 21
            },
            "logging": {
                "client": {
                    "argument": "-Dlog4j.configurationFile=${path}",
                    "file": {
                        "id": log_id,
                        "sha1": format!("{:x}", Sha1::digest(log_bytes)),
                        "size": log_bytes.len(),
                        "url": log_url
                    },
                    "type": "log4j2-xml"
                }
            }
        }))
        .expect("registered VersionBundle metadata")
    }

    fn seed_registered_version_bundle(
        managed_root: &std::path::Path,
        version_id: &str,
        version_json: &[u8],
        client_jar: &[u8],
        log_id: &str,
        log_config: &[u8],
    ) {
        let version_root = managed_root.join("versions").join(version_id);
        let log_root = managed_root.join("assets/log_configs");
        fs::create_dir_all(&version_root).expect("registered version root");
        fs::create_dir_all(&log_root).expect("registered log root");
        fs::write(
            version_root.join(format!("{version_id}.json")),
            version_json,
        )
        .expect("registered metadata");
        fs::write(version_root.join(format!("{version_id}.jar")), client_jar)
            .expect("registered client");
        fs::write(log_root.join(log_id), log_config).expect("registered log config");
    }

    async fn serve_single_version_bundle_member(
        bytes: &'static [u8],
    ) -> (String, tokio::sync::oneshot::Receiver<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("VersionBundle byte server");
        let address = listener.local_addr().expect("VersionBundle byte address");
        let (request_tx, request_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut request = [0_u8; 1024];
            let read = socket.read(&mut request).await.unwrap_or_default();
            let request = String::from_utf8_lossy(&request[..read]);
            let path = request
                .lines()
                .next()
                .and_then(|line| line.split_ascii_whitespace().nth(1))
                .unwrap_or_default()
                .to_string();
            let _ = request_tx.send(path);
            let headers = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                bytes.len()
            );
            if socket.write_all(headers.as_bytes()).await.is_ok() {
                let _ = socket.write_all(bytes).await;
            }
        });
        (format!("http://{address}/member"), request_rx)
    }

    #[tokio::test]
    async fn invalid_ids_fail_at_the_public_boundary_without_durable_effects() {
        let root = tempfile::tempdir().expect("sentinel root");
        let sentinel = root.path().join("untouched");
        fs::write(&sentinel, b"untouched").expect("sentinel");

        for invalid in ["loader-v2-", "loader-v2-not-base64!", "loader-v2-_w=="] {
            assert!(matches!(
                reconstruct_known_good(invalid).await,
                Err(KnownGoodReconstructionError::Loader)
            ));
            assert_sentinel_untouched(root.path(), &sentinel);
        }

        for invalid in ["../escape", " vanilla "] {
            assert!(matches!(
                reconstruct_known_good(invalid).await,
                Err(KnownGoodReconstructionError::Vanilla)
            ));
            assert_sentinel_untouched(root.path(), &sentinel);
        }
    }

    #[cfg(feature = "test-support")]
    #[tokio::test]
    async fn test_support_fixture_executes_the_committed_libraries_lifecycle() {
        const VERSION_ID: &str = "fixture-libraries-1.0.0";
        const CANONICAL_PATH: &str = "libraries/org/axial/fixture/1.0.0/fixture-1.0.0.jar";
        let root = tempfile::tempdir().expect("managed fixture root");

        let receipt = super::rebuild_managed_libraries_fixture_for_test(root.path(), VERSION_ID)
            .await
            .expect("committed fixture rebuild");

        assert_eq!(receipt.version_id(), VERSION_ID);
        assert!(receipt.matches_root(root.path()).await);
        assert!(receipt.revalidate().await);

        let canonical = root.path().join(CANONICAL_PATH);
        let mut corrupted = fs::read(&canonical).expect("read canonical fixture JAR");
        corrupted[0] ^= 0xff;
        fs::write(&canonical, corrupted).expect("corrupt canonical fixture JAR");
        assert!(!receipt.revalidate().await);
    }

    #[cfg(feature = "test-support")]
    #[tokio::test]
    async fn test_support_fixture_executes_the_committed_assets_lifecycle() {
        use sha1::{Digest as _, Sha1};

        const VERSION_ID: &str = "fixture-assets-1.0.0";
        const INDEX_PATH: &str = "assets/indexes/fixture-assets.json";
        const OBJECT_BYTES: &[u8] = b"axial managed Assets fixture";
        let root = tempfile::tempdir().expect("managed fixture root");

        let receipt = super::rebuild_managed_assets_fixture_for_test(root.path(), VERSION_ID)
            .await
            .expect("committed fixture rebuild");

        assert_eq!(receipt.version_id(), VERSION_ID);
        assert!(receipt.matches_root(root.path()).await);
        assert!(receipt.revalidate().await);
        let object_digest = format!("{:x}", Sha1::digest(OBJECT_BYTES));
        let empty_digest = format!("{:x}", Sha1::digest([]));
        assert_eq!(
            fs::read(
                root.path()
                    .join("assets/objects")
                    .join(&object_digest[..2])
                    .join(&object_digest),
            )
            .expect("fixture object"),
            OBJECT_BYTES
        );
        assert_eq!(
            fs::read(
                root.path()
                    .join("assets/objects")
                    .join(&empty_digest[..2])
                    .join(&empty_digest),
            )
            .expect("fixture empty object"),
            b""
        );

        let mut corrupted = fs::read(root.path().join(INDEX_PATH)).expect("read fixture index");
        corrupted[0] ^= 0xff;
        fs::write(root.path().join(INDEX_PATH), corrupted).expect("corrupt fixture index");
        assert!(!receipt.revalidate().await);
    }

    #[cfg(feature = "test-support")]
    #[tokio::test]
    async fn registered_component_receipts_bind_the_retained_library_authority() {
        let root = tempfile::tempdir().expect("registered managed root");
        let authority =
            ManagedLibraryTestAuthority::open(root.path()).expect("retain managed root");
        let foreign_root = tempfile::tempdir().expect("foreign managed root");
        let foreign = ManagedLibraryTestAuthority::open(foreign_root.path())
            .expect("retain foreign managed root");

        let libraries = super::rebuild_registered_managed_libraries_fixture_for_test(
            authority.operation().clone(),
            "fixture-libraries-1.0.0",
        )
        .await
        .expect("registered Libraries fixture");
        assert!(libraries.matches_managed_library(authority.operation()));
        assert!(!libraries.matches_managed_library(foreign.operation()));
        drop(libraries);

        let assets = super::rebuild_registered_managed_assets_fixture_for_test(
            authority.operation().clone(),
            "fixture-assets-1.0.0",
        )
        .await
        .expect("registered Assets fixture");
        assert!(assets.matches_managed_library(authority.operation()));
        assert!(!assets.matches_managed_library(foreign.operation()));
    }

    #[tokio::test]
    async fn version_bundle_fixture_settles_exact_effect_without_touching_user_owned_state() {
        const VERSION_ID: &str = "fixture-version-bundle-1.0.0";
        const CLIENT_PATH: &str =
            "versions/fixture-version-bundle-1.0.0/fixture-version-bundle-1.0.0.jar";
        let root = tempfile::tempdir().expect("managed fixture root");
        let authority =
            ManagedLibraryTestAuthority::open(root.path()).expect("guard VersionBundle root");
        let user_sentinel = root.path().join("mods/user-owned.txt");
        fs::create_dir_all(user_sentinel.parent().expect("user sentinel parent"))
            .expect("create user sentinel parent");
        fs::write(&user_sentinel, b"user-owned").expect("seed user sentinel");

        let receipt = super::rebuild_managed_version_bundle_fixture_for_test(
            authority.operation().clone(),
            VERSION_ID,
        )
        .await
        .expect("committed fixture rebuild");

        assert_eq!(receipt.version_id(), VERSION_ID);
        assert!(receipt.matches_managed_library(authority.operation()));
        let foreign_root = tempfile::tempdir().expect("foreign managed fixture root");
        let foreign_authority = ManagedLibraryTestAuthority::open(foreign_root.path())
            .expect("guard foreign VersionBundle root");
        assert!(!receipt.matches_managed_library(foreign_authority.operation()));
        assert!(receipt.revalidate().await);
        assert_eq!(
            fs::read(&user_sentinel).expect("user sentinel remains"),
            b"user-owned"
        );
        let mut corrupted = fs::read(root.path().join(CLIENT_PATH)).expect("read fixture client");
        corrupted[0] ^= 0xff;
        fs::write(root.path().join(CLIENT_PATH), corrupted).expect("corrupt fixture client");
        assert!(!receipt.revalidate().await);
    }

    #[tokio::test]
    async fn version_bundle_rebuild_recovery_retains_root_and_projection() {
        const VERSION_ID: &str = "fixture-version-bundle-recovery";
        let root = tempfile::tempdir().expect("managed fixture root");
        let authority =
            ManagedLibraryTestAuthority::open(root.path()).expect("guard recovery root");
        let lane = root.path().join(".axial-publication/version-bundle");
        fs::create_dir_all(&lane).expect("create malformed VersionBundle lane");
        fs::write(lane.join("intent.json"), b"{").expect("write malformed intent");

        let recovery = match super::rebuild_managed_version_bundle_fixture_for_test(
            authority.operation().clone(),
            VERSION_ID,
        )
        .await
        {
            Err(super::ManagedVersionBundleRebuildError::Indeterminate(recovery)) => recovery,
            other => panic!("malformed rebuild did not retain recovery: {other:?}"),
        };
        let competing_root = authority
            .managed_directory()
            .expect("project competing rebuild root");
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(50),
                crate::managed_publication::ManagedRootPublicationLease::acquire(competing_root),
            )
            .await
            .is_err(),
            "rebuild recovery must retain the exclusive root lease"
        );

        fs::remove_file(lane.join("intent.json")).expect("remove malformed intent");
        let receipt = recovery
            .retry()
            .await
            .expect("resume retained VersionBundle rebuild");
        assert_eq!(receipt.version_id(), VERSION_ID);
        assert!(receipt.matches_managed_library(authority.operation()));
        assert!(receipt.revalidate().await);
        assert!(matches!(
            receipt.acknowledge().await,
            super::ManagedVersionBundleAcknowledgementOutcome::Acknowledged
        ));
        let repeated = super::rebuild_managed_version_bundle_fixture_for_test(
            authority.operation().clone(),
            VERSION_ID,
        )
        .await
        .expect("acknowledged VersionBundle lane is reusable");
        assert!(matches!(
            repeated.acknowledge().await,
            super::ManagedVersionBundleAcknowledgementOutcome::Acknowledged
        ));
    }

    #[tokio::test]
    async fn version_bundle_fixture_returns_settled_rollback_with_exact_effect() {
        const VERSION_ID: &str = "fixture-version-bundle-rollback";
        let root = tempfile::tempdir().expect("managed fixture root");
        let authority =
            ManagedLibraryTestAuthority::open(root.path()).expect("guard rollback root");
        let user_sentinel = root.path().join("saves/user-owned/level.dat");
        fs::create_dir_all(user_sentinel.parent().expect("user sentinel parent"))
            .expect("create user sentinel parent");
        fs::write(&user_sentinel, b"user-owned").expect("seed user sentinel");
        crate::version_bundle_publication::fail_after_promotions_for_test(VERSION_ID, 1);

        let super::ManagedVersionBundleRebuildError::RolledBack(receipt) =
            super::rebuild_managed_version_bundle_fixture_for_test(
                authority.operation().clone(),
                VERSION_ID,
            )
            .await
            .expect_err("injected rebuild must roll back")
        else {
            panic!("rebuild must return its settled rollback receipt");
        };
        assert_eq!(receipt.version_id(), VERSION_ID);
        assert!(receipt.matches_managed_library(authority.operation()));
        assert_eq!(
            receipt.effect(),
            super::ManagedVersionBundleRollbackEffect::Promotion
        );
        for canonical in [
            format!("versions/{VERSION_ID}/{VERSION_ID}.json"),
            format!("versions/{VERSION_ID}/{VERSION_ID}.jar"),
            "assets/log_configs/guardian-version-bundle.xml".to_string(),
        ] {
            assert!(
                !root.path().join(canonical).exists(),
                "rolled-back projected file must be absent"
            );
        }
        assert_eq!(
            fs::read(&user_sentinel).expect("user sentinel remains"),
            b"user-owned"
        );
        assert!(matches!(
            receipt.acknowledge().await,
            super::ManagedVersionBundleAcknowledgementOutcome::Acknowledged
        ));
        let committed = super::rebuild_managed_version_bundle_fixture_for_test(
            authority.operation().clone(),
            VERSION_ID,
        )
        .await
        .expect("acknowledged rollback lane is reusable");
        assert!(matches!(
            committed.acknowledge().await,
            super::ManagedVersionBundleAcknowledgementOutcome::Acknowledged
        ));
    }

    #[tokio::test]
    async fn version_bundle_receipts_bind_the_exact_activation_contract() {
        const COMMIT_VERSION: &str = "fixture-version-bundle-contract-commit";
        let commit_root = tempfile::tempdir().expect("commit managed fixture root");
        let commit_authority = ManagedLibraryTestAuthority::open(commit_root.path())
            .expect("guard commit contract root");
        let commit_source = version_bundle_fixture_activation_source(COMMIT_VERSION);
        let foreign_contract = crate::ManagedInstallActivationContractId::from_digest([0xf1; 32]);
        let commit = super::rebuild_managed_version_bundle_fixture_for_test(
            commit_authority.operation().clone(),
            COMMIT_VERSION,
        )
        .await
        .expect("committed fixture rebuild");
        assert!(commit.matches_activation_contract(commit_source.activation_contract_id()));
        assert!(!commit.matches_activation_contract(&foreign_contract));
        acknowledge_version_bundle(commit.acknowledge().await).await;

        const ROLLBACK_VERSION: &str = "fixture-version-bundle-contract-rollback";
        let rollback_root = tempfile::tempdir().expect("rollback managed fixture root");
        let rollback_authority = ManagedLibraryTestAuthority::open(rollback_root.path())
            .expect("guard rollback contract root");
        let rollback_source = version_bundle_fixture_activation_source(ROLLBACK_VERSION);
        crate::version_bundle_publication::fail_after_promotions_for_test(ROLLBACK_VERSION, 1);
        let super::ManagedVersionBundleRebuildError::RolledBack(rollback) =
            super::rebuild_managed_version_bundle_fixture_for_test(
                rollback_authority.operation().clone(),
                ROLLBACK_VERSION,
            )
            .await
            .expect_err("injected rebuild must roll back")
        else {
            panic!("injected rebuild did not return rollback authority");
        };
        assert!(rollback.matches_activation_contract(rollback_source.activation_contract_id()));
        assert!(!rollback.matches_activation_contract(&foreign_contract));
        acknowledge_version_bundle(rollback.acknowledge().await).await;
    }

    #[tokio::test]
    async fn source_bound_version_bundle_fixture_preserves_the_registered_contract() {
        const VERSION_ID: &str = "fixture-version-bundle-source-bound-contract";
        let root = tempfile::tempdir().expect("source-bound managed fixture root");
        let authority =
            ManagedLibraryTestAuthority::open(root.path()).expect("guard source-bound root");
        let fixture_source = version_bundle_fixture_activation_source(VERSION_ID);
        let expected =
            registered_authority(VERSION_ID, Arc::clone(fixture_source.inventory()), 0xd7);
        assert_ne!(
            expected.activation_contract_id(),
            fixture_source.activation_contract_id()
        );

        let receipt = super::rebuild_managed_version_bundle_fixture_for_source_test(
            authority.operation().clone(),
            &expected,
        )
        .await
        .expect("source-bound fixture rebuild");

        assert!(receipt.matches_known_good_inventory(expected.inventory()));
        assert!(receipt.matches_activation_contract(expected.activation_contract_id()));
        assert!(!receipt.matches_activation_contract(fixture_source.activation_contract_id()));
        assert!(receipt.revalidate().await);
        acknowledge_version_bundle(receipt.acknowledge().await).await;
    }

    #[tokio::test]
    async fn source_bound_version_bundle_rollback_preserves_the_registered_contract() {
        const VERSION_ID: &str = "fixture-version-bundle-source-bound-rollback";
        let root = tempfile::tempdir().expect("source-bound rollback fixture root");
        let authority = ManagedLibraryTestAuthority::open(root.path())
            .expect("guard source-bound rollback root");
        let fixture_source = version_bundle_fixture_activation_source(VERSION_ID);
        let expected =
            registered_authority(VERSION_ID, Arc::clone(fixture_source.inventory()), 0xd9);

        let super::ManagedVersionBundleRebuildError::RolledBack(receipt) =
            super::rebuild_managed_version_bundle_rollback_fixture_for_source_test(
                authority.operation().clone(),
                &expected,
            )
            .await
            .expect_err("source-bound rollback fixture must roll back")
        else {
            panic!("source-bound rollback fixture did not retain rollback authority");
        };

        assert!(receipt.matches_known_good_inventory(expected.inventory()));
        assert!(receipt.matches_activation_contract(expected.activation_contract_id()));
        assert!(!receipt.matches_activation_contract(fixture_source.activation_contract_id()));
        acknowledge_version_bundle(receipt.acknowledge().await).await;
    }

    #[tokio::test]
    async fn source_bound_version_bundle_fixture_rejects_foreign_projection() {
        const VERSION_ID: &str = "fixture-version-bundle-source-bound-mismatch";
        let root = tempfile::tempdir().expect("source-bound mismatch fixture root");
        let authority = ManagedLibraryTestAuthority::open(root.path())
            .expect("guard source-bound mismatch root");
        let foreign =
            version_bundle_fixture_activation_source("fixture-version-bundle-foreign-projection");
        let expected = registered_authority(VERSION_ID, Arc::clone(foreign.inventory()), 0xd8);

        assert!(matches!(
            super::rebuild_managed_version_bundle_fixture_for_source_test(
                authority.operation().clone(),
                &expected,
            )
            .await,
            Err(super::ManagedVersionBundleRebuildError::Authority)
        ));
        assert!(
            !root
                .path()
                .join(".axial-publication/version-bundle")
                .exists(),
            "projection rejection must precede publication"
        );
    }

    #[tokio::test]
    async fn version_bundle_receipt_acknowledges_only_its_exact_settlement_marker() {
        const VERSION_ID: &str = "fixture-version-bundle-exact-acknowledgement";
        let root = tempfile::tempdir().expect("managed fixture root");
        let authority =
            ManagedLibraryTestAuthority::open(root.path()).expect("guard exact ack root");
        let receipt = super::rebuild_managed_version_bundle_fixture_for_test(
            authority.operation().clone(),
            VERSION_ID,
        )
        .await
        .expect("committed fixture rebuild");
        let settlement = root
            .path()
            .join(".axial-publication/version-bundle/settlement.json");
        let marker = fs::read(&settlement).expect("read exact settlement marker");
        fs::remove_file(&settlement).expect("remove exact settlement marker");
        fs::write(&settlement, marker).expect("replace settlement marker with identical bytes");

        let super::ManagedVersionBundleAcknowledgementOutcome::Indeterminate(_recovery) =
            receipt.acknowledge().await
        else {
            panic!("same-content settlement replacement must remain indeterminate");
        };
    }

    #[tokio::test]
    async fn version_bundle_commit_restart_acknowledgement_reuses_lane() {
        const VERSION_ID: &str = "fixture-version-bundle-commit-restart";
        let root = tempfile::tempdir().expect("managed fixture root");
        let authority =
            ManagedLibraryTestAuthority::open(root.path()).expect("guard commit restart root");
        let source = version_bundle_fixture_activation_source(VERSION_ID);
        let receipt = super::rebuild_managed_version_bundle_fixture_for_test(
            authority.operation().clone(),
            VERSION_ID,
        )
        .await
        .expect("committed fixture rebuild");
        let evidence_id = receipt.evidence_id();
        drop(receipt);

        assert!(matches!(
            super::recover_managed_version_bundle_acknowledgement(
                authority.operation().clone(),
                &source,
                super::ManagedVersionBundleExpectedSettlement::Committed,
                &evidence_id,
            )
            .await,
            super::ManagedVersionBundleAcknowledgementOutcome::Acknowledged
        ));
        let repeated = super::rebuild_managed_version_bundle_fixture_for_test(
            authority.operation().clone(),
            VERSION_ID,
        )
        .await
        .expect("restart acknowledgement must release the lane");
        assert!(matches!(
            repeated.acknowledge().await,
            super::ManagedVersionBundleAcknowledgementOutcome::Acknowledged
        ));
    }

    #[tokio::test]
    async fn version_bundle_rollback_restart_acknowledgement_reuses_lane() {
        const VERSION_ID: &str = "fixture-version-bundle-rollback-restart";
        let root = tempfile::tempdir().expect("managed fixture root");
        let authority =
            ManagedLibraryTestAuthority::open(root.path()).expect("guard rollback restart root");
        let source = version_bundle_fixture_activation_source(VERSION_ID);
        crate::version_bundle_publication::fail_after_promotions_for_test(VERSION_ID, 1);
        let super::ManagedVersionBundleRebuildError::RolledBack(receipt) =
            super::rebuild_managed_version_bundle_fixture_for_test(
                authority.operation().clone(),
                VERSION_ID,
            )
            .await
            .expect_err("injected rebuild must roll back")
        else {
            panic!("injected rebuild must return rollback authority");
        };
        assert_eq!(
            receipt.effect(),
            super::ManagedVersionBundleRollbackEffect::Promotion
        );
        let evidence_id = receipt.evidence_id();
        drop(receipt);

        assert!(matches!(
            super::recover_managed_version_bundle_acknowledgement(
                authority.operation().clone(),
                &source,
                super::ManagedVersionBundleExpectedSettlement::RolledBack,
                &evidence_id,
            )
            .await,
            super::ManagedVersionBundleAcknowledgementOutcome::Acknowledged
        ));
        let committed = super::rebuild_managed_version_bundle_fixture_for_test(
            authority.operation().clone(),
            VERSION_ID,
        )
        .await
        .expect("restart rollback acknowledgement must release the lane");
        assert!(matches!(
            committed.acknowledge().await,
            super::ManagedVersionBundleAcknowledgementOutcome::Acknowledged
        ));
    }

    #[tokio::test]
    async fn guardian_version_bundle_orphan_commit_retains_lane_and_exact_evidence() {
        const VERSION_ID: &str = "fixture-version-bundle-orphan-commit";
        let root = tempfile::tempdir().expect("managed fixture root");
        let authority =
            ManagedLibraryTestAuthority::open(root.path()).expect("guard orphan commit root");
        let source = version_bundle_fixture_activation_source(VERSION_ID);
        let receipt = super::rebuild_managed_version_bundle_fixture_for_test(
            authority.operation().clone(),
            VERSION_ID,
        )
        .await
        .expect("committed fixture rebuild");
        let expected_evidence_id = receipt.evidence_id();
        drop(receipt);

        let super::ManagedVersionBundleOrphanOutcome::Settled(settlement) =
            super::recover_guardian_version_bundle_orphan(authority.operation().clone(), &source)
                .await
        else {
            panic!("Guardian commit orphan was not recovered");
        };
        assert_eq!(settlement.evidence_id(), expected_evidence_id);
        assert_eq!(
            settlement.outcome(),
            super::ManagedVersionBundleSettlementOutcome::Committed
        );
        assert!(
            root.path()
                .join(".axial-publication/version-bundle/settlement.json")
                .is_file(),
            "orphan discovery must not acknowledge the marker"
        );

        let competing_root = authority
            .managed_directory()
            .expect("project competing orphan root");
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(50),
                crate::managed_publication::ManagedRootPublicationLease::acquire(competing_root),
            )
            .await
            .is_err(),
            "orphan settlement carrier must retain the exclusive lane"
        );

        acknowledge_version_bundle(settlement.acknowledge().await).await;
        assert!(matches!(
            super::recover_guardian_version_bundle_orphan(authority.operation().clone(), &source,)
                .await,
            super::ManagedVersionBundleOrphanOutcome::NoSettlement
        ));
    }

    #[tokio::test]
    async fn guardian_version_bundle_orphan_rollback_recovers_exact_effect_and_evidence() {
        const VERSION_ID: &str = "fixture-version-bundle-orphan-rollback";
        let root = tempfile::tempdir().expect("managed fixture root");
        let authority =
            ManagedLibraryTestAuthority::open(root.path()).expect("guard orphan rollback root");
        let source = version_bundle_fixture_activation_source(VERSION_ID);
        crate::version_bundle_publication::fail_after_promotions_for_test(VERSION_ID, 1);
        let super::ManagedVersionBundleRebuildError::RolledBack(receipt) =
            super::rebuild_managed_version_bundle_fixture_for_test(
                authority.operation().clone(),
                VERSION_ID,
            )
            .await
            .expect_err("injected rebuild must roll back")
        else {
            panic!("injected rebuild did not return rollback authority");
        };
        let expected_evidence_id = receipt.evidence_id();
        drop(receipt);

        let super::ManagedVersionBundleOrphanOutcome::Settled(settlement) =
            super::recover_guardian_version_bundle_orphan(authority.operation().clone(), &source)
                .await
        else {
            panic!("Guardian rollback orphan was not recovered");
        };
        assert_eq!(settlement.evidence_id(), expected_evidence_id);
        assert_eq!(
            settlement.outcome(),
            super::ManagedVersionBundleSettlementOutcome::RolledBack {
                effect: super::ManagedVersionBundleRollbackEffect::Promotion,
            }
        );
        acknowledge_version_bundle(settlement.acknowledge().await).await;
    }

    #[tokio::test]
    async fn guardian_version_bundle_orphan_rejects_install_and_foreign_markers() {
        const INSTALL_VERSION: &str = "fixture-version-bundle-install-owner";
        let install_root = tempfile::tempdir().expect("install managed root");
        let install_authority = ManagedLibraryTestAuthority::open(install_root.path())
            .expect("guard install managed root");
        let install_receipt = crate::download::publish_managed_install_fixture_for_test(
            install_authority.operation().clone(),
            INSTALL_VERSION,
        )
        .await
        .expect("publish install-owned VersionBundle settlement");
        let install_source = install_receipt.into_activation_source();

        let marker = fs::read_to_string(
            install_root
                .path()
                .join(".axial-publication/version-bundle/settlement.json"),
        )
        .expect("read install-owned settlement marker");
        assert!(marker.contains("\"purpose\":\"install\""));

        match super::recover_guardian_version_bundle_orphan(
            install_authority.operation().clone(),
            &install_source,
        )
        .await
        {
            super::ManagedVersionBundleOrphanOutcome::Mismatch => {}
            super::ManagedVersionBundleOrphanOutcome::NoSettlement => {
                panic!("install-owned settlement was hidden")
            }
            super::ManagedVersionBundleOrphanOutcome::Settled(_) => {
                panic!("install-owned settlement was claimed by Guardian")
            }
            super::ManagedVersionBundleOrphanOutcome::Indeterminate(_) => {
                panic!("install-owned settlement was not rejected terminally")
            }
        }
        let install_outcome = crate::download::classify_managed_install_publication(
            install_authority.operation().clone(),
            INSTALL_VERSION,
        )
        .await;
        assert!(
            matches!(
                install_outcome,
                crate::download::ManagedInstallDurableOutcome::Committed(_)
            ),
            "Guardian orphan recovery must leave the install marker intact"
        );
        drop(install_outcome);

        const GUARDIAN_VERSION: &str = "fixture-version-bundle-foreign-source";
        let guardian_root = tempfile::tempdir().expect("Guardian managed root");
        let guardian_authority = ManagedLibraryTestAuthority::open(guardian_root.path())
            .expect("guard Guardian managed root");
        let exact_source = version_bundle_fixture_activation_source(GUARDIAN_VERSION);
        let foreign_source = version_bundle_fixture_activation_source("foreign-version-bundle");
        let wrong_contract_source =
            registered_authority(GUARDIAN_VERSION, Arc::clone(exact_source.inventory()), 0xa7);
        let wrong_projection_source =
            crate::known_good::KnownGoodActivationSource::from_registered_snapshot(
                GUARDIAN_VERSION,
                Arc::clone(foreign_source.inventory()),
                exact_source.activation_contract_id().clone(),
            )
            .expect("foreign projection authority fixture");
        let receipt = super::rebuild_managed_version_bundle_fixture_for_test(
            guardian_authority.operation().clone(),
            GUARDIAN_VERSION,
        )
        .await
        .expect("publish Guardian-owned VersionBundle settlement");
        drop(receipt);

        assert!(matches!(
            super::recover_guardian_version_bundle_orphan(
                guardian_authority.operation().clone(),
                &foreign_source,
            )
            .await,
            super::ManagedVersionBundleOrphanOutcome::Mismatch
        ));
        assert!(matches!(
            super::recover_guardian_version_bundle_orphan(
                guardian_authority.operation().clone(),
                &wrong_contract_source,
            )
            .await,
            super::ManagedVersionBundleOrphanOutcome::Mismatch
        ));
        assert!(matches!(
            super::recover_guardian_version_bundle_orphan(
                guardian_authority.operation().clone(),
                &wrong_projection_source,
            )
            .await,
            super::ManagedVersionBundleOrphanOutcome::Mismatch
        ));
        let super::ManagedVersionBundleOrphanOutcome::Settled(settlement) =
            super::recover_guardian_version_bundle_orphan(
                guardian_authority.operation().clone(),
                &exact_source,
            )
            .await
        else {
            panic!("foreign-source rejection changed the exact Guardian settlement");
        };
        acknowledge_version_bundle(settlement.acknowledge().await).await;
    }

    #[tokio::test]
    async fn guardian_version_bundle_orphan_distinguishes_empty_and_malformed_lanes() {
        const VERSION_ID: &str = "fixture-version-bundle-orphan-malformed";
        let root = tempfile::tempdir().expect("managed fixture root");
        let authority =
            ManagedLibraryTestAuthority::open(root.path()).expect("guard malformed orphan root");
        let source = version_bundle_fixture_activation_source(VERSION_ID);

        assert!(matches!(
            super::recover_guardian_version_bundle_orphan(authority.operation().clone(), &source,)
                .await,
            super::ManagedVersionBundleOrphanOutcome::NoSettlement
        ));

        let lane = root.path().join(".axial-publication/version-bundle");
        fs::create_dir_all(&lane).expect("create malformed VersionBundle lane");
        fs::write(lane.join("settlement.json"), b"{").expect("write malformed settlement");
        let super::ManagedVersionBundleOrphanOutcome::Indeterminate(recovery) =
            super::recover_guardian_version_bundle_orphan(authority.operation().clone(), &source)
                .await
        else {
            panic!("malformed settlement did not retain retry authority");
        };
        fs::remove_file(lane.join("settlement.json")).expect("remove malformed settlement");
        assert!(matches!(
            recovery.retry().await,
            super::ManagedVersionBundleOrphanOutcome::NoSettlement
        ));
    }

    #[tokio::test]
    async fn guardian_version_bundle_acquisition_is_bounded_while_lane_is_held() {
        const VERSION_ID: &str = "fixture-version-bundle-held-lane";
        let root = tempfile::tempdir().expect("held-lane fixture root");
        let authority =
            ManagedLibraryTestAuthority::open(root.path()).expect("guard held-lane root");
        let source = version_bundle_fixture_activation_source(VERSION_ID);
        let held = crate::managed_publication::ManagedRootPublicationLease::acquire(
            authority
                .managed_directory()
                .expect("project held-lane root"),
        )
        .await
        .expect("hold VersionBundle publication lane");

        let super::ManagedVersionBundleOrphanOutcome::Indeterminate(orphan_recovery) =
            tokio::time::timeout(
                std::time::Duration::from_secs(15),
                super::recover_guardian_version_bundle_orphan(
                    authority.operation().clone(),
                    &source,
                ),
            )
            .await
            .expect("orphan acquisition must return without waiting")
        else {
            panic!("held orphan lane did not retain bounded acquisition recovery");
        };
        drop(held);
        assert!(matches!(
            orphan_recovery.retry().await,
            super::ManagedVersionBundleOrphanOutcome::NoSettlement
        ));

        let receipt = super::rebuild_managed_version_bundle_fixture_for_source_test(
            authority.operation().clone(),
            &source,
        )
        .await
        .expect("publish exact settlement for held acknowledgement lane");
        let evidence_id = receipt.evidence_id();
        drop(receipt);
        let held = crate::managed_publication::ManagedRootPublicationLease::acquire(
            authority
                .managed_directory()
                .expect("project held acknowledgement root"),
        )
        .await
        .expect("hold VersionBundle acknowledgement lane");
        let super::ManagedVersionBundleAcknowledgementOutcome::Indeterminate(
            acknowledgement_recovery,
        ) = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            super::recover_managed_version_bundle_acknowledgement(
                authority.operation().clone(),
                &source,
                super::ManagedVersionBundleExpectedSettlement::Committed,
                &evidence_id,
            ),
        )
        .await
        .expect("acknowledgement acquisition must return without waiting")
        else {
            panic!("held acknowledgement lane did not retain bounded acquisition recovery");
        };
        drop(held);
        assert!(matches!(
            acknowledgement_recovery.retry().await,
            super::ManagedVersionBundleAcknowledgementOutcome::Acknowledged
        ));
    }

    #[tokio::test]
    async fn version_bundle_receipt_pins_retirement_until_acknowledgement() {
        const VERSION_ID: &str = "fixture-version-bundle-retirement-pin";
        let root = tempfile::tempdir().expect("retirement fixture root");
        let authority =
            ManagedLibraryTestAuthority::open(root.path()).expect("guard retirement root");
        let receipt = super::rebuild_managed_version_bundle_fixture_for_test(
            authority.operation().clone(),
            VERSION_ID,
        )
        .await
        .expect("publish retirement-pinning receipt");

        let (operation, retirement) = authority.begin_retirement_for_test();
        drop(operation);
        let mut drain = tokio::spawn(async move { retirement.drain_and_settle().await });
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), &mut drain)
                .await
                .is_err(),
            "settled VersionBundle receipt must pin its managed generation"
        );

        acknowledge_version_bundle(receipt.acknowledge().await).await;
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(5), drain)
                .await
                .expect("retirement must drain after receipt acknowledgement")
                .expect("retirement task")
                .expect("retirement settlement"),
            crate::managed_fs::ManagedLibraryRetirementBinding::BindingIntact
        );
    }

    #[tokio::test]
    async fn version_bundle_acquire_recovery_pins_retirement_until_release() {
        const VERSION_ID: &str = "fixture-version-bundle-acquire-recovery-pin";
        let root = tempfile::tempdir().expect("acquire recovery fixture root");
        let authority =
            ManagedLibraryTestAuthority::open(root.path()).expect("guard acquire recovery root");
        let source = version_bundle_fixture_activation_source(VERSION_ID);
        let held = crate::managed_publication::ManagedRootPublicationLease::acquire(
            authority
                .managed_directory()
                .expect("project acquire recovery root"),
        )
        .await
        .expect("hold acquire recovery lane");
        let super::ManagedVersionBundleOrphanOutcome::Indeterminate(recovery) =
            super::recover_guardian_version_bundle_orphan(authority.operation().clone(), &source)
                .await
        else {
            panic!("held lane did not retain Acquire recovery");
        };
        drop(held);

        let (operation, retirement) = authority.begin_retirement_for_test();
        drop(operation);
        let mut drain = tokio::spawn(async move { retirement.drain_and_settle().await });
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), &mut drain)
                .await
                .is_err(),
            "Acquire recovery must pin its managed generation"
        );

        drop(recovery);
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(5), drain)
                .await
                .expect("retirement must drain after recovery release")
                .expect("retirement task")
                .expect("retirement settlement"),
            crate::managed_fs::ManagedLibraryRetirementBinding::BindingIntact
        );
    }

    #[test]
    fn guardian_version_bundle_orphan_authorities_are_send_static() {
        fn assert_send_static<T: Send + 'static>() {}

        assert_send_static::<super::ManagedVersionBundleOrphanOutcome>();
        assert_send_static::<super::ManagedVersionBundleOrphanSettlement>();
        assert_send_static::<super::ManagedVersionBundleOrphanRecovery>();
    }

    #[tokio::test]
    async fn version_bundle_restart_rejects_mismatch_and_proves_absent_root_binding() {
        const VERSION_ID: &str = "fixture-version-bundle-restart-mismatch";
        let root = tempfile::tempdir().expect("managed fixture root");
        let other_root = tempfile::tempdir().expect("other managed fixture root");
        let authority =
            ManagedLibraryTestAuthority::open(root.path()).expect("guard restart mismatch root");
        let other_authority = ManagedLibraryTestAuthority::open(other_root.path())
            .expect("guard alternate restart root");
        let source = version_bundle_fixture_activation_source(VERSION_ID);
        let receipt = super::rebuild_managed_version_bundle_fixture_for_test(
            authority.operation().clone(),
            VERSION_ID,
        )
        .await
        .expect("committed fixture rebuild");
        let evidence_id = receipt.evidence_id();
        drop(receipt);

        assert!(matches!(
            super::recover_managed_version_bundle_acknowledgement(
                authority.operation().clone(),
                &source,
                super::ManagedVersionBundleExpectedSettlement::RolledBack,
                &evidence_id,
            )
            .await,
            super::ManagedVersionBundleAcknowledgementOutcome::Mismatch
        ));
        assert!(matches!(
            super::recover_managed_version_bundle_acknowledgement(
                authority.operation().clone(),
                &source,
                super::ManagedVersionBundleExpectedSettlement::Committed,
                &evidence_id,
            )
            .await,
            super::ManagedVersionBundleAcknowledgementOutcome::Acknowledged
        ));
        let repeated = super::rebuild_managed_version_bundle_fixture_for_test(
            authority.operation().clone(),
            VERSION_ID,
        )
        .await
        .expect("publish a repeated same-contract settlement");
        let repeated_evidence_id = repeated.evidence_id();
        drop(repeated);
        assert!(matches!(
            super::recover_managed_version_bundle_acknowledgement(
                authority.operation().clone(),
                &source,
                super::ManagedVersionBundleExpectedSettlement::Committed,
                &evidence_id,
            )
            .await,
            super::ManagedVersionBundleAcknowledgementOutcome::Mismatch
        ));
        assert!(matches!(
            super::recover_managed_version_bundle_acknowledgement(
                authority.operation().clone(),
                &source,
                super::ManagedVersionBundleExpectedSettlement::Committed,
                &repeated_evidence_id,
            )
            .await,
            super::ManagedVersionBundleAcknowledgementOutcome::Acknowledged
        ));
        assert!(matches!(
            super::recover_managed_version_bundle_acknowledgement(
                authority.operation().clone(),
                &source,
                super::ManagedVersionBundleExpectedSettlement::Committed,
                &repeated_evidence_id,
            )
            .await,
            super::ManagedVersionBundleAcknowledgementOutcome::NoSettlement
        ));
        assert!(matches!(
            super::recover_managed_version_bundle_acknowledgement(
                other_authority.operation().clone(),
                &source,
                super::ManagedVersionBundleExpectedSettlement::Committed,
                &repeated_evidence_id,
            )
            .await,
            super::ManagedVersionBundleAcknowledgementOutcome::Mismatch
        ));
    }

    #[tokio::test]
    async fn version_bundle_unsettled_move_reconciles_without_a_false_terminal() {
        const VERSION_ID: &str = "fixture-version-bundle-unsettled-move";
        let root = tempfile::tempdir().expect("managed fixture root");
        let authority =
            ManagedLibraryTestAuthority::open(root.path()).expect("guard unsettled move root");
        crate::version_bundle_publication::report_first_move_unsettled_for_test(VERSION_ID);

        let super::ManagedVersionBundleRebuildError::RolledBack(receipt) =
            super::rebuild_managed_version_bundle_fixture_for_test(
                authority.operation().clone(),
                VERSION_ID,
            )
            .await
            .expect_err("unsettled move must enter durable reconciliation")
        else {
            panic!("unsettled move must return its reconciled rollback receipt");
        };
        assert_eq!(receipt.version_id(), VERSION_ID);
        assert!(receipt.matches_managed_library(authority.operation()));
        assert_eq!(
            receipt.effect(),
            super::ManagedVersionBundleRollbackEffect::Promotion
        );
        for canonical in [
            format!("versions/{VERSION_ID}/{VERSION_ID}.json"),
            format!("versions/{VERSION_ID}/{VERSION_ID}.jar"),
            "assets/log_configs/guardian-version-bundle.xml".to_string(),
        ] {
            assert!(
                !root.path().join(canonical).exists(),
                "reconciled rollback must not retain an unproven canonical file"
            );
        }
    }

    #[test]
    fn public_errors_are_closed_and_source_free() {
        for (error, message) in [
            (
                KnownGoodReconstructionError::Vanilla,
                "vanilla known-good reconstruction failed",
            ),
            (
                KnownGoodReconstructionError::Loader,
                "loader known-good reconstruction failed",
            ),
            (
                KnownGoodReconstructionError::ManagedRoot,
                "managed root admission failed",
            ),
        ] {
            assert_eq!(error.to_string(), message);
            assert!(std::error::Error::source(&error).is_none());
        }
        for error in [
            super::ManagedLibrariesRebuildError::Preparation,
            super::ManagedLibrariesRebuildError::Indeterminate,
            super::ManagedLibrariesRebuildError::Reconstruction(
                KnownGoodReconstructionError::Vanilla,
            ),
        ] {
            assert!(std::error::Error::source(&error).is_none());
            assert!(!error.to_string().contains('/'));
        }
        for error in [
            super::ManagedAssetsRebuildError::Preparation,
            super::ManagedAssetsRebuildError::Indeterminate,
            super::ManagedAssetsRebuildError::Reconstruction(KnownGoodReconstructionError::Loader),
        ] {
            assert!(std::error::Error::source(&error).is_none());
            assert!(!error.to_string().contains('/'));
        }
        for error in [
            super::ManagedVersionBundleRebuildError::Source,
            super::ManagedVersionBundleRebuildError::Authority,
            super::ManagedVersionBundleRebuildError::LocalPreparation,
            super::ManagedVersionBundleRebuildError::Preparation,
            super::ManagedVersionBundleRebuildError::Interrupted,
            super::ManagedVersionBundleRebuildError::Unsettled,
            super::ManagedVersionBundleRebuildError::Reconstruction(
                KnownGoodReconstructionError::Loader,
            ),
        ] {
            assert!(std::error::Error::source(&error).is_none());
            assert!(!error.to_string().contains('/'));
        }
        assert!(
            std::mem::size_of::<super::ManagedLibrariesRebuildError>()
                <= 2 * std::mem::size_of::<usize>()
        );
        assert!(
            std::mem::size_of::<super::ManagedAssetsRebuildError>()
                <= 2 * std::mem::size_of::<usize>()
        );
        assert!(
            std::mem::size_of::<super::ManagedVersionBundleRebuildError>()
                <= 2 * std::mem::size_of::<usize>()
        );
    }

    #[test]
    fn split_reconstruction_entry_points_are_not_public() {
        let crate_root = include_str!("lib.rs");
        let dispatcher = include_str!("known_good_reconstruction.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("dispatcher production source");
        let downloader = include_str!("download/install.rs");
        let loaders = include_str!("loaders/mod.rs");
        let loader_strategies = include_str!("loaders/strategies/common.rs");

        assert!(crate_root.contains("rebuild_managed_libraries"));
        assert!(crate_root.contains("rebuild_managed_assets"));
        assert!(crate_root.contains("rebuild_managed_version_bundle"));
        assert!(!crate_root.contains("prepare_managed_libraries_reconstruction"));
        assert!(crate_root.contains("reconstruct_known_good"));
        assert!(!crate_root.contains("KnownGoodActivationSource"));
        assert!(!crate_root.contains("reconstruct_build,"));
        assert!(!dispatcher.contains(concat!("PathBuf", "::new()")));
        assert!(!downloader.contains("    pub async fn reconstruct_version("));
        assert!(!loaders.contains("pub async fn reconstruct_build("));
        assert!(!downloader.contains("ReconstructionLibraryContext"));
        assert!(!loaders.contains("reconstruct_managed_libraries"));
        assert!(!loader_strategies.contains("reconstruct_libraries_from_"));
        assert!(!loader_strategies.contains(concat!("Downloader::new(", "PathBuf::new())")));
        assert!(!crate_root.contains("VersionBundleTransactionCommitReceipt"));
        assert!(!crate_root.contains("ManagedVersionBundleDisposition"));
        assert!(!crate_root.contains("ManagedReconstructionContext"));
        assert!(!crate_root.contains("ManagedRootPublicationLease"));
    }

    fn assert_sentinel_untouched(root: &std::path::Path, sentinel: &std::path::Path) {
        assert_eq!(fs::read(sentinel).expect("sentinel remains"), b"untouched");
        assert_eq!(fs::read_dir(root).expect("sentinel root").count(), 1);
    }
}
