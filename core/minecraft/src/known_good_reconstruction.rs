use crate::download::{
    Downloader, ManagedProjectionSequenceEffect, ManagedProjectionSequenceError,
    ManagedProjectionSequenceOutcome, ManagedReconstructionContext,
    publish_managed_projection_sequence,
};
#[cfg(test)]
use crate::download::{
    ManagedProjectionSequenceFault, publish_managed_projection_sequence_with_fault,
};
use crate::known_good::{
    KnownGoodInventory, KnownGoodReconstructionReceipt, ManagedAssetsReconstruction,
    ManagedKnownGoodComponent, ManagedLibrariesReconstruction, ManagedVersionBundleReconstruction,
    ManagedWholeInstanceReconstruction, RetainedKnownGoodReconstruction,
};
use crate::managed_component_lifecycle::{
    ManagedComponentCommittedReceipt, ManagedComponentLifecycleOutcome,
    ManagedComponentRolledBackReceipt, publish_managed_component_effect,
    revalidate_managed_component_projection,
};
use crate::managed_component_publication::ComponentRollbackEffect;
use crate::managed_component_table::ManagedComponentKind;
use crate::managed_fs::ManagedDir;
use crate::managed_publication::{
    ManagedPublicationLifetimeGuard, ManagedRootPublicationLease, run_publication_blocking,
};
#[cfg(feature = "test-support")]
use crate::runtime::{
    ComponentManifest, ComponentManifestDownload, ComponentManifestDownloads,
    ComponentManifestFile, authenticated_runtime_source_from_manifest_for_test,
    runtime_java_relative_path,
};
use crate::runtime::{
    ManagedRuntimeCache, ManagedRuntimeCommitReceipt, ManagedRuntimeFailureReceipt,
    ManagedRuntimeQuarantineObligation, ManagedRuntimeRebuildError, RuntimeId,
    finalize_managed_runtime_commit, rebuild_managed_runtime_component_from_source,
};
use crate::version_bundle_publication::{
    VersionBundleTransactionEffect, VersionBundleTransactionSettledOutcome, publish_version_bundle,
    revalidate_settled_version_bundle, settle_version_bundle_publication,
    settled_version_bundle_matches_root,
};
#[cfg(feature = "test-support")]
use sha1::{Digest as _, Sha1};
use std::path::{Path, PathBuf};
#[cfg(feature = "test-support")]
use std::{collections::HashMap, io};

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
    effect: ManagedVersionBundleRollbackEffect,
}

pub struct ManagedWholeInstanceCommitReceipt {
    authority: Box<CommittedWholeInstanceAuthority>,
}

pub struct ManagedWholeInstanceRollbackReceipt {
    authority: Box<RolledBackWholeInstanceAuthority>,
}

struct CommittedWholeInstanceAuthority {
    projection: KnownGoodReconstructionReceipt,
    root_lease: ManagedRootPublicationLease,
    runtime: ManagedRuntimeCommitReceipt,
}

struct RolledBackWholeInstanceAuthority {
    projection: KnownGoodReconstructionReceipt,
    root: WholeInstanceRootAuthority,
    runtime: WholeInstanceRuntimeTerminal,
    effect: ManagedWholeInstanceRollbackEffect,
}

enum WholeInstanceRootAuthority {
    Lease(ManagedRootPublicationLease),
    Guard(ManagedPublicationLifetimeGuard),
}

enum WholeInstanceRuntimeTerminal {
    Committed(ManagedRuntimeCommitReceipt),
    Failed(ManagedRuntimeFailureReceipt),
}

struct SettledVersionBundleRebuildAuthority {
    projection: KnownGoodReconstructionReceipt,
    lease: ManagedRootPublicationLease,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManagedWholeInstanceRollbackEffect {
    RuntimePublication,
    Assets(ManagedAssetsRollbackEffect),
    Libraries(ManagedLibrariesRollbackEffect),
    VersionBundle(ManagedVersionBundleRollbackEffect),
    ComponentPublication(ManagedKnownGoodComponent),
    ExactPostcheck,
    RuntimeFinalization,
}

pub enum ManagedLibrariesRebuildError {
    Reconstruction(KnownGoodReconstructionError),
    Preparation,
    RolledBack(ManagedLibrariesRollbackReceipt),
}

pub enum ManagedAssetsRebuildError {
    Reconstruction(KnownGoodReconstructionError),
    Preparation,
    RolledBack(ManagedAssetsRollbackReceipt),
}

pub enum ManagedVersionBundleRebuildError {
    Reconstruction(KnownGoodReconstructionError),
    Preparation,
    RolledBack(ManagedVersionBundleRollbackReceipt),
}

pub enum ManagedWholeInstanceRebuildError {
    Reconstruction(KnownGoodReconstructionError),
    Preparation,
    RuntimePreparation,
    RolledBack(ManagedWholeInstanceRollbackReceipt),
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

impl std::fmt::Debug for ManagedWholeInstanceCommitReceipt {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ManagedWholeInstanceCommitReceipt { .. }")
    }
}

impl std::fmt::Debug for ManagedWholeInstanceRollbackReceipt {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ManagedWholeInstanceRollbackReceipt { .. }")
    }
}

impl std::fmt::Debug for ManagedLibrariesRebuildError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Reconstruction(_) => "ManagedLibrariesRebuildError::Reconstruction(..)",
            Self::Preparation => "ManagedLibrariesRebuildError::Preparation",
            Self::RolledBack(_) => "ManagedLibrariesRebuildError::RolledBack(..)",
        })
    }
}

impl std::fmt::Debug for ManagedAssetsRebuildError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Reconstruction(_) => "ManagedAssetsRebuildError::Reconstruction(..)",
            Self::Preparation => "ManagedAssetsRebuildError::Preparation",
            Self::RolledBack(_) => "ManagedAssetsRebuildError::RolledBack(..)",
        })
    }
}

impl std::fmt::Debug for ManagedVersionBundleRebuildError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Reconstruction(_) => "ManagedVersionBundleRebuildError::Reconstruction(..)",
            Self::Preparation => "ManagedVersionBundleRebuildError::Preparation",
            Self::RolledBack(_) => "ManagedVersionBundleRebuildError::RolledBack(..)",
        })
    }
}

impl std::fmt::Debug for ManagedWholeInstanceRebuildError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Reconstruction(_) => "ManagedWholeInstanceRebuildError::Reconstruction(..)",
            Self::Preparation => "ManagedWholeInstanceRebuildError::Preparation",
            Self::RuntimePreparation => "ManagedWholeInstanceRebuildError::RuntimePreparation",
            Self::RolledBack(_) => "ManagedWholeInstanceRebuildError::RolledBack(..)",
        })
    }
}

impl std::fmt::Display for ManagedLibrariesRebuildError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Reconstruction(_) => "managed Libraries reconstruction failed",
            Self::Preparation => "managed Libraries rebuild failed before its canonical effect",
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
            Self::RolledBack(_) => "managed Assets rebuild rolled back",
        })
    }
}

impl std::error::Error for ManagedAssetsRebuildError {}

impl std::fmt::Display for ManagedVersionBundleRebuildError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Reconstruction(_) => "managed VersionBundle reconstruction failed",
            Self::Preparation => "managed VersionBundle rebuild failed before its canonical effect",
            Self::RolledBack(_) => "managed VersionBundle rebuild rolled back",
        })
    }
}

impl std::error::Error for ManagedVersionBundleRebuildError {}

