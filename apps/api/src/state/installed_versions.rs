use super::{
    IntegrityForegroundLease, ProducerLease,
    managed_library::{LibraryGenerationId, LibraryOperation},
};
use axial_minecraft::{
    VersionScanDependencyStamp, VersionScanIssue, VersionScanIssueKind, VersionScanReport,
    VersionScanState, scan_versions_snapshot,
};
use std::{
    path::Path,
    sync::{Arc, Mutex},
};
use tokio::sync::watch;

const INDEX_LOCK_INVARIANT: &str =
    "installed versions index lock poisoned; cached scan state may be inconsistent";
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InstalledVersionsLookupSource {
    Hit,
    Refreshed,
    Coalesced,
    Unavailable,
}

impl InstalledVersionsLookupSource {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Hit => "hit",
            Self::Refreshed => "refreshed",
            Self::Coalesced => "coalesced",
            Self::Unavailable => "unavailable",
        }
    }
}

#[derive(Clone)]
pub(crate) struct InstalledVersionsSnapshot {
    report: Arc<VersionScanReport>,
}

impl InstalledVersionsSnapshot {
    pub(crate) fn report(&self) -> &VersionScanReport {
        self.report.as_ref()
    }
}

pub(crate) struct InstalledVersionsLookup {
    pub(crate) snapshot: InstalledVersionsSnapshot,
    pub(crate) source: InstalledVersionsLookupSource,
    pub(crate) refresh_count: u32,
    retry_recommended: bool,
    operation: LibraryOperation,
}

impl InstalledVersionsLookup {
    pub(crate) fn library_dir(&self) -> &Path {
        self.operation.configured_path()
    }

    pub(super) fn operation(&self) -> &LibraryOperation {
        &self.operation
    }

    pub(super) const fn retry_recommended(&self) -> bool {
        self.retry_recommended
    }

    pub(super) fn add_refreshes(&mut self, previous: u32) {
        self.refresh_count = self.refresh_count.saturating_add(previous);
    }
}

#[derive(Clone, Eq, PartialEq)]
struct RefreshKey {
    library_generation: LibraryGenerationId,
    index_generation: u64,
}

struct CachedSnapshot {
    key: RefreshKey,
    revision: u64,
    snapshot: InstalledVersionsSnapshot,
    validation: VersionScanDependencyStamp,
}

#[derive(Clone)]
enum RefreshCompletion {
    Ready {
        key: RefreshKey,
        snapshot: InstalledVersionsSnapshot,
        cacheable: bool,
    },
    Retry {
        key: RefreshKey,
    },
}

struct InFlightRefresh {
    key: RefreshKey,
    completed: watch::Sender<Option<RefreshCompletion>>,
}

struct RefreshOwnerGuard {
    index: Arc<InstalledVersionsIndex>,
    key: RefreshKey,
    completed: watch::Sender<Option<RefreshCompletion>>,
    foreground: IntegrityForegroundLease,
    armed: bool,
}

#[derive(Default)]
struct IndexState {
    generation: u64,
    cache_revision: u64,
    cached: Option<CachedSnapshot>,
    in_flight: Option<InFlightRefresh>,
}

#[derive(Default)]
pub(crate) struct InstalledVersionsIndex {
    state: Mutex<IndexState>,
    #[cfg(test)]
    walk_count: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    forced_refresh_retries: std::sync::atomic::AtomicUsize,
}

