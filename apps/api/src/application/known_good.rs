use crate::state::{AppState, IntegrityForegroundLease, KnownGoodRebuildError, ProducerLease};
use axial_config::INSTANCE_REGISTRY_MAX_ENTRIES;
use axial_minecraft::{KnownGoodReconstructionError, KnownGoodReconstructionReceipt};
use futures_util::{StreamExt, future::join_all, stream};
use std::collections::HashMap;
use std::future::Future;

const MAX_STARTUP_REBUILD_GROUPS: usize = 2;

pub(crate) async fn rebuild_registered_known_good(
    state: &AppState,
    foreground: &IntegrityForegroundLease,
    producer: &ProducerLease,
    instance_id: &str,
) -> Result<(), KnownGoodRebuildError> {
    state
        .rebuild_known_good_for_registered_instance(
            foreground,
            producer,
            instance_id,
            |version_id| async move { axial_minecraft::reconstruct_known_good(&version_id).await },
        )
        .await
}

pub(crate) async fn registered_known_good_is_live(
    state: &AppState,
    foreground: &IntegrityForegroundLease,
    instance_id: &str,
) -> bool {
    state
        .registered_instance_has_live_known_good(foreground, instance_id)
        .await
        .unwrap_or(false)
}

pub(crate) fn spawn_startup_known_good_rebuilds(state: &AppState, producer: ProducerLease) {
    spawn_startup_known_good_rebuilds_with(state, producer, |version_id| async move {
        axial_minecraft::reconstruct_known_good(&version_id).await
    });
}

fn spawn_startup_known_good_rebuilds_with<Reconstruct, ReconstructFuture>(
    state: &AppState,
    producer: ProducerLease,
    reconstruct: Reconstruct,
) where
    Reconstruct: Fn(String) -> ReconstructFuture + Clone + Send + Sync + 'static,
    ReconstructFuture: Future<Output = Result<KnownGoodReconstructionReceipt, KnownGoodReconstructionError>>
        + Send
        + 'static,
{
    let shutdown = state.subscribe_shutdown();
    if *shutdown.borrow() {
        return;
    }
    let groups = startup_rebuild_groups(state);
    if groups.is_empty() {
        return;
    }
    let Ok(foreground) = state.register_integrity_foreground() else {
        return;
    };
    let state = state.clone();
    let rebuild_owner = producer.claim_child();
    producer.spawn(async move {
        let foreground = foreground.wait_for_settlement().await;
        stream::iter(groups)
            .for_each_concurrent(MAX_STARTUP_REBUILD_GROUPS, |instance_ids| {
                let state = state.clone();
                let shutdown = shutdown.clone();
                let reconstruct = reconstruct.clone();
                let foreground = &foreground;
                let rebuild_owner = &rebuild_owner;
                async move {
                    if *shutdown.borrow() {
                        return;
                    }
                    let rebuilds = instance_ids.into_iter().map(|instance_id| {
                        let state = state.clone();
                        let reconstruct = reconstruct.clone();
                        async move {
                            let _ = state
                                .rebuild_known_good_for_registered_instance(
                                    foreground,
                                    rebuild_owner,
                                    &instance_id,
                                    reconstruct,
                                )
                                .await;
                        }
                    });
                    join_all(rebuilds).await;
                }
            })
            .await;
    });
}