impl std::fmt::Display for ManagedWholeInstanceRebuildError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Reconstruction(_) => "managed whole-instance reconstruction failed",
            Self::Preparation => {
                "managed whole-instance rematerialization failed before its canonical effect"
            }
            Self::RuntimePreparation => {
                "managed Runtime rematerialization failed before its canonical effect"
            }
            Self::RolledBack(_) => "managed whole-instance rematerialization retained rollback",
        })
    }
}

impl std::error::Error for ManagedWholeInstanceRebuildError {}

impl ManagedLibrariesCommitReceipt {
    pub fn version_id(&self) -> &str {
        self.authority.projection.version_id()
    }

    pub async fn matches_root(&self, expected: &Path) -> bool {
        self.authority.terminal.matches_root(expected).await
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

    pub async fn matches_root(&self, expected: &Path) -> bool {
        settled_version_bundle_matches_root(&self.authority.lease, expected).await
    }

    pub fn matches_known_good_inventory(&self, expected: &KnownGoodInventory) -> bool {
        version_bundle_projections_match(&self.authority.projection, expected)
    }

    pub async fn revalidate(&self) -> bool {
        let Ok(projection) = self
            .authority
            .projection
            .component_projection(ManagedKnownGoodComponent::VersionBundle)
        else {
            return false;
        };
        revalidate_settled_version_bundle(&self.authority.lease, projection).await
    }
}

impl ManagedVersionBundleRollbackReceipt {
    pub fn version_id(&self) -> &str {
        self.authority.projection.version_id()
    }

    pub async fn matches_root(&self, expected: &Path) -> bool {
        settled_version_bundle_matches_root(&self.authority.lease, expected).await
    }

    pub fn matches_known_good_inventory(&self, expected: &KnownGoodInventory) -> bool {
        version_bundle_projections_match(&self.authority.projection, expected)
    }

    pub fn effect(&self) -> ManagedVersionBundleRollbackEffect {
        self.effect
    }
}

impl ManagedWholeInstanceCommitReceipt {
    pub fn version_id(&self) -> &str {
        self.authority.projection.version_id()
    }

    pub fn runtime_component(&self) -> &RuntimeId {
        self.authority.runtime.component()
    }

    pub async fn matches_root(&self, expected: &Path) -> bool {
        self.authority
            .root_lease
            .lifetime_guard()
            .matches_root(expected)
            .await
    }

    pub fn matches_runtime_cache(&self, expected: &ManagedRuntimeCache) -> bool {
        self.authority.runtime.matches_cache(expected)
    }

    pub fn matches_known_good_inventory(&self, expected: &KnownGoodInventory) -> bool {
        self.authority.projection.matches_inventory(expected)
            && self
                .authority
                .runtime
                .matches_known_good_inventory(expected)
    }

    pub async fn revalidate(&self, runtime_cache: &ManagedRuntimeCache) -> bool {
        revalidate_whole_projection(&self.authority.root_lease, &self.authority.projection).await
            && self
                .authority
                .runtime
                .revalidate(runtime_cache, self.authority.runtime.component())
                .await
    }
}

impl ManagedWholeInstanceRollbackReceipt {
    pub fn version_id(&self) -> &str {
        self.authority.projection.version_id()
    }

    pub fn runtime_component(&self) -> &RuntimeId {
        match &self.authority.runtime {
            WholeInstanceRuntimeTerminal::Committed(receipt) => receipt.component(),
            WholeInstanceRuntimeTerminal::Failed(receipt) => receipt.component(),
        }
    }

    pub fn effect(&self) -> ManagedWholeInstanceRollbackEffect {
        self.authority.effect
    }

    pub fn runtime_quarantine_obligation(&self) -> Option<&ManagedRuntimeQuarantineObligation> {
        match &self.authority.runtime {
            WholeInstanceRuntimeTerminal::Committed(receipt) => receipt.quarantine_obligation(),
            WholeInstanceRuntimeTerminal::Failed(receipt) => receipt.quarantine_obligation(),
        }
    }

    pub async fn matches_root(&self, expected: &Path) -> bool {
        match &self.authority.root {
            WholeInstanceRootAuthority::Lease(lease) => {
                lease.lifetime_guard().matches_root(expected).await
            }
            WholeInstanceRootAuthority::Guard(guard) => guard.matches_root(expected).await,
        }
    }

    pub fn matches_runtime_cache(&self, expected: &ManagedRuntimeCache) -> bool {
        match &self.authority.runtime {
            WholeInstanceRuntimeTerminal::Committed(receipt) => receipt.matches_cache(expected),
            WholeInstanceRuntimeTerminal::Failed(receipt) => receipt.matches_cache(expected),
        }
    }

