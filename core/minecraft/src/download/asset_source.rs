use super::model::DownloadError;
use super::model::SelectedDownloadArtifactKind;
use super::transfer::AuthenticatedSelectedArtifactSource;
use crate::artifact_path::ArtifactRelativePath;
use crate::known_good::{
    KnownGoodArtifactKind, KnownGoodIntegrity, KnownGoodRoot, MAX_TIER2_AGGREGATE_BYTES,
    MAX_TIER2_ARTIFACT_BYTES, ManagedComponentProjection, ManagedKnownGoodComponent,
};
use crate::loaders::types::LoaderError;
use crate::managed_component_lifecycle::{
    ComponentPublicationSourceIdentity, RetainedComponentPublicationSource,
};
use crate::managed_component_source_spool::{
    RetainedComponentSourceAllocation, RetainedComponentSourceAppendError,
    RetainedComponentSourceSpool, RetainedComponentSourceSpoolError,
};
use crate::managed_component_table::ManagedComponentArtifactKind;
use crate::managed_fs::ManagedDir;
use crate::managed_publication::ManagedPublicationLifetimeGuard;
#[cfg(any(test, feature = "test-support"))]
use sha1::{Digest as _, Sha1};
use std::collections::BTreeMap;
use std::io::{self, Cursor};
use std::sync::Arc;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

const ASSET_SOURCE_BUDGET_UNIT_BYTES: u64 = 1 << 20;
const ASSET_SOURCE_BUDGET_UNITS: u32 =
    (MAX_TIER2_ARTIFACT_BYTES / ASSET_SOURCE_BUDGET_UNIT_BYTES) as u32;

#[derive(Clone)]
pub(crate) struct AssetSourcePool {
    acquisition_permits: Arc<Semaphore>,
    spool: Arc<RetainedComponentSourceSpool>,
}

pub(super) struct AssetSourceScratchPermit {
    _permit: Option<OwnedSemaphorePermit>,
}

pub(crate) struct RetainedAssetComponentSource {
    allocation: RetainedComponentSourceAllocation,
    relative_path: ArtifactRelativePath,
    observed_size: u64,
    observed_sha1: [u8; 20],
    kind: ManagedComponentArtifactKind,
}

#[derive(Default)]
pub(crate) struct RetainedAssetSourceSet {
    sources: BTreeMap<ArtifactRelativePath, RetainedAssetComponentSource>,
    portable_paths: BTreeMap<String, ArtifactRelativePath>,
    retained_bytes: u64,
}

pub(crate) struct AuthenticatedAssetCacheProof {
    relative_path: ArtifactRelativePath,
    observed_size: u64,
    observed_sha1: [u8; 20],
}

#[derive(Default)]
pub(crate) struct AuthenticatedAssetCacheProofSet {
    proofs: BTreeMap<ArtifactRelativePath, AuthenticatedAssetCacheProof>,
    portable_paths: BTreeMap<String, ArtifactRelativePath>,
}

impl AssetSourcePool {
    pub(crate) fn new() -> Result<Self, DownloadError> {
        Ok(Self {
            acquisition_permits: Arc::new(Semaphore::new(ASSET_SOURCE_BUDGET_UNITS as usize)),
            spool: RetainedComponentSourceSpool::new(MAX_TIER2_AGGREGATE_BYTES)
                .map_err(retained_spool_download_error)?,
        })
    }

    pub(super) async fn reserve(
        &self,
        expected_size: u64,
    ) -> Result<AssetSourceScratchPermit, DownloadError> {
        if expected_size > MAX_TIER2_ARTIFACT_BYTES {
            return Err(asset_source_integrity_error(
                "exceeds the bounded scratch limit",
            ));
        }
        if expected_size == 0 {
            return Ok(AssetSourceScratchPermit { _permit: None });
        }
        let units = expected_size.div_ceil(ASSET_SOURCE_BUDGET_UNIT_BYTES) as u32;
        Arc::clone(&self.acquisition_permits)
            .acquire_many_owned(units)
            .await
            .map(|permit| AssetSourceScratchPermit {
                _permit: Some(permit),
            })
            .map_err(|_| asset_source_integrity_error("scratch budget is closed"))
    }

