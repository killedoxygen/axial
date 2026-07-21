use super::facts::{
    emit_execution_download_facts, execution_download_fact, no_download_fact_fields,
    size_mismatch_fact,
};
use super::model::{
    DownloadError, ExactLibraryDownloadProof, ExecutionDownloadFact, ExecutionDownloadFactKind,
    ExpectedIntegrity,
};
use crate::portable_path::{PortablePathKey, PortableRelativePath};
use crate::known_good::MAX_TIER2_AGGREGATE_BYTES;
use crate::known_good::{
    KnownGoodArtifactKind, KnownGoodIntegrity, KnownGoodRoot, ManagedComponentProjection,
    ManagedKnownGoodComponent,
};
use crate::loaders::types::LoaderError;
#[cfg(test)]
use crate::managed_blocking::ManagedBlockingAttemptGuard;
use crate::managed_blocking::{
    ManagedBlockingCheckpoint, ManagedBlockingTaskError, ManagedBlockingWorkers,
    ManagedCancellation,
};
use crate::managed_component_lifecycle::{
    ComponentPublicationSourceIdentity, RetainedComponentPublicationSource,
    StagedComponentPublicationSource,
};
use crate::managed_component_source_spool::{
    RetainedComponentSourceAllocation, RetainedComponentSourceAppendError,
    RetainedComponentSourceReader, RetainedComponentSourceSpool, RetainedComponentSourceSpoolError,
};
use crate::managed_component_table::ManagedComponentArtifactKind;
use crate::managed_fs::ManagedDir;
use crate::managed_publication::ManagedPublicationLifetimeGuard;
use futures_util::StreamExt as _;
use sha1::{Digest as _, Sha1};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, Cursor, Read, Seek, SeekFrom};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncWriteExt as _;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};

pub(super) const LIBRARY_SOURCE_MAX_BYTES: u64 = 512 << 20;
const LIBRARY_SOURCE_BUDGET_UNIT_BYTES: u64 = 1 << 20;
const LIBRARY_SOURCE_BUDGET_UNITS: u32 =
    (LIBRARY_SOURCE_MAX_BYTES / LIBRARY_SOURCE_BUDGET_UNIT_BYTES) as u32;
const LIBRARY_SOURCE_RETRY_DELAYS: [Duration; 3] = [
    Duration::from_millis(500),
    Duration::from_millis(1_500),
    Duration::from_millis(4_000),
];
const MAX_JAR_ENTRIES: u16 = 32_768;
const MAX_JAR_CENTRAL_DIRECTORY_BYTES: u32 = 16 << 20;
const MAX_JAR_ENTRY_NAME_BYTES: usize = 1 << 10;
const MAX_JAR_TOTAL_NAME_BYTES: usize = 8 << 20;
const MAX_JAR_ENTRY_UNCOMPRESSED_BYTES: u64 = LIBRARY_SOURCE_MAX_BYTES;
const MAX_JAR_TOTAL_UNCOMPRESSED_BYTES: u64 = LIBRARY_SOURCE_MAX_BYTES;
const ZIP_END_OF_CENTRAL_DIRECTORY_BYTES: usize = 22;
const ZIP_MAX_COMMENT_BYTES: usize = u16::MAX as usize;

#[derive(Clone)]
pub(super) struct LibrarySourcePool {
    acquisition_permits: Arc<Semaphore>,
    spool: Arc<RetainedComponentSourceSpool>,
    workers: ManagedBlockingWorkers,
}

impl LibrarySourcePool {
    pub(super) fn new_with_workers(workers: ManagedBlockingWorkers) -> Result<Self, DownloadError> {
        Self::with_retained_limit(MAX_TIER2_AGGREGATE_BYTES, workers)
    }

    pub(super) fn with_retained_limit(
        retained_bytes: u64,
        workers: ManagedBlockingWorkers,
    ) -> Result<Self, DownloadError> {
        Ok(Self {
            acquisition_permits: Arc::new(Semaphore::new(LIBRARY_SOURCE_BUDGET_UNITS as usize)),
            spool: RetainedComponentSourceSpool::new(retained_bytes)
                .map_err(retained_spool_download_error)?,
            workers,
        })
    }

    pub(super) fn ensure_active(&self) -> Result<(), DownloadError> {
        self.workers
            .ensure_active()
            .map_err(managed_blocking_download_error)
    }

    async fn reserve(&self, hard_limit: u64) -> Result<OwnedSemaphorePermit, DownloadError> {
        self.ensure_active()?;
        if hard_limit == 0 || hard_limit > LIBRARY_SOURCE_MAX_BYTES {
            return Err(source_integrity_error("exceeds the bounded scratch limit"));
        }
        let units = hard_limit.div_ceil(LIBRARY_SOURCE_BUDGET_UNIT_BYTES) as u32;
        Arc::clone(&self.acquisition_permits)
            .acquire_many_owned(units)
            .await
            .map_err(|_| source_integrity_error("scratch budget is closed"))
    }

    async fn retain_validated_jar(
        &self,
        file: File,
        observed_size: u64,
        observed_sha1: [u8; 20],
        acquisition_permit: OwnedSemaphorePermit,
    ) -> Result<RetainedComponentSourceAllocation, DownloadError> {
        let spool = Arc::clone(&self.spool);
        let result = self
            .workers
            .run(move |cancellation| {
                let allocation =
                    spool.append_authenticated(file, observed_size, observed_sha1, &cancellation);
                drop(acquisition_permit);
                allocation
            })
            .await
            .map_err(managed_blocking_download_error)?;
        match result {
            Ok(allocation) => Ok(allocation),
            Err(RetainedComponentSourceAppendError::Cancelled) => Err(
                managed_blocking_download_error(ManagedBlockingTaskError::Cancelled),
            ),
            Err(RetainedComponentSourceAppendError::SourceRejected) => Err(source_integrity_error(
                "validated JAR changed during retained source admission",
            )),
            Err(RetainedComponentSourceAppendError::Spool(error)) => {
                Err(retained_spool_download_error(error))
            }
        }
    }

    pub(super) async fn try_retain_authenticated_jar_reader<R>(
        &self,
        mut reader: R,
        observed_size: u64,
        observed_sha1: [u8; 20],
    ) -> Result<Option<RetainedComponentSourceAllocation>, DownloadError>
    where
        R: Read + Seek + Send + 'static,
    {
        let permit = self.reserve(observed_size).await?;
        let spool = Arc::clone(&self.spool);
        self.workers
            .run(move |cancellation| {
                let validation = validate_and_rewind_bounded_jar(&mut reader, &cancellation);
                let allocation = if validation.is_err() && cancellation.is_cancelled() {
                    Err(managed_blocking_download_error(
                        ManagedBlockingTaskError::Cancelled,
                    ))
                } else if validation.is_err() {
                    Ok(None)
                } else {
                    match spool.append_authenticated(
                        reader,
                        observed_size,
                        observed_sha1,
                        &cancellation,
                    ) {
                        Ok(allocation) => Ok(Some(allocation)),
                        Err(RetainedComponentSourceAppendError::Cancelled) => Err(
                            managed_blocking_download_error(ManagedBlockingTaskError::Cancelled),
                        ),
                        Err(RetainedComponentSourceAppendError::SourceRejected) => Ok(None),
                        Err(RetainedComponentSourceAppendError::Spool(error)) => {
                            Err(retained_spool_download_error(error))
                        }
                    }
                };
                drop(permit);
                allocation
            })
            .await
            .map_err(managed_blocking_download_error)?
    }

    pub(super) async fn retain_authenticated_local_source(
        &self,
        source: AuthenticatedLocalLibraryBytes,
    ) -> Result<RetainedLibraryComponentSource, DownloadError> {
        let (relative_path, kind, bytes, observed_size, observed_sha1) = source.into_parts();
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
                    source_integrity_error("changed during retained local source admission")
                }
                RetainedComponentSourceAppendError::Spool(error) => {
                    retained_spool_download_error(error)
                }
            })?;
        Ok(
            RetainedLibraryComponentSource::from_authenticated_local_allocation(
                allocation,
                relative_path,
                observed_size,
                observed_sha1,
                kind,
            ),
        )
    }

    #[cfg(test)]
    pub(super) fn available_bytes(&self) -> u64 {
        self.acquisition_permits.available_permits() as u64 * LIBRARY_SOURCE_BUDGET_UNIT_BYTES
    }

    #[cfg(test)]
    pub(super) fn retained_available_bytes(&self) -> u64 {
        self.spool.available_bytes()
    }
}

struct AcquiredLibrarySource {
    allocation: RetainedComponentSourceAllocation,
    relative_path: PortableRelativePath,
    observed_size: u64,
    observed_sha1: [u8; 20],
    expected: ExpectedIntegrity,
    provider_url: String,
}

impl AcquiredLibrarySource {
    #[cfg(test)]
    pub(super) fn observed_size(&self) -> u64 {
        self.observed_size
    }

    #[cfg(test)]
    pub(super) fn observed_sha1(&self) -> [u8; 20] {
        self.observed_sha1
    }

    #[cfg(test)]
    pub(super) fn expected(&self) -> &ExpectedIntegrity {
        &self.expected
    }

    #[cfg(test)]
    pub(super) fn relative_path(&self) -> &PortableRelativePath {
        &self.relative_path
    }

