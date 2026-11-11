use super::model::DownloadError;
use super::model::SelectedDownloadArtifactKind;
use super::transfer::AuthenticatedSelectedArtifactSource;
use crate::known_good::{MAX_TIER2_AGGREGATE_BYTES, MAX_TIER2_ARTIFACT_BYTES};
use crate::loaders::types::LoaderError;
use crate::managed::blocking::{ManagedBlockingTaskError, ManagedBlockingWorkers};
use crate::managed::component::lifecycle::{
    ComponentPublicationSourceIdentity, RetainedComponentPublicationSource,
    StagedComponentPublicationSource,
};
use crate::managed::component::source_spool::{
    RetainedComponentSourceAllocation, RetainedComponentSourceAppendError,
    RetainedComponentSourceSpool, RetainedComponentSourceSpoolError,
};
use crate::managed::component::table::ManagedComponentArtifactKind;
use crate::managed::fs::ManagedDir;
use crate::managed::publication::ManagedPublicationLifetimeGuard;
use crate::portable_path::{PortablePathKey, PortableRelativePath};
use axial_resource::{PhysicalScratchPermit, process_physical_work};
#[cfg(test)]
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
    workers: ManagedBlockingWorkers,
}

pub(super) struct AssetSourceScratchPermit {
    _local: Option<OwnedSemaphorePermit>,
    _global: Option<PhysicalScratchPermit>,
}

pub(crate) struct RetainedAssetComponentSource {
    allocation: RetainedComponentSourceAllocation,
    relative_path: PortableRelativePath,
    observed_size: u64,
    observed_sha1: [u8; 20],
    kind: ManagedComponentArtifactKind,
}

#[derive(Default)]
pub(crate) struct RetainedAssetSourceSet {
    sources: BTreeMap<PortableRelativePath, RetainedAssetComponentSource>,
    portable_paths: BTreeMap<PortablePathKey, PortableRelativePath>,
    retained_bytes: u64,
}

impl AssetSourcePool {
    pub(crate) fn new_with_workers(workers: ManagedBlockingWorkers) -> Result<Self, DownloadError> {
        Ok(Self {
            acquisition_permits: Arc::new(Semaphore::new(ASSET_SOURCE_BUDGET_UNITS as usize)),
            spool: RetainedComponentSourceSpool::new(MAX_TIER2_AGGREGATE_BYTES)
                .map_err(retained_spool_download_error)?,
            workers,
        })
    }

    pub(super) fn ensure_active(&self) -> Result<(), DownloadError> {
        self.workers
            .ensure_active()
            .map_err(managed_blocking_download_error)
    }

    #[cfg(test)]
    fn available_bytes(&self) -> u64 {
        self.acquisition_permits.available_permits() as u64 * ASSET_SOURCE_BUDGET_UNIT_BYTES
    }

    #[cfg(test)]
    fn retained_available_bytes(&self) -> u64 {
        self.spool.available_bytes()
    }

    pub(super) async fn reserve(
        &self,
        expected_size: u64,
    ) -> Result<AssetSourceScratchPermit, DownloadError> {
        self.ensure_active()?;
        if expected_size > MAX_TIER2_ARTIFACT_BYTES {
            return Err(asset_source_integrity_error(
                "exceeds the bounded scratch limit",
            ));
        }
        if expected_size == 0 {
            return Ok(AssetSourceScratchPermit {
                _local: None,
                _global: None,
            });
        }
        let units = expected_size.div_ceil(ASSET_SOURCE_BUDGET_UNIT_BYTES) as u32;
        let local = Arc::clone(&self.acquisition_permits)
            .acquire_many_owned(units)
            .await
            .map_err(|_| asset_source_integrity_error("scratch budget is closed"))?;
        let global = process_physical_work()
            .reserve_scratch(expected_size)
            .await
            .map_err(|_| asset_source_integrity_error("process scratch budget is closed"))?;
        Ok(AssetSourceScratchPermit {
            _local: Some(local),
            _global: global,
        })
    }

    pub(super) async fn retain_index(
        &self,
        source: &AuthenticatedSelectedArtifactSource,
        relative_path: PortableRelativePath,
    ) -> Result<RetainedAssetComponentSource, DownloadError> {
        self.retain(
            source,
            relative_path,
            ManagedComponentArtifactKind::AssetIndex,
            SelectedDownloadArtifactKind::AssetIndex,
            AssetSourceScratchPermit {
                _local: None,
                _global: None,
            },
        )
        .await
    }