impl InstalledVersionsIndex {
    pub(super) async fn lookup(
        self: &Arc<Self>,
        operation: LibraryOperation,
        producer: &ProducerLease,
        foreground: IntegrityForegroundLease,
    ) -> InstalledVersionsLookup {
        if let Some(hit) = self
            .validated_hit(operation.clone(), producer, &foreground)
            .await
        {
            return hit;
        }

        let claim = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|_| panic!("{INDEX_LOCK_INVARIANT}"));
            let key = RefreshKey {
                library_generation: operation.generation(),
                index_generation: state.generation,
            };
            if let Some(refresh) = state.in_flight.as_ref() {
                if refresh.key == key {
                    RefreshClaim::Wait {
                        expected: key,
                        receiver: refresh.completed.subscribe(),
                    }
                } else {
                    RefreshClaim::Handoff {
                        expected: key,
                        receiver: refresh.completed.subscribe(),
                    }
                }
            } else {
                let (completed, _) = watch::channel(None);
                state.in_flight = Some(InFlightRefresh {
                    key: key.clone(),
                    completed: completed.clone(),
                });
                RefreshClaim::Own {
                    key,
                    receiver: completed.subscribe(),
                    completed,
                    producer: producer.claim_child(),
                    foreground: foreground.retained(),
                }
            }
        };

        let (completion, coalesced, refresh_count) = match claim {
            RefreshClaim::Wait {
                expected,
                mut receiver,
            } => {
                let completion = wait_for_refresh(&mut receiver, &expected).await;
                let counted = matches!(
                    &completion,
                    RefreshCompletion::Ready { key, .. } if key == &expected
                ) || matches!(&completion, RefreshCompletion::Retry { key } if key == &expected);
                (completion, true, u32::from(counted))
            }
            RefreshClaim::Handoff {
                expected,
                mut receiver,
            } => {
                let _ = wait_for_refresh(&mut receiver, &expected).await;
                (
                    RefreshCompletion::Retry { key: expected },
                    false,
                    0,
                )
            }
            RefreshClaim::Own {
                key,
                completed,
                mut receiver,
                producer,
                foreground,
            } => {
                self.spawn_refresh(
                    key.clone(),
                    operation.clone(),
                    completed,
                    producer,
                    foreground,
                );
                (wait_for_refresh(&mut receiver, &key).await, false, 1)
            }
        };

        match completion {
            RefreshCompletion::Ready {
                key,
                snapshot,
                cacheable,
            } if self.refresh_key_is_current(&key, &operation) => InstalledVersionsLookup {
                snapshot,
                source: if !cacheable {
                    InstalledVersionsLookupSource::Unavailable
                } else if coalesced {
                    InstalledVersionsLookupSource::Coalesced
                } else {
                    InstalledVersionsLookupSource::Refreshed
                },
                refresh_count,
                retry_recommended: false,
                operation,
            },
            RefreshCompletion::Ready { .. } | RefreshCompletion::Retry { .. } => {
                InstalledVersionsLookup {
                    snapshot: degraded_snapshot(),
                    source: InstalledVersionsLookupSource::Unavailable,
                    refresh_count,
                    retry_recommended: true,
                    operation,
                }
            }
        }
    }

    pub(crate) fn invalidate(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|_| panic!("{INDEX_LOCK_INVARIANT}"));
        state.generation = state.generation.wrapping_add(1);
        state.cached = None;
    }

    fn refresh_key_is_current(&self, key: &RefreshKey, operation: &LibraryOperation) -> bool {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|_| panic!("{INDEX_LOCK_INVARIANT}"));
        key.library_generation == operation.generation()
            && key.index_generation == state.generation
    }

    async fn validated_hit(
        &self,
        operation: LibraryOperation,
        producer: &ProducerLease,
        foreground: &IntegrityForegroundLease,
    ) -> Option<InstalledVersionsLookup> {
        let candidate = {
            let state = self
                .state
                .lock()
                .unwrap_or_else(|_| panic!("{INDEX_LOCK_INVARIANT}"));
            let cached = state.cached.as_ref()?;
            (cached.key.library_generation == operation.generation()
                && cached.key.index_generation == state.generation)
                .then(|| {
                    (
                        cached.key.clone(),
                        cached.revision,
                        cached.snapshot.clone(),
                        cached.validation.clone(),
                    )
                })
        }?;
        let (key, revision, snapshot, validation) = candidate;
        if !revalidate_owned(validation.clone(), producer, foreground.retained()).await {
            return None;
        }
        self.cached_revision_is_current(&key, revision)
            .then(|| InstalledVersionsLookup {
                snapshot,
                source: InstalledVersionsLookupSource::Hit,
                refresh_count: 0,
                retry_recommended: false,
                operation,
            })
    }

    fn cached_revision_is_current(&self, key: &RefreshKey, revision: u64) -> bool {
        {
            let state = self
                .state
                .lock()
                .unwrap_or_else(|_| panic!("{INDEX_LOCK_INVARIANT}"));
            state.cached.as_ref().is_some_and(|cached| {
                cached.key == *key
                    && cached.revision == revision
                    && state.generation == key.index_generation
            })
        }
    }

    fn spawn_refresh(
        self: &Arc<Self>,
        key: RefreshKey,
        operation: LibraryOperation,
        completed: watch::Sender<Option<RefreshCompletion>>,
        producer: ProducerLease,
        foreground: IntegrityForegroundLease,
    ) {
        let index = self.clone();
        #[cfg(test)]
        self.walk_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut owner = RefreshOwnerGuard {
            index: index.clone(),
            key: key.clone(),
            completed: completed.clone(),
            foreground,
            armed: true,
        };
        let scan_foreground = owner.foreground.retained();
        producer.spawn(async move {
            let scanned = tokio::task::spawn_blocking(move || {
                let _foreground = scan_foreground;
                scan_with_validation(&operation)
            })
            .await
            .ok()
            .flatten();
            let Some((snapshot, validation)) = scanned else {
                index.finish_refresh(
                    &key,
                    &completed,
                    RefreshResult::Unavailable(degraded_snapshot()),
                );
                owner.disarm();
                return;
            };
            let revalidated = {
                let validation = validation.clone();
                let revalidation_foreground = owner.foreground.retained();
                tokio::task::spawn_blocking(move || {
                    let _foreground = revalidation_foreground;
                    validation.is_revalidated()
                })
                .await
            };
            let result = match revalidated {
                Ok(true) if !index.force_retry_for_test() => RefreshResult::Stable {
                    snapshot,
                    validation,
                },
                Ok(true) => RefreshResult::Retry,
                Ok(false) => RefreshResult::Retry,
                Err(_) => RefreshResult::Unavailable(snapshot),
            };
            index.finish_refresh(&key, &completed, result);
            owner.disarm();
        });
    }

    fn finish_refresh(
        &self,
        key: &RefreshKey,
        completed: &watch::Sender<Option<RefreshCompletion>>,
        result: RefreshResult,
    ) {
        let completion = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|_| panic!("{INDEX_LOCK_INVARIANT}"));
            let current = state.generation == key.index_generation
                && state
                    .in_flight
                    .as_ref()
                    .is_some_and(|refresh| refresh.key == *key);
            if !current {
                if state
                    .in_flight
                    .as_ref()
                    .is_some_and(|refresh| refresh.key == *key)
                {
                    state.in_flight = None;
                }
                RefreshCompletion::Retry { key: key.clone() }
            } else {
                state.in_flight = None;
                match result {
                    RefreshResult::Stable {
                        snapshot,
                        validation,
                    } => {
                        let revision = state
                            .cache_revision
                            .checked_add(1)
                            .expect("installed versions cache revision overflowed");
                        state.cache_revision = revision;
                        state.cached = Some(CachedSnapshot {
                            key: key.clone(),
                            revision,
                            snapshot: snapshot.clone(),
                            validation,
                        });
                        RefreshCompletion::Ready {
                            key: key.clone(),
                            snapshot,
                            cacheable: true,
                        }
                    }
                    RefreshResult::Unavailable(snapshot) => RefreshCompletion::Ready {
                        key: key.clone(),
                        snapshot,
                        cacheable: false,
                    },
                    RefreshResult::Retry => RefreshCompletion::Retry { key: key.clone() },
                }
            }
        };
        completed.send_replace(Some(completion));
    }

    #[cfg(test)]
    pub(crate) fn walk_count(&self) -> usize {
        self.walk_count.load(std::sync::atomic::Ordering::Relaxed)
    }

    #[cfg(test)]
    fn force_retry_for_test(&self) -> bool {
        self.forced_refresh_retries
            .fetch_update(
                std::sync::atomic::Ordering::Relaxed,
                std::sync::atomic::Ordering::Relaxed,
                |remaining| remaining.checked_sub(1),
            )
            .is_ok()
    }

    #[cfg(not(test))]
    const fn force_retry_for_test(&self) -> bool {
        false
    }
}