    #[cfg(test)]
    pub(super) fn observed_expected(&self) -> ExpectedIntegrity {
        ExpectedIntegrity {
            size: Some(self.observed_size),
            sha1: Some(format!("{}", HexSha1(&self.observed_sha1))),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LibraryComponentSourceKind {
    Library,
    NativeLibrary,
}

pub(crate) struct RetainedLibraryComponentSource {
    storage: RetainedLibraryComponentStorage,
    relative_path: PortableRelativePath,
    observed_size: u64,
    observed_sha1: [u8; 20],
    origin: RetainedLibraryComponentOrigin,
    kind: LibraryComponentSourceKind,
}

pub(crate) struct AuthenticatedLocalLibraryBytes {
    relative_path: PortableRelativePath,
    kind: LibraryComponentSourceKind,
    bytes: Vec<u8>,
    observed_size: u64,
    observed_sha1: [u8; 20],
}

pub(crate) struct AuthenticatedLibraryCacheProof {
    relative_path: PortableRelativePath,
    kind: LibraryComponentSourceKind,
    observed_size: u64,
    observed_sha1: [u8; 20],
}

#[derive(Default)]
pub(crate) struct AuthenticatedLibraryCacheProofSet {
    proofs: BTreeMap<PortableRelativePath, AuthenticatedLibraryCacheProof>,
    portable_paths: BTreeMap<PortablePathKey, PortableRelativePath>,
}

impl AuthenticatedLibraryCacheProofSet {
    pub(crate) fn insert(
        &mut self,
        proof: AuthenticatedLibraryCacheProof,
    ) -> Result<(), DownloadError> {
        let path = proof.relative_path.clone();
        let portable = path.key();
        if self
            .portable_paths
            .get(&portable)
            .is_some_and(|existing| existing != &path)
        {
            return Err(source_integrity_error(
                "duplicates an authenticated cache-proof identity",
            ));
        }
        self.portable_paths.insert(portable, path.clone());
        self.proofs.insert(path, proof);
        Ok(())
    }
}

impl AuthenticatedLibraryCacheProof {
    pub(crate) fn new(
        relative_path: PortableRelativePath,
        kind: LibraryComponentSourceKind,
        observed_size: u64,
        observed_sha1: [u8; 20],
    ) -> Self {
        Self {
            relative_path,
            kind,
            observed_size,
            observed_sha1,
        }
    }
}

impl AuthenticatedLocalLibraryBytes {
    pub(crate) fn new(
        relative_path: PortableRelativePath,
        kind: LibraryComponentSourceKind,
        bytes: Vec<u8>,
        expected_size: u64,
        expected_sha1: [u8; 20],
    ) -> Result<Self, LoaderError> {
        let observed_size = bytes.len() as u64;
        let observed_sha1: [u8; 20] = Sha1::digest(&bytes).into();
        if expected_size == 0 || observed_size != expected_size || observed_sha1 != expected_sha1 {
            return Err(LoaderError::Verify(
                "authenticated local library bytes changed".to_string(),
            ));
        }
        Ok(Self {
            relative_path,
            kind,
            bytes,
            observed_size,
            observed_sha1,
        })
    }

    fn into_parts(
        self,
    ) -> (
        PortableRelativePath,
        LibraryComponentSourceKind,
        Vec<u8>,
        u64,
        [u8; 20],
    ) {
        (
            self.relative_path,
            self.kind,
            self.bytes,
            self.observed_size,
            self.observed_sha1,
        )
    }

    #[cfg(test)]
    pub(crate) fn relative_path(&self) -> &PortableRelativePath {
        &self.relative_path
    }
}

#[derive(Default)]
pub(crate) struct RetainedLibrarySourceSet {
    sources: BTreeMap<PortableRelativePath, RetainedLibraryComponentSource>,
    portable_paths: BTreeMap<PortablePathKey, PortableRelativePath>,
    retained_bytes: u64,
}

impl RetainedLibrarySourceSet {
    pub(crate) fn new() -> Self {
        Self {
            sources: BTreeMap::new(),
            portable_paths: BTreeMap::new(),
            retained_bytes: 0,
        }
    }

    pub(crate) fn insert(
        &mut self,
        source: RetainedLibraryComponentSource,
    ) -> Result<(), DownloadError> {
        self.admit(source, false)
    }

    fn admit(
        &mut self,
        source: RetainedLibraryComponentSource,
        replace_exact_path: bool,
    ) -> Result<(), DownloadError> {
        let path = source.relative_path().clone();
        let portable = path.key();
        if (!replace_exact_path && self.sources.contains_key(&path))
            || self
                .portable_paths
                .get(&portable)
                .is_some_and(|existing| existing != &path)
        {
            return Err(source_integrity_error(
                "duplicates a retained source identity",
            ));
        }
        let replaced_bytes = if replace_exact_path {
            self.sources
                .get(&path)
                .map_or(0, RetainedLibraryComponentSource::observed_size)
        } else {
            0
        };
        let retained_bytes = self
            .retained_bytes
            .checked_sub(replaced_bytes)
            .and_then(|bytes| bytes.checked_add(source.observed_size()))
            .filter(|bytes| *bytes <= MAX_TIER2_AGGREGATE_BYTES)
            .ok_or_else(|| source_integrity_error("exceeds the retained aggregate limit"))?;
        self.portable_paths.insert(portable, path.clone());
        self.sources.insert(path, source);
        self.retained_bytes = retained_bytes;
        Ok(())
    }

    pub(crate) fn merge(&mut self, other: Self) -> Result<(), DownloadError> {
        for source in other.sources.into_values() {
            self.admit(source, true)?;
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn reconcile_projection(
        &mut self,
        projection: &ManagedComponentProjection<'_>,
    ) -> Result<(), DownloadError> {
        self.reconcile_sparse_projection(projection, AuthenticatedLibraryCacheProofSet::default())
    }

    pub(crate) fn reconcile_sparse_projection(
        &mut self,
        projection: &ManagedComponentProjection<'_>,
        mut cache_proofs: AuthenticatedLibraryCacheProofSet,
    ) -> Result<(), DownloadError> {
        if projection.component() != ManagedKnownGoodComponent::Libraries {
            return Err(source_integrity_error(
                "is bound to a non-library projection",
            ));
        }
        let mut reconciled = BTreeMap::new();
        let mut portable_paths = BTreeMap::new();
        let mut retained_bytes = 0_u64;
        for projected in projection.entries().iter().copied() {
            let entry = projected.entry();
            if entry.root() != &KnownGoodRoot::Libraries {
                return Err(source_integrity_error("has a non-library projection root"));
            }
            let path = PortableRelativePath::new(entry.path().as_str())
                .map_err(|_| source_integrity_error("has an invalid projection path"))?;
            let portable = path.key();
            if portable_paths.insert(portable, path.clone()).is_some() {
                return Err(source_integrity_error(
                    "duplicates a portable projection identity",
                ));
            }
            let expected_kind = match entry.kind() {
                KnownGoodArtifactKind::Library => LibraryComponentSourceKind::Library,
                KnownGoodArtifactKind::NativeLibrary => LibraryComponentSourceKind::NativeLibrary,
                _ => return Err(source_integrity_error("has a non-library projection kind")),
            };
            let (expected_sha1, expected_size) = match entry.integrity() {
                KnownGoodIntegrity::Sha1 { digest, size }
                | KnownGoodIntegrity::ExactBytes { digest, size } => (digest.to_bytes(), *size),
                KnownGoodIntegrity::Directory | KnownGoodIntegrity::LinkTarget(_) => {
                    return Err(source_integrity_error(
                        "has a non-file projection integrity",
                    ));
                }
            };
            let source = self.sources.remove(&path);
            let proof = cache_proofs.proofs.remove(&path);
            match (source, proof) {
                (Some(source), _)
                    if source.source_kind() == expected_kind
                        && source.observed_size() == expected_size
                        && source.observed_sha1() == expected_sha1 =>
                {
                    retained_bytes = retained_bytes
                        .checked_add(source.observed_size())
                        .filter(|bytes| *bytes <= MAX_TIER2_AGGREGATE_BYTES)
                        .ok_or_else(|| {
                            source_integrity_error("exceeds the retained aggregate limit")
                        })?;
                    reconciled.insert(path, source);
                }
                (_, Some(proof))
                    if proof.kind == expected_kind
                        && proof.observed_size == expected_size
                        && proof.observed_sha1 == expected_sha1 => {}
                (Some(_), _) => {
                    return Err(source_integrity_error(
                        "does not match its final projection row",
                    ));
                }
                (None, Some(_)) => {
                    return Err(source_integrity_error(
                        "has a cache proof that does not match its final projection row",
                    ));
                }
                (None, None) => {
                    return Err(source_integrity_error("is missing a projected source"));
                }
            }
        }
        self.sources = reconciled;
        self.portable_paths = portable_paths;
        self.retained_bytes = retained_bytes;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.sources.len()
    }

    #[cfg(test)]
    pub(crate) fn retained_bytes(&self) -> u64 {
        self.retained_bytes
    }

    pub(crate) fn into_sources(self) -> Vec<RetainedLibraryComponentSource> {
        self.sources.into_values().collect()
    }
}

enum RetainedLibraryComponentStorage {
    Aggregate(RetainedComponentSourceAllocation),
    Owned(Vec<u8>),
}

enum RetainedLibraryComponentOrigin {
    Network {
        expected: ExpectedIntegrity,
        provider_url: String,
    },
    Local,
}

pub(crate) enum RetainedLibrarySourceReader {
    Aggregate(RetainedComponentSourceReader),
    Owned(Cursor<Vec<u8>>),
}

pub(crate) struct RetainedLibrarySourceReplay {
    reader: RetainedLibrarySourceReader,
    observed_size: u64,
    observed_sha1: [u8; 20],
}

impl RetainedLibrarySourceReplay {
    pub(crate) fn into_parts(self) -> (RetainedLibrarySourceReader, u64, [u8; 20]) {
        (self.reader, self.observed_size, self.observed_sha1)
    }
}

impl Read for RetainedLibrarySourceReader {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Aggregate(reader) => reader.read(output),
            Self::Owned(reader) => reader.read(output),
        }
    }
}

impl Seek for RetainedLibrarySourceReader {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        match self {
            Self::Aggregate(reader) => reader.seek(position),
            Self::Owned(reader) => reader.seek(position),
        }
    }
}

impl RetainedLibraryComponentStorage {
    fn replay_reader(&self) -> Result<RetainedLibrarySourceReader, LoaderError> {
        match self {
            Self::Aggregate(allocation) => allocation
                .replay_reader()
                .map_err(retained_spool_loader_error)
                .map(RetainedLibrarySourceReader::Aggregate),
            Self::Owned(_) => Err(LoaderError::Verify(
                "owned local library bytes do not expose replay authority".to_string(),
            )),
        }
    }

    fn into_reader(self) -> Result<RetainedLibrarySourceReader, LoaderError> {
        match self {
            Self::Aggregate(allocation) => allocation
                .into_reader()
                .map_err(retained_spool_loader_error)
                .map(RetainedLibrarySourceReader::Aggregate),
            Self::Owned(bytes) => Ok(RetainedLibrarySourceReader::Owned(Cursor::new(bytes))),
        }
    }
}

impl RetainedLibraryComponentSource {
    pub(super) fn from_authenticated_allocation(
        allocation: RetainedComponentSourceAllocation,
        relative_path: PortableRelativePath,
        observed_size: u64,
        observed_sha1: [u8; 20],
        expected: ExpectedIntegrity,
        provider_url: String,
        kind: LibraryComponentSourceKind,
    ) -> Self {
        Self {
            storage: RetainedLibraryComponentStorage::Aggregate(allocation),
            relative_path,
            observed_size,
            observed_sha1,
            origin: RetainedLibraryComponentOrigin::Network {
                expected,
                provider_url,
            },
            kind,
        }
    }

    pub(crate) fn from_authenticated_local_bytes(
        relative_path: PortableRelativePath,
        kind: LibraryComponentSourceKind,
        bytes: Vec<u8>,
        expected_size: u64,
        expected_sha1: [u8; 20],
    ) -> Result<Self, LoaderError> {
        let source = AuthenticatedLocalLibraryBytes::new(
            relative_path,
            kind,
            bytes,
            expected_size,
            expected_sha1,
        )?;
        let (relative_path, kind, bytes, observed_size, observed_sha1) = source.into_parts();
        Ok(Self {
            storage: RetainedLibraryComponentStorage::Owned(bytes),
            relative_path,
            observed_size,
            observed_sha1,
            origin: RetainedLibraryComponentOrigin::Local,
            kind,
        })
    }

    fn from_authenticated_local_allocation(
        allocation: RetainedComponentSourceAllocation,
        relative_path: PortableRelativePath,
        observed_size: u64,
        observed_sha1: [u8; 20],
        kind: LibraryComponentSourceKind,
    ) -> Self {
        Self {
            storage: RetainedLibraryComponentStorage::Aggregate(allocation),
            relative_path,
            observed_size,
            observed_sha1,
            origin: RetainedLibraryComponentOrigin::Local,
            kind,
        }
    }

    #[cfg(test)]
    pub(crate) fn from_test_identity(
        relative_path: PortableRelativePath,
        is_native: bool,
        provider_url: String,
        expected: ExpectedIntegrity,
        observed_size: u64,
        observed_sha1: [u8; 20],
    ) -> Self {
        Self {
            storage: RetainedLibraryComponentStorage::Owned(Vec::new()),
            relative_path,
            observed_size,
            observed_sha1,
            origin: RetainedLibraryComponentOrigin::Network {
                expected,
                provider_url,
            },
            kind: if is_native {
                LibraryComponentSourceKind::NativeLibrary
            } else {
                LibraryComponentSourceKind::Library
            },
        }
    }

    pub(crate) fn observed_size(&self) -> u64 {
        self.observed_size
    }

    pub(crate) fn exact_download_proof(&self) -> Option<ExactLibraryDownloadProof> {
        let RetainedLibraryComponentOrigin::Network {
            expected,
            provider_url,
        } = &self.origin
        else {
            return None;
        };
        Some(ExactLibraryDownloadProof::new(
            self.relative_path.clone(),
            self.kind == LibraryComponentSourceKind::NativeLibrary,
            provider_url.clone(),
            expected.clone(),
            self.observed_size,
            self.observed_sha1,
        ))
    }

    pub(crate) fn relative_path(&self) -> &PortableRelativePath {
        &self.relative_path
    }

    pub(crate) fn observed_sha1(&self) -> [u8; 20] {
        self.observed_sha1
    }

    pub(crate) fn network_origin(&self) -> Option<(&str, &ExpectedIntegrity)> {
        match &self.origin {
            RetainedLibraryComponentOrigin::Network {
                expected,
                provider_url,
            } => Some((provider_url, expected)),
            RetainedLibraryComponentOrigin::Local => None,
        }
    }

    pub(crate) fn source_kind(&self) -> LibraryComponentSourceKind {
        self.kind
    }

    pub(crate) fn replay(&self) -> Result<RetainedLibrarySourceReplay, LoaderError> {
        Ok(RetainedLibrarySourceReplay {
            reader: self.storage.replay_reader()?,
            observed_size: self.observed_size,
            observed_sha1: self.observed_sha1,
        })
    }

    #[cfg(test)]
    async fn stage_create_new_with_hook(
        self,
        staging_bucket: &ManagedDir,
        slot: &str,
        lifetime_guard: ManagedPublicationLifetimeGuard,
        blocking_hook: BlockingValidationHook,
    ) -> Result<StagedComponentPublicationSource, LoaderError> {
        let Self {
            storage,
            relative_path,
            observed_size,
            observed_sha1,
            origin: _,
            kind,
        } = self;
        let reader = storage.into_reader()?;
        let file = staging_bucket
            .import_authenticated_create_new_with_hook(
                slot,
                reader,
                observed_size,
                observed_sha1,
                lifetime_guard,
                blocking_hook,
            )
            .await?;
        Ok(StagedComponentPublicationSource::new(
            ComponentPublicationSourceIdentity::new(
                relative_path,
                component_source_kind(kind),
                observed_size,
                observed_sha1,
            ),
            file,
        ))
    }
}

impl RetainedComponentPublicationSource for RetainedLibraryComponentSource {
    fn relative_path(&self) -> &PortableRelativePath {
        &self.relative_path
    }

    fn kind(&self) -> ManagedComponentArtifactKind {
        component_source_kind(self.kind)
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
        let Self {
            storage,
            relative_path,
            observed_size,
            observed_sha1,
            origin: _,
            kind,
        } = self;
        let reader = storage.into_reader()?;
        let file = staging_bucket
            .import_authenticated_create_new(
                slot,
                reader,
                observed_size,
                observed_sha1,
                lifetime_guard,
            )
            .await?;
        Ok(StagedComponentPublicationSource::new(
            ComponentPublicationSourceIdentity::new(
                relative_path,
                component_source_kind(kind),
                observed_size,
                observed_sha1,
            ),
            file,
        ))
    }
}

fn component_source_kind(kind: LibraryComponentSourceKind) -> ManagedComponentArtifactKind {
    match kind {
        LibraryComponentSourceKind::Library => ManagedComponentArtifactKind::Library,
        LibraryComponentSourceKind::NativeLibrary => ManagedComponentArtifactKind::NativeLibrary,
    }
}

pub(super) struct LibrarySourceRequest<'a> {
    pub(super) client: &'a reqwest::Client,
    pub(super) url: &'a str,
    pub(super) expected: &'a ExpectedIntegrity,
    pub(super) relative_path: &'a PortableRelativePath,
    pub(super) max_bytes: u64,
    pub(super) target: &'a str,
    pub(super) pool: &'a LibrarySourcePool,
    pub(super) fact_tx: Option<&'a mpsc::UnboundedSender<ExecutionDownloadFact>>,
}

async fn acquire_library_source(
    request: LibrarySourceRequest<'_>,
) -> Result<AcquiredLibrarySource, DownloadError> {
    acquire_library_source_with_retry_delays(request, &LIBRARY_SOURCE_RETRY_DELAYS).await
}

pub(super) async fn acquire_retained_library_component_source(
    request: LibrarySourceRequest<'_>,
    kind: LibraryComponentSourceKind,
) -> Result<RetainedLibraryComponentSource, DownloadError> {
    let source = acquire_library_source(request).await?;
    Ok(RetainedLibraryComponentSource {
        storage: RetainedLibraryComponentStorage::Aggregate(source.allocation),
        relative_path: source.relative_path,
        observed_size: source.observed_size,
        observed_sha1: source.observed_sha1,
        origin: RetainedLibraryComponentOrigin::Network {
            expected: source.expected,
            provider_url: source.provider_url,
        },
        kind,
    })
}

async fn acquire_library_source_with_retry_delays(
    request: LibrarySourceRequest<'_>,
    retry_delays: &[Duration],
) -> Result<AcquiredLibrarySource, DownloadError> {
    let mut next_delay = 0;
    loop {
        request.pool.ensure_active()?;
        match acquire_library_source_attempt(&request).await {
            Ok(source) => return Ok(source),
            Err(error) if error.retryable && next_delay < retry_delays.len() => {
                tokio::time::sleep(retry_delays[next_delay]).await;
                next_delay += 1;
            }
            Err(error) => return Err(error.error),
        }
    }
}

struct LibrarySourceAttemptError {
    error: DownloadError,
    retryable: bool,
}

fn emit_source_facts<const N: usize>(
    request: &LibrarySourceRequest<'_>,
    facts: [ExecutionDownloadFact; N],
) {
    emit_execution_download_facts(request.fact_tx, &facts);
}

fn emit_source_fact<K, V, I>(
    request: &LibrarySourceRequest<'_>,
    kind: ExecutionDownloadFactKind,
    fields: I,
) where
    K: AsRef<str>,
    V: AsRef<str>,
    I: IntoIterator<Item = (K, V)>,
{
    emit_source_facts(
        request,
        [execution_download_fact(kind, request.target, fields)],
    );
}

fn emit_source_size_mismatch(request: &LibrarySourceRequest<'_>, expected: u64, actual: u64) {
    emit_source_facts(
        request,
        [size_mismatch_fact(request.target, expected, actual)],
    );
}

async fn acquire_library_source_attempt(
    request: &LibrarySourceRequest<'_>,
) -> Result<AcquiredLibrarySource, LibrarySourceAttemptError> {
    acquire_library_source_attempt_inner(request, None).await
}

type BlockingValidationHook = Box<dyn FnOnce() + Send + 'static>;

async fn acquire_library_source_attempt_inner(
    request: &LibrarySourceRequest<'_>,
    validation_hook: Option<BlockingValidationHook>,
) -> Result<AcquiredLibrarySource, LibrarySourceAttemptError> {
    request.pool.ensure_active().map_err(nonretryable)?;
    if request
        .expected
        .sha1
        .as_deref()
        .is_some_and(|sha1| sha1.len() != 40 || !sha1.bytes().all(|byte| byte.is_ascii_hexdigit()))
    {
        emit_source_fact(
            request,
            ExecutionDownloadFactKind::MetadataInvalid,
            vec![("field", "sha1")],
        );
        return Err(nonretryable(source_integrity_error(
            "checksum metadata is invalid",
        )));
    }
    let hard_limit = request
        .expected
        .size
        .unwrap_or(LIBRARY_SOURCE_MAX_BYTES)
        .min(request.max_bytes)
        .min(LIBRARY_SOURCE_MAX_BYTES);
    if request.expected.size == Some(0)
        || request
            .expected
            .size
            .is_some_and(|size| size > request.max_bytes || size > LIBRARY_SOURCE_MAX_BYTES)
    {
        let actual = request.expected.size.unwrap_or(request.max_bytes);
        emit_source_size_mismatch(
            request,
            request.max_bytes.min(LIBRARY_SOURCE_MAX_BYTES),
            actual,
        );
        return Err(nonretryable(source_integrity_error(
            "exceeds the bounded scratch limit",
        )));
    }
    let permit = request
        .pool
        .reserve(hard_limit)
        .await
        .map_err(nonretryable)?;
    let response = request.client.get(request.url).send().await.map_err(|_| {
        emit_source_fact(
            request,
            ExecutionDownloadFactKind::NetworkFailure,
            no_download_fact_fields(),
        );
        retryable("request failed")
    })?;
    if !response.status().is_success() {
        let status = response.status();
        emit_source_fact(
            request,
            ExecutionDownloadFactKind::ProviderFailure,
            vec![("status", status.as_u16().to_string())],
        );
        return Err(LibrarySourceAttemptError {
            error: source_integrity_error("provider rejected the request"),
            retryable: status.is_server_error() || status.as_u16() == 429,
        });
    }
    let declared_length = response.content_length();
    if declared_length.is_some_and(|length| length > hard_limit) {
        emit_source_size_mismatch(request, hard_limit, declared_length.unwrap_or(hard_limit));
        return Err(nonretryable(source_integrity_error(
            "declared response exceeds the bounded scratch limit",
        )));
    }

    let file = tempfile::tempfile().map_err(|error| {
        emit_source_fact(
            request,
            ExecutionDownloadFactKind::TempWriteFailed,
            no_download_fact_fields(),
        );
        nonretryable(error.into())
    })?;
    let mut output = tokio::fs::File::from_std(file);
    let mut stream = response.bytes_stream();
    let mut hasher = Sha1::new();
    let mut observed_size = 0_u64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| {
            emit_source_facts(
                request,
                [
                    execution_download_fact(
                        ExecutionDownloadFactKind::Interrupted,
                        request.target,
                        no_download_fact_fields(),
                    ),
                    execution_download_fact(
                        ExecutionDownloadFactKind::NetworkFailure,
                        request.target,
                        no_download_fact_fields(),
                    ),
                ],
            );
            retryable("response stream was interrupted")
        })?;
        observed_size = observed_size
            .checked_add(chunk.len() as u64)
            .ok_or_else(|| nonretryable(source_integrity_error("source size overflow")))?;
        if observed_size > hard_limit {
            emit_source_size_mismatch(request, hard_limit, observed_size);
            return Err(nonretryable(source_integrity_error(
                "response exceeds the bounded scratch limit",
            )));
        }
        hasher.update(&chunk);
        output.write_all(&chunk).await.map_err(|error| {
            emit_source_fact(
                request,
                ExecutionDownloadFactKind::TempWriteFailed,
                no_download_fact_fields(),
            );
            nonretryable(error.into())
        })?;
    }
    if declared_length.is_some_and(|length| length != observed_size) {
        let declared = declared_length.unwrap_or(observed_size);
        emit_source_facts(
            request,
            [
                execution_download_fact(
                    ExecutionDownloadFactKind::Interrupted,
                    request.target,
                    no_download_fact_fields(),
                ),
                size_mismatch_fact(request.target, declared, observed_size),
            ],
        );
        return Err(retryable("response stream was interrupted"));
    }
    output.flush().await.map_err(|error| {
        emit_source_fact(
            request,
            ExecutionDownloadFactKind::TempWriteFailed,
            no_download_fact_fields(),
        );
        nonretryable(error.into())
    })?;
    output.sync_data().await.map_err(|error| {
        emit_source_fact(
            request,
            ExecutionDownloadFactKind::TempWriteFailed,
            no_download_fact_fields(),
        );
        nonretryable(error.into())
    })?;
    let observed_sha1: [u8; 20] = hasher.finalize().into();
    if let Some(expected_size) = request.expected.size
        && expected_size != observed_size
    {
        emit_source_size_mismatch(request, expected_size, observed_size);
        return Err(nonretryable(source_integrity_error(
            "source size does not match metadata",
        )));
    }
    if let Some(expected_sha1) = request.expected.sha1.as_deref()
        && !expected_sha1.eq_ignore_ascii_case(&format!("{}", HexSha1(&observed_sha1)))
    {
        emit_source_fact(
            request,
            ExecutionDownloadFactKind::ChecksumMismatch,
            vec![("algorithm", "sha1")],
        );
        return Err(nonretryable(source_integrity_error(
            "source checksum does not match metadata",
        )));
    }

    let mut file = output.into_std().await;
    let verified = request
        .pool
        .workers
        .run(move |cancellation| {
            if let Some(hook) = validation_hook {
                hook();
            }
            cancellation.checkpoint(ManagedBlockingCheckpoint::LibraryValidation);
            cancellation.check_io()?;
            let verified_sha1 = hash_open_file(&mut file, observed_size, &cancellation)?;
            validate_and_rewind_bounded_jar(&mut file, &cancellation)?;
            Ok::<_, std::io::Error>((file, verified_sha1, permit))
        })
        .await
        .map_err(|error| {
            emit_source_fact(
                request,
                ExecutionDownloadFactKind::TempWriteFailed,
                no_download_fact_fields(),
            );
            nonretryable(managed_blocking_download_error(error))
        })?
        .map_err(|error| {
            if error.kind() == io::ErrorKind::Interrupted {
                return nonretryable(error.into());
            }
            emit_source_fact(
                request,
                ExecutionDownloadFactKind::ChecksumMismatch,
                vec![("algorithm", "sha1")],
            );
            nonretryable(error.into())
        })?;
    let (file, verified_sha1, permit) = verified;
    if verified_sha1 != observed_sha1 {
        emit_source_fact(
            request,
            ExecutionDownloadFactKind::ChecksumMismatch,
            vec![("algorithm", "sha1")],
        );
        return Err(nonretryable(source_integrity_error(
            "retained source identity changed after download",
        )));
    }

    let allocation = request
        .pool
        .retain_validated_jar(file, observed_size, observed_sha1, permit)
        .await
        .map_err(nonretryable)?;

    Ok(AcquiredLibrarySource {
        allocation,
        relative_path: request.relative_path.clone(),
        observed_size,
        observed_sha1,
        expected: request.expected.clone(),
        provider_url: request.url.to_string(),
    })
}

