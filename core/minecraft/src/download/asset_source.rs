use super::model::DownloadError;
use super::model::SelectedDownloadArtifactKind;
use super::transfer::AuthenticatedSelectedArtifactSource;
use crate::artifact_path::ArtifactRelativePath;
use crate::known_good::{MAX_TIER2_AGGREGATE_BYTES, MAX_TIER2_ARTIFACT_BYTES};
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
use std::io::{self, Cursor};
use std::sync::Arc;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

const ASSET_SOURCE_BUDGET_UNIT_BYTES: u64 = 1 << 20;
const ASSET_SOURCE_BUDGET_UNITS: u32 =
    (MAX_TIER2_ARTIFACT_BYTES / ASSET_SOURCE_BUDGET_UNIT_BYTES) as u32;

#[derive(Clone)]
pub(super) struct AssetSourcePool {
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

impl AssetSourcePool {
    pub(super) fn new() -> Result<Self, DownloadError> {
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