    pub(super) async fn retain_object(
        &self,
        source: &AuthenticatedSelectedArtifactSource,
        relative_path: PortableRelativePath,
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
        relative_path: PortableRelativePath,
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
        let allocation = self
            .workers
            .run(move |cancellation| {
                let allocation = spool.append_authenticated(
                    Cursor::new(bytes),
                    observed_size,
                    observed_sha1,
                    &cancellation,
                );
                drop(permit);
                allocation
            })
            .await
            .map_err(managed_blocking_download_error)?;
        let allocation = match allocation {
            Ok(allocation) => allocation,
            Err(RetainedComponentSourceAppendError::Cancelled) => {
                return Err(managed_blocking_download_error(
                    ManagedBlockingTaskError::Cancelled,
                ));
            }
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

    #[cfg(test)]
    pub(crate) async fn retain_authenticated_local_bytes(
        &self,
        relative_path: PortableRelativePath,
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
        let allocation = self
            .workers
            .run(move |cancellation| {
                let allocation = spool.append_authenticated(
                    Cursor::new(bytes),
                    observed_size,
                    observed_sha1,
                    &cancellation,
                );
                drop(permit);
                allocation
            })
            .await
            .map_err(managed_blocking_download_error)?
            .map_err(|error| match error {
                RetainedComponentSourceAppendError::Cancelled => {
                    managed_blocking_download_error(ManagedBlockingTaskError::Cancelled)
                }
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
        let portable = path.key();
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

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.sources.len()
    }

    pub(crate) fn into_sources(self) -> Vec<RetainedAssetComponentSource> {
        self.sources.into_values().collect()
    }

    pub(crate) fn retained_replay(&self) -> Self {
        Self {
            sources: self
                .sources
                .iter()
                .map(|(path, source)| {
                    (
                        path.clone(),
                        RetainedAssetComponentSource {
                            allocation: source.allocation.retained_replay(),
                            relative_path: source.relative_path.clone(),
                            observed_size: source.observed_size,
                            observed_sha1: source.observed_sha1,
                            kind: source.kind,
                        },
                    )
                })
                .collect(),
            portable_paths: self.portable_paths.clone(),
            retained_bytes: self.retained_bytes,
        }
    }
}

impl RetainedComponentPublicationSource for RetainedAssetComponentSource {
    fn relative_path(&self) -> &PortableRelativePath {
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
    ) -> Result<StagedComponentPublicationSource, LoaderError> {
        let reader = self
            .allocation
            .into_reader()
            .map_err(retained_spool_loader_error)?;
        let file = staging_bucket
            .import_authenticated_create_new(
                slot,
                reader,
                self.observed_size,
                self.observed_sha1,
                lifetime_guard,
            )
            .await?;
        Ok(StagedComponentPublicationSource::new(
            ComponentPublicationSourceIdentity::new(
                self.relative_path,
                self.kind,
                self.observed_size,
                self.observed_sha1,
            ),
            file,
        ))
    }
}

fn asset_source_integrity_error(message: &str) -> DownloadError {
    DownloadError::Integrity(format!("asset source {message}"))
}

fn managed_blocking_download_error(error: ManagedBlockingTaskError) -> DownloadError {
    let (kind, message) = match error {
        ManagedBlockingTaskError::Cancelled => (
            io::ErrorKind::Interrupted,
            "retained asset source work was cancelled",
        ),
        ManagedBlockingTaskError::TaskStopped => (
            io::ErrorKind::Other,
            "retained asset source task stopped unexpectedly",
        ),
    };
    DownloadError::FileOperation(io::Error::new(kind, message))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::managed::blocking::{ManagedBlockingCheckpoint, ManagedBlockingWorkers};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Condvar, Mutex};
    use tokio::sync::oneshot;

    #[tokio::test]
    async fn cancellation_drain_joins_blocked_asset_spool_and_restores_budgets() {
        let (entered_tx, entered_rx) = oneshot::channel();
        let entered_tx = Arc::new(Mutex::new(Some(entered_tx)));
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let exited = Arc::new(AtomicUsize::new(0));
        let hook_release = Arc::clone(&release);
        let hook_exited = Arc::clone(&exited);
        let workers =
            ManagedBlockingWorkers::new_with_checkpoint_hook(Arc::new(move |checkpoint| {
                if checkpoint != ManagedBlockingCheckpoint::SourceSpool {
                    return;
                }
                let Some(entered) = entered_tx.lock().expect("asset spool entered lock").take()
                else {
                    return;
                };
                let _ = entered.send(());
                let (lock, condition) = &*hook_release;
                let released = lock.lock().expect("asset spool gate lock");
                drop(
                    condition
                        .wait_while(released, |released| !*released)
                        .expect("asset spool gate wait"),
                );
                hook_exited.fetch_add(1, Ordering::Release);
            }));
        let pool = AssetSourcePool::new_with_workers(workers.clone()).expect("asset source pool");
        let pool_for_task = pool.clone();
        let task = tokio::spawn(async move {
            pool_for_task
                .retain_authenticated_local_bytes(
                    PortableRelativePath::new("objects/aa/aa01").expect("asset path"),
                    ManagedComponentArtifactKind::AssetObject,
                    b"blocked asset spool".to_vec(),
                )
                .await
        });

        entered_rx.await.expect("asset spool worker entered");
        assert!(pool.available_bytes() < MAX_TIER2_ARTIFACT_BYTES);
        assert!(pool.retained_available_bytes() < MAX_TIER2_AGGREGATE_BYTES);
        workers.cancel();
        task.abort();
        let _ = task.await;
        let mut drain = Box::pin(workers.drain());
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(25), &mut drain)
                .await
                .is_err()
        );

        let (lock, condition) = &*release;
        *lock.lock().expect("asset spool release lock") = true;
        condition.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(1), drain)
            .await
            .expect("asset spool worker must acknowledge cancellation");
        assert_eq!(exited.load(Ordering::Acquire), 1);
        assert_eq!(pool.available_bytes(), MAX_TIER2_ARTIFACT_BYTES);
        assert_eq!(pool.retained_available_bytes(), MAX_TIER2_AGGREGATE_BYTES);
    }
}