    pub(super) async fn retain_index(
        &self,
        source: &AuthenticatedSelectedArtifactSource,
        relative_path: ArtifactRelativePath,
    ) -> Result<RetainedAssetComponentSource, DownloadError> {
        self.retain(
            source,
            relative_path,
            ManagedComponentArtifactKind::AssetIndex,
            SelectedDownloadArtifactKind::AssetIndex,
            AssetSourceScratchPermit { _permit: None },
        )
        .await
    }

    pub(super) async fn retain_object(
        &self,
        source: &AuthenticatedSelectedArtifactSource,
        relative_path: ArtifactRelativePath,
        permit: AssetSourceScratchPermit,
    ) -> Result<RetainedAssetComponentSource, DownloadError> {
        self.retain(
            source,
            relative_path,
            ManagedComponentArtifactKind::AssetObject,
            SelectedDownloadArtifactKind::AssetObject,
            permit,
        )
        .await
    }

    async fn retain(
        &self,
        source: &AuthenticatedSelectedArtifactSource,
        relative_path: ArtifactRelativePath,
        kind: ManagedComponentArtifactKind,
        source_kind: SelectedDownloadArtifactKind,
        permit: AssetSourceScratchPermit,
    ) -> Result<RetainedAssetComponentSource, DownloadError> {
        if source.kind() != source_kind {
            return Err(asset_source_integrity_error("kind is invalid"));
        }
        let bytes = source.shared_bytes();
        let observed_size = source.observed_size();
        let observed_sha1 = source.observed_sha1();
        let spool = Arc::clone(&self.spool);
        let allocation = tokio::task::spawn_blocking(move || {
            let allocation =
                spool.append_authenticated(Cursor::new(bytes), observed_size, observed_sha1);
            drop(permit);
            allocation
        })
        .await
        .map_err(|error| {
            DownloadError::FileOperation(io::Error::other(format!(
                "retained asset source spool task stopped unexpectedly: {error}"
            )))
        })?;
        let allocation = match allocation {
            Ok(allocation) => allocation,
            Err(RetainedComponentSourceAppendError::SourceRejected) => {
                return Err(asset_source_integrity_error(
                    "changed during retained admission",
                ));
            }
            Err(RetainedComponentSourceAppendError::Spool(error)) => {
                return Err(retained_spool_download_error(error));
            }
        };
        Ok(RetainedAssetComponentSource {
            allocation,
            relative_path,
            observed_size,
            observed_sha1,
            kind,
        })
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) async fn retain_authenticated_local_bytes(
        &self,
        relative_path: ArtifactRelativePath,
        kind: ManagedComponentArtifactKind,
        bytes: Vec<u8>,
    ) -> Result<RetainedAssetComponentSource, DownloadError> {
        if !matches!(
            kind,
            ManagedComponentArtifactKind::AssetIndex | ManagedComponentArtifactKind::AssetObject
        ) {
            return Err(asset_source_integrity_error("kind is invalid"));
        }
        let observed_size = u64::try_from(bytes.len())
            .map_err(|_| asset_source_integrity_error("size exceeds the platform bound"))?;
        let observed_sha1 = Sha1::digest(&bytes).into();
        let permit = self.reserve(observed_size).await?;
        let spool = Arc::clone(&self.spool);
        let allocation = tokio::task::spawn_blocking(move || {
            let allocation =
                spool.append_authenticated(Cursor::new(bytes), observed_size, observed_sha1);
            drop(permit);
            allocation
        })
        .await
        .map_err(|error| {
            DownloadError::FileOperation(io::Error::other(format!(
                "retained local asset source spool task stopped unexpectedly: {error}"
            )))
        })?
        .map_err(|error| match error {
            RetainedComponentSourceAppendError::SourceRejected => {
                asset_source_integrity_error("changed during retained local admission")
            }
            RetainedComponentSourceAppendError::Spool(error) => {
                retained_spool_download_error(error)
            }
        })?;
        Ok(RetainedAssetComponentSource {
            allocation,
            relative_path,
            observed_size,
            observed_sha1,
            kind,
        })
    }
}