fn hash_open_file(
    file: &mut File,
    expected_size: u64,
    cancellation: &ManagedCancellation,
) -> std::io::Result<[u8; 20]> {
    cancellation.check_io()?;
    if file.metadata()?.len() != expected_size {
        return Err(std::io::Error::other(
            "retained library source size changed",
        ));
    }
    file.seek(SeekFrom::Start(0))?;
    let mut hasher = Sha1::new();
    let mut observed = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        cancellation.check_io()?;
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        observed = observed.saturating_add(read as u64);
        hasher.update(&buffer[..read]);
    }
    if observed != expected_size {
        return Err(std::io::Error::other(
            "retained library source read was incomplete",
        ));
    }
    file.seek(SeekFrom::Start(0))?;
    Ok(hasher.finalize().into())
}

fn validate_bounded_jar<R: Read + Seek>(
    file: &mut R,
    cancellation: &ManagedCancellation,
) -> std::io::Result<()> {
    cancellation.check_io()?;
    preflight_zip_central_directory(file, cancellation)?;
    file.seek(SeekFrom::Start(0))?;
    let mut archive = zip::ZipArchive::new(file)?;
    if archive.is_empty() || archive.len() > usize::from(MAX_JAR_ENTRIES) {
        return Err(std::io::Error::other(
            "library JAR entry count exceeds the bounded limit",
        ));
    }
    let mut total_name_bytes = 0_usize;
    let mut total_uncompressed_bytes = 0_u64;
    let mut has_file = false;
    for index in 0..archive.len() {
        cancellation.check_io()?;
        let entry = archive.by_index(index)?;
        let name_bytes = entry.name_raw().len();
        if name_bytes > MAX_JAR_ENTRY_NAME_BYTES {
            return Err(std::io::Error::other("library JAR entry name is too large"));
        }
        total_name_bytes = total_name_bytes
            .checked_add(name_bytes)
            .ok_or_else(|| std::io::Error::other("library JAR name budget overflow"))?;
        if total_name_bytes > MAX_JAR_TOTAL_NAME_BYTES {
            return Err(std::io::Error::other(
                "library JAR entry names exceed the bounded limit",
            ));
        }
        if entry.size() > MAX_JAR_ENTRY_UNCOMPRESSED_BYTES {
            return Err(std::io::Error::other(
                "library JAR entry expands beyond the bounded limit",
            ));
        }
        total_uncompressed_bytes = total_uncompressed_bytes
            .checked_add(entry.size())
            .ok_or_else(|| std::io::Error::other("library JAR size budget overflow"))?;
        if total_uncompressed_bytes > MAX_JAR_TOTAL_UNCOMPRESSED_BYTES {
            return Err(std::io::Error::other(
                "library JAR expands beyond the bounded limit",
            ));
        }
        has_file |= !entry.is_dir();
    }
    if !has_file {
        return Err(std::io::Error::other(
            "library source is not a readable non-empty JAR",
        ));
    }
    Ok(())
}

