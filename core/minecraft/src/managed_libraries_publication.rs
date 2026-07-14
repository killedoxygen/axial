use crate::artifact_path::ArtifactRelativePath;
use crate::known_good::{
    KnownGoodArtifactKind, KnownGoodIntegrity, KnownGoodRoot, ManagedKnownGoodComponent,
    PendingKnownGoodInstallAuthority,
};
use crate::loaders::types::LoaderError;
use crate::managed_component_effects::{
    ComponentCanonicalObservation, ComponentEffectsError, ComponentIntentCandidate,
    ComponentIntentPublishFailure, ComponentIntentPublished, ComponentLane,
    component_root_binding_sha256, component_slot_name, plan_component_canonical_path,
};
use crate::managed_component_spool::{ComponentTableSpool, ComponentTableSpoolError};
use crate::managed_component_table::{
    COMPONENT_TABLE_ROWS_PER_SHARD, ComponentIntentManifest, ComponentPriorFile,
    ComponentTableBuilder, ComponentTableError, ComponentTableRow, ComponentTableSummary,
    ManagedComponentArtifactKind, ManagedComponentKind,
};
use crate::managed_fs::ManagedDir;
use crate::managed_publication::{
    ManagedPublicationError, ManagedPublicationLifetimeGuard, ManagedRootPublicationLease,
    run_publication_blocking,
};
use std::collections::{BTreeMap, VecDeque};
use std::future::Future;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LibrariesPublicationSourceIdentity {
    relative_path: ArtifactRelativePath,
    kind: ManagedComponentArtifactKind,
    size: u64,
    sha1: [u8; 20],
}

pub(crate) trait RetainedLibrariesPublicationSource: Send + Sized {
    fn relative_path(&self) -> &ArtifactRelativePath;
    fn kind(&self) -> ManagedComponentArtifactKind;
    fn observed_size(&self) -> u64;
    fn observed_sha1(&self) -> [u8; 20];

    fn stage_create_new(
        self,
        staging_bucket: &ManagedDir,
        slot: &str,
        lifetime_guard: ManagedPublicationLifetimeGuard,
    ) -> impl Future<Output = Result<LibrariesPublicationSourceIdentity, LoaderError>> + Send;
}

pub(crate) struct PreparedLibrariesIntent {
    authority: PendingKnownGoodInstallAuthority,
    publication: ComponentIntentPublished,
}

pub(crate) struct PreparedLibrariesIntentCandidate {
    authority: PendingKnownGoodInstallAuthority,
    candidate: ComponentIntentCandidate,
}