impl RetainedAssetSourceSet {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn insert(
        &mut self,
        source: RetainedAssetComponentSource,
    ) -> Result<(), DownloadError> {
        let path = source.relative_path.clone();
        let portable = path
            .portable_persisted_key()
            .map_err(|_| asset_source_integrity_error("has a non-portable retained identity"))?;
        if self.sources.contains_key(&path)
            || self
                .portable_paths
                .get(&portable)
                .is_some_and(|existing| existing != &path)
        {
            return Err(asset_source_integrity_error(
                "duplicates a retained source identity",
            ));
        }
        let retained_bytes = self
            .retained_bytes
            .checked_add(source.observed_size)
            .filter(|bytes| *bytes <= MAX_TIER2_AGGREGATE_BYTES)
            .ok_or_else(|| asset_source_integrity_error("exceeds the retained aggregate limit"))?;
        self.portable_paths.insert(portable, path.clone());
        self.sources.insert(path, source);
        self.retained_bytes = retained_bytes;
        Ok(())
    }

    pub(crate) fn reconcile_sparse_projection(
        &mut self,
        projection: &ManagedComponentProjection<'_>,
        mut cache_proofs: AuthenticatedAssetCacheProofSet,
    ) -> Result<(), DownloadError> {
        if projection.component() != ManagedKnownGoodComponent::Assets {
            return Err(asset_source_integrity_error(
                "is bound to a non-Assets projection",
            ));
        }
        let mut reconciled = BTreeMap::new();
        let mut projection_portable_paths = BTreeMap::new();
        let mut retained_portable_paths = BTreeMap::new();
        let mut retained_bytes = 0_u64;
        for projected in projection.entries().iter().copied() {
            let entry = projected.entry();
            if entry.root() != &KnownGoodRoot::Assets {
                return Err(asset_source_integrity_error(
                    "has a non-Assets projection root",
                ));
            }
            let path = ArtifactRelativePath::new(entry.path().as_str())
                .map_err(|_| asset_source_integrity_error("has an invalid projection path"))?;
            let portable = path.portable_persisted_key().map_err(|_| {
                asset_source_integrity_error("has a non-portable projection identity")
            })?;
            if projection_portable_paths
                .insert(portable.clone(), path.clone())
                .is_some()
            {
                return Err(asset_source_integrity_error(
                    "duplicates a portable projection identity",
                ));
            }
            let expected_kind = match entry.kind() {
                KnownGoodArtifactKind::AssetIndex => ManagedComponentArtifactKind::AssetIndex,
                KnownGoodArtifactKind::AssetObject => ManagedComponentArtifactKind::AssetObject,
                _ => {
                    return Err(asset_source_integrity_error(
                        "has a non-Assets projection kind",
                    ));
                }
            };
            let (expected_sha1, expected_size) = match entry.integrity() {
                KnownGoodIntegrity::Sha1 { digest, size }
                | KnownGoodIntegrity::ExactBytes { digest, size } => (digest.to_bytes(), *size),
                KnownGoodIntegrity::Directory | KnownGoodIntegrity::LinkTarget(_) => {
                    return Err(asset_source_integrity_error(
                        "has a non-file projection integrity",
                    ));
                }
            };
            let source = self.sources.remove(&path);
            let proof = cache_proofs.proofs.remove(&path);
            match (source, proof) {
                (Some(source), None)
                    if source.kind == expected_kind
                        && source.observed_size == expected_size
                        && source.observed_sha1 == expected_sha1 =>
                {
                    retained_bytes = retained_bytes
                        .checked_add(source.observed_size)
                        .filter(|bytes| *bytes <= MAX_TIER2_AGGREGATE_BYTES)
                        .ok_or_else(|| {
                            asset_source_integrity_error("exceeds the retained aggregate limit")
                        })?;
                    retained_portable_paths.insert(portable, path.clone());
                    reconciled.insert(path, source);
                }
                (None, Some(proof))
                    if expected_kind == ManagedComponentArtifactKind::AssetObject
                        && proof.observed_size == expected_size
                        && proof.observed_sha1 == expected_sha1 => {}
                (Some(_), Some(_)) => {
                    return Err(asset_source_integrity_error(
                        "has both retained and cached authority for one row",
                    ));
                }
                (Some(_), None) => {
                    return Err(asset_source_integrity_error(
                        "does not match its final projection row",
                    ));
                }
                (None, Some(_)) => {
                    return Err(asset_source_integrity_error(
                        "has a cache proof that does not match its final projection row",
                    ));
                }
                (None, None) => {
                    return Err(asset_source_integrity_error(
                        "is missing a projected source",
                    ));
                }
            }
        }
        if !self.sources.is_empty() || !cache_proofs.proofs.is_empty() {
            return Err(asset_source_integrity_error(
                "contains a source outside the final projection",
            ));
        }
        self.sources = reconciled;
        self.portable_paths = retained_portable_paths;
        self.retained_bytes = retained_bytes;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.sources.len()
    }