fn startup_rebuild_groups(state: &AppState) -> Vec<Vec<String>> {
    let mut group_indexes = HashMap::<String, usize>::new();
    let mut groups = Vec::<Vec<String>>::new();
    for instance in state
        .instances()
        .list()
        .into_iter()
        .take(INSTANCE_REGISTRY_MAX_ENTRIES)
    {
        let group_index = match group_indexes.get(&instance.version_id) {
            Some(index) => *index,
            None => {
                let index = groups.len();
                group_indexes.insert(instance.version_id, index);
                groups.push(Vec::new());
                index
            }
        };
        groups[group_index].push(instance.id);
    }
    groups
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{AppStateInit, InstallStore, SessionStore};
    use axial_config::{AppPaths, ConfigStore, InstanceRegistrySnapshot, InstanceStore};
    use axial_performance::PerformanceManager;
    use std::{
        path::{Path, PathBuf},
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::{SystemTime, UNIX_EPOCH},
    };
    use tokio::{
        sync::{Notify, Semaphore, mpsc},
        time::{Duration, timeout},
    };

    fn state_fixture(label: &str) -> (AppState, PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "axial-application-known-good-{label}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let config_dir = root.join("config");
        let paths = AppPaths {
            config_file: config_dir.join("config.json"),
            instances_file: config_dir.join("instances.json"),
            instances_dir: root.join("instances"),
            music_dir: root.join("music"),
            library_dir: root.join("library"),
            config_dir,
        };
        let config = Arc::new(ConfigStore::load_from(paths.clone()).expect("load config"));
        let instances = Arc::new(
            InstanceStore::from_snapshot(paths.clone(), InstanceRegistrySnapshot::default())
                .expect("load instances"),
        );
        let state = AppState::new(AppStateInit {
            app_name: "Axial".to_string(),
            version: "test".to_string(),
            config,
            instances,
            installs: Arc::new(InstallStore::new()),
            sessions: Arc::new(SessionStore::new()),
            performance: Arc::new(
                PerformanceManager::load_for_startup(&paths.config_dir)
                    .expect("performance manager"),
            ),
            startup_warnings: Vec::new(),
        });
        std::fs::create_dir_all(&paths.library_dir).expect("create library root");
        state.set_library_dir_for_test(paths.library_dir.to_string_lossy().into_owned());
        (state, root)
    }

    async fn close_fixture(state: AppState, root: &Path) {
        state
            .close_known_good_inventories()
            .await
            .expect("close known-good store");
        state
            .close_instance_registry()
            .await
            .expect("close instance registry");
        drop(state);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn startup_foreground_waits_for_cancelled_sweep_settlement_before_source_entry() {
        let (state, root) = state_fixture("sweep-settlement");
        state
            .instances()
            .insert_for_test("Sweep", "1.21.1")
            .expect("register instance");
        let idle = state.subscribe_integrity_idle();
        let epoch = idle.borrow().epoch();
        let reservation = state
            .try_reserve_idle_sweep(
                epoch,
                state.try_claim_producer().expect("claim sweep producer"),
            )
            .expect("reserve active sweep");
        let cancellation = reservation.cancellation();
        let (entered_tx, mut entered_rx) = mpsc::unbounded_channel();
        let calls = Arc::new(AtomicUsize::new(0));
        spawn_startup_known_good_rebuilds_with(
            &state,
            state.try_claim_producer().expect("claim startup producer"),
            {
                let calls = calls.clone();
                move |_| {
                    let entered_tx = entered_tx.clone();
                    let calls = calls.clone();
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        entered_tx.send(()).expect("record source entry");
                        Err(KnownGoodReconstructionError::Vanilla)
                    }
                }
            },
        );

        assert!(cancellation.is_cancelled());
        for _ in 0..4 {
            tokio::task::yield_now().await;
            assert!(entered_rx.try_recv().is_err());
        }
        drop(reservation);
        timeout(Duration::from_secs(5), entered_rx.recv())
            .await
            .expect("source enters after settlement")
            .expect("source entry");
        state.quiesce().await.expect("startup source drains");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        close_fixture(state, &root).await;
    }

    #[tokio::test]
    async fn empty_and_pre_shutdown_startup_do_not_register_or_enter_a_source() {
        let (empty_state, empty_root) = state_fixture("empty-admission");
        let empty_idle = empty_state.subscribe_integrity_idle();
        let empty_before = *empty_idle.borrow();
        let empty_calls = Arc::new(AtomicUsize::new(0));
        spawn_startup_known_good_rebuilds_with(
            &empty_state,
            empty_state
                .try_claim_producer()
                .expect("claim empty startup producer"),
            {
                let empty_calls = empty_calls.clone();
                move |_| {
                    let empty_calls = empty_calls.clone();
                    async move {
                        empty_calls.fetch_add(1, Ordering::SeqCst);
                        Err(KnownGoodReconstructionError::Vanilla)
                    }
                }
            },
        );
        assert_eq!(*empty_idle.borrow(), empty_before);
        assert_eq!(empty_calls.load(Ordering::SeqCst), 0);
        close_fixture(empty_state, &empty_root).await;

        let (closing_state, closing_root) = state_fixture("closing-admission");
        closing_state
            .instances()
            .insert_for_test("Closing", "1.21.1")
            .expect("register closing instance");
        let closing_idle = closing_state.subscribe_integrity_idle();
        let closing_epoch = closing_idle.borrow().epoch();
        let producer = closing_state
            .try_claim_producer()
            .expect("claim closing startup producer");
        let mut shutdown = closing_state.subscribe_shutdown();
        let quiesce_state = closing_state.clone();
        let quiesce = tokio::spawn(async move { quiesce_state.quiesce().await });
        timeout(Duration::from_secs(5), async {
            loop {
                if *shutdown.borrow_and_update() {
                    return;
                }
                shutdown
                    .changed()
                    .await
                    .expect("shutdown signal remains live");
            }
        })
        .await
        .expect("shutdown starts");
        let closing_calls = Arc::new(AtomicUsize::new(0));
        spawn_startup_known_good_rebuilds_with(&closing_state, producer, {
            let closing_calls = closing_calls.clone();
            move |_| {
                let closing_calls = closing_calls.clone();
                async move {
                    closing_calls.fetch_add(1, Ordering::SeqCst);
                    Err(KnownGoodReconstructionError::Vanilla)
                }
            }
        });
        assert_eq!(closing_idle.borrow().epoch(), closing_epoch);
        assert_eq!(closing_calls.load(Ordering::SeqCst), 0);
        timeout(Duration::from_secs(5), quiesce)
            .await
            .expect("closing producer releases")
            .expect("quiesce task")
            .expect("quiesce succeeds");
        close_fixture(closing_state, &closing_root).await;
    }

    #[tokio::test]
    async fn startup_same_version_instances_share_one_source_failure() {
        let (state, root) = state_fixture("same-version");
        for index in 0..32 {
            state
                .instances()
                .insert_for_test(format!("Instance {index}"), "1.21.1".to_string())
                .expect("register instance");
        }
        let warnings_before = state.startup_warnings();
        let source_calls = Arc::new(AtomicUsize::new(0));
        let source_entered = Arc::new(Notify::new());
        let source_release = Arc::new(Semaphore::new(0));
        let producer = state.try_claim_producer().expect("claim startup owner");
        spawn_startup_known_good_rebuilds_with(&state, producer, {
            let source_calls = source_calls.clone();
            let source_entered = source_entered.clone();
            let source_release = source_release.clone();
            move |_| {
                let source_calls = source_calls.clone();
                let source_entered = source_entered.clone();
                let source_release = source_release.clone();
                async move {
                    source_calls.fetch_add(1, Ordering::SeqCst);
                    source_entered.notify_one();
                    let permit = source_release.acquire().await.expect("release source");
                    permit.forget();
                    Err(KnownGoodReconstructionError::Vanilla)
                }
            }
        });

        timeout(Duration::from_secs(5), source_entered.notified())
            .await
            .expect("source entered");
        assert_eq!(source_calls.load(Ordering::SeqCst), 1);
        let shutdown_state = state.clone();
        let quiesce = tokio::spawn(async move { shutdown_state.quiesce().await });
        assert!(!quiesce.is_finished());
        source_release.add_permits(1);
        timeout(Duration::from_secs(5), quiesce)
            .await
            .expect("startup owner drains")
            .expect("quiesce task")
            .expect("quiesce succeeds");

        assert_eq!(source_calls.load(Ordering::SeqCst), 1);
        assert_eq!(state.startup_warnings(), warnings_before);
        close_fixture(state, &root).await;
    }

    #[tokio::test]
    async fn startup_shutdown_drains_two_active_groups_and_skips_queued_groups() {
        let (state, root) = state_fixture("shutdown-groups");
        for version_id in ["1.21.1", "1.21.2", "1.21.3"] {
            state
                .instances()
                .insert_for_test(format!("Instance {version_id}"), version_id.to_string())
                .expect("register instance");
        }
        let (entered_tx, mut entered_rx) = mpsc::unbounded_channel::<String>();
        let source_release = Arc::new(Semaphore::new(0));
        let producer = state.try_claim_producer().expect("claim startup owner");
        spawn_startup_known_good_rebuilds_with(&state, producer, {
            let source_release = source_release.clone();
            move |version_id| {
                let entered_tx = entered_tx.clone();
                let source_release = source_release.clone();
                async move {
                    entered_tx.send(version_id).expect("record source entry");
                    let permit = source_release.acquire().await.expect("release source");
                    permit.forget();
                    Err(KnownGoodReconstructionError::Vanilla)
                }
            }
        });

        let first = timeout(Duration::from_secs(5), entered_rx.recv())
            .await
            .expect("first source enters")
            .expect("first source id");
        let second = timeout(Duration::from_secs(5), entered_rx.recv())
            .await
            .expect("second source enters")
            .expect("second source id");
        assert_ne!(first, second);
        let shutdown_state = state.clone();
        let quiesce = tokio::spawn(async move { shutdown_state.quiesce().await });
        tokio::task::yield_now().await;
        assert!(!quiesce.is_finished());
        source_release.add_permits(2);
        timeout(Duration::from_secs(5), quiesce)
            .await
            .expect("active groups drain")
            .expect("quiesce task")
            .expect("quiesce succeeds");

        assert!(
            entered_rx.try_recv().is_err(),
            "queued group must not enter"
        );
        close_fixture(state, &root).await;
    }
}