enum RefreshClaim {
    Wait {
        expected: RefreshKey,
        receiver: watch::Receiver<Option<RefreshCompletion>>,
    },
    Handoff {
        expected: RefreshKey,
        receiver: watch::Receiver<Option<RefreshCompletion>>,
    },
    Own {
        key: RefreshKey,
        completed: watch::Sender<Option<RefreshCompletion>>,
        receiver: watch::Receiver<Option<RefreshCompletion>>,
        producer: ProducerLease,
        foreground: IntegrityForegroundLease,
    },
}

enum RefreshResult {
    Stable {
        snapshot: InstalledVersionsSnapshot,
        validation: VersionScanDependencyStamp,
    },
    Unavailable(InstalledVersionsSnapshot),
    Retry,
}

impl RefreshOwnerGuard {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for RefreshOwnerGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let mut state = self
            .index
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if state
            .in_flight
            .as_ref()
            .is_some_and(|refresh| refresh.key == self.key)
        {
            state.in_flight = None;
        }
        self.completed.send_replace(Some(RefreshCompletion::Retry {
            key: self.key.clone(),
        }));
    }
}

async fn wait_for_refresh(
    completed: &mut watch::Receiver<Option<RefreshCompletion>>,
    expected: &RefreshKey,
) -> RefreshCompletion {
    loop {
        if let Some(completion) = completed.borrow_and_update().clone() {
            return completion;
        }
        if completed.changed().await.is_err() {
            return RefreshCompletion::Retry {
                key: expected.clone(),
            };
        }
    }
}