fn validate_and_rewind_bounded_jar<R: Read + Seek>(
    file: &mut R,
    cancellation: &ManagedCancellation,
) -> std::io::Result<()> {
    validate_bounded_jar(file, cancellation)?;
    cancellation.check_io()?;
    file.seek(SeekFrom::Start(0))?;
    Ok(())
}

fn preflight_zip_central_directory<R: Read + Seek>(
    file: &mut R,
    cancellation: &ManagedCancellation,
) -> std::io::Result<()> {
    cancellation.check_io()?;
    let length = file.seek(SeekFrom::End(0))?;
    let tail_len =
        length.min((ZIP_END_OF_CENTRAL_DIRECTORY_BYTES + ZIP_MAX_COMMENT_BYTES) as u64) as usize;
    file.seek(SeekFrom::End(-(tail_len as i64)))?;
    let mut tail = vec![0_u8; tail_len];
    file.read_exact(&mut tail)?;
    let eocd = tail
        .windows(4)
        .rposition(|bytes| bytes == [0x50, 0x4b, 0x05, 0x06])
        .ok_or_else(|| std::io::Error::other("library source has no ZIP directory"))?;
    if tail.len().saturating_sub(eocd) < ZIP_END_OF_CENTRAL_DIRECTORY_BYTES {
        return Err(std::io::Error::other("library ZIP directory is truncated"));
    }
    let disk = u16::from_le_bytes([tail[eocd + 4], tail[eocd + 5]]);
    let directory_disk = u16::from_le_bytes([tail[eocd + 6], tail[eocd + 7]]);
    let disk_entries = u16::from_le_bytes([tail[eocd + 8], tail[eocd + 9]]);
    let entries = u16::from_le_bytes([tail[eocd + 10], tail[eocd + 11]]);
    let directory_bytes = u32::from_le_bytes(tail[eocd + 12..eocd + 16].try_into().unwrap());
    let directory_offset = u32::from_le_bytes(tail[eocd + 16..eocd + 20].try_into().unwrap());
    let comment_bytes = u16::from_le_bytes([tail[eocd + 20], tail[eocd + 21]]) as usize;
    if eocd
        .checked_add(ZIP_END_OF_CENTRAL_DIRECTORY_BYTES)
        .and_then(|end| end.checked_add(comment_bytes))
        != Some(tail.len())
    {
        return Err(std::io::Error::other(
            "library ZIP directory does not end at physical EOF",
        ));
    }
    if disk != 0
        || directory_disk != 0
        || disk_entries != entries
        || entries == u16::MAX
        || directory_bytes == u32::MAX
        || directory_offset == u32::MAX
        || entries == 0
        || entries > MAX_JAR_ENTRIES
        || directory_bytes > MAX_JAR_CENTRAL_DIRECTORY_BYTES
    {
        return Err(std::io::Error::other(
            "library ZIP directory exceeds the bounded limit",
        ));
    }
    let eocd_offset = length
        .checked_sub(tail_len as u64)
        .and_then(|offset| offset.checked_add(eocd as u64))
        .ok_or_else(|| std::io::Error::other("library ZIP directory offset overflow"))?;
    if u64::from(directory_offset).checked_add(u64::from(directory_bytes)) != Some(eocd_offset) {
        return Err(std::io::Error::other(
            "library ZIP central directory is not contiguous with its terminator",
        ));
    }

    file.seek(SeekFrom::Start(u64::from(directory_offset)))?;
    let mut consumed = 0_u64;
    let mut total_name_bytes = 0_usize;
    let mut total_uncompressed_bytes = 0_u64;
    let mut header = [0_u8; 46];
    for _ in 0..entries {
        cancellation.check_io()?;
        file.read_exact(&mut header)?;
        consumed = consumed
            .checked_add(header.len() as u64)
            .ok_or_else(|| std::io::Error::other("library ZIP directory size overflow"))?;
        if header[0..4] != [0x50, 0x4b, 0x01, 0x02] {
            return Err(std::io::Error::other(
                "library ZIP central header signature is invalid",
            ));
        }
        let compressed = u32::from_le_bytes(header[20..24].try_into().unwrap());
        let uncompressed = u32::from_le_bytes(header[24..28].try_into().unwrap());
        let name_bytes = u16::from_le_bytes(header[28..30].try_into().unwrap()) as usize;
        let extra_bytes = u16::from_le_bytes(header[30..32].try_into().unwrap()) as u64;
        let entry_comment_bytes = u16::from_le_bytes(header[32..34].try_into().unwrap()) as u64;
        let start_disk = u16::from_le_bytes(header[34..36].try_into().unwrap());
        let local_offset = u32::from_le_bytes(header[42..46].try_into().unwrap());
        if compressed == u32::MAX
            || uncompressed == u32::MAX
            || local_offset == u32::MAX
            || start_disk != 0
            || name_bytes == 0
        {
            return Err(std::io::Error::other(
                "library ZIP central header exceeds the bounded format",
            ));
        }
        if name_bytes > MAX_JAR_ENTRY_NAME_BYTES {
            return Err(std::io::Error::other("library JAR entry name is too large"));
        }
        if u64::from(uncompressed) > MAX_JAR_ENTRY_UNCOMPRESSED_BYTES {
            return Err(std::io::Error::other(
                "library JAR entry expands beyond the bounded limit",
            ));
        }
        total_name_bytes = total_name_bytes
            .checked_add(name_bytes)
            .ok_or_else(|| std::io::Error::other("library ZIP name budget overflow"))?;
        total_uncompressed_bytes = total_uncompressed_bytes
            .checked_add(u64::from(uncompressed))
            .ok_or_else(|| std::io::Error::other("library ZIP size budget overflow"))?;
        if total_name_bytes > MAX_JAR_TOTAL_NAME_BYTES {
            return Err(std::io::Error::other(
                "library JAR entry names exceed the bounded limit",
            ));
        }
        if total_uncompressed_bytes > MAX_JAR_TOTAL_UNCOMPRESSED_BYTES {
            return Err(std::io::Error::other(
                "library JAR expands beyond the bounded limit",
            ));
        }
        let variable_bytes = (name_bytes as u64)
            .checked_add(extra_bytes)
            .and_then(|bytes| bytes.checked_add(entry_comment_bytes))
            .ok_or_else(|| std::io::Error::other("library ZIP header length overflow"))?;
        consumed = consumed
            .checked_add(variable_bytes)
            .ok_or_else(|| std::io::Error::other("library ZIP directory size overflow"))?;
        if consumed > u64::from(directory_bytes) {
            return Err(std::io::Error::other(
                "library ZIP central directory is truncated",
            ));
        }
        file.seek(SeekFrom::Current(i64::try_from(variable_bytes).map_err(
            |_| std::io::Error::other("library ZIP header skip is too large"),
        )?))?;
    }
    if consumed != u64::from(directory_bytes) {
        return Err(std::io::Error::other(
            "library ZIP central directory count does not match its size",
        ));
    }
    Ok(())
}

struct HexSha1<'a>(&'a [u8; 20]);

impl std::fmt::Display for HexSha1<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

fn retryable(message: &'static str) -> LibrarySourceAttemptError {
    LibrarySourceAttemptError {
        error: source_integrity_error(message),
        retryable: true,
    }
}

fn nonretryable(error: DownloadError) -> LibrarySourceAttemptError {
    LibrarySourceAttemptError {
        error,
        retryable: false,
    }
}

fn source_integrity_error(message: &str) -> DownloadError {
    DownloadError::Integrity(format!("library source {message}"))
}

fn managed_blocking_download_error(error: ManagedBlockingTaskError) -> DownloadError {
    let (kind, message) = match error {
        ManagedBlockingTaskError::Cancelled => (
            io::ErrorKind::Interrupted,
            "retained library source work was cancelled",
        ),
        ManagedBlockingTaskError::TaskStopped => (
            io::ErrorKind::Other,
            "retained library source task stopped unexpectedly",
        ),
    };
    DownloadError::FileOperation(io::Error::new(kind, message))
}

