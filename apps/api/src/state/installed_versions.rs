use super::{IntegrityForegroundLease, ProducerLease};
use axial_minecraft::{
    VersionScanDependencyStamp, VersionScanIssue, VersionScanIssueKind, VersionScanReport,
    VersionScanState, scan_versions_snapshot,
};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use tokio::sync::watch;

const INDEX_LOCK_INVARIANT: &str =
    "installed versions index lock poisoned; cached scan state may be inconsistent";
const MAX_REFRESHES_PER_LOOKUP: u32 = 2;

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
    library_dir: PathBuf,
}

impl InstalledVersionsLookup {
    pub(crate) fn library_dir(&self) -> &Path {
        &self.library_dir
    }
}

#[derive(Clone, Eq, PartialEq)]
struct RefreshKey {
    library_dir: PathBuf,
    generation: u64,
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
        library_dir: PathBuf,
        producer: &ProducerLease,
        foreground: IntegrityForegroundLease,
    ) -> InstalledVersionsLookup {
        let mut refresh_count = 0_u32;
        loop {
            if let Some(hit) = self
                .validated_hit(&library_dir, producer, &foreground, refresh_count)
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
                    library_dir: library_dir.clone(),
                    generation: state.generation,
                };
                if let Some(refresh) = state.in_flight.as_ref() {
                    RefreshClaim::Wait {
                        expected: key,
                        receiver: refresh.completed.subscribe(),
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

            let (completion, coalesced) = match claim {
                RefreshClaim::Wait {
                    expected,
                    mut receiver,
                } => {
                    let completion = wait_for_refresh(&mut receiver, &expected).await;
                    let counted = matches!(
                        &completion,
                        RefreshCompletion::Ready { key, .. } if key == &expected
                    ) || matches!(&completion, RefreshCompletion::Retry { key } if key == &expected);
                    if counted {
                        refresh_count = refresh_count.saturating_add(1);
                    }
                    (completion, true)
                }
                RefreshClaim::Own {
                    key,
                    completed,
                    mut receiver,
                    producer,
                    foreground,
                } => {
                    refresh_count = refresh_count.saturating_add(1);
                    self.spawn_refresh(key.clone(), completed, producer, foreground);
                    (wait_for_refresh(&mut receiver, &key).await, false)
                }
            };

            match completion {
                RefreshCompletion::Ready {
                    key,
                    snapshot,
                    cacheable,
                } if key.library_dir == library_dir => {
                    return InstalledVersionsLookup {
                        snapshot,
                        source: if !cacheable {
                            InstalledVersionsLookupSource::Unavailable
                        } else if coalesced {
                            InstalledVersionsLookupSource::Coalesced
                        } else {
                            InstalledVersionsLookupSource::Refreshed
                        },
                        refresh_count,
                        library_dir,
                    };
                }
                RefreshCompletion::Ready { .. } => continue,
                RefreshCompletion::Retry { key }
                    if key.library_dir != library_dir
                        || refresh_count < MAX_REFRESHES_PER_LOOKUP =>
                {
                    continue;
                }
                RefreshCompletion::Retry { .. } => {
                    return InstalledVersionsLookup {
                        snapshot: degraded_snapshot(),
                        source: InstalledVersionsLookupSource::Unavailable,
                        refresh_count,
                        library_dir,
                    };
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

    async fn validated_hit(
        &self,
        library_dir: &Path,
        producer: &ProducerLease,
        foreground: &IntegrityForegroundLease,
        refresh_count: u32,
    ) -> Option<InstalledVersionsLookup> {
        let candidate = {
            let state = self
                .state
                .lock()
                .unwrap_or_else(|_| panic!("{INDEX_LOCK_INVARIANT}"));
            let cached = state.cached.as_ref()?;
            (cached.key.library_dir == library_dir && cached.key.generation == state.generation)
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
                refresh_count,
                library_dir: library_dir.to_path_buf(),
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
                    && state.generation == key.generation
            })
        }
    }

    fn spawn_refresh(
        self: &Arc<Self>,
        key: RefreshKey,
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
            let scan_dir = key.library_dir.clone();
            let scanned = tokio::task::spawn_blocking(move || {
                let _foreground = scan_foreground;
                scan_with_validation(&scan_dir)
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
            let current = state.generation == key.generation
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
    library_dir: &Path,
) -> Option<(InstalledVersionsSnapshot, VersionScanDependencyStamp)> {
    let scanned = scan_versions_snapshot(library_dir).ok()?;
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
    use crate::state::{AppLifecycle, integrity_activity::IntegrityActivityCoordinator};
    use axial_minecraft::versions_dir;
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
        library_dir: PathBuf,
        producer: &ProducerLease,
    ) -> InstalledVersionsLookup {
        index
            .lookup(library_dir, producer, foreground_lease().await)
            .await
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
        let root = test_root("hit");
        create_empty_library(&root);
        let index = Arc::new(InstalledVersionsIndex::default());
        let (_request, producer) = accepted_producer();

        let first = test_lookup(&index, root.clone(), &producer).await;
        let second = test_lookup(&index, root.clone(), &producer).await;

        assert_eq!(first.source, InstalledVersionsLookupSource::Refreshed);
        assert_eq!(second.source, InstalledVersionsLookupSource::Hit);
        assert_eq!(index.walk_count(), 1);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn healthy_in_place_metadata_corruption_refreshes_then_degraded_hits() {
        let root = test_root("corruption");
        create_empty_library(&root);
        write_version(&root, "1.21.1");
        let index = Arc::new(InstalledVersionsIndex::default());
        let (_request, producer) = accepted_producer();
        let healthy = test_lookup(&index, root.clone(), &producer).await;
        assert_ne!(healthy.snapshot.report().state, VersionScanState::Degraded);

        std::fs::write(
            versions_dir(&root).join("1.21.1").join("1.21.1.json"),
            b"{malformed",
        )
        .expect("corrupt version metadata");
        let degraded = test_lookup(&index, root.clone(), &producer).await;
        let unchanged = test_lookup(&index, root.clone(), &producer).await;

        assert_eq!(degraded.snapshot.report().state, VersionScanState::Degraded);
        assert_eq!(degraded.source, InstalledVersionsLookupSource::Refreshed);
        assert_eq!(unchanged.source, InstalledVersionsLookupSource::Hit);
        assert_eq!(index.walk_count(), 2);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn explicit_invalidation_and_path_change_refresh() {
        let first_root = test_root("path-a");
        let second_root = test_root("path-b");
        create_empty_library(&first_root);
        create_empty_library(&second_root);
        write_version(&first_root, "1.20.1");
        write_version(&second_root, "1.21.1");
        let index = Arc::new(InstalledVersionsIndex::default());
        let (_request, producer) = accepted_producer();
        let _ = test_lookup(&index, first_root.clone(), &producer).await;
        index.invalidate();
        let invalidated = test_lookup(&index, first_root.clone(), &producer).await;
        let changed = test_lookup(&index, second_root.clone(), &producer).await;

        assert_eq!(invalidated.source, InstalledVersionsLookupSource::Refreshed);
        assert_eq!(changed.snapshot.report().versions[0].id, "1.21.1");
        assert_eq!(index.walk_count(), 3);
        let _ = std::fs::remove_dir_all(first_root);
        let _ = std::fs::remove_dir_all(second_root);
    }

    #[tokio::test]
    async fn controlled_in_flight_completion_is_coalesced() {
        let root = test_root("coalesced");
        create_empty_library(&root);
        let index = Arc::new(InstalledVersionsIndex::default());
        let (_request, producer) = accepted_producer();
        let key = RefreshKey {
            library_dir: root.clone(),
            generation: 0,
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

        let lookup = test_lookup(&index, root, &producer).await;
        assert_eq!(lookup.source, InstalledVersionsLookupSource::Coalesced);
        assert_eq!(lookup.refresh_count, 1);
        assert_eq!(index.walk_count(), 0);
    }

    #[tokio::test]
    async fn same_key_replacement_invalidates_the_captured_cache_revision() {
        let root = test_root("revision");
        create_empty_library(&root);
        let index = Arc::new(InstalledVersionsIndex::default());
        let (_request, producer) = accepted_producer();
        let _ = test_lookup(&index, root.clone(), &producer).await;

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
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn repeated_scan_churn_stops_after_two_refreshes() {
        let root = test_root("bounded-churn");
        create_empty_library(&root);
        let index = Arc::new(InstalledVersionsIndex::default());
        index
            .forced_refresh_retries
            .store(2, std::sync::atomic::Ordering::Relaxed);
        let (_request, producer) = accepted_producer();

        let lookup = test_lookup(&index, root.clone(), &producer).await;

        assert_eq!(lookup.source, InstalledVersionsLookupSource::Unavailable);
        assert_eq!(lookup.refresh_count, 2);
        assert_eq!(lookup.snapshot.report().state, VersionScanState::Degraded);
        assert_eq!(index.walk_count(), 2);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn request_producer_children_remain_authorized_while_requests_drain() {
        let root = test_root("drain");
        create_empty_library(&root);
        let index = Arc::new(InstalledVersionsIndex::default());
        let lifecycle = AppLifecycle::new();
        let request = lifecycle.try_admit_request().expect("admit request");
        let producer = request
            .producer_handoff()
            .try_claim()
            .expect("claim request producer");
        lifecycle.begin_quiesce();
        tokio::task::yield_now().await;

        let lookup = test_lookup(&index, root.clone(), &producer).await;

        assert_ne!(lookup.snapshot.report().state, VersionScanState::Degraded);
        assert_eq!(index.walk_count(), 1);
        drop(request);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn abandoned_refresh_owner_releases_waiters_for_retry() {
        let index = Arc::new(InstalledVersionsIndex::default());
        let key = RefreshKey {
            library_dir: test_root("owner-drop"),
            generation: 0,
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
        let root = test_root("cancelled-waiter-child");
        create_empty_library(&root);
        let index = Arc::new(InstalledVersionsIndex::default());
        let coordinator = IntegrityActivityCoordinator::new();
        let foreground = coordinator
            .register_foreground()
            .expect("register lookup foreground")
            .wait_for_settlement()
            .await;
        let key = RefreshKey {
            library_dir: root.clone(),
            generation: 0,
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
            let lookup_root = root.clone();
            async move {
                index
                    .lookup(lookup_root, &lookup_producer, foreground)
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
        let _ = std::fs::remove_dir_all(root);
    }
}
