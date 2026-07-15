use crate::download::{Downloader, ManagedReconstructionContext};
use crate::known_good::{
    KnownGoodInventory, KnownGoodReconstructionReceipt, ManagedAssetsReconstruction,
    ManagedKnownGoodComponent, ManagedLibrariesReconstruction, ManagedVersionBundleReconstruction,
    RetainedKnownGoodReconstruction,
};
use crate::managed_component_lifecycle::{
    ManagedComponentCommittedReceipt, ManagedComponentLifecycleOutcome,
    ManagedComponentRolledBackReceipt, publish_managed_component_effect,
};
use crate::managed_component_publication::ComponentRollbackEffect;
use crate::managed_component_table::ManagedComponentKind;
use crate::managed_fs::ManagedDir;
use crate::managed_publication::{ManagedRootPublicationLease, run_publication_blocking};
use crate::version_bundle_publication::{
    VersionBundleTransactionEffect, VersionBundleTransactionSettledOutcome, publish_version_bundle,
    revalidate_settled_version_bundle, settle_version_bundle_publication,
    settled_version_bundle_matches_root,
};
use std::path::{Path, PathBuf};

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
        KnownGoodReconstructionError, ReconstructionKind, reconstruct_known_good,
        reconstruction_kind,
    };
    use std::fs;

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
    }

    fn assert_sentinel_untouched(root: &std::path::Path, sentinel: &std::path::Path) {
        assert_eq!(fs::read(sentinel).expect("sentinel remains"), b"untouched");
        assert_eq!(fs::read_dir(root).expect("sentinel root").count(), 1);
    }
}