fn retained_spool_download_error(error: RetainedComponentSourceSpoolError) -> DownloadError {
    if error.is_capacity_exceeded() {
        source_integrity_error("exceeds the aggregate retained-source limit")
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
    use crate::managed_publication::ManagedRootPublicationLease;
    use sha1::Sha1;
    use std::collections::VecDeque;
    use std::io::Write;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Condvar, Mutex};
    use tokio::io::AsyncReadExt as _;
    use tokio::sync::oneshot;
    use zip::ZipWriter;
    use zip::write::SimpleFileOptions;

    const PUBLICATION_ACQUIRE_TIMEOUT: Duration = Duration::from_secs(5);

    fn uncancelled() -> ManagedCancellation {
        ManagedBlockingWorkers::new().cancellation()
    }

    fn source_pool_for_test() -> (
        LibrarySourcePool,
        ManagedBlockingWorkers,
        ManagedBlockingAttemptGuard,
    ) {
        source_pool_with_retained_limit_for_test(MAX_TIER2_AGGREGATE_BYTES)
    }

    fn source_pool_with_retained_limit_for_test(
        retained_bytes: u64,
    ) -> (
        LibrarySourcePool,
        ManagedBlockingWorkers,
        ManagedBlockingAttemptGuard,
    ) {
        let workers = ManagedBlockingWorkers::new();
        let attempt = workers.attempt_guard();
        let pool = LibrarySourcePool::with_retained_limit(retained_bytes, workers.clone())
            .expect("test component source pool");
        (pool, workers, attempt)
    }

    struct ScriptedResponse {
        status: &'static str,
        content_length: Option<usize>,
        body: Vec<u8>,
        started: Option<oneshot::Sender<()>>,
        body_gate: Option<oneshot::Receiver<()>>,
    }

    struct ReadFailure(Cursor<Vec<u8>>);

    impl Read for ReadFailure {
        fn read(&mut self, _output: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::other("injected cache reader read failure"))
        }
    }

    impl Seek for ReadFailure {
        fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
            self.0.seek(position)
        }
    }

    struct SeekFailure(Cursor<Vec<u8>>);

    impl Read for SeekFailure {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            Read::read(&mut self.0, output)
        }
    }

    impl Seek for SeekFailure {
        fn seek(&mut self, _position: SeekFrom) -> io::Result<u64> {
            Err(io::Error::other("injected cache reader seek failure"))
        }
    }

    impl ScriptedResponse {
        fn full(body: Vec<u8>) -> Self {
            Self {
                status: "200 OK",
                content_length: Some(body.len()),
                body,
                started: None,
                body_gate: None,
            }
        }

        fn partial(content_length: usize, body: Vec<u8>) -> Self {
            Self {
                status: "200 OK",
                content_length: Some(content_length),
                body,
                started: None,
                body_gate: None,
            }
        }

        fn without_length(body: Vec<u8>) -> Self {
            Self {
                status: "200 OK",
                content_length: None,
                body,
                started: None,
                body_gate: None,
            }
        }

        fn status(status: &'static str) -> Self {
            Self {
                status,
                content_length: Some(0),
                body: Vec::new(),
                started: None,
                body_gate: None,
            }
        }

        fn gated(
            body: Vec<u8>,
            started: oneshot::Sender<()>,
            body_gate: oneshot::Receiver<()>,
        ) -> Self {
            Self {
                status: "200 OK",
                content_length: Some(body.len()),
                body,
                started: Some(started),
                body_gate: Some(body_gate),
            }
        }
    }

    async fn spawn_server(responses: Vec<ScriptedResponse>) -> (String, Arc<AtomicUsize>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind library source server");
        let url = format!("http://{}/fixture.jar", listener.local_addr().unwrap());
        let requests = Arc::new(AtomicUsize::new(0));
        let requests_for_server = Arc::clone(&requests);
        tokio::spawn(async move {
            let mut responses = VecDeque::from(responses);
            while let Some(response) = responses.pop_front() {
                let Ok((socket, _)) = listener.accept().await else {
                    return;
                };
                requests_for_server.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(write_response(socket, response));
            }
        });
        (url, requests)
    }

    async fn write_response(mut socket: tokio::net::TcpStream, mut response: ScriptedResponse) {
        let mut request = Vec::new();
        let mut buffer = [0_u8; 512];
        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
            let Ok(read) = socket.read(&mut buffer).await else {
                return;
            };
            if read == 0 {
                return;
            }
            request.extend_from_slice(&buffer[..read]);
        }
        let length = response
            .content_length
            .map(|length| format!("Content-Length: {length}\r\n"))
            .unwrap_or_default();
        let headers = format!(
            "HTTP/1.1 {}\r\n{length}Connection: close\r\n\r\n",
            response.status
        );
        if socket.write_all(headers.as_bytes()).await.is_err() {
            return;
        }
        if let Some(started) = response.started.take() {
            let _ = started.send(());
        }
        if let Some(gate) = response.body_gate.take() {
            let _ = gate.await;
        }
        let _ = socket.write_all(&response.body).await;
    }

    fn jar_bytes(payload: &[u8]) -> Vec<u8> {
        let mut writer = ZipWriter::new(std::io::Cursor::new(Vec::new()));
        writer
            .start_file("fixture.class", SimpleFileOptions::default())
            .expect("start JAR entry");
        writer.write_all(payload).expect("write JAR payload");
        writer.finish().expect("finish JAR").into_inner()
    }

    fn jar_with_entries(payloads: &[&[u8]]) -> Vec<u8> {
        let mut writer = ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for (index, payload) in payloads.iter().enumerate() {
            writer
                .start_file(
                    format!("fixture-{index}.class"),
                    SimpleFileOptions::default(),
                )
                .expect("start JAR entry");
            writer.write_all(payload).expect("write JAR payload");
        }
        writer.finish().expect("finish JAR").into_inner()
    }

    fn patch_uncompressed_sizes(bytes: &mut [u8], sizes: &[u32]) {
        let mut local_index = 0;
        let mut central_index = 0;
        for offset in 0..bytes.len().saturating_sub(4) {
            match &bytes[offset..offset + 4] {
                [0x50, 0x4b, 0x03, 0x04] => {
                    bytes[offset + 22..offset + 26]
                        .copy_from_slice(&sizes[local_index].to_le_bytes());
                    local_index += 1;
                }
                [0x50, 0x4b, 0x01, 0x02] => {
                    bytes[offset + 24..offset + 28]
                        .copy_from_slice(&sizes[central_index].to_le_bytes());
                    central_index += 1;
                }
                _ => {}
            }
        }
        assert_eq!(local_index, sizes.len());
        assert_eq!(central_index, sizes.len());
    }

    async fn wait_for_requests(requests: &AtomicUsize, expected: usize) {
        for _ in 0..100 {
            if requests.load(Ordering::SeqCst) == expected {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(requests.load(Ordering::SeqCst), expected);
    }

    fn sha1_bytes(bytes: &[u8]) -> [u8; 20] {
        Sha1::digest(bytes).into()
    }

    fn sha1_hex(bytes: &[u8]) -> String {
        format!("{}", HexSha1(&sha1_bytes(bytes)))
    }

    async fn acquire(
        url: &str,
        expected: &ExpectedIntegrity,
        max_bytes: u64,
        target: &str,
        pool: &LibrarySourcePool,
    ) -> Result<AcquiredLibrarySource, DownloadError> {
        let client = reqwest::Client::new();
        let relative_path = fixture_relative_path();
        acquire_library_source_with_retry_delays(
            LibrarySourceRequest {
                client: &client,
                url,
                expected,
                relative_path: &relative_path,
                max_bytes,
                target,
                pool,
                fact_tx: None,
            },
            &[],
        )
        .await
    }

    async fn acquire_component(
        url: &str,
        expected: &ExpectedIntegrity,
        max_bytes: u64,
        target: &str,
        pool: &LibrarySourcePool,
        kind: LibraryComponentSourceKind,
    ) -> Result<RetainedLibraryComponentSource, DownloadError> {
        let client = reqwest::Client::new();
        let relative_path = fixture_relative_path();
        acquire_retained_library_component_source(
            LibrarySourceRequest {
                client: &client,
                url,
                expected,
                relative_path: &relative_path,
                max_bytes,
                target,
                pool,
                fact_tx: None,
            },
            kind,
        )
        .await
    }

    fn fixture_relative_path() -> PortableRelativePath {
        PortableRelativePath::new("org/example/fixture/1/fixture-1.jar")
            .expect("fixture relative path")
    }

    fn rejected(
        result: Result<AcquiredLibrarySource, DownloadError>,
        context: &str,
    ) -> DownloadError {
        result.err().unwrap_or_else(|| panic!("{context}"))
    }

    fn assert_source(
        source: &AcquiredLibrarySource,
        body: &[u8],
        expected: &ExpectedIntegrity,
        _target: &str,
    ) {
        assert_eq!(source.observed_size(), body.len() as u64);
        assert_eq!(source.observed_sha1(), sha1_bytes(body));
        assert_eq!(source.expected(), expected);
        assert_eq!(source.relative_path(), &fixture_relative_path());
        assert_eq!(
            source.observed_expected(),
            ExpectedIntegrity {
                size: Some(body.len() as u64),
                sha1: Some(sha1_hex(body)),
            }
        );
        let mut file = source
            .allocation
            .replay_reader()
            .expect("replay retained source");
        let mut actual = Vec::new();
        file.read_to_end(&mut actual).expect("read retained source");
        assert_eq!(actual, body);
    }

    #[tokio::test]
    async fn acquires_sha_only_exact_source_without_destination_authority() {
        let body = jar_bytes(b"sha-only");
        let expected = ExpectedIntegrity {
            size: None,
            sha1: Some(sha1_hex(&body)),
        };
        let (url, _) = spawn_server(vec![ScriptedResponse::full(body.clone())]).await;
        let (pool, _workers, _attempt) = source_pool_for_test();
        let absent_destination = std::env::temp_dir().join(format!(
            "axial-library-source-no-destination-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&absent_destination);

        let source = acquire(&url, &expected, body.len() as u64, "library:sha", &pool)
            .await
            .expect("authenticated SHA-only source");

        assert_source(&source, &body, &expected, "library:sha");
        assert!(!absent_destination.exists());
    }

    #[tokio::test]
    async fn acquires_size_only_exact_source() {
        let body = jar_bytes(b"size-only");
        let expected = ExpectedIntegrity {
            size: Some(body.len() as u64),
            sha1: None,
        };
        let (url, _) = spawn_server(vec![ScriptedResponse::full(body.clone())]).await;
        let (pool, _workers, _attempt) = source_pool_for_test();

        let source = acquire(&url, &expected, body.len() as u64, "library:size", &pool)
            .await
            .expect("authenticated size-only source");

        assert_source(&source, &body, &expected, "library:size");
    }

    #[tokio::test]
    async fn acquires_source_without_declared_integrity() {
        let body = jar_bytes(b"observed-only");
        let expected = ExpectedIntegrity::default();
        let (url, _) = spawn_server(vec![ScriptedResponse::full(body.clone())]).await;
        let (pool, _workers, _attempt) = source_pool_for_test();

        let source = acquire(
            &url,
            &expected,
            body.len() as u64,
            "library:observed",
            &pool,
        )
        .await
        .expect("authenticated observed source");

        assert_source(&source, &body, &expected, "library:observed");
    }

    #[tokio::test]
    async fn invalid_sha_metadata_fails_before_network_or_budget_effects() {
        let body = jar_bytes(b"invalid-sha-metadata");
        let (url, requests) = spawn_server(vec![ScriptedResponse::full(body)]).await;
        let (pool, _workers, _attempt) = source_pool_for_test();
        let expected = ExpectedIntegrity {
            size: None,
            sha1: Some("not-a-sha1".to_string()),
        };

        let error = rejected(
            acquire(
                &url,
                &expected,
                LIBRARY_SOURCE_MAX_BYTES,
                "library:invalid-sha-metadata",
                &pool,
            )
            .await,
            "expected invalid SHA metadata rejection",
        );

        assert!(error.to_string().contains("checksum metadata is invalid"));
        assert_eq!(requests.load(Ordering::SeqCst), 0);
        assert_eq!(pool.available_bytes(), LIBRARY_SOURCE_MAX_BYTES);
    }

    #[tokio::test]
    async fn expected_size_is_a_hard_limit_for_lengthless_streams() {
        let body = jar_bytes(b"larger-than-tiny-expectation");
        let expected = ExpectedIntegrity {
            size: Some(1),
            sha1: None,
        };
        let (url, _) = spawn_server(vec![ScriptedResponse::without_length(body)]).await;
        let (pool, _workers, _attempt) = source_pool_for_test();

        let error = rejected(
            acquire(
                &url,
                &expected,
                LIBRARY_SOURCE_MAX_BYTES,
                "library:tiny-contract",
                &pool,
            )
            .await,
            "expected stream hard-limit rejection",
        );

        assert!(error.to_string().contains("response exceeds"));
        assert_eq!(pool.available_bytes(), LIBRARY_SOURCE_MAX_BYTES);
    }

    #[tokio::test]
    async fn unknown_source_releases_scratch_after_aggregate_retention() {
        const REQUEST_CAP: u64 = 17 << 20;
        let body = jar_bytes(b"small-source-large-contract");
        let (url, _) = spawn_server(vec![ScriptedResponse::full(body.clone())]).await;
        let (pool, _workers, _attempt) = source_pool_for_test();

        let source = acquire(
            &url,
            &ExpectedIntegrity::default(),
            REQUEST_CAP,
            "library:request-cap",
            &pool,
        )
        .await
        .expect("acquire unknown source");

        assert_source(
            &source,
            &body,
            &ExpectedIntegrity::default(),
            "library:request-cap",
        );
        assert_eq!(pool.available_bytes(), LIBRARY_SOURCE_MAX_BYTES);
        assert_eq!(
            pool.retained_available_bytes(),
            MAX_TIER2_AGGREGATE_BYTES - body.len() as u64
        );
        drop(source);
        assert_eq!(pool.available_bytes(), LIBRARY_SOURCE_MAX_BYTES);
        assert_eq!(
            pool.retained_available_bytes(),
            MAX_TIER2_AGGREGATE_BYTES - body.len() as u64
        );
    }

    #[test]
    fn owned_local_library_component_source_preserves_allocation_and_rejects_contract_drift() {
        let path = fixture_relative_path();
        let bytes = jar_bytes(b"owned local component");
        let pointer = bytes.as_ptr();
        let size = bytes.len() as u64;
        let sha1 = sha1_bytes(&bytes);
        let source = RetainedLibraryComponentSource::from_authenticated_local_bytes(
            path.clone(),
            LibraryComponentSourceKind::Library,
            bytes,
            size,
            sha1,
        )
        .expect("authenticated owned source");
        let RetainedLibraryComponentStorage::Owned(stored) = &source.storage else {
            panic!("local source must retain owned bytes");
        };
        assert_eq!(
            stored.as_ptr(),
            pointer,
            "Vec allocation must move without copying"
        );
        assert!(source.network_origin().is_none());
        assert!(source.replay().is_err());

        let drift = jar_bytes(b"owned local drift");
        let drift_size = drift.len() as u64;
        let drift_sha1 = sha1_bytes(&drift);
        assert!(
            RetainedLibraryComponentSource::from_authenticated_local_bytes(
                path.clone(),
                LibraryComponentSourceKind::Library,
                drift.clone(),
                drift_size + 1,
                drift_sha1,
            )
            .is_err()
        );
        assert!(
            RetainedLibraryComponentSource::from_authenticated_local_bytes(
                path,
                LibraryComponentSourceKind::Library,
                drift,
                drift_size,
                [0; 20],
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn authenticated_local_reconstruction_source_uses_shared_retained_spool() {
        let path = fixture_relative_path();
        let bytes = b"authenticated local reconstruction output".to_vec();
        let size = bytes.len() as u64;
        let sha1 = sha1_bytes(&bytes);
        let (pool, _workers, _attempt) = source_pool_with_retained_limit_for_test(size);
        let candidate = AuthenticatedLocalLibraryBytes::new(
            path,
            LibraryComponentSourceKind::Library,
            bytes.clone(),
            size,
            sha1,
        )
        .expect("authenticated local source candidate");

        let source = pool
            .retain_authenticated_local_source(candidate)
            .await
            .expect("retained local source");

        assert!(matches!(
            &source.storage,
            RetainedLibraryComponentStorage::Aggregate(_)
        ));
        assert!(source.network_origin().is_none());
        assert_eq!(pool.available_bytes(), LIBRARY_SOURCE_MAX_BYTES);
        assert_eq!(pool.retained_available_bytes(), 0);

        let (mut replay, replay_size, replay_sha1) =
            source.replay().expect("local replay source").into_parts();
        let mut replayed = Vec::new();
        replay
            .read_to_end(&mut replayed)
            .expect("read local replay source");
        assert_eq!(replayed, bytes);
        assert_eq!(replay_size, size);
        assert_eq!(replay_sha1, sha1);
        assert_eq!(pool.retained_available_bytes(), 0);
    }

    #[test]
    fn retained_library_source_set_rejects_portable_aliases_and_aggregate_overflow() {
        let source = |path: &str, size: u64| {
            RetainedLibraryComponentSource::from_test_identity(
                PortableRelativePath::new(path).unwrap(),
                false,
                "https://example.invalid/library.jar".to_string(),
                ExpectedIntegrity::default(),
                size,
                [1; 20],
            )
        };

        let mut aliases = RetainedLibrarySourceSet::new();
        aliases
            .insert(source("Example/shared/1/shared-1.jar", 1))
            .expect("first portable identity");
        assert!(
            aliases
                .insert(source("example/shared/1/shared-1.jar", 1))
                .is_err()
        );

        let mut aggregate = RetainedLibrarySourceSet::new();
        aggregate
            .insert(source(
                "example/aggregate/1/aggregate-1.jar",
                MAX_TIER2_AGGREGATE_BYTES,
            ))
            .expect("aggregate limit boundary");
        assert!(
            aggregate
                .insert(source("example/overflow/1/overflow-1.jar", 1))
                .is_err()
        );

        let overlay_path = "example/overlay/1/overlay-1.jar";
        let mut base = RetainedLibrarySourceSet::new();
        base.insert(source(overlay_path, 7)).unwrap();
        let mut child = RetainedLibrarySourceSet::new();
        child
            .insert(RetainedLibraryComponentSource::from_test_identity(
                PortableRelativePath::new(overlay_path).unwrap(),
                false,
                "https://example.invalid/child.jar".to_string(),
                ExpectedIntegrity::default(),
                9,
                [2; 20],
            ))
            .unwrap();
        base.merge(child).expect("child overlay source");
        let sources = base.into_sources();
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].observed_size(), 9);
        assert_eq!(sources[0].observed_sha1(), [2; 20]);
    }

    #[test]
    fn retained_library_source_set_reconciles_child_overlays_to_final_projection() {
        let library_path = "example/final/1/final-1.jar";
        let native_path = "example/native/1/native-1-natives.jar";
        let authority = crate::known_good::PendingKnownGoodInstallAuthority::component_for_test([
            (
                KnownGoodRoot::Libraries,
                library_path.to_string(),
                KnownGoodArtifactKind::Library,
                [1; 20],
                7,
            ),
            (
                KnownGoodRoot::Libraries,
                native_path.to_string(),
                KnownGoodArtifactKind::NativeLibrary,
                [2; 20],
                9,
            ),
        ]);
        let projection = authority
            .component_projection(ManagedKnownGoodComponent::Libraries)
            .expect("final Libraries projection");
        let source = |path: &str, native: bool, size: u64, sha1: [u8; 20]| {
            RetainedLibraryComponentSource::from_test_identity(
                PortableRelativePath::new(path).unwrap(),
                native,
                "https://example.invalid/library.jar".to_string(),
                ExpectedIntegrity::default(),
                size,
                sha1,
            )
        };
        let mut sources = RetainedLibrarySourceSet::new();
        sources
            .insert(source("example/stale/1/stale-1.jar", false, 5, [3; 20]))
            .unwrap();
        sources
            .insert(source(library_path, false, 7, [1; 20]))
            .unwrap();
        sources
            .insert(source(native_path, true, 9, [2; 20]))
            .unwrap();

        sources
            .reconcile_projection(&projection)
            .expect("complete final source projection");
        let sources = sources.into_sources();
        assert_eq!(sources.len(), 2);
        assert!(
            sources
                .iter()
                .all(|source| source.relative_path().as_str() != "example/stale/1/stale-1.jar")
        );

        let mut incomplete = RetainedLibrarySourceSet::new();
        incomplete
            .insert(source(library_path, false, 7, [1; 20]))
            .unwrap();
        assert!(incomplete.reconcile_projection(&projection).is_err());
    }

    #[test]
    fn sparse_reconciliation_prefers_matching_overlay_cache_proof_over_stale_base_source() {
        let path = "example/overlay/1/overlay-1.jar";
        let authority =
            crate::known_good::PendingKnownGoodInstallAuthority::component_for_test([(
                KnownGoodRoot::Libraries,
                path.to_string(),
                KnownGoodArtifactKind::Library,
                [2; 20],
                9,
            )]);
        let projection = authority
            .component_projection(ManagedKnownGoodComponent::Libraries)
            .expect("final Libraries projection");
        let relative_path = PortableRelativePath::new(path).unwrap();
        let mut sources = RetainedLibrarySourceSet::new();
        sources
            .insert(RetainedLibraryComponentSource::from_test_identity(
                relative_path.clone(),
                false,
                "https://example.invalid/base.jar".to_string(),
                ExpectedIntegrity::default(),
                7,
                [1; 20],
            ))
            .unwrap();
        let mut cache_proofs = AuthenticatedLibraryCacheProofSet::default();
        cache_proofs
            .insert(AuthenticatedLibraryCacheProof::new(
                relative_path,
                LibraryComponentSourceKind::Library,
                9,
                [2; 20],
            ))
            .unwrap();

        sources
            .reconcile_sparse_projection(&projection, cache_proofs)
            .expect("matching final overlay cache proof");

        assert!(sources.into_sources().is_empty());
    }

    #[tokio::test]
    async fn library_acquisition_reports_exhausted_retained_capacity_without_leaking_scratch() {
        let body = jar_bytes(b"aggregate-overflow");
        let (url, requests) = spawn_server(vec![
            ScriptedResponse::full(body.clone()),
            ScriptedResponse::full(body.clone()),
        ])
        .await;
        let (pool, _workers, _attempt) =
            source_pool_with_retained_limit_for_test(body.len() as u64);
        let first = acquire_component(
            &url,
            &ExpectedIntegrity::default(),
            LIBRARY_SOURCE_MAX_BYTES,
            "library:aggregate-owner",
            &pool,
            LibraryComponentSourceKind::Library,
        )
        .await
        .expect("aggregate owner");

        let result = tokio::time::timeout(
            Duration::from_secs(1),
            acquire_component(
                &url,
                &ExpectedIntegrity::default(),
                LIBRARY_SOURCE_MAX_BYTES,
                "library:aggregate-overflow",
                &pool,
                LibraryComponentSourceKind::Library,
            ),
        )
        .await
        .expect("aggregate overflow must fail without waiting");
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("aggregate overflow must be rejected"),
        };

        assert!(
            error
                .to_string()
                .contains("aggregate retained-source limit")
        );
        assert_eq!(requests.load(Ordering::SeqCst), 2);
        assert_eq!(pool.available_bytes(), LIBRARY_SOURCE_MAX_BYTES);
        assert_eq!(pool.retained_available_bytes(), 0);
        drop(first);
        assert_eq!(pool.retained_available_bytes(), 0);
    }

    #[tokio::test]
    async fn retained_library_component_source_stages_create_only_and_derives_exact_proof() {
        let body = jar_bytes(b"component-staging");
        let (url, _) = spawn_server(vec![
            ScriptedResponse::full(body.clone()),
            ScriptedResponse::full(body.clone()),
        ])
        .await;
        let (pool, _workers, _attempt) = source_pool_for_test();
        let source = acquire_component(
            &url,
            &ExpectedIntegrity::default(),
            LIBRARY_SOURCE_MAX_BYTES,
            "library:component-staging",
            &pool,
            LibraryComponentSourceKind::NativeLibrary,
        )
        .await
        .expect("retained staging source");
        assert_eq!(source.relative_path(), &fixture_relative_path());
        assert_eq!(source.observed_size(), body.len() as u64);
        assert_eq!(source.observed_sha1(), sha1_bytes(&body));
        assert_eq!(source.kind(), ManagedComponentArtifactKind::NativeLibrary);

        let temp = tempfile::tempdir().expect("component staging root");
        let root = ManagedDir::open_root(temp.path()).expect("managed component staging root");
        let lease = ManagedRootPublicationLease::acquire(root.clone())
            .await
            .expect("component staging lease");
        let staging = root.create_child_new("staging").expect("staging directory");
        let bucket = staging.create_child_new("000000").expect("staging bucket");
        let slot = "000001";
        let proof = source.exact_download_proof().expect("network source proof");
        let staged = source
            .stage_create_new(&bucket, slot, lease.lifetime_guard())
            .await
            .expect("stage retained component source");
        assert!(staged.matches(
            &fixture_relative_path(),
            ManagedComponentArtifactKind::NativeLibrary,
            body.len() as u64,
            sha1_bytes(&body),
        ));
        assert_eq!(
            std::fs::read(bucket.path().join(slot)).expect("read staged source"),
            body
        );
        assert_eq!(
            pool.retained_available_bytes(),
            MAX_TIER2_AGGREGATE_BYTES - body.len() as u64
        );

        let (path, is_native, provider_url, expected, size, sha1) = proof.into_parts();
        assert_eq!(path, fixture_relative_path());
        assert!(is_native);
        assert_eq!(provider_url, url);
        assert_eq!(expected, ExpectedIntegrity::default());
        assert_eq!(size, body.len() as u64);
        assert_eq!(sha1, sha1_bytes(&body));

        let occupied = acquire_component(
            &url,
            &ExpectedIntegrity::default(),
            LIBRARY_SOURCE_MAX_BYTES,
            "library:component-occupied",
            &pool,
            LibraryComponentSourceKind::Library,
        )
        .await
        .expect("retained occupied-slot source");
        let error = match occupied
            .stage_create_new(&bucket, slot, lease.lifetime_guard())
            .await
        {
            Err(error) => error,
            Ok(_) => panic!("occupied staging slot must fail closed"),
        };
        assert!(matches!(error, LoaderError::Io(_)));
        assert_eq!(
            std::fs::read(bucket.path().join(slot)).expect("read unchanged staged source"),
            body
        );
        assert_eq!(
            pool.retained_available_bytes(),
            MAX_TIER2_AGGREGATE_BYTES - 2 * body.len() as u64
        );
    }

    #[tokio::test]
    async fn retained_library_replay_then_final_consume_does_not_charge_budget_twice() {
        let body = jar_bytes(b"replay then final consume");
        let (url, _) = spawn_server(vec![ScriptedResponse::full(body.clone())]).await;
        let (pool, _workers, _attempt) = source_pool_for_test();
        let source = acquire_component(
            &url,
            &ExpectedIntegrity::default(),
            LIBRARY_SOURCE_MAX_BYTES,
            "library:replay-final-consume",
            &pool,
            LibraryComponentSourceKind::Library,
        )
        .await
        .expect("retained replay source");
        let retained_budget = pool.retained_available_bytes();

        let (mut replay, replay_size, replay_sha1) =
            source.replay().expect("borrow replay reader").into_parts();
        let mut replayed = Vec::new();
        replay
            .read_to_end(&mut replayed)
            .expect("read replay source");
        assert_eq!(replayed, body);
        assert_eq!(replay_size, body.len() as u64);
        assert_eq!(replay_sha1, sha1_bytes(&body));
        assert_eq!(pool.retained_available_bytes(), retained_budget);
        drop(replay);

        let temp = tempfile::tempdir().expect("final component staging root");
        let root = ManagedDir::open_root(temp.path()).expect("managed staging root");
        let lease = ManagedRootPublicationLease::acquire(root.clone())
            .await
            .expect("component staging lease");
        let staging = root.create_child_new("staging").expect("staging directory");
        let bucket = staging.create_child_new("000000").expect("staging bucket");
        source
            .stage_create_new(&bucket, "000001", lease.lifetime_guard())
            .await
            .expect("consume final source authority");
        assert_eq!(
            std::fs::read(bucket.path().join("000001")).expect("read final staged source"),
            body
        );
        assert_eq!(pool.retained_available_bytes(), retained_budget);
    }

    #[tokio::test]
    async fn cancelled_library_staging_keeps_retained_source_alive_until_copy_finishes() {
        let body = jar_bytes(b"cancelled-component-staging");
        let (url, _) = spawn_server(vec![ScriptedResponse::full(body.clone())]).await;
        let (pool, _workers, _attempt) =
            source_pool_with_retained_limit_for_test(body.len() as u64);
        let source = acquire_component(
            &url,
            &ExpectedIntegrity::default(),
            LIBRARY_SOURCE_MAX_BYTES,
            "library:cancelled-component-staging",
            &pool,
            LibraryComponentSourceKind::Library,
        )
        .await
        .expect("retained cancellation source");
        assert_eq!(pool.retained_available_bytes(), 0);

        let temp = tempfile::tempdir().expect("cancelled staging root");
        let root = ManagedDir::open_root(temp.path()).expect("managed cancellation root");
        let lease = ManagedRootPublicationLease::acquire(root.clone())
            .await
            .expect("cancellation staging lease");
        let staging = root.create_child_new("staging").expect("staging directory");
        let bucket = staging.create_child_new("000000").expect("staging bucket");
        let slot = "000002";
        let (entered_tx, entered_rx) = oneshot::channel();
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let release_for_hook = Arc::clone(&release);
        let task_bucket = bucket.clone();
        let lifetime_guard = lease.lifetime_guard();
        let task = tokio::spawn(async move {
            source
                .stage_create_new_with_hook(
                    &task_bucket,
                    slot,
                    lifetime_guard,
                    Box::new(move || {
                        let _ = entered_tx.send(());
                        let (lock, condition) = &*release_for_hook;
                        let released = lock.lock().expect("staging gate lock");
                        drop(
                            condition
                                .wait_while(released, |released| !*released)
                                .expect("staging gate wait"),
                        );
                    }),
                )
                .await
        });
        entered_rx.await.expect("blocking staging owns source");
        assert_eq!(pool.retained_available_bytes(), 0);

        task.abort();
        let _ = task.await;
        assert_eq!(pool.retained_available_bytes(), 0);
        drop(lease);

        let waiter = tokio::spawn(ManagedRootPublicationLease::acquire(
            ManagedDir::open_root(temp.path()).expect("waiting cancellation root"),
        ));
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!waiter.is_finished());

        let (lock, condition) = &*release;
        *lock.lock().expect("staging release lock") = true;
        condition.notify_one();
        for _ in 0..100 {
            if bucket.path().join(slot).exists() {
                assert_eq!(
                    std::fs::read(bucket.path().join(slot))
                        .expect("completed detached staging copy"),
                    body
                );
                assert_eq!(pool.retained_available_bytes(), 0);
                tokio::time::timeout(PUBLICATION_ACQUIRE_TIMEOUT, waiter)
                    .await
                    .expect("blocking copy released writer exclusion")
                    .expect("waiting writer task")
                    .expect("waiting writer lease");
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("detached staging did not finish");
    }

    #[tokio::test]
    async fn rejects_sha_mismatch() {
        let body = jar_bytes(b"sha-mismatch");
        let expected = ExpectedIntegrity {
            size: None,
            sha1: Some("0000000000000000000000000000000000000000".to_string()),
        };
        let (url, _) = spawn_server(vec![ScriptedResponse::full(body.clone())]).await;
        let (pool, _workers, _attempt) = source_pool_for_test();

        let error = rejected(
            acquire(
                &url,
                &expected,
                body.len() as u64,
                "library:sha-mismatch",
                &pool,
            )
            .await,
            "expected mismatched SHA rejection",
        );

        assert!(error.to_string().contains("checksum does not match"));
    }

    #[tokio::test]
    async fn rejects_size_mismatch() {
        let body = jar_bytes(b"size-mismatch");
        let expected = ExpectedIntegrity {
            size: Some(body.len() as u64 + 1),
            sha1: None,
        };
        let (url, _) = spawn_server(vec![ScriptedResponse::full(body.clone())]).await;
        let (pool, _workers, _attempt) = source_pool_for_test();

        let error = rejected(
            acquire(
                &url,
                &expected,
                body.len() as u64 + 1,
                "library:size-mismatch",
                &pool,
            )
            .await,
            "expected mismatched size rejection",
        );

        assert!(error.to_string().contains("size does not match"));
    }

    #[tokio::test]
    async fn rejects_declared_content_length_over_request_cap() {
        let response = ScriptedResponse::partial(65, Vec::new());
        let (url, _) = spawn_server(vec![response]).await;
        let (pool, _workers, _attempt) = source_pool_for_test();

        let error = rejected(
            acquire(
                &url,
                &ExpectedIntegrity::default(),
                64,
                "library:declared-oversize",
                &pool,
            )
            .await,
            "expected declared oversize rejection",
        );

        assert!(error.to_string().contains("declared response exceeds"));
    }

    #[tokio::test]
    async fn rejects_stream_over_request_cap_without_content_length() {
        let body = jar_bytes(b"stream-over-cap");
        let cap = body.len() as u64 - 1;
        let (url, _) = spawn_server(vec![ScriptedResponse::without_length(body)]).await;
        let (pool, _workers, _attempt) = source_pool_for_test();

        let error = rejected(
            acquire(
                &url,
                &ExpectedIntegrity::default(),
                cap,
                "library:stream-oversize",
                &pool,
            )
            .await,
            "expected streamed oversize rejection",
        );

        assert!(error.to_string().contains("response exceeds"));
    }

    #[tokio::test]
    async fn retries_interrupted_response_with_zero_delays() {
        let body = jar_bytes(b"retry-exact");
        let truncated = body[..body.len() / 2].to_vec();
        let responses = vec![
            ScriptedResponse::partial(body.len(), truncated.clone()),
            ScriptedResponse::partial(body.len(), truncated),
            ScriptedResponse::full(body.clone()),
        ];
        let (url, requests) = spawn_server(responses).await;
        let expected = ExpectedIntegrity {
            size: Some(body.len() as u64),
            sha1: Some(sha1_hex(&body)),
        };
        let (pool, _workers, _attempt) = source_pool_for_test();

        let client = reqwest::Client::new();
        let relative_path = fixture_relative_path();
        let source = acquire_library_source_with_retry_delays(
            LibrarySourceRequest {
                client: &client,
                url: &url,
                expected: &expected,
                relative_path: &relative_path,
                max_bytes: body.len() as u64,
                target: "library:retry",
                pool: &pool,
                fact_tx: None,
            },
            &[Duration::ZERO, Duration::ZERO],
        )
        .await
        .expect("retry interrupted streams");

        assert_source(&source, &body, &expected, "library:retry");
        assert_eq!(requests.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn request_timeout_status_is_not_retried() {
        let body = jar_bytes(b"must-not-be-requested");
        let responses = vec![
            ScriptedResponse::status("408 Request Timeout"),
            ScriptedResponse::full(body),
        ];
        let (url, requests) = spawn_server(responses).await;
        let client = reqwest::Client::new();
        let expected = ExpectedIntegrity::default();
        let relative_path = fixture_relative_path();
        let (pool, _workers, _attempt) = source_pool_for_test();

        let error = acquire_library_source_with_retry_delays(
            LibrarySourceRequest {
                client: &client,
                url: &url,
                expected: &expected,
                relative_path: &relative_path,
                max_bytes: LIBRARY_SOURCE_MAX_BYTES,
                target: "library:408",
                pool: &pool,
                fact_tx: None,
            },
            &[Duration::ZERO],
        )
        .await
        .err()
        .expect("408 must not retry");

        assert!(error.to_string().contains("provider rejected"));
        assert_eq!(requests.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn rate_limit_status_is_retried() {
        let body = jar_bytes(b"retry-after-rate-limit");
        let responses = vec![
            ScriptedResponse::status("429 Too Many Requests"),
            ScriptedResponse::full(body.clone()),
        ];
        let (url, requests) = spawn_server(responses).await;
        let client = reqwest::Client::new();
        let expected = ExpectedIntegrity::default();
        let relative_path = fixture_relative_path();
        let (pool, _workers, _attempt) = source_pool_for_test();

        let source = acquire_library_source_with_retry_delays(
            LibrarySourceRequest {
                client: &client,
                url: &url,
                expected: &expected,
                relative_path: &relative_path,
                max_bytes: LIBRARY_SOURCE_MAX_BYTES,
                target: "library:429",
                pool: &pool,
                fact_tx: None,
            },
            &[Duration::ZERO],
        )
        .await
        .expect("429 retry");

        assert_source(&source, &body, &expected, "library:429");
        assert_eq!(requests.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn rejects_zero_length_library_contract_before_network_or_retention() {
        let (url, requests) = spawn_server(vec![ScriptedResponse::full(Vec::new())]).await;
        let (pool, _workers, _attempt) = source_pool_for_test();

        let error = rejected(
            acquire(
                &url,
                &ExpectedIntegrity {
                    size: Some(0),
                    sha1: Some(sha1_hex(&[])),
                },
                LIBRARY_SOURCE_MAX_BYTES,
                "library:zero-length",
                &pool,
            )
            .await,
            "expected zero-length library rejection",
        );

        assert!(error.to_string().contains("scratch limit"));
        assert_eq!(requests.load(Ordering::SeqCst), 0);
        assert_eq!(pool.available_bytes(), LIBRARY_SOURCE_MAX_BYTES);
        assert_eq!(pool.retained_available_bytes(), MAX_TIER2_AGGREGATE_BYTES);
    }

    #[tokio::test]
    async fn fresh_invalid_jar_restores_scratch_without_retained_charge() {
        let body = b"not-a-jar".to_vec();
        let (url, requests) = spawn_server(vec![ScriptedResponse::full(body.clone())]).await;
        let (pool, _workers, _attempt) = source_pool_for_test();

        let error = rejected(
            acquire(
                &url,
                &ExpectedIntegrity::default(),
                body.len() as u64,
                "library:invalid-jar",
                &pool,
            )
            .await,
            "expected invalid JAR rejection",
        );

        assert!(error.to_string().contains("file operation failed"));
        assert_eq!(requests.load(Ordering::SeqCst), 1);
        assert_eq!(pool.available_bytes(), LIBRARY_SOURCE_MAX_BYTES);
        assert_eq!(pool.retained_available_bytes(), MAX_TIER2_AGGREGATE_BYTES);
    }

    #[tokio::test]
    async fn cache_jar_reader_failures_restore_scratch_without_retained_charge() {
        let body = jar_bytes(b"cache reader failure");
        let sha1 = sha1_bytes(&body);
        let (pool, _workers, _attempt) = source_pool_for_test();

        assert!(
            pool.try_retain_authenticated_jar_reader(
                SeekFailure(Cursor::new(body.clone())),
                body.len() as u64,
                sha1,
            )
            .await
            .expect("seek failure is a cache miss")
            .is_none()
        );
        assert_eq!(pool.available_bytes(), LIBRARY_SOURCE_MAX_BYTES);
        assert_eq!(pool.retained_available_bytes(), MAX_TIER2_AGGREGATE_BYTES);

        assert!(
            pool.try_retain_authenticated_jar_reader(
                ReadFailure(Cursor::new(body.clone())),
                body.len() as u64,
                sha1,
            )
            .await
            .expect("read failure is a cache miss")
            .is_none()
        );
        assert_eq!(pool.available_bytes(), LIBRARY_SOURCE_MAX_BYTES);
        assert_eq!(pool.retained_available_bytes(), MAX_TIER2_AGGREGATE_BYTES);
    }

    fn eocd(entries: u16, directory_bytes: u32) -> File {
        let mut bytes = vec![0_u8; ZIP_END_OF_CENTRAL_DIRECTORY_BYTES];
        bytes[..4].copy_from_slice(&[0x50, 0x4b, 0x05, 0x06]);
        bytes[8..10].copy_from_slice(&entries.to_le_bytes());
        bytes[10..12].copy_from_slice(&entries.to_le_bytes());
        bytes[12..16].copy_from_slice(&directory_bytes.to_le_bytes());
        let mut file = tempfile::tempfile().expect("preflight file");
        file.write_all(&bytes).expect("write EOCD");
        file.seek(SeekFrom::Start(0)).expect("rewind EOCD");
        file
    }

    #[test]
    fn preflight_rejects_oversized_entry_count_and_central_directory() {
        let mut too_many_entries = eocd(MAX_JAR_ENTRIES + 1, 1);
        assert!(preflight_zip_central_directory(&mut too_many_entries, &uncancelled()).is_err());

        let mut oversized_directory = eocd(1, MAX_JAR_CENTRAL_DIRECTORY_BYTES + 1);
        assert!(preflight_zip_central_directory(&mut oversized_directory, &uncancelled()).is_err());
    }

    #[test]
    fn preflight_rejects_trailing_bytes_and_fake_terminal_eocd() {
        let mut trailing = jar_bytes(b"trailing");
        trailing.push(0);
        let mut trailing_file = tempfile::tempfile().expect("trailing file");
        trailing_file
            .write_all(&trailing)
            .expect("write trailing fixture");
        assert!(preflight_zip_central_directory(&mut trailing_file, &uncancelled()).is_err());

        let mut fake_terminal = jar_bytes(b"fake terminal");
        fake_terminal.extend_from_slice(&[
            0x50, 0x4b, 0x05, 0x06, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ]);
        let mut fake_file = tempfile::tempfile().expect("fake EOCD file");
        fake_file
            .write_all(&fake_terminal)
            .expect("write fake EOCD fixture");
        assert!(preflight_zip_central_directory(&mut fake_file, &uncancelled()).is_err());
    }

    #[test]
    fn jar_validation_rejects_oversized_name_budget() {
        let name = "x".repeat(MAX_JAR_ENTRY_NAME_BYTES + 1);
        let mut writer = ZipWriter::new(std::io::Cursor::new(Vec::new()));
        writer
            .start_file(name, SimpleFileOptions::default())
            .expect("start oversized-name entry");
        writer.write_all(b"payload").expect("write payload");
        let bytes = writer.finish().expect("finish fixture").into_inner();
        let mut file = tempfile::tempfile().expect("JAR file");
        file.write_all(&bytes).expect("write JAR fixture");
        file.seek(SeekFrom::Start(0)).expect("rewind JAR fixture");

        let error =
            validate_bounded_jar(&mut file, &uncancelled()).expect_err("reject oversized name");

        assert!(error.to_string().contains("entry name is too large"));
    }

    #[test]
    fn jar_validation_rejects_single_entry_zip_bomb_metadata() {
        let mut bytes = jar_with_entries(&[b"tiny"]);
        patch_uncompressed_sizes(&mut bytes, &[(MAX_JAR_ENTRY_UNCOMPRESSED_BYTES + 1) as u32]);
        let mut file = tempfile::tempfile().expect("JAR file");
        file.write_all(&bytes).expect("write JAR fixture");
        file.seek(SeekFrom::Start(0)).expect("rewind JAR fixture");

        let error =
            validate_bounded_jar(&mut file, &uncancelled()).expect_err("reject per-entry ZIP bomb");

        assert!(error.to_string().contains("entry expands beyond"));
    }

    #[test]
    fn jar_validation_rejects_aggregate_zip_bomb_metadata() {
        const ENTRY_SIZE: u32 = 300 << 20;
        let mut bytes = jar_with_entries(&[b"first", b"second"]);
        patch_uncompressed_sizes(&mut bytes, &[ENTRY_SIZE, ENTRY_SIZE]);
        let mut file = tempfile::tempfile().expect("JAR file");
        file.write_all(&bytes).expect("write JAR fixture");
        file.seek(SeekFrom::Start(0)).expect("rewind JAR fixture");

        let error =
            validate_bounded_jar(&mut file, &uncancelled()).expect_err("reject aggregate ZIP bomb");

        assert!(error.to_string().contains("JAR expands beyond"));
    }

    #[tokio::test]
    async fn cancellation_during_body_restores_pool_permits() {
        let body = jar_bytes(b"cancel-body");
        let (started_tx, started_rx) = oneshot::channel();
        let (gate_tx, gate_rx) = oneshot::channel();
        let response = ScriptedResponse::gated(body.clone(), started_tx, gate_rx);
        let (url, _) = spawn_server(vec![response]).await;
        let (pool, _workers, _attempt) = source_pool_for_test();
        let pool_for_task = pool.clone();
        let task = tokio::spawn(async move {
            acquire(
                &url,
                &ExpectedIntegrity::default(),
                LIBRARY_SOURCE_MAX_BYTES,
                "library:cancel-body",
                &pool_for_task,
            )
            .await
        });
        started_rx.await.expect("request reached body gate");
        assert_eq!(pool.available_bytes(), 0);

        task.abort();
        let _ = task.await;
        let _ = gate_tx.send(());

        assert_eq!(pool.available_bytes(), LIBRARY_SOURCE_MAX_BYTES);
    }

    #[tokio::test]
    async fn cancellation_drain_joins_blocked_validation_and_restores_budget() {
        let body = jar_bytes(b"cancel-validation");
        let (url, _) = spawn_server(vec![ScriptedResponse::full(body.clone())]).await;
        let workers = ManagedBlockingWorkers::new();
        let pool = LibrarySourcePool::new_with_workers(workers.clone())
            .expect("test component source pool");
        let pool_for_task = pool.clone();
        let (entered_tx, entered_rx) = oneshot::channel();
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let release_for_hook = Arc::clone(&release);
        let hook_exited = Arc::new(AtomicUsize::new(0));
        let hook_exited_for_task = Arc::clone(&hook_exited);
        let task = tokio::spawn(async move {
            let client = reqwest::Client::new();
            let expected = ExpectedIntegrity::default();
            let relative_path = fixture_relative_path();
            let request = LibrarySourceRequest {
                client: &client,
                url: &url,
                expected: &expected,
                relative_path: &relative_path,
                max_bytes: LIBRARY_SOURCE_MAX_BYTES,
                target: "library:cancel-validation",
                pool: &pool_for_task,
                fact_tx: None,
            };
            acquire_library_source_attempt_inner(
                &request,
                Some(Box::new(move || {
                    let _ = entered_tx.send(());
                    let (lock, condition) = &*release_for_hook;
                    let released = lock.lock().expect("validation gate lock");
                    drop(
                        condition
                            .wait_while(released, |released| !*released)
                            .expect("validation gate wait"),
                    );
                    hook_exited_for_task.store(1, Ordering::Release);
                })),
            )
            .await
        });
        entered_rx.await.expect("blocking validation entered");
        assert_eq!(pool.available_bytes(), 0);

        workers.cancel();
        task.abort();
        let _ = task.await;
        assert_eq!(pool.available_bytes(), 0);
        let mut drain = Box::pin(workers.drain());
        assert!(
            tokio::time::timeout(Duration::from_millis(25), &mut drain)
                .await
                .is_err()
        );
        assert_eq!(hook_exited.load(Ordering::Acquire), 0);

        let (lock, condition) = &*release;
        *lock.lock().expect("validation release lock") = true;
        condition.notify_one();
        tokio::time::timeout(Duration::from_secs(1), drain)
            .await
            .expect("validation worker must acknowledge cancellation");
        assert_eq!(hook_exited.load(Ordering::Acquire), 1);
        assert_eq!(pool.available_bytes(), LIBRARY_SOURCE_MAX_BYTES);
        assert_eq!(pool.retained_available_bytes(), MAX_TIER2_AGGREGATE_BYTES);
    }

    #[tokio::test]
    async fn sixteen_unknown_sources_serialize_on_the_aggregate_budget() {
        let body = jar_bytes(b"aggregate-budget");
        let (started_tx, started_rx) = oneshot::channel();
        let (gate_tx, gate_rx) = oneshot::channel();
        let mut responses = vec![ScriptedResponse::gated(body.clone(), started_tx, gate_rx)];
        responses.extend((1..16).map(|_| ScriptedResponse::full(body.clone())));
        let (url, requests) = spawn_server(responses).await;
        let (pool, _workers, _attempt) = source_pool_for_test();

        let first_url = url.clone();
        let first_pool = pool.clone();
        let first = tokio::spawn(async move {
            let source = acquire(
                &first_url,
                &ExpectedIntegrity::default(),
                LIBRARY_SOURCE_MAX_BYTES,
                "library:aggregate-first",
                &first_pool,
            )
            .await?;
            drop(source);
            Ok::<_, DownloadError>(())
        });
        started_rx.await.expect("first request reached body gate");

        let mut followers = Vec::new();
        for index in 1..16 {
            let follower_url = url.clone();
            let follower_pool = pool.clone();
            followers.push(tokio::spawn(async move {
                let target = format!("library:aggregate-{index}");
                let source = acquire(
                    &follower_url,
                    &ExpectedIntegrity::default(),
                    LIBRARY_SOURCE_MAX_BYTES,
                    &target,
                    &follower_pool,
                )
                .await?;
                drop(source);
                Ok::<_, DownloadError>(())
            }));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(requests.load(Ordering::SeqCst), 1);

        gate_tx.send(()).expect("release first response");
        first.await.expect("first task").expect("first source");
        for follower in followers {
            follower
                .await
                .expect("follower task")
                .expect("follower source");
        }
        assert_eq!(requests.load(Ordering::SeqCst), 16);
    }

    #[tokio::test]
    async fn weighted_known_sources_cannot_overbook_the_aggregate_budget() {
        const EXPECTED_SIZE: u64 = 33 << 20;
        let body = jar_bytes(b"weighted-budget");
        let mut responses = Vec::new();
        let mut gates = Vec::new();
        for _ in 0..16 {
            let (gate_tx, gate_rx) = oneshot::channel();
            gates.push(gate_tx);
            responses.push(ScriptedResponse {
                status: "200 OK",
                content_length: Some(body.len()),
                body: body.clone(),
                started: None,
                body_gate: Some(gate_rx),
            });
        }
        let (url, requests) = spawn_server(responses).await;
        let (pool, _workers, _attempt) = source_pool_for_test();
        let mut tasks = Vec::new();
        for index in 0..16 {
            let task_url = url.clone();
            let task_pool = pool.clone();
            tasks.push(tokio::spawn(async move {
                let target = format!("library:weighted-{index}");
                let expected = ExpectedIntegrity {
                    size: Some(EXPECTED_SIZE),
                    sha1: None,
                };
                acquire(
                    &task_url,
                    &expected,
                    LIBRARY_SOURCE_MAX_BYTES,
                    &target,
                    &task_pool,
                )
                .await
            }));
        }

        wait_for_requests(&requests, 15).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(requests.load(Ordering::SeqCst), 15);

        for gate in gates {
            let _ = gate.send(());
        }
        for task in tasks {
            assert!(task.await.expect("weighted task").is_err());
        }
        assert_eq!(requests.load(Ordering::SeqCst), 16);
        assert_eq!(pool.available_bytes(), LIBRARY_SOURCE_MAX_BYTES);
    }
}