pub(crate) struct LibrariesIntentPublishFailure {
    authority: PendingKnownGoodInstallAuthority,
    publication: ComponentIntentPublishFailure,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum PrepareLibrariesIntentError {
    #[error("authenticated Libraries projection is invalid")]
    Projection,
    #[error("retained Libraries sources do not match the authenticated projection")]
    SourceSet,
    #[error("managed Libraries table summaries disagree")]
    TableSummary,
    #[error(transparent)]
    Effects(#[from] ComponentEffectsError),
    #[error(transparent)]
    Table(#[from] ComponentTableError),
    #[error(transparent)]
    Spool(#[from] ComponentTableSpoolError),
    #[error(transparent)]
    Filesystem(#[from] LoaderError),
    #[error(transparent)]
    Publication(#[from] ManagedPublicationError),
}

impl std::fmt::Debug for LibrariesIntentPublishFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LibrariesIntentPublishFailure")
            .finish_non_exhaustive()
    }
}

impl LibrariesPublicationSourceIdentity {
    pub(crate) fn new(
        relative_path: ArtifactRelativePath,
        kind: ManagedComponentArtifactKind,
        size: u64,
        sha1: [u8; 20],
    ) -> Self {
        Self {
            relative_path,
            kind,
            size,
            sha1,
        }
    }

    fn matches(
        &self,
        relative_path: &ArtifactRelativePath,
        kind: ManagedComponentArtifactKind,
        size: u64,
        sha1: [u8; 20],
    ) -> bool {
        self.relative_path == *relative_path
            && self.kind == kind
            && self.size == size
            && self.sha1 == sha1
    }
}

impl PreparedLibrariesIntentCandidate {
    pub(crate) fn publish_intent(
        self,
    ) -> Result<PreparedLibrariesIntent, LibrariesIntentPublishFailure> {
        let Self {
            authority,
            candidate,
        } = self;
        match candidate.publish_intent() {
            Ok(publication) => Ok(PreparedLibrariesIntent {
                authority,
                publication,
            }),
            Err(publication) => Err(LibrariesIntentPublishFailure {
                authority,
                publication,
            }),
        }
    }
}

struct LibrariesProjectionRow {
    inventory_ordinal: u32,
    path: ArtifactRelativePath,
    kind: ManagedComponentArtifactKind,
    size: u64,
    sha1: [u8; 20],
}

struct SparseSourceCounts {
    required: usize,
    supplied_exact: usize,
}

pub(crate) async fn prepare_libraries_intent<S>(
    lease: ManagedRootPublicationLease,
    authority: PendingKnownGoodInstallAuthority,
    sources: Vec<S>,
) -> Result<PreparedLibrariesIntentCandidate, PrepareLibrariesIntentError>
where
    S: RetainedLibrariesPublicationSource + 'static,
{
    let (projection_rows, sources) = {
        let projection = authority
            .libraries_projection()
            .map_err(|_| PrepareLibrariesIntentError::Projection)?;
        let projection_rows = projection_rows(&projection)?;
        let sources = index_sparse_sources(&projection_rows, sources)?;
        (projection_rows, sources)
    };
    let total_rows = projection_rows.len();
    let (mut lease, mut lane, mut builder, mut spool, planned_rows, mut sources, source_counts) =
        run_publication_blocking(move || {
            let planned_rows = plan_projection(&lease, projection_rows)?;
            let source_counts = validate_sparse_source_coverage(&planned_rows, &sources)?;
            let lane = ComponentLane::prepare_fresh(&lease, ManagedComponentKind::Libraries)?;
            let root_binding_sha256 = component_root_binding_sha256(lease.root())?;
            let transaction_nonce = *uuid::Uuid::new_v4().as_bytes();
            let builder = ComponentTableBuilder::new(
                ManagedComponentKind::Libraries,
                total_rows,
                transaction_nonce,
                root_binding_sha256,
            )?;
            let spool = ComponentTableSpool::new(total_rows)?;
            Ok::<_, PrepareLibrariesIntentError>((
                lease,
                lane,
                builder,
                spool,
                planned_rows,
                sources,
                source_counts,
            ))
        })
        .await??;
    let mut planned_rows = VecDeque::from(planned_rows);
    let mut staged_sources = 0_usize;
    let mut dropped_exact_sources = 0_usize;
    let mut shard_index = 0_usize;
    while !planned_rows.is_empty() {
        let shard_len = planned_rows.len().min(COMPONENT_TABLE_ROWS_PER_SHARD);
        let mut shard_rows = Vec::new();
        shard_rows
            .try_reserve_exact(shard_len)
            .map_err(|_| PrepareLibrariesIntentError::SourceSet)?;
        for _ in 0..shard_len {
            shard_rows.push(
                planned_rows
                    .pop_front()
                    .ok_or(PrepareLibrariesIntentError::SourceSet)?,
            );
        }
        let prepared = run_publication_blocking(move || {
            let buckets = lane.create_shard_buckets(shard_index)?;
            Ok::<_, PrepareLibrariesIntentError>((lease, lane, buckets, shard_rows))
        })
        .await??;
        let (returned_lease, returned_lane, buckets, shard_rows) = prepared;
        lease = returned_lease;
        lane = returned_lane;
        let mut table_rows = Vec::new();
        table_rows
            .try_reserve_exact(shard_len)
            .map_err(|_| PrepareLibrariesIntentError::SourceSet)?;
        for (row_in_shard, row) in shard_rows.into_iter().enumerate() {
            let source = sources.remove(&row.path);
            if row.prior_is_final() {
                if let Some(source) = source {
                    drop(source);
                    dropped_exact_sources += 1;
                }
            } else {
                let source = source.ok_or(PrepareLibrariesIntentError::SourceSet)?;
                let slot = component_slot_name(row_in_shard)?;
                let staged = source
                    .stage_create_new(buckets.staging(), &slot, lease.lifetime_guard())
                    .await?;
                if !staged.matches(&row.path, row.kind, row.final_size, row.final_sha1) {
                    return Err(PrepareLibrariesIntentError::SourceSet);
                }
                staged_sources += 1;
            }
            table_rows.push(row);
        }
        let pushed = run_publication_blocking(move || {
            let (encoded, descriptor) = builder.push_shard(table_rows)?;
            spool.append(encoded, descriptor)?;
            Ok::<_, PrepareLibrariesIntentError>((lease, lane, builder, spool))
        })
        .await??;
        (lease, lane, builder, spool) = pushed;
        shard_index += 1;
    }
    if !sources.is_empty()
        || staged_sources != source_counts.required
        || dropped_exact_sources != source_counts.supplied_exact
    {
        return Err(PrepareLibrariesIntentError::SourceSet);
    }
    let candidate = run_publication_blocking(move || {
        let (manifest, summary) = builder.finish()?;
        let replay = spool.finish(&manifest)?;
        let durable_summary = lane.publish_table(replay, &manifest)?;
        validate_table_summary(&summary, &durable_summary, manifest.shards.len(), &manifest)?;
        Ok::<_, PrepareLibrariesIntentError>(lane.into_intent_candidate(lease, manifest)?)
    })
    .await??;
    Ok(PreparedLibrariesIntentCandidate {
        authority,
        candidate,
    })
}

fn projection_rows(
    projection: &crate::known_good::ManagedComponentProjection<'_>,
) -> Result<Vec<LibrariesProjectionRow>, PrepareLibrariesIntentError> {
    if projection.component() != ManagedKnownGoodComponent::Libraries {
        return Err(PrepareLibrariesIntentError::Projection);
    }
    let mut rows = Vec::new();
    rows.try_reserve_exact(projection.entry_count())
        .map_err(|_| PrepareLibrariesIntentError::SourceSet)?;
    for projected in projection.entries().iter().copied() {
        let entry = projected.entry();
        if entry.root() != &KnownGoodRoot::Libraries {
            return Err(PrepareLibrariesIntentError::Projection);
        }
        let relative_path = ArtifactRelativePath::new(entry.path().as_str())
            .map_err(|_| PrepareLibrariesIntentError::Projection)?;
        let kind = component_kind(entry.kind())?;
        let (sha1, size) = sha1_integrity(entry.integrity())?;
        rows.push(LibrariesProjectionRow {
            inventory_ordinal: u32::try_from(projected.inventory_ordinal())
                .map_err(|_| PrepareLibrariesIntentError::Projection)?,
            path: relative_path,
            kind,
            size,
            sha1,
        });
    }
    Ok(rows)
}

fn index_sparse_sources<S>(
    projection: &[LibrariesProjectionRow],
    sources: Vec<S>,
) -> Result<BTreeMap<ArtifactRelativePath, S>, PrepareLibrariesIntentError>
where
    S: RetainedLibrariesPublicationSource,
{
    let mut projected = BTreeMap::new();
    for row in projection {
        if projected.insert(row.path.clone(), row).is_some() {
            return Err(PrepareLibrariesIntentError::Projection);
        }
    }
    let mut indexed = BTreeMap::new();
    for source in sources {
        let relative_path = source.relative_path().clone();
        let expected = projected
            .get(&relative_path)
            .ok_or(PrepareLibrariesIntentError::SourceSet)?;
        if source.kind() != expected.kind
            || source.observed_size() != expected.size
            || source.observed_sha1() != expected.sha1
        {
            return Err(PrepareLibrariesIntentError::SourceSet);
        }
        if indexed.insert(relative_path, source).is_some() {
            return Err(PrepareLibrariesIntentError::SourceSet);
        }
    }
    Ok(indexed)
}

fn plan_projection(
    lease: &ManagedRootPublicationLease,
    projection: Vec<LibrariesProjectionRow>,
) -> Result<Vec<ComponentTableRow>, PrepareLibrariesIntentError> {
    let mut rows = Vec::new();
    rows.try_reserve_exact(projection.len())
        .map_err(|_| PrepareLibrariesIntentError::SourceSet)?;
    for projected in projection {
        let path_plan = plan_component_canonical_path(
            lease.root(),
            ManagedComponentKind::Libraries,
            &projected.path,
        )?;
        let first_created_depth = path_plan.first_created_depth();
        let prior = match path_plan.observe()? {
            ComponentCanonicalObservation::Absent => None,
            ComponentCanonicalObservation::Regular(observed) => Some(ComponentPriorFile {
                size: observed.size(),
                sha1: observed.sha1(),
            }),
        };
        rows.push(ComponentTableRow {
            inventory_ordinal: projected.inventory_ordinal,
            final_size: projected.size,
            final_sha1: projected.sha1,
            kind: projected.kind,
            path: projected.path,
            first_created_depth,
            prior,
        });
    }
    lease.revalidate()?;
    Ok(rows)
}

fn validate_sparse_source_coverage<S>(
    planned: &[ComponentTableRow],
    sources: &BTreeMap<ArtifactRelativePath, S>,
) -> Result<SparseSourceCounts, PrepareLibrariesIntentError>
where
    S: RetainedLibrariesPublicationSource,
{
    let mut counts = SparseSourceCounts {
        required: 0,
        supplied_exact: 0,
    };
    for row in planned {
        if row.prior_is_final() {
            if sources.contains_key(&row.path) {
                counts.supplied_exact += 1;
            }
        } else {
            if !sources.contains_key(&row.path) {
                return Err(PrepareLibrariesIntentError::SourceSet);
            }
            counts.required += 1;
        }
    }
    if sources.len() != counts.required + counts.supplied_exact {
        return Err(PrepareLibrariesIntentError::SourceSet);
    }
    Ok(counts)
}

fn component_kind(
    kind: KnownGoodArtifactKind,
) -> Result<ManagedComponentArtifactKind, PrepareLibrariesIntentError> {
    match kind {
        KnownGoodArtifactKind::Library => Ok(ManagedComponentArtifactKind::Library),
        KnownGoodArtifactKind::NativeLibrary => Ok(ManagedComponentArtifactKind::NativeLibrary),
        KnownGoodArtifactKind::VersionMetadata
        | KnownGoodArtifactKind::ClientJar
        | KnownGoodArtifactKind::AssetIndex
        | KnownGoodArtifactKind::AssetObject
        | KnownGoodArtifactKind::LogConfig
        | KnownGoodArtifactKind::RuntimeManifestProof
        | KnownGoodArtifactKind::RuntimeReadyMarker
        | KnownGoodArtifactKind::RuntimeFile
        | KnownGoodArtifactKind::RuntimeExecutable
        | KnownGoodArtifactKind::RuntimeDirectory
        | KnownGoodArtifactKind::RuntimeLink => Err(PrepareLibrariesIntentError::Projection),
    }
}

fn sha1_integrity(
    integrity: &KnownGoodIntegrity,
) -> Result<([u8; 20], u64), PrepareLibrariesIntentError> {
    match integrity {
        KnownGoodIntegrity::Sha1 { digest, size } => Ok((digest.to_bytes(), *size)),
        KnownGoodIntegrity::ExactBytes { .. }
        | KnownGoodIntegrity::Directory
        | KnownGoodIntegrity::LinkTarget(_) => Err(PrepareLibrariesIntentError::Projection),
    }
}

fn validate_table_summary(
    built: &ComponentTableSummary,
    durable: &ComponentTableSummary,
    durable_shards: usize,
    manifest: &ComponentIntentManifest,
) -> Result<(), PrepareLibrariesIntentError> {
    if built != durable || durable_shards != manifest.shards.len() {
        return Err(PrepareLibrariesIntentError::TableSummary);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::managed_component_publication::COMPONENT_INTENT_FILE;
    use crate::managed_component_table::decode_component_table_shard;
    use sha1::{Digest as _, Sha1};
    use std::fs;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct TestSourceEvents {
        staged: AtomicUsize,
        dropped: AtomicUsize,
    }

    #[derive(Clone)]
    struct TestSource {
        identity: LibrariesPublicationSourceIdentity,
        bytes: Vec<u8>,
        events: Option<Arc<TestSourceEvents>>,
    }

    impl Drop for TestSource {
        fn drop(&mut self) {
            if let Some(events) = &self.events {
                events.dropped.fetch_add(1, Ordering::SeqCst);
            }
        }
    }

    impl RetainedLibrariesPublicationSource for TestSource {
        fn relative_path(&self) -> &ArtifactRelativePath {
            &self.identity.relative_path
        }

        fn kind(&self) -> ManagedComponentArtifactKind {
            self.identity.kind
        }

        fn observed_size(&self) -> u64 {
            self.identity.size
        }

        fn observed_sha1(&self) -> [u8; 20] {
            self.identity.sha1
        }

        async fn stage_create_new(
            self,
            staging_bucket: &ManagedDir,
            slot: &str,
            lifetime_guard: ManagedPublicationLifetimeGuard,
        ) -> Result<LibrariesPublicationSourceIdentity, LoaderError> {
            let _lifetime_guard = lifetime_guard;
            if let Some(events) = &self.events {
                events.staged.fetch_add(1, Ordering::SeqCst);
            }
            staging_bucket.write_new_exact(slot, &self.bytes)?;
            Ok(self.identity.clone())
        }
    }

    fn test_source(
        path: &str,
        kind: ManagedComponentArtifactKind,
        bytes: impl Into<Vec<u8>>,
    ) -> TestSource {
        let bytes = bytes.into();
        TestSource {
            identity: LibrariesPublicationSourceIdentity::new(
                ArtifactRelativePath::new(path).expect("test source path"),
                kind,
                u64::try_from(bytes.len()).expect("test source size"),
                Sha1::digest(&bytes).into(),
            ),
            bytes,
            events: None,
        }
    }

    fn tracked_test_source(
        path: &str,
        kind: ManagedComponentArtifactKind,
        bytes: impl Into<Vec<u8>>,
    ) -> (TestSource, Arc<TestSourceEvents>) {
        let mut source = test_source(path, kind, bytes);
        let events = Arc::new(TestSourceEvents::default());
        source.events = Some(Arc::clone(&events));
        (source, events)
    }

    fn test_authority(sources: &[TestSource]) -> PendingKnownGoodInstallAuthority {
        PendingKnownGoodInstallAuthority::libraries_for_test(sources.iter().map(|source| {
            (
                source.identity.relative_path.as_str().to_string(),
                match source.identity.kind {
                    ManagedComponentArtifactKind::Library => KnownGoodArtifactKind::Library,
                    ManagedComponentArtifactKind::NativeLibrary => {
                        KnownGoodArtifactKind::NativeLibrary
                    }
                    ManagedComponentArtifactKind::AssetIndex
                    | ManagedComponentArtifactKind::AssetObject => {
                        panic!("test Libraries source kind")
                    }
                },
                source.identity.sha1,
                source.identity.size,
            )
        }))
    }

    async fn test_lease(temporary: &tempfile::TempDir) -> ManagedRootPublicationLease {
        let root = ManagedDir::open_root(temporary.path()).expect("test managed root");
        ManagedRootPublicationLease::acquire(root)
            .await
            .expect("test publication lease")
    }

    #[tokio::test]
    async fn sparse_sources_keep_projection_slots_across_shard_boundary() {
        let temporary = tempfile::tempdir().expect("test root");
        let expected = (0..257)
            .map(|index| {
                test_source(
                    &format!("org/example/{index:03}.jar"),
                    if index % 2 == 0 {
                        ManagedComponentArtifactKind::Library
                    } else {
                        ManagedComponentArtifactKind::NativeLibrary
                    },
                    format!("source-{index:03}").into_bytes(),
                )
            })
            .collect::<Vec<_>>();
        for source in &expected[..255] {
            let canonical = source
                .identity
                .relative_path
                .join_under(&temporary.path().join("libraries"));
            fs::create_dir_all(canonical.parent().unwrap()).unwrap();
            fs::write(canonical, &source.bytes).unwrap();
        }
        let authority = test_authority(&expected);
        let sparse = vec![expected[256].clone(), expected[255].clone()];
        let candidate = prepare_libraries_intent(test_lease(&temporary).await, authority, sparse)
            .await
            .expect("prepared sparse Libraries intent candidate");
        let lane = temporary.path().join(".axial-publication/libraries");

        assert!(!lane.join("staging/000000/000").exists());
        assert!(lane.join("staging/000000/255").is_file());
        assert!(lane.join("staging/000001/000").is_file());
        assert_eq!(
            fs::read_dir(lane.join("staging/000000")).unwrap().count(),
            1
        );
        assert_eq!(
            fs::read_dir(lane.join("staging/000001")).unwrap().count(),
            1
        );
        assert!(lane.join("table/000000.tbl").is_file());
        assert!(lane.join("table/000001.tbl").is_file());
        assert!(!lane.join(COMPONENT_INTENT_FILE).exists());

        let prepared = candidate
            .publish_intent()
            .expect("durable Libraries intent");
        assert!(lane.join(COMPONENT_INTENT_FILE).is_file());
        drop(prepared);
    }

    #[tokio::test]
    async fn supplied_exact_source_is_dropped_while_replacement_is_staged() {
        let temporary = tempfile::tempdir().expect("test root");
        let (exact, exact_events) = tracked_test_source(
            "org/example/a.jar",
            ManagedComponentArtifactKind::Library,
            b"exact-prior".to_vec(),
        );
        let (replacement, replacement_events) = tracked_test_source(
            "org/example/b.jar",
            ManagedComponentArtifactKind::NativeLibrary,
            b"replacement".to_vec(),
        );
        fs::create_dir_all(temporary.path().join("libraries/org/example")).unwrap();
        fs::write(
            temporary.path().join("libraries/org/example/a.jar"),
            &exact.bytes,
        )
        .unwrap();
        fs::write(
            temporary.path().join("libraries/org/example/b.jar"),
            b"wrong-prior",
        )
        .unwrap();
        let sources = vec![exact, replacement];
        let authority = test_authority(&sources);
        let candidate = prepare_libraries_intent(test_lease(&temporary).await, authority, sources)
            .await
            .expect("prepared mixed-prior candidate");
        let lane = temporary.path().join(".axial-publication/libraries");
        let staging = lane.join("staging/000000");
        let quarantine = lane.join("quarantine/000000");
        let shard = decode_component_table_shard(
            &fs::read(lane.join("table/000000.tbl")).expect("durable table shard"),
        )
        .expect("decoded table shard");

        assert!(!staging.join("000").exists());
        assert!(staging.join("001").is_file());
        assert_eq!(fs::read_dir(quarantine).unwrap().count(), 0);
        assert!(shard.rows[0].prior_is_final());
        assert!(!shard.rows[1].prior_is_final());
        assert_eq!(shard.rows[0].first_created_depth, None);
        assert_eq!(shard.rows[1].first_created_depth, None);
        assert_eq!(exact_events.staged.load(Ordering::SeqCst), 0);
        assert_eq!(exact_events.dropped.load(Ordering::SeqCst), 1);
        assert_eq!(replacement_events.staged.load(Ordering::SeqCst), 1);
        assert_eq!(replacement_events.dropped.load(Ordering::SeqCst), 1);
        drop(candidate);
    }

    #[tokio::test]
    async fn exact_inherited_projection_prepares_with_zero_sources() {
        let temporary = tempfile::tempdir().expect("test root");
        let expected = vec![
            test_source(
                "org/example/a.jar",
                ManagedComponentArtifactKind::Library,
                b"exact-a".to_vec(),
            ),
            test_source(
                "org/example/b.jar",
                ManagedComponentArtifactKind::NativeLibrary,
                b"exact-b".to_vec(),
            ),
        ];
        for source in &expected {
            let canonical = source
                .identity
                .relative_path
                .join_under(&temporary.path().join("libraries"));
            fs::create_dir_all(canonical.parent().unwrap()).unwrap();
            fs::write(canonical, &source.bytes).unwrap();
        }
        let authority = test_authority(&expected);

        let candidate = prepare_libraries_intent::<TestSource>(
            test_lease(&temporary).await,
            authority,
            Vec::new(),
        )
        .await
        .expect("exact inherited rows need no retained sources");
        let lane = temporary.path().join(".axial-publication/libraries");
        let shard = decode_component_table_shard(
            &fs::read(lane.join("table/000000.tbl")).expect("durable table shard"),
        )
        .expect("decoded table shard");

        assert!(
            fs::read_dir(lane.join("staging/000000"))
                .unwrap()
                .next()
                .is_none()
        );
        assert!(shard.rows.iter().all(ComponentTableRow::prior_is_final));
        drop(candidate);
    }

    #[tokio::test]
    async fn missing_non_exact_source_is_rejected_before_lane_creation() {
        let temporary = tempfile::tempdir().expect("test root");
        let expected = vec![test_source(
            "org/example/missing.jar",
            ManagedComponentArtifactKind::Library,
            b"required-source".to_vec(),
        )];
        let authority = test_authority(&expected);

        let error = match prepare_libraries_intent::<TestSource>(
            test_lease(&temporary).await,
            authority,
            Vec::new(),
        )
        .await
        {
            Err(error) => error,
            Ok(_) => panic!("missing non-exact source was accepted"),
        };

        assert!(matches!(error, PrepareLibrariesIntentError::SourceSet));
        assert!(
            !temporary
                .path()
                .join(".axial-publication/libraries")
                .exists()
        );
    }

    #[tokio::test]
    async fn sparse_mixed_projection_stages_only_non_exact_sources() {
        let temporary = tempfile::tempdir().expect("test root");
        let expected = vec![
            test_source(
                "org/example/a.jar",
                ManagedComponentArtifactKind::Library,
                b"exact-a".to_vec(),
            ),
            test_source(
                "org/example/b.jar",
                ManagedComponentArtifactKind::NativeLibrary,
                b"replacement-b".to_vec(),
            ),
            test_source(
                "org/example/c.jar",
                ManagedComponentArtifactKind::Library,
                b"missing-c".to_vec(),
            ),
        ];
        let libraries = temporary.path().join("libraries/org/example");
        fs::create_dir_all(&libraries).unwrap();
        fs::write(libraries.join("a.jar"), &expected[0].bytes).unwrap();
        fs::write(libraries.join("b.jar"), b"wrong-b").unwrap();
        let authority = test_authority(&expected);
        let sparse = vec![expected[2].clone(), expected[1].clone()];

        let candidate = prepare_libraries_intent(test_lease(&temporary).await, authority, sparse)
            .await
            .expect("sparse mixed projection");
        let lane = temporary.path().join(".axial-publication/libraries");
        let staging = lane.join("staging/000000");
        let shard = decode_component_table_shard(
            &fs::read(lane.join("table/000000.tbl")).expect("durable table shard"),
        )
        .expect("decoded table shard");

        assert!(!staging.join("000").exists());
        assert_eq!(fs::read(staging.join("001")).unwrap(), b"replacement-b");
        assert_eq!(fs::read(staging.join("002")).unwrap(), b"missing-c");
        assert!(shard.rows[0].prior_is_final());
        assert!(!shard.rows[1].prior_is_final());
        assert!(shard.rows[2].prior.is_none());
        drop(candidate);
    }

    #[tokio::test]
    async fn empty_projection_prepares_zero_shards_and_buckets() {
        let temporary = tempfile::tempdir().expect("test root");
        let authority = PendingKnownGoodInstallAuthority::libraries_for_test([]);
        let candidate = prepare_libraries_intent::<TestSource>(
            test_lease(&temporary).await,
            authority,
            Vec::new(),
        )
        .await
        .expect("empty Libraries candidate");
        let lane = temporary.path().join(".axial-publication/libraries");

        assert_eq!(fs::read_dir(lane.join("table")).unwrap().count(), 0);
        assert_eq!(fs::read_dir(lane.join("staging")).unwrap().count(), 0);
        assert_eq!(fs::read_dir(lane.join("quarantine")).unwrap().count(), 0);
        let prepared = candidate.publish_intent().expect("empty durable intent");
        assert!(lane.join(COMPONENT_INTENT_FILE).is_file());
        drop(prepared);
    }

    #[tokio::test]
    async fn duplicate_foreign_and_mismatched_sources_are_preeffect() {
        enum Mutation {
            Missing,
            Extra,
            Duplicate,
            Kind,
            Size,
            Sha1,
        }
        for mutation in [
            Mutation::Missing,
            Mutation::Extra,
            Mutation::Duplicate,
            Mutation::Kind,
            Mutation::Size,
            Mutation::Sha1,
        ] {
            let temporary = tempfile::tempdir().expect("test root");
            let expected = vec![
                test_source(
                    "org/example/a.jar",
                    ManagedComponentArtifactKind::Library,
                    b"source-a".to_vec(),
                ),
                test_source(
                    "org/example/b.jar",
                    ManagedComponentArtifactKind::NativeLibrary,
                    b"source-b".to_vec(),
                ),
            ];
            let authority = test_authority(&expected);
            let mut sources = expected;
            match mutation {
                Mutation::Missing => {
                    sources.pop();
                }
                Mutation::Extra => sources.push(test_source(
                    "org/example/c.jar",
                    ManagedComponentArtifactKind::Library,
                    b"source-c".to_vec(),
                )),
                Mutation::Duplicate => {
                    sources[1] = sources[0].clone();
                }
                Mutation::Kind => {
                    sources[0].identity.kind = ManagedComponentArtifactKind::NativeLibrary;
                }
                Mutation::Size => {
                    sources[0].identity.size += 1;
                }
                Mutation::Sha1 => {
                    sources[0].identity.sha1[0] ^= 0xff;
                }
            }

            let error =
                match prepare_libraries_intent(test_lease(&temporary).await, authority, sources)
                    .await
                {
                    Err(error) => error,
                    Ok(_) => panic!("source bijection mismatch must fail"),
                };
            assert!(matches!(error, PrepareLibrariesIntentError::SourceSet));
            assert!(
                !temporary
                    .path()
                    .join(".axial-publication/libraries")
                    .exists()
            );
        }
    }
}