    pub fn matches_known_good_inventory(&self, expected: &KnownGoodInventory) -> bool {
        self.authority.projection.matches_inventory(expected)
            && match &self.authority.runtime {
                WholeInstanceRuntimeTerminal::Committed(receipt) => {
                    receipt.matches_known_good_inventory(expected)
                }
                WholeInstanceRuntimeTerminal::Failed(receipt) => {
                    receipt.matches_known_good_inventory(expected)
                }
            }
    }
}

async fn revalidate_whole_projection(
    lease: &ManagedRootPublicationLease,
    projection: &KnownGoodReconstructionReceipt,
) -> bool {
    for (component, kind) in [
        (
            ManagedKnownGoodComponent::Assets,
            ManagedComponentKind::Assets,
        ),
        (
            ManagedKnownGoodComponent::Libraries,
            ManagedComponentKind::Libraries,
        ),
    ] {
        let Ok(component) = projection.component_projection(component) else {
            return false;
        };
        if !revalidate_managed_component_projection(lease, &component, kind).await {
            return false;
        }
    }
    let Ok(version_bundle) =
        projection.component_projection(ManagedKnownGoodComponent::VersionBundle)
    else {
        return false;
    };
    revalidate_settled_version_bundle(lease, version_bundle).await
}

fn whole_rollback_effect(
    effect: ManagedProjectionSequenceEffect,
) -> ManagedWholeInstanceRollbackEffect {
    match effect {
        ManagedProjectionSequenceEffect::Assets(effect) => {
            ManagedWholeInstanceRollbackEffect::Assets(match effect {
                ComponentRollbackEffect::None => ManagedAssetsRollbackEffect::None,
                ComponentRollbackEffect::Execution => ManagedAssetsRollbackEffect::Execution,
                ComponentRollbackEffect::Reconciliation => {
                    ManagedAssetsRollbackEffect::Reconciliation
                }
            })
        }
        ManagedProjectionSequenceEffect::Libraries(effect) => {
            ManagedWholeInstanceRollbackEffect::Libraries(match effect {
                ComponentRollbackEffect::None => ManagedLibrariesRollbackEffect::None,
                ComponentRollbackEffect::Execution => ManagedLibrariesRollbackEffect::Execution,
                ComponentRollbackEffect::Reconciliation => {
                    ManagedLibrariesRollbackEffect::Reconciliation
                }
            })
        }
        ManagedProjectionSequenceEffect::VersionBundle(effect) => {
            ManagedWholeInstanceRollbackEffect::VersionBundle(match effect {
                VersionBundleTransactionEffect::Promotion => {
                    ManagedVersionBundleRollbackEffect::Promotion
                }
                VersionBundleTransactionEffect::Postcheck => {
                    ManagedVersionBundleRollbackEffect::Postcheck
                }
                VersionBundleTransactionEffect::Rollback => {
                    ManagedVersionBundleRollbackEffect::Rollback
                }
            })
        }
    }
}

fn version_bundle_projections_match(
    reconstructed: &KnownGoodReconstructionReceipt,
    expected: &KnownGoodInventory,
) -> bool {
    let Ok(reconstructed) =
        reconstructed.component_projection(ManagedKnownGoodComponent::VersionBundle)
    else {
        return false;
    };
    let Ok(expected) =
        expected.managed_component_projection(ManagedKnownGoodComponent::VersionBundle)
    else {
        return false;
    };
    reconstructed.entry_count() == expected.entry_count()
        && reconstructed
            .entries()
            .iter()
            .zip(expected.entries())
            .all(|(left, right)| {
                left.inventory_ordinal() == right.inventory_ordinal()
                    && left.entry() == right.entry()
            })
}

pub async fn rebuild_managed_libraries(
    managed_root: impl Into<PathBuf>,
    version_id: &str,
) -> Result<ManagedLibrariesCommitReceipt, ManagedLibrariesRebuildError> {
    let managed_root = managed_root.into();
    let version_id = version_id.to_string();
    let owner = tokio::spawn(async move {
        let reconstruction = prepare_managed_libraries_reconstruction(managed_root, &version_id)
            .await
            .map_err(ManagedLibrariesRebuildError::Reconstruction)?;
        publish_managed_libraries_reconstruction(reconstruction).await
    });
    owner
        .await
        .map_err(|_| ManagedLibrariesRebuildError::Preparation)?
}

pub async fn rebuild_managed_assets(
    managed_root: impl Into<PathBuf>,
    version_id: &str,
) -> Result<ManagedAssetsCommitReceipt, ManagedAssetsRebuildError> {
    let managed_root = managed_root.into();
    let version_id = version_id.to_string();
    let owner = tokio::spawn(async move {
        let reconstruction = prepare_managed_assets_reconstruction(managed_root, &version_id)
            .await
            .map_err(ManagedAssetsRebuildError::Reconstruction)?;
        publish_managed_assets_reconstruction(reconstruction).await
    });
    owner
        .await
        .map_err(|_| ManagedAssetsRebuildError::Preparation)?
}

pub async fn rebuild_managed_version_bundle(
    managed_root: impl Into<PathBuf>,
    version_id: &str,
) -> Result<ManagedVersionBundleCommitReceipt, ManagedVersionBundleRebuildError> {
    let managed_root = managed_root.into();
    let version_id = version_id.to_string();
    let owner = tokio::spawn(async move {
        let reconstruction =
            prepare_managed_version_bundle_reconstruction(managed_root, &version_id)
                .await
                .map_err(ManagedVersionBundleRebuildError::Reconstruction)?;
        publish_managed_version_bundle_reconstruction(reconstruction).await
    });
    owner
        .await
        .map_err(|_| ManagedVersionBundleRebuildError::Preparation)?
}

pub async fn rematerialize_managed_instance(
    managed_root: impl Into<PathBuf>,
    runtime_cache: &ManagedRuntimeCache,
    version_id: &str,
) -> Result<ManagedWholeInstanceCommitReceipt, ManagedWholeInstanceRebuildError> {
    let managed_root = managed_root.into();
    let runtime_cache = runtime_cache.clone();
    let version_id = version_id.to_string();
    let owner = spawn_managed_whole_instance_owner(async move {
        let reconstruction =
            prepare_managed_whole_instance_reconstruction(managed_root, &version_id)
                .await
                .map_err(ManagedWholeInstanceRebuildError::Reconstruction)?;
        publish_managed_whole_instance_reconstruction(reconstruction, runtime_cache).await
    });
    await_managed_whole_instance_owner(owner).await
}

fn spawn_managed_whole_instance_owner<F>(
    owner: F,
) -> tokio::task::JoinHandle<
    Result<ManagedWholeInstanceCommitReceipt, ManagedWholeInstanceRebuildError>,
>
where
    F: std::future::Future<
            Output = Result<ManagedWholeInstanceCommitReceipt, ManagedWholeInstanceRebuildError>,
        > + Send
        + 'static,
{
    tokio::spawn(owner)
}

async fn await_managed_whole_instance_owner(
    owner: tokio::task::JoinHandle<
        Result<ManagedWholeInstanceCommitReceipt, ManagedWholeInstanceRebuildError>,
    >,
) -> Result<ManagedWholeInstanceCommitReceipt, ManagedWholeInstanceRebuildError> {
    owner
        .await
        .map_err(|_| ManagedWholeInstanceRebuildError::Preparation)?
}

async fn publish_managed_whole_instance_reconstruction(
    reconstruction: ManagedWholeInstanceReconstruction,
    runtime_cache: ManagedRuntimeCache,
) -> Result<ManagedWholeInstanceCommitReceipt, ManagedWholeInstanceRebuildError> {
    publish_managed_whole_instance_reconstruction_inner(
        reconstruction,
        runtime_cache,
        #[cfg(test)]
        None,
        #[cfg(any(test, feature = "test-support"))]
        false,
    )
    .await
}

#[cfg(test)]
async fn publish_managed_whole_instance_reconstruction_with_fault(
    reconstruction: ManagedWholeInstanceReconstruction,
    runtime_cache: ManagedRuntimeCache,
    fault: ManagedProjectionSequenceFault,
) -> Result<ManagedWholeInstanceCommitReceipt, ManagedWholeInstanceRebuildError> {
    publish_managed_whole_instance_reconstruction_inner(
        reconstruction,
        runtime_cache,
        Some(fault),
        false,
    )
    .await
}

async fn publish_managed_whole_instance_reconstruction_inner(
    reconstruction: ManagedWholeInstanceReconstruction,
    runtime_cache: ManagedRuntimeCache,
    #[cfg(test)] fault: Option<ManagedProjectionSequenceFault>,
    #[cfg(any(test, feature = "test-support"))] fail_runtime_finalization: bool,
) -> Result<ManagedWholeInstanceCommitReceipt, ManagedWholeInstanceRebuildError> {
    let (
        root_lease,
        projection,
        version_bundle_source,
        library_sources,
        asset_sources,
        runtime_source,
    ) = reconstruction.into_effect_parts();
    let root_guard = root_lease.lifetime_guard();
    let runtime_component = runtime_source.component().clone();
    let mut observer = |_| {};
    let runtime = match rebuild_managed_runtime_component_from_source(
        &runtime_cache,
        &runtime_component,
        runtime_source,
        &mut observer,
    )
    .await
    {
        Ok(receipt) => receipt,
        Err(ManagedRuntimeRebuildError::Preparation(_)) => {
            return Err(ManagedWholeInstanceRebuildError::RuntimePreparation);
        }
        Err(ManagedRuntimeRebuildError::Effect(receipt)) => {
            return Err(whole_rollback(
                projection,
                WholeInstanceRootAuthority::Lease(root_lease),
                WholeInstanceRuntimeTerminal::Failed(receipt),
                ManagedWholeInstanceRollbackEffect::RuntimePublication,
            ));
        }
    };
    if !runtime.revalidate(&runtime_cache, &runtime_component).await
        || !runtime.matches_known_good_inventory(projection.inventory())
    {
        let ManagedRuntimeRebuildError::Effect(runtime) =
            runtime.into_failure(crate::runtime::JavaRuntimeLookupError::Install(
                "whole-instance Runtime failed its exact postcheck".to_string(),
            ))
        else {
            unreachable!("a retained Runtime commit always becomes effect evidence")
        };
        return Err(whole_rollback(
            projection,
            WholeInstanceRootAuthority::Lease(root_lease),
            WholeInstanceRuntimeTerminal::Failed(runtime),
            ManagedWholeInstanceRollbackEffect::ExactPostcheck,
        ));
    }
    #[cfg(test)]
    let sequence = match fault {
        Some(fault) => {
            publish_managed_projection_sequence_with_fault(
                root_lease,
                &projection,
                asset_sources,
                library_sources,
                version_bundle_source,
                fault,
            )
            .await
        }
        None => {
            publish_managed_projection_sequence(
                root_lease,
                &projection,
                asset_sources,
                library_sources,
                version_bundle_source,
            )
            .await
        }
    };
    #[cfg(not(test))]
    let sequence = publish_managed_projection_sequence(
        root_lease,
        &projection,
        asset_sources,
        library_sources,
        version_bundle_source,
    )
    .await;
    let root_lease = match sequence {
        Ok(ManagedProjectionSequenceOutcome::Committed(lease)) => lease,
        Ok(ManagedProjectionSequenceOutcome::RolledBack { lease, effect }) => {
            return Err(whole_rollback(
                projection,
                WholeInstanceRootAuthority::Lease(lease),
                WholeInstanceRuntimeTerminal::Committed(runtime),
                whole_rollback_effect(effect),
            ));
        }
        Err(error) => {
            return Err(whole_sequence_failure(
                projection, root_guard, runtime, error,
            ));
        }
    };
    if !revalidate_whole_projection(&root_lease, &projection).await
        || !runtime.revalidate(&runtime_cache, &runtime_component).await
        || !runtime.matches_known_good_inventory(projection.inventory())
    {
        return Err(whole_rollback(
            projection,
            WholeInstanceRootAuthority::Lease(root_lease),
            WholeInstanceRuntimeTerminal::Committed(runtime),
            ManagedWholeInstanceRollbackEffect::ExactPostcheck,
        ));
    }
    let runtime = match settle_whole_runtime_commit(
        runtime,
        #[cfg(any(test, feature = "test-support"))]
        fail_runtime_finalization,
    )
    .await
    {
        Ok(runtime) => runtime,
        Err(runtime) => {
            return Err(whole_rollback(
                projection,
                WholeInstanceRootAuthority::Lease(root_lease),
                WholeInstanceRuntimeTerminal::Failed(runtime),
                ManagedWholeInstanceRollbackEffect::RuntimeFinalization,
            ));
        }
    };
    if !revalidate_whole_projection(&root_lease, &projection).await
        || !runtime.revalidate(&runtime_cache, &runtime_component).await
        || !runtime.matches_known_good_inventory(projection.inventory())
    {
        return Err(whole_rollback(
            projection,
            WholeInstanceRootAuthority::Lease(root_lease),
            WholeInstanceRuntimeTerminal::Committed(runtime),
            ManagedWholeInstanceRollbackEffect::ExactPostcheck,
        ));
    }
    Ok(ManagedWholeInstanceCommitReceipt {
        authority: Box::new(CommittedWholeInstanceAuthority {
            projection,
            root_lease,
            runtime,
        }),
    })
}

async fn settle_whole_runtime_commit(
    runtime: ManagedRuntimeCommitReceipt,
    #[cfg(any(test, feature = "test-support"))] fail_for_test: bool,
) -> Result<ManagedRuntimeCommitReceipt, ManagedRuntimeFailureReceipt> {
    #[cfg(any(test, feature = "test-support"))]
    if fail_for_test {
        return crate::runtime::finalize_managed_runtime_commit_with_failure_for_test(runtime)
            .await;
    }
    finalize_managed_runtime_commit(runtime).await
}

fn whole_sequence_failure(
    projection: KnownGoodReconstructionReceipt,
    root_guard: ManagedPublicationLifetimeGuard,
    runtime: ManagedRuntimeCommitReceipt,
    error: ManagedProjectionSequenceError,
) -> ManagedWholeInstanceRebuildError {
    whole_rollback(
        projection,
        WholeInstanceRootAuthority::Guard(root_guard),
        WholeInstanceRuntimeTerminal::Committed(runtime),
        ManagedWholeInstanceRollbackEffect::ComponentPublication(error.component()),
    )
}

fn whole_rollback(
    projection: KnownGoodReconstructionReceipt,
    root: WholeInstanceRootAuthority,
    runtime: WholeInstanceRuntimeTerminal,
    effect: ManagedWholeInstanceRollbackEffect,
) -> ManagedWholeInstanceRebuildError {
    ManagedWholeInstanceRebuildError::RolledBack(ManagedWholeInstanceRollbackReceipt {
        authority: Box::new(RolledBackWholeInstanceAuthority {
            projection,
            root,
            runtime,
            effect,
        }),
    })
}

#[cfg(feature = "test-support")]
pub async fn rebuild_managed_libraries_fixture_for_test(
    managed_root: impl Into<PathBuf>,
    version_id: &str,
) -> Result<ManagedLibrariesCommitReceipt, ManagedLibrariesRebuildError> {
    let managed_root = managed_root.into();
    let version_id = version_id.to_string();
    let owner = tokio::spawn(async move {
        let guarded_root = run_publication_blocking(move || ManagedDir::open_root(&managed_root))
            .await
            .map_err(|_| ManagedLibrariesRebuildError::Preparation)?
            .map_err(|_| ManagedLibrariesRebuildError::Preparation)?;
        let reconstruction = crate::known_good::managed_libraries_reconstruction_fixture_for_test(
            guarded_root,
            &version_id,
        )
        .map_err(|_| ManagedLibrariesRebuildError::Preparation)?;
        publish_managed_libraries_reconstruction(reconstruction).await
    });
    owner
        .await
        .map_err(|_| ManagedLibrariesRebuildError::Preparation)?
}

#[cfg(feature = "test-support")]
pub struct ManagedWholeInstanceFixtureForTest {
    reconstruction: ManagedWholeInstanceReconstruction,
}

#[cfg(feature = "test-support")]
impl ManagedWholeInstanceFixtureForTest {
    pub fn known_good_inventory(&self) -> KnownGoodInventory {
        self.reconstruction.fixture_inventory_for_test()
    }
}

#[cfg(feature = "test-support")]
pub async fn prepare_managed_whole_instance_fixture_for_test(
    managed_root: impl Into<PathBuf>,
    version_id: &str,
    runtime_component: RuntimeId,
    runtime_url: &str,
    runtime_bytes: &[u8],
) -> io::Result<ManagedWholeInstanceFixtureForTest> {
    let runtime_source = authenticated_runtime_source_from_manifest_for_test(
        runtime_component,
        ComponentManifest {
            files: HashMap::from([(
                runtime_java_relative_path().replace('\\', "/"),
                ComponentManifestFile {
                    kind: "file".to_string(),
                    executable: true,
                    downloads: Some(ComponentManifestDownloads {
                        raw: Some(ComponentManifestDownload {
                            url: runtime_url.to_string(),
                            sha1: Some(format!("{:x}", Sha1::digest(runtime_bytes))),
                            size: Some(runtime_bytes.len() as u64),
                        }),
                        lzma: None,
                    }),
                    target: None,
                },
            )]),
        },
    )
    .map_err(|_| io::Error::other("whole-instance Runtime fixture authentication failed"))?;
    let managed_root = managed_root.into();
    let managed_root = ManagedDir::open_root(&managed_root)
        .map_err(|_| io::Error::other("whole-instance fixture root admission failed"))?;
    let reconstruction = crate::known_good::managed_whole_instance_reconstruction_fixture_for_test(
        managed_root,
        version_id,
        runtime_source,
    )
    .await
    .map_err(|_| io::Error::other("whole-instance fixture reconstruction failed"))?;
    Ok(ManagedWholeInstanceFixtureForTest { reconstruction })
}

#[cfg(feature = "test-support")]
pub async fn publish_managed_whole_instance_fixture_for_test(
    fixture: ManagedWholeInstanceFixtureForTest,
    runtime_cache: &ManagedRuntimeCache,
) -> Result<ManagedWholeInstanceCommitReceipt, ManagedWholeInstanceRebuildError> {
    publish_managed_whole_instance_reconstruction(fixture.reconstruction, runtime_cache.clone())
        .await
}

#[cfg(feature = "test-support")]
pub async fn publish_managed_whole_instance_rollback_fixture_for_test(
    fixture: ManagedWholeInstanceFixtureForTest,
    runtime_cache: &ManagedRuntimeCache,
) -> Result<ManagedWholeInstanceCommitReceipt, ManagedWholeInstanceRebuildError> {
    publish_managed_whole_instance_reconstruction_inner(
        fixture.reconstruction,
        runtime_cache.clone(),
        #[cfg(test)]
        None,
        true,
    )
    .await
}

#[cfg(feature = "test-support")]
pub async fn rebuild_managed_assets_fixture_for_test(
    managed_root: impl Into<PathBuf>,
    version_id: &str,
) -> Result<ManagedAssetsCommitReceipt, ManagedAssetsRebuildError> {
    let managed_root = managed_root.into();
    let version_id = version_id.to_string();
    let owner = tokio::spawn(async move {
        let guarded_root = run_publication_blocking(move || ManagedDir::open_root(&managed_root))
            .await
            .map_err(|_| ManagedAssetsRebuildError::Preparation)?
            .map_err(|_| ManagedAssetsRebuildError::Preparation)?;
        let reconstruction = crate::known_good::managed_assets_reconstruction_fixture_for_test(
            guarded_root,
            &version_id,
        )
        .await
        .map_err(|_| ManagedAssetsRebuildError::Preparation)?;
        publish_managed_assets_reconstruction(reconstruction).await
    });
    owner
        .await
        .map_err(|_| ManagedAssetsRebuildError::Preparation)?
}

#[cfg(any(test, feature = "test-support"))]
pub async fn rebuild_managed_version_bundle_fixture_for_test(
    managed_root: impl Into<PathBuf>,
    version_id: &str,
) -> Result<ManagedVersionBundleCommitReceipt, ManagedVersionBundleRebuildError> {
    let managed_root = managed_root.into();
    let version_id = version_id.to_string();
    let owner = tokio::spawn(async move {
        let guarded_root = run_publication_blocking(move || ManagedDir::open_root(&managed_root))
            .await
            .map_err(|_| ManagedVersionBundleRebuildError::Preparation)?
            .map_err(|_| ManagedVersionBundleRebuildError::Preparation)?;
        let reconstruction =
            crate::known_good::managed_version_bundle_reconstruction_fixture_for_test(
                guarded_root,
                &version_id,
            )
            .map_err(|_| ManagedVersionBundleRebuildError::Preparation)?;
        publish_managed_version_bundle_reconstruction(reconstruction).await
    });
    owner
        .await
        .map_err(|_| ManagedVersionBundleRebuildError::Preparation)?
}

#[cfg(feature = "test-support")]
pub async fn rebuild_managed_version_bundle_rollback_fixture_for_test(
    managed_root: impl Into<PathBuf>,
    version_id: &str,
) -> Result<ManagedVersionBundleCommitReceipt, ManagedVersionBundleRebuildError> {
    crate::version_bundle_publication::fail_after_promotions_for_test(version_id, 1);
    rebuild_managed_version_bundle_fixture_for_test(managed_root, version_id).await
}

async fn publish_managed_libraries_reconstruction(
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
    let lease = ManagedRootPublicationLease::acquire(managed_root)
        .await
        .map_err(|_| ManagedVersionBundleRebuildError::Preparation)?;
    let publication = {
        let version_bundle = projection
            .component_projection(ManagedKnownGoodComponent::VersionBundle)
            .map_err(|_| ManagedVersionBundleRebuildError::Preparation)?;
        publish_version_bundle(lease, source, version_bundle).await
    };
    let settled = settle_version_bundle_publication(publication)
        .await
        .map_err(|_| ManagedVersionBundleRebuildError::Preparation)?;
    match settled {
        VersionBundleTransactionSettledOutcome::Committed(lease) => {
            Ok(ManagedVersionBundleCommitReceipt {
                authority: Box::new(SettledVersionBundleRebuildAuthority { projection, lease }),
            })
        }
        VersionBundleTransactionSettledOutcome::RolledBack { lease, effect } => Err(
            ManagedVersionBundleRebuildError::RolledBack(ManagedVersionBundleRollbackReceipt {
                authority: Box::new(SettledVersionBundleRebuildAuthority { projection, lease }),
                effect: match effect {
                    VersionBundleTransactionEffect::Promotion => {
                        ManagedVersionBundleRollbackEffect::Promotion
                    }
                    VersionBundleTransactionEffect::Postcheck => {
                        ManagedVersionBundleRollbackEffect::Postcheck
                    }
                    VersionBundleTransactionEffect::Rollback => {
                        ManagedVersionBundleRollbackEffect::Rollback
                    }
                },
            }),
        ),
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
    managed_root: impl Into<PathBuf>,
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
    managed_root: impl Into<PathBuf>,
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

async fn prepare_managed_version_bundle_reconstruction(
    managed_root: impl Into<PathBuf>,
    version_id: &str,
) -> Result<ManagedVersionBundleReconstruction, KnownGoodReconstructionError> {
    let kind = reconstruction_kind(version_id);
    let managed_root = managed_root.into();
    let guarded_root = run_publication_blocking(move || ManagedDir::open_root(&managed_root))
        .await
        .map_err(|_| KnownGoodReconstructionError::ManagedRoot)?
        .map_err(|_| KnownGoodReconstructionError::ManagedRoot)?;
    let context = ManagedReconstructionContext::version_bundle();
    let reconstruction = reconstruct_managed_authority(version_id, &context, kind).await?;
    reconstruction
        .bind_managed_version_bundle(guarded_root)
        .map_err(|_| reconstruction_error_for(kind))
}

async fn prepare_managed_whole_instance_reconstruction(
    managed_root: impl Into<PathBuf>,
    version_id: &str,
) -> Result<ManagedWholeInstanceReconstruction, KnownGoodReconstructionError> {
    let kind = reconstruction_kind(version_id);
    let managed_root = managed_root.into();
    let guarded_root = run_publication_blocking(move || ManagedDir::open_root(&managed_root))
        .await
        .map_err(|_| KnownGoodReconstructionError::ManagedRoot)?
        .map_err(|_| KnownGoodReconstructionError::ManagedRoot)?;
    let root_lease = ManagedRootPublicationLease::acquire(guarded_root.clone())
        .await
        .map_err(|_| KnownGoodReconstructionError::ManagedRoot)?;
    let context = ManagedReconstructionContext::bind_whole_instance(guarded_root.clone())
        .await
        .map_err(|_| KnownGoodReconstructionError::ManagedRoot)?;
    let reconstruction = reconstruct_managed_authority(version_id, &context, kind).await?;
    let (library_cache_proofs, asset_sources, asset_cache_proofs) = context
        .take_whole_instance_authority()
        .map_err(|_| reconstruction_error_for(kind))?;
    reconstruction
        .bind_managed_whole_instance(
            root_lease,
            library_cache_proofs,
            asset_sources,
            asset_cache_proofs,
        )
        .map_err(|_| reconstruction_error_for(kind))
}

async fn prepare_managed_reconstruction(
    managed_root: impl Into<PathBuf>,
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
    let managed_root = managed_root.into();
    let guarded_root = run_publication_blocking(move || ManagedDir::open_root(&managed_root))
        .await
        .map_err(|_| KnownGoodReconstructionError::ManagedRoot)?
        .map_err(|_| KnownGoodReconstructionError::ManagedRoot)?;
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
        KnownGoodReconstructionError, ManagedWholeInstanceRebuildError,
        ManagedWholeInstanceRollbackEffect, ReconstructionKind, reconstruct_known_good,
        reconstruction_kind,
    };
    use crate::download::ManagedProjectionSequenceFault;
    use crate::managed_component_effects::ComponentExecutionFault;
    use crate::runtime::{
        ComponentManifest, ComponentManifestDownload, ComponentManifestDownloads,
        ComponentManifestFile, ManagedRuntimeCache, RuntimeId,
        authenticated_runtime_source_from_manifest_for_test, runtime_java_relative_path,
    };
    use sha1::{Digest as _, Sha1};
    use std::collections::HashMap;
    use std::fs;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

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
    async fn whole_instance_commit_is_exact_exclusive_and_user_owned_free() {
        const VERSION_ID: &str = "whole-instance-success";
        let managed = tempfile::tempdir().expect("managed root");
        let runtime_cache = ManagedRuntimeCache::isolated_for_test().expect("runtime cache");
        let runtime_component = RuntimeId::from("jre-legacy");
        let runtime_root = runtime_cache
            .component_root(runtime_component.as_str())
            .expect("runtime root");
        fs::create_dir(&runtime_root).expect("prior runtime");
        fs::write(runtime_root.join("user-prior"), b"prior runtime").expect("prior runtime bytes");
        let user_sentinels = seed_user_owned_sentinels(managed.path());
        let runtime_url = serve_runtime_bytes(b"whole-instance java", 2).await;
        let reconstruction = whole_instance_fixture(
            managed.path(),
            VERSION_ID,
            &runtime_component,
            &runtime_url,
            b"whole-instance java",
        )
        .await;
        let receipt = super::publish_managed_whole_instance_reconstruction(
            reconstruction,
            runtime_cache.clone(),
        )
        .await
        .expect("whole-instance commit");

        assert_eq!(receipt.version_id(), VERSION_ID);
        assert_eq!(receipt.runtime_component(), &runtime_component);
        assert!(receipt.matches_root(managed.path()).await);
        assert!(receipt.matches_runtime_cache(&runtime_cache));
        assert!(receipt.revalidate(&runtime_cache).await);
        assert!(receipt.matches_known_good_inventory(receipt.authority.projection.inventory()));
        assert!(
            !runtime_root
                .with_file_name("jre-legacy.quarantine")
                .exists()
        );
        assert_user_owned_sentinels(&user_sentinels);

        let waiting_root =
            crate::managed_fs::ManagedDir::open_root(managed.path()).expect("waiting managed root");
        let mut waiter = tokio::spawn(
            crate::managed_publication::ManagedRootPublicationLease::acquire(waiting_root),
        );
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(30), &mut waiter)
                .await
                .is_err(),
            "whole commit must retain managed-root exclusion"
        );
        let client = managed
            .path()
            .join(format!("versions/{VERSION_ID}/{VERSION_ID}.jar"));
        fs::write(&client, b"tampered").expect("tamper committed client");
        assert!(!receipt.revalidate(&runtime_cache).await);
        assert_user_owned_sentinels(&user_sentinels);
        drop(receipt);
        waiter.await.expect("waiting task").expect("waiting lease");
    }