    pub(crate) fn into_sources(self) -> Vec<RetainedAssetComponentSource> {
        self.sources.into_values().collect()
    }
}

impl AuthenticatedAssetCacheProofSet {
    pub(crate) fn insert(
        &mut self,
        proof: AuthenticatedAssetCacheProof,
    ) -> Result<(), DownloadError> {
        let path = proof.relative_path.clone();
        let portable = path
            .portable_persisted_key()
            .map_err(|_| asset_source_integrity_error("has a non-portable cache identity"))?;
        if self.proofs.contains_key(&path)
            || self
                .portable_paths
                .get(&portable)
                .is_some_and(|existing| existing != &path)
        {
            return Err(asset_source_integrity_error(
                "duplicates an authenticated cache identity",
            ));
        }
        self.portable_paths.insert(portable, path.clone());
        self.proofs.insert(path, proof);
        Ok(())
    }
}

impl AuthenticatedAssetCacheProof {
    pub(crate) fn new(
        relative_path: ArtifactRelativePath,
        observed_size: u64,
        observed_sha1: [u8; 20],
    ) -> Self {
        Self {
            relative_path,
            observed_size,
            observed_sha1,
        }
    }
}

impl RetainedComponentPublicationSource for RetainedAssetComponentSource {
    fn relative_path(&self) -> &ArtifactRelativePath {
        &self.relative_path
    }

    fn kind(&self) -> ManagedComponentArtifactKind {
        self.kind
    }

    fn observed_size(&self) -> u64 {
        self.observed_size
    }

    fn observed_sha1(&self) -> [u8; 20] {
        self.observed_sha1
    }

    async fn stage_create_new(
        self,
        staging_bucket: &ManagedDir,
        slot: &str,
        lifetime_guard: ManagedPublicationLifetimeGuard,
    ) -> Result<ComponentPublicationSourceIdentity, LoaderError> {
        let reader = self
            .allocation
            .into_reader()
            .map_err(retained_spool_loader_error)?;
        staging_bucket
            .import_authenticated_create_new(
                slot,
                reader,
                self.observed_size,
                self.observed_sha1,
                lifetime_guard,
            )
            .await?;
        Ok(ComponentPublicationSourceIdentity::new(
            self.relative_path,
            self.kind,
            self.observed_size,
            self.observed_sha1,
        ))
    }
}

fn asset_source_integrity_error(message: &str) -> DownloadError {
    DownloadError::Integrity(format!("asset source {message}"))
}

fn retained_spool_download_error(error: RetainedComponentSourceSpoolError) -> DownloadError {
    if error.is_capacity_exceeded() {
        asset_source_integrity_error("exceeds the aggregate retained-source limit")
    } else {
        DownloadError::FileOperation(io::Error::other(error.to_string()))
    }
}

fn retained_spool_loader_error(error: RetainedComponentSourceSpoolError) -> LoaderError {
    LoaderError::Io(io::Error::other(error.to_string()))
}