async fn revalidate_owned(
    validation: VersionScanDependencyStamp,
    producer: &ProducerLease,
    foreground: IntegrityForegroundLease,
) -> bool {
    let blocking_foreground = foreground.retained();
    producer
        .claim_child()
        .spawn_joinable(async move {
            let _foreground = foreground;
            tokio::task::spawn_blocking(move || {
                let _foreground = blocking_foreground;
                validation.is_revalidated()
            })
            .await
            .unwrap_or(false)
        })
        .await
        .unwrap_or(false)
}

fn scan_with_validation(
    operation: &LibraryOperation,
) -> Option<(InstalledVersionsSnapshot, VersionScanDependencyStamp)> {
    let scanned = scan_versions_snapshot(operation.core()).ok()?;
    let validation = scanned.dependencies().clone();
    Some((
        InstalledVersionsSnapshot {
            report: Arc::new(scanned.report),
        },
        validation,
    ))
}

fn degraded_snapshot() -> InstalledVersionsSnapshot {
    InstalledVersionsSnapshot {
        report: Arc::new(degraded_report()),
    }
}

fn degraded_report() -> VersionScanReport {
    VersionScanReport {
        state: VersionScanState::Degraded,
        versions: Vec::new(),
        issues: vec![VersionScanIssue {
            kind: VersionScanIssueKind::VersionsDirectoryUnreadable,
            version_id: None,
        }],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{
        AppLifecycle,
        integrity_activity::IntegrityActivityCoordinator,
        managed_library::{
            ManagedLibraryCommitOutcome, ManagedLibraryOwner, ManagedLibraryStartup,
            ManagedLibraryStartupSelection,
        },
    };
    use axial_config::{AppConfig, AppPaths, AppRootSession};
    use axial_minecraft::versions_dir;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static TEST_ROOT_SEQUENCE: AtomicUsize = AtomicUsize::new(0);

    fn test_root(label: &str) -> PathBuf {
        let sequence = TEST_ROOT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "axial-installed-versions-{label}-{}-{sequence}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        root
    }

    struct TestLibrary {
        workspace: PathBuf,
        paths: AppPaths,
        root_session: Option<Arc<AppRootSession>>,
        owner: Option<ManagedLibraryOwner>,
        library_dir: PathBuf,
    }

    impl TestLibrary {
        fn new(label: &str) -> Self {
            let workspace = test_root(label);
            let app_root = workspace.join("app");
            std::fs::create_dir_all(&app_root).expect("create test app root");
            let paths = AppPaths::from_root(app_root).expect("test app paths");
            let library_dir = workspace.join("library");
            std::fs::create_dir(&library_dir).expect("create test library root");
            let root_session = Arc::new(paths.open_root_session().expect("test root session"));
            let startup = ManagedLibraryStartup::prepare(
                Arc::clone(&root_session),
                &paths,
                &AppConfig {
                    library_mode: "existing".to_string(),
                    library_dir: library_dir.to_string_lossy().into_owned(),
                    ..AppConfig::default()
                },
            )
            .expect("test library startup");
            let (owner, degraded) = startup.into_parts();
            assert_eq!(degraded, None);
            Self {
                workspace,
                paths,
                root_session: Some(root_session),
                owner: Some(owner),
                library_dir,
            }
        }

        fn root(&self) -> &Path {
            &self.library_dir
        }

        fn operation(&self) -> LibraryOperation {
            self.owner
                .as_ref()
                .expect("test library owner")
                .try_acquire()
                .expect("test library operation")
        }

        async fn replace_with(&mut self, library_dir: PathBuf) {
            std::fs::create_dir(&library_dir).expect("create replacement library root");
            let selection = ManagedLibraryStartupSelection::from_config(
                &AppConfig {
                    library_mode: "existing".to_string(),
                    library_dir: library_dir.to_string_lossy().into_owned(),
                    ..AppConfig::default()
                },
                &self.paths,
            )
            .expect("replacement library selection");
            let prepared = self
                .owner
                .as_ref()
                .expect("test library owner")
                .prepare_change(selection)
                .await
                .expect("prepare replacement library")
                .expect("replacement changes the generation");
            assert_eq!(prepared.commit(), ManagedLibraryCommitOutcome::Ready);
            self.library_dir = library_dir;
        }
    }

    impl Drop for TestLibrary {
        fn drop(&mut self) {
            drop(self.owner.take());
            drop(self.root_session.take());
            let _ = std::fs::remove_dir_all(&self.workspace);
        }
    }

    fn accepted_producer() -> (crate::state::RequestLease, ProducerLease) {
        let lifecycle = AppLifecycle::new();
        let request = lifecycle.try_admit_request().expect("admit request");
        let producer = request
            .producer_handoff()
            .try_claim()
            .expect("claim request producer");
        (request, producer)
    }

    async fn foreground_lease() -> IntegrityForegroundLease {
        IntegrityActivityCoordinator::new()
            .register_foreground()
            .expect("register test scan foreground")
            .wait_for_settlement()
            .await
    }

    async fn test_lookup(
        index: &Arc<InstalledVersionsIndex>,
        library: &TestLibrary,
        producer: &ProducerLease,
    ) -> InstalledVersionsLookup {
        let foreground = foreground_lease().await;
        let mut completed_refreshes = 0;
        for attempt in 0..2 {
            let mut lookup = index
                .lookup(library.operation(), producer, foreground.retained())
                .await;
            lookup.add_refreshes(completed_refreshes);
            completed_refreshes = lookup.refresh_count;
            assert!(
                library
                    .owner
                    .as_ref()
                    .expect("test library owner")
                    .validate_current(lookup.operation())
                    .is_ok()
            );
            if !lookup.retry_recommended() || attempt == 1 {
                return lookup;
            }
        }
        unreachable!("bounded test lookup always returns")
    }

    fn create_empty_library(root: &Path) {
        std::fs::create_dir_all(versions_dir(root)).expect("create versions root");
    }

    fn write_version(root: &Path, id: &str) {
        let version_dir = versions_dir(root).join(id);
        std::fs::create_dir_all(&version_dir).expect("create version directory");
        std::fs::write(
            version_dir.join(format!("{id}.json")),
            br#"{"type":"release"}"#,
        )
        .expect("write version metadata");
        std::fs::write(version_dir.join(format!("{id}.jar")), b"client").expect("write client jar");
    }

    #[tokio::test]
    async fn warm_lookup_hits_without_another_walk() {
        let library = TestLibrary::new("hit");
        create_empty_library(library.root());
        let index = Arc::new(InstalledVersionsIndex::default());
        let (_request, producer) = accepted_producer();

        let first = test_lookup(&index, &library, &producer).await;
        let second = test_lookup(&index, &library, &producer).await;

        assert_eq!(first.source, InstalledVersionsLookupSource::Refreshed);
        assert_eq!(second.source, InstalledVersionsLookupSource::Hit);
        assert_eq!(index.walk_count(), 1);
    }

    #[tokio::test]
    async fn healthy_in_place_metadata_corruption_refreshes_then_degraded_hits() {
        let library = TestLibrary::new("corruption");
        create_empty_library(library.root());
        write_version(library.root(), "1.21.1");
        let index = Arc::new(InstalledVersionsIndex::default());
        let (_request, producer) = accepted_producer();
        let healthy = test_lookup(&index, &library, &producer).await;
        assert_ne!(healthy.snapshot.report().state, VersionScanState::Degraded);

        std::fs::write(
            versions_dir(library.root())
                .join("1.21.1")
                .join("1.21.1.json"),
            b"{malformed",
        )
        .expect("corrupt version metadata");
        let degraded = test_lookup(&index, &library, &producer).await;
        let unchanged = test_lookup(&index, &library, &producer).await;

        assert_eq!(degraded.snapshot.report().state, VersionScanState::Degraded);
        assert_eq!(degraded.source, InstalledVersionsLookupSource::Refreshed);
        assert_eq!(unchanged.source, InstalledVersionsLookupSource::Hit);
        assert_eq!(index.walk_count(), 2);
    }

    #[tokio::test]
    async fn explicit_invalidation_and_path_change_refresh() {
        let mut library = TestLibrary::new("path-change");
        let second_root = library.workspace.join("second-library");
        create_empty_library(library.root());
        write_version(library.root(), "1.20.1");
        let index = Arc::new(InstalledVersionsIndex::default());
        let (_request, producer) = accepted_producer();
        let _ = test_lookup(&index, &library, &producer).await;
        index.invalidate();
        let invalidated = test_lookup(&index, &library, &producer).await;

        assert_eq!(invalidated.source, InstalledVersionsLookupSource::Refreshed);
        drop(invalidated);
        library.replace_with(second_root).await;
        create_empty_library(library.root());
        write_version(library.root(), "1.21.1");
        let changed = test_lookup(&index, &library, &producer).await;
        assert_eq!(changed.snapshot.report().versions[0].id, "1.21.1");
        assert_eq!(index.walk_count(), 3);
    }

    #[tokio::test]
    async fn controlled_in_flight_completion_is_coalesced() {
        let library = TestLibrary::new("coalesced");
        create_empty_library(library.root());
        let index = Arc::new(InstalledVersionsIndex::default());
        let (_request, producer) = accepted_producer();
        let key = RefreshKey {
            library_generation: library.operation().generation(),
            index_generation: 0,
        };
        let (completed, _) = watch::channel(None);
        index.state.lock().expect("index state").in_flight = Some(InFlightRefresh {
            key: key.clone(),
            completed: completed.clone(),
        });
        let index_for_completion = index.clone();
        let snapshot = InstalledVersionsSnapshot {
            report: Arc::new(VersionScanReport {
                state: VersionScanState::Empty,
                versions: Vec::new(),
                issues: Vec::new(),
            }),
        };
        tokio::spawn(async move {
            while completed.receiver_count() == 0 {
                tokio::task::yield_now().await;
            }
            index_for_completion
                .state
                .lock()
                .expect("index state")
                .in_flight = None;
            completed.send_replace(Some(RefreshCompletion::Ready {
                key,
                snapshot,
                cacheable: true,
            }));
        });

        let lookup = test_lookup(&index, &library, &producer).await;
        assert_eq!(lookup.source, InstalledVersionsLookupSource::Coalesced);
        assert_eq!(lookup.refresh_count, 1);
        assert_eq!(index.walk_count(), 0);
    }

    #[tokio::test]
    async fn invalidation_after_finish_before_waiter_wake_rejects_ready_snapshot() {
        let library = TestLibrary::new("invalidate-after-finish");
        create_empty_library(library.root());
        let index = Arc::new(InstalledVersionsIndex::default());
        let (_request, producer) = accepted_producer();
        let key = RefreshKey {
            library_generation: library.operation().generation(),
            index_generation: 0,
        };
        let (completed, _) = watch::channel(None);
        index.state.lock().expect("index state").in_flight = Some(InFlightRefresh {
            key: key.clone(),
            completed: completed.clone(),
        });
        let waiter = tokio::spawn({
            let index = index.clone();
            let producer = producer.claim_child();
            let operation = library.operation();
            async move {
                index
                    .lookup(operation, &producer, foreground_lease().await)
                    .await
            }
        });
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while completed.receiver_count() == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("waiter subscribes to the controlled refresh");

        index.finish_refresh(
            &key,
            &completed,
            RefreshResult::Unavailable(degraded_snapshot()),
        );
        index.invalidate();

        let lookup = waiter.await.expect("waiter lookup");
        assert_eq!(lookup.source, InstalledVersionsLookupSource::Unavailable);
        assert!(lookup.retry_recommended());
        assert_eq!(lookup.refresh_count, 1);
        assert_eq!(index.walk_count(), 0);
    }

    #[tokio::test]
    async fn different_generation_waits_for_handoff_before_starting_one_scan() {
        let mut library = TestLibrary::new("generation-handoff");
        create_empty_library(library.root());
        let index = Arc::new(InstalledVersionsIndex::default());
        let (_request, producer) = accepted_producer();
        let old_key = RefreshKey {
            library_generation: library.operation().generation(),
            index_generation: 0,
        };
        let (completed, _) = watch::channel(None);
        index.state.lock().expect("index state").in_flight = Some(InFlightRefresh {
            key: old_key.clone(),
            completed: completed.clone(),
        });

        let replacement = library.workspace.join("replacement-library");
        library.replace_with(replacement).await;
        create_empty_library(library.root());
        let first = tokio::spawn({
            let index = index.clone();
            let producer = producer.claim_child();
            let operation = library.operation();
            async move {
                index
                    .lookup(operation, &producer, foreground_lease().await)
                    .await
            }
        });
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while completed.receiver_count() == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("new generation waits for old refresh handoff");
        index.state.lock().expect("index state").in_flight = None;
        completed.send_replace(Some(RefreshCompletion::Ready {
            key: old_key,
            snapshot: degraded_snapshot(),
            cacheable: false,
        }));

        let handoff = first.await.expect("handoff lookup");
        assert!(handoff.retry_recommended());
        assert_eq!(handoff.refresh_count, 0);
        assert_eq!(index.walk_count(), 0);
        drop(handoff);

        let refreshed = test_lookup(&index, &library, &producer).await;
        assert_eq!(refreshed.source, InstalledVersionsLookupSource::Refreshed);
        assert_eq!(index.walk_count(), 1);
    }

    #[tokio::test]
    async fn returned_lookup_pins_its_generation_until_downstream_use_finishes() {
        let mut library = TestLibrary::new("lookup-generation-pin");
        create_empty_library(library.root());
        let old_path = library.root().to_path_buf();
        let index = Arc::new(InstalledVersionsIndex::default());
        let (_request, producer) = accepted_producer();
        let lookup = test_lookup(&index, &library, &producer).await;

        let replacement = library.workspace.join("replacement-library");
        library.replace_with(replacement.clone()).await;
        assert_eq!(lookup.library_dir(), old_path.as_path());
        assert!(
            library
                .owner
                .as_ref()
                .expect("test library owner")
                .status()
                .retirement_pending
        );
        let owner = library
            .owner
            .as_ref()
            .expect("test library owner")
            .clone();
        let settlement = tokio::spawn(async move { owner.settle_retirement().await });
        tokio::task::yield_now().await;
        assert!(!settlement.is_finished());

        drop(lookup);
        tokio::time::timeout(std::time::Duration::from_secs(1), settlement)
            .await
            .expect("lookup generation retires after release")
            .expect("retirement settlement task")
            .expect("settle lookup generation");
        assert_eq!(library.operation().configured_path(), replacement.as_path());
    }

    #[tokio::test]
    async fn same_key_replacement_invalidates_the_captured_cache_revision() {
        let library = TestLibrary::new("revision");
        create_empty_library(library.root());
        let index = Arc::new(InstalledVersionsIndex::default());
        let (_request, producer) = accepted_producer();
        let _ = test_lookup(&index, &library, &producer).await;

        let (key, old_revision) = {
            let mut state = index.state.lock().expect("index state");
            let cached = state.cached.take().expect("cached snapshot");
            let key = cached.key.clone();
            let old_revision = cached.revision;
            let revision = state
                .cache_revision
                .checked_add(1)
                .expect("test cache revision");
            state.cache_revision = revision;
            state.cached = Some(CachedSnapshot {
                key: cached.key,
                revision,
                snapshot: cached.snapshot,
                validation: cached.validation,
            });
            (key, old_revision)
        };

        assert!(!index.cached_revision_is_current(&key, old_revision));
    }

    #[tokio::test]
    async fn repeated_scan_churn_stops_after_two_refreshes() {
        let library = TestLibrary::new("bounded-churn");
        create_empty_library(library.root());
        let index = Arc::new(InstalledVersionsIndex::default());
        index
            .forced_refresh_retries
            .store(2, std::sync::atomic::Ordering::Relaxed);
        let (_request, producer) = accepted_producer();

        let lookup = test_lookup(&index, &library, &producer).await;

        assert_eq!(lookup.source, InstalledVersionsLookupSource::Unavailable);
        assert_eq!(lookup.refresh_count, 2);
        assert_eq!(lookup.snapshot.report().state, VersionScanState::Degraded);
        assert_eq!(index.walk_count(), 2);
    }

    #[tokio::test]
    async fn request_producer_children_remain_authorized_while_requests_drain() {
        let library = TestLibrary::new("drain");
        create_empty_library(library.root());
        let index = Arc::new(InstalledVersionsIndex::default());
        let lifecycle = AppLifecycle::new();
        let request = lifecycle.try_admit_request().expect("admit request");
        let producer = request
            .producer_handoff()
            .try_claim()
            .expect("claim request producer");
        lifecycle.begin_quiesce();
        tokio::task::yield_now().await;

        let lookup = test_lookup(&index, &library, &producer).await;

        assert_ne!(lookup.snapshot.report().state, VersionScanState::Degraded);
        assert_eq!(index.walk_count(), 1);
        drop(request);
    }

    #[tokio::test]
    async fn abandoned_refresh_owner_releases_waiters_for_retry() {
        let library = TestLibrary::new("owner-drop");
        let index = Arc::new(InstalledVersionsIndex::default());
        let key = RefreshKey {
            library_generation: library.operation().generation(),
            index_generation: 0,
        };
        let foreground = foreground_lease().await;
        let (completed, mut receiver) = watch::channel(None);
        index.state.lock().expect("index state").in_flight = Some(InFlightRefresh {
            key: key.clone(),
            completed: completed.clone(),
        });

        drop(RefreshOwnerGuard {
            index: index.clone(),
            key: key.clone(),
            completed,
            foreground,
            armed: true,
        });

        assert!(index.state.lock().expect("index state").in_flight.is_none());
        assert!(matches!(
            wait_for_refresh(&mut receiver, &key).await,
            RefreshCompletion::Retry { .. }
        ));
    }

    #[tokio::test]
    async fn cancelled_waiter_retains_foreground_through_child_retry_publication() {
        let library = TestLibrary::new("cancelled-waiter-child");
        create_empty_library(library.root());
        let index = Arc::new(InstalledVersionsIndex::default());
        let coordinator = IntegrityActivityCoordinator::new();
        let foreground = coordinator
            .register_foreground()
            .expect("register lookup foreground")
            .wait_for_settlement()
            .await;
        let key = RefreshKey {
            library_generation: library.operation().generation(),
            index_generation: 0,
        };
        let (completed, mut completion) = watch::channel(None);
        index.state.lock().expect("index state").in_flight = Some(InFlightRefresh {
            key: key.clone(),
            completed: completed.clone(),
        });
        let owner = RefreshOwnerGuard {
            index: index.clone(),
            key: key.clone(),
            completed: completed.clone(),
            foreground: foreground.retained(),
            armed: true,
        };
        let (_request, producer) = accepted_producer();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let child = producer.claim_child().spawn_joinable(async move {
            let _ = release_rx.await;
            drop(owner);
        });
        let lookup = tokio::spawn({
            let index = index.clone();
            let lookup_producer = producer.claim_child();
            let operation = library.operation();
            async move {
                index
                    .lookup(operation, &lookup_producer, foreground)
                    .await
            }
        });

        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while completed.receiver_count() < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("lookup subscribes to child completion");
        lookup.abort();
        assert!(matches!(lookup.await, Err(error) if error.is_cancelled()));
        assert!(!coordinator.subscribe_idle().borrow().is_stably_idle());

        let _ = release_tx.send(());
        child.await.expect("refresh child");
        assert!(matches!(
            wait_for_refresh(&mut completion, &key).await,
            RefreshCompletion::Retry { .. }
        ));
        assert!(coordinator.subscribe_idle().borrow().is_stably_idle());
    }
}