    #[tokio::test]
    async fn whole_instance_late_rollback_restarts_monotonically() {
        const VERSION_ID: &str = "whole-instance-restart";
        let managed = tempfile::tempdir().expect("managed root");
        let runtime_cache = ManagedRuntimeCache::isolated_for_test().expect("runtime cache");
        let runtime_component = RuntimeId::from("jre-legacy");
        let runtime_root = runtime_cache
            .component_root(runtime_component.as_str())
            .expect("runtime root");
        fs::create_dir(&runtime_root).expect("prior runtime");
        fs::write(runtime_root.join("user-prior"), b"prior runtime").expect("prior runtime bytes");
        let user_sentinels = seed_user_owned_sentinels(managed.path());
        let runtime_url = serve_runtime_bytes(b"restart java", 4).await;
        let first = whole_instance_fixture(
            managed.path(),
            VERSION_ID,
            &runtime_component,
            &runtime_url,
            b"restart java",
        )
        .await;
        crate::version_bundle_publication::fail_after_promotions_for_test(VERSION_ID, 1);
        let ManagedWholeInstanceRebuildError::RolledBack(rollback) =
            super::publish_managed_whole_instance_reconstruction(first, runtime_cache.clone())
                .await
                .expect_err("injected VersionBundle rollback")
        else {
            panic!("late failure must retain whole rollback evidence");
        };
        assert_eq!(
            rollback.effect(),
            ManagedWholeInstanceRollbackEffect::VersionBundle(
                super::ManagedVersionBundleRollbackEffect::Promotion
            )
        );
        assert!(rollback.matches_root(managed.path()).await);
        assert!(rollback.matches_runtime_cache(&runtime_cache));
        let runtime_quarantine = rollback
            .runtime_quarantine_obligation()
            .expect("late rollback must expose retained Runtime quarantine");
        assert!(runtime_quarantine.matches_cache(&runtime_cache));
        assert_eq!(
            runtime_quarantine.observation(),
            crate::runtime::ManagedRuntimeQuarantineObservation::Present
        );
        assert!(
            runtime_root
                .with_file_name("jre-legacy.quarantine")
                .exists()
        );
        assert_user_owned_sentinels(&user_sentinels);
        drop(rollback);

        let retry = whole_instance_fixture(
            managed.path(),
            VERSION_ID,
            &runtime_component,
            &runtime_url,
            b"restart java",
        )
        .await;
        let receipt =
            super::publish_managed_whole_instance_reconstruction(retry, runtime_cache.clone())
                .await
                .expect("restart settles exact generation");
        assert!(receipt.revalidate(&runtime_cache).await);
        assert!(
            !runtime_root
                .with_file_name("jre-legacy.quarantine")
                .exists()
        );
        assert_user_owned_sentinels(&user_sentinels);
    }

    #[tokio::test]
    async fn whole_instance_component_rollbacks_retain_runtime_and_root_authority() {
        for (label, fault, expected_effect) in [
            (
                "assets",
                ManagedProjectionSequenceFault::Assets(ComponentExecutionFault::AfterFirstRow),
                ManagedWholeInstanceRollbackEffect::Assets(
                    super::ManagedAssetsRollbackEffect::Execution,
                ),
            ),
            (
                "libraries",
                ManagedProjectionSequenceFault::Libraries(ComponentExecutionFault::AfterFirstRow),
                ManagedWholeInstanceRollbackEffect::Libraries(
                    super::ManagedLibrariesRollbackEffect::Execution,
                ),
            ),
        ] {
            let version_id = format!("whole-instance-{label}-rollback");
            let managed = tempfile::tempdir().expect("managed root");
            let runtime_cache = ManagedRuntimeCache::isolated_for_test().expect("runtime cache");
            let runtime_component = RuntimeId::from("jre-legacy");
            let runtime_root = runtime_cache
                .component_root(runtime_component.as_str())
                .expect("runtime root");
            fs::create_dir(&runtime_root).expect("prior runtime");
            fs::write(runtime_root.join("user-prior"), b"prior runtime")
                .expect("prior runtime bytes");
            let user_sentinels = seed_user_owned_sentinels(managed.path());
            let runtime_url = serve_runtime_bytes(b"component rollback java", 2).await;
            let reconstruction = whole_instance_fixture(
                managed.path(),
                &version_id,
                &runtime_component,
                &runtime_url,
                b"component rollback java",
            )
            .await;

            let ManagedWholeInstanceRebuildError::RolledBack(rollback) =
                super::publish_managed_whole_instance_reconstruction_with_fault(
                    reconstruction,
                    runtime_cache.clone(),
                    fault,
                )
                .await
                .expect_err("injected component rollback")
            else {
                panic!("component failure must retain whole rollback evidence");
            };
            assert_eq!(rollback.effect(), expected_effect);
            assert!(rollback.matches_root(managed.path()).await);
            assert!(rollback.matches_runtime_cache(&runtime_cache));
            assert!(
                rollback.matches_known_good_inventory(rollback.authority.projection.inventory())
            );
            let quarantine = rollback
                .runtime_quarantine_obligation()
                .expect("component rollback must retain displaced Runtime");
            assert!(quarantine.matches_cache(&runtime_cache));
            assert_eq!(
                quarantine.observation(),
                crate::runtime::ManagedRuntimeQuarantineObservation::Present
            );
            assert!(
                runtime_root
                    .with_file_name("jre-legacy.quarantine")
                    .exists()
            );
            assert_user_owned_sentinels(&user_sentinels);

            let waiting_root = crate::managed_fs::ManagedDir::open_root(managed.path())
                .expect("waiting managed root");
            let mut waiter = Box::pin(
                crate::managed_publication::ManagedRootPublicationLease::acquire(waiting_root),
            );
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(30), &mut waiter)
                    .await
                    .is_err(),
                "component rollback must retain whole-root exclusion"
            );
            drop(rollback);
            drop(waiter.await.expect("waiting root lease"));
        }
    }

    #[tokio::test]
    async fn whole_instance_caller_cancellation_retains_admitted_work_until_settlement() {
        const VERSION_ID: &str = "whole-instance-cancelled-caller";
        let managed = tempfile::tempdir().expect("managed root");
        let runtime_cache = ManagedRuntimeCache::isolated_for_test().expect("runtime cache");
        let runtime_component = RuntimeId::from("jre-legacy");
        let runtime_root = runtime_cache
            .component_root(runtime_component.as_str())
            .expect("runtime root");
        fs::create_dir(&runtime_root).expect("prior runtime");
        fs::write(runtime_root.join("user-prior"), b"prior runtime").expect("prior runtime bytes");
        let user_sentinels = seed_user_owned_sentinels(managed.path());
        let (runtime_url, runtime_reached, release_runtime) =
            serve_blocked_runtime_bytes(b"cancelled caller java").await;
        let reconstruction = whole_instance_fixture(
            managed.path(),
            VERSION_ID,
            &runtime_component,
            &runtime_url,
            b"cancelled caller java",
        )
        .await;
        let cache_for_owner = runtime_cache.clone();
        let (settled_tx, settled_rx) = tokio::sync::oneshot::channel();
        let owner = super::spawn_managed_whole_instance_owner(async move {
            let result = super::publish_managed_whole_instance_reconstruction(
                reconstruction,
                cache_for_owner,
            )
            .await;
            let _ = settled_tx.send(result.is_ok());
            result
        });
        let caller = tokio::spawn(super::await_managed_whole_instance_owner(owner));

        runtime_reached
            .await
            .expect("admitted Runtime work reached its retained source");
        let waiting_root =
            crate::managed_fs::ManagedDir::open_root(managed.path()).expect("waiting managed root");
        let mut waiter = Box::pin(
            crate::managed_publication::ManagedRootPublicationLease::acquire(waiting_root),
        );
        caller.abort();
        assert!(
            caller
                .await
                .expect_err("caller task must be cancelled")
                .is_cancelled()
        );
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(30), &mut waiter)
                .await
                .is_err(),
            "detached whole-instance work must retain root exclusion"
        );

        release_runtime
            .send(())
            .expect("release retained Runtime source");
        assert!(settled_rx.await.expect("detached settlement observation"));
        drop(waiter.await.expect("waiting root lease"));
        assert_eq!(
            fs::read(runtime_root.join(runtime_java_relative_path()))
                .expect("settled Runtime executable"),
            b"cancelled caller java"
        );
        assert!(
            managed
                .path()
                .join(format!("versions/{VERSION_ID}/{VERSION_ID}.jar"))
                .is_file()
        );
        assert_user_owned_sentinels(&user_sentinels);

        let retry_url = serve_runtime_bytes(b"cancelled caller java", 2).await;
        let retry = whole_instance_fixture(
            managed.path(),
            VERSION_ID,
            &runtime_component,
            &retry_url,
            b"cancelled caller java",
        )
        .await;
        let receipt =
            super::publish_managed_whole_instance_reconstruction(retry, runtime_cache.clone())
                .await
                .expect("settled cancelled generation must retry monotonically");
        assert!(receipt.revalidate(&runtime_cache).await);
        assert_user_owned_sentinels(&user_sentinels);
    }

    #[tokio::test]
    async fn whole_instance_runtime_finalization_failure_retains_both_authorities() {
        const VERSION_ID: &str = "whole-instance-runtime-finalization";
        let managed = tempfile::tempdir().expect("managed root");
        let runtime_cache = ManagedRuntimeCache::isolated_for_test().expect("runtime cache");
        let runtime_component = RuntimeId::from("jre-legacy");
        let runtime_root = runtime_cache
            .component_root(runtime_component.as_str())
            .expect("runtime root");
        fs::create_dir(&runtime_root).expect("prior runtime");
        fs::write(runtime_root.join("user-prior"), b"prior runtime").expect("prior runtime bytes");
        let user_sentinels = seed_user_owned_sentinels(managed.path());
        let runtime_url = serve_runtime_bytes(b"finalization java", 2).await;
        let reconstruction = whole_instance_fixture(
            managed.path(),
            VERSION_ID,
            &runtime_component,
            &runtime_url,
            b"finalization java",
        )
        .await;
        let ManagedWholeInstanceRebuildError::RolledBack(rollback) =
            super::publish_managed_whole_instance_reconstruction_inner(
                reconstruction,
                runtime_cache.clone(),
                None,
                true,
            )
            .await
            .expect_err("injected Runtime finalization failure")
        else {
            panic!("Runtime finalization failure must retain whole rollback evidence");
        };

        assert_eq!(
            rollback.effect(),
            ManagedWholeInstanceRollbackEffect::RuntimeFinalization
        );
        assert!(rollback.matches_root(managed.path()).await);
        assert!(rollback.matches_runtime_cache(&runtime_cache));
        assert!(rollback.matches_known_good_inventory(rollback.authority.projection.inventory()));
        let runtime_quarantine = rollback
            .runtime_quarantine_obligation()
            .expect("Runtime finalization rollback must expose its quarantine");
        assert!(runtime_quarantine.matches_cache(&runtime_cache));
        assert_eq!(
            runtime_quarantine.observation(),
            crate::runtime::ManagedRuntimeQuarantineObservation::Present
        );
        let super::WholeInstanceRuntimeTerminal::Failed(runtime) = &rollback.authority.runtime
        else {
            panic!("Runtime finalization failure must retain Runtime failure authority");
        };
        assert!(runtime.revalidate(&runtime_cache, &runtime_component).await);
        assert!(
            runtime_root
                .with_file_name("jre-legacy.quarantine")
                .exists()
        );
        assert_user_owned_sentinels(&user_sentinels);
    }

    async fn whole_instance_fixture(
        managed_root: &std::path::Path,
        version_id: &str,
        runtime_component: &RuntimeId,
        runtime_url: &str,
        runtime_bytes: &[u8],
    ) -> crate::known_good::ManagedWholeInstanceReconstruction {
        let runtime_source = authenticated_runtime_source_from_manifest_for_test(
            runtime_component.clone(),
            ComponentManifest {
                files: HashMap::from([(
                    runtime_java_relative_path().replace('\\', "/"),
                    ComponentManifestFile {
                        kind: "file".to_string(),
                        executable: true,
                        downloads: Some(ComponentManifestDownloads {
                            raw: Some(ComponentManifestDownload {
                                url: runtime_url.to_string(),
                                sha1: Some(format!("{:x}", Sha1::digest(runtime_bytes))),
                                size: Some(runtime_bytes.len() as u64),
                            }),
                            lzma: None,
                        }),
                        target: None,
                    },
                )]),
            },
        )
        .expect("authenticated Runtime fixture");
        crate::known_good::managed_whole_instance_reconstruction_fixture_for_test(
            crate::managed_fs::ManagedDir::open_root(managed_root).expect("guarded managed root"),
            version_id,
            runtime_source,
        )
        .await
        .expect("whole-instance reconstruction fixture")
    }

    async fn serve_runtime_bytes(bytes: &'static [u8], requests: usize) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("runtime byte server");
        let address = listener.local_addr().expect("runtime byte address");
        tokio::spawn(async move {
            for _ in 0..requests {
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

    async fn serve_blocked_runtime_bytes(
        bytes: &'static [u8],
    ) -> (
        String,
        tokio::sync::oneshot::Receiver<()>,
        tokio::sync::oneshot::Sender<()>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("blocked Runtime byte server");
        let address = listener.local_addr().expect("blocked Runtime byte address");
        let (reached_tx, reached_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut request = [0_u8; 1024];
            let _ = socket.read(&mut request).await;
            let _ = reached_tx.send(());
            let _ = release_rx.await;
            let headers = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                bytes.len()
            );
            if socket.write_all(headers.as_bytes()).await.is_ok() {
                let _ = socket.write_all(bytes).await;
            }
        });
        (format!("http://{address}/java"), reached_rx, release_tx)
    }

    fn seed_user_owned_sentinels(root: &std::path::Path) -> Vec<std::path::PathBuf> {
        ["mods", "config", "saves", "resourcepacks"]
            .into_iter()
            .map(|directory| {
                let directory = root.join(directory);
                fs::create_dir(&directory).expect("user-owned directory");
                let sentinel = directory.join("guardian-user-owned");
                fs::write(&sentinel, b"user-owned").expect("user-owned sentinel");
                sentinel
            })
            .collect()
    }

    fn assert_user_owned_sentinels(sentinels: &[std::path::PathBuf]) {
        for sentinel in sentinels {
            assert_eq!(
                fs::read(sentinel).expect("user-owned sentinel"),
                b"user-owned"
            );
        }
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

    #[tokio::test]
    async fn version_bundle_fixture_settles_exact_effect_without_touching_user_owned_state() {
        const VERSION_ID: &str = "fixture-version-bundle-1.0.0";
        const CLIENT_PATH: &str =
            "versions/fixture-version-bundle-1.0.0/fixture-version-bundle-1.0.0.jar";
        let root = tempfile::tempdir().expect("managed fixture root");
        let user_sentinel = root.path().join("mods/user-owned.txt");
        fs::create_dir_all(user_sentinel.parent().expect("user sentinel parent"))
            .expect("create user sentinel parent");
        fs::write(&user_sentinel, b"user-owned").expect("seed user sentinel");

        let receipt =
            super::rebuild_managed_version_bundle_fixture_for_test(root.path(), VERSION_ID)
                .await
                .expect("committed fixture rebuild");

        assert_eq!(receipt.version_id(), VERSION_ID);
        assert!(receipt.matches_root(root.path()).await);
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
    async fn version_bundle_fixture_returns_settled_rollback_with_exact_effect() {
        const VERSION_ID: &str = "fixture-version-bundle-rollback";
        let root = tempfile::tempdir().expect("managed fixture root");
        let user_sentinel = root.path().join("saves/user-owned/level.dat");
        fs::create_dir_all(user_sentinel.parent().expect("user sentinel parent"))
            .expect("create user sentinel parent");
        fs::write(&user_sentinel, b"user-owned").expect("seed user sentinel");
        crate::version_bundle_publication::fail_after_promotions_for_test(VERSION_ID, 1);

        let super::ManagedVersionBundleRebuildError::RolledBack(receipt) =
            super::rebuild_managed_version_bundle_fixture_for_test(root.path(), VERSION_ID)
                .await
                .expect_err("injected rebuild must roll back")
        else {
            panic!("rebuild must return its settled rollback receipt");
        };
        assert_eq!(receipt.version_id(), VERSION_ID);
        assert!(receipt.matches_root(root.path()).await);
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
            super::ManagedLibrariesRebuildError::Reconstruction(
                KnownGoodReconstructionError::Vanilla,
            ),
        ] {
            assert!(std::error::Error::source(&error).is_none());
            assert!(!error.to_string().contains('/'));
        }
        for error in [
            super::ManagedAssetsRebuildError::Preparation,
            super::ManagedAssetsRebuildError::Reconstruction(KnownGoodReconstructionError::Loader),
        ] {
            assert!(std::error::Error::source(&error).is_none());
            assert!(!error.to_string().contains('/'));
        }
        for error in [
            super::ManagedVersionBundleRebuildError::Preparation,
            super::ManagedVersionBundleRebuildError::Reconstruction(
                KnownGoodReconstructionError::Loader,
            ),
        ] {
            assert!(std::error::Error::source(&error).is_none());
            assert!(!error.to_string().contains('/'));
        }
        for error in [
            super::ManagedWholeInstanceRebuildError::Preparation,
            super::ManagedWholeInstanceRebuildError::RuntimePreparation,
            super::ManagedWholeInstanceRebuildError::Reconstruction(
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
        assert!(
            std::mem::size_of::<super::ManagedWholeInstanceRebuildError>()
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
        assert!(crate_root.contains("rematerialize_managed_instance"));
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
