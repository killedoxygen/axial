use super::*;
use crate::tasks::{ArtifactKey, ExclusionError, Exclusions};
use std::sync::{
    Barrier,
    atomic::{AtomicBool, Ordering},
};

struct DropFlag(Arc<AtomicBool>);

impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

struct NotifyDrop(Option<oneshot::Sender<()>>);

impl Drop for NotifyDrop {
    fn drop(&mut self) {
        if let Some(sender) = self.0.take() {
            let _ = sender.send(());
        }
    }
}

#[tokio::test]
async fn dropping_request_waiter_and_owner_does_not_cancel_accepted_work() {
    let owner = TaskOwner::new(1).unwrap();
    let dropped = Arc::new(AtomicBool::new(false));
    let (release, released) = oneshot::channel();
    let (effect, observed_effect) = oneshot::channel();
    let (guard_released, observed_guard_release) = oneshot::channel();
    let handle = owner
        .try_spawn(
            (
                DropFlag(Arc::clone(&dropped)),
                NotifyDrop(Some(guard_released)),
            ),
            move |cancel| async move {
                released.await.unwrap();
                assert!(!cancel.is_cancelled());
                effect.send("published").unwrap();
            },
        )
        .unwrap();
    drop(handle);
    drop(owner);
    assert!(!dropped.load(Ordering::SeqCst));
    release.send(()).unwrap();
    assert_eq!(observed_effect.await.unwrap(), "published");
    observed_guard_release.await.unwrap();
    assert!(dropped.load(Ordering::SeqCst));
}

#[tokio::test]
async fn cancellation_and_shutdown_wait_for_domain_settlement() {
    let owner = TaskOwner::new(1).unwrap();
    let dropped = Arc::new(AtomicBool::new(false));
    let (cancel_seen, cancellation_observed) = oneshot::channel();
    let (settle, settlement) = oneshot::channel();
    let handle = owner
        .try_spawn(DropFlag(Arc::clone(&dropped)), move |cancel| async move {
            cancel.cancelled().await;
            cancel_seen.send(()).unwrap();
            settlement.await.unwrap();
            "settled cancellation"
        })
        .unwrap();
    let shutdown_owner = owner.clone();
    let shutdown =
        tokio::spawn(async move { shutdown_owner.shutdown(Duration::from_secs(5)).await });
    cancellation_observed.await.unwrap();
    assert!(!dropped.load(Ordering::SeqCst));
    assert!(!shutdown.is_finished());
    assert!(owner.status().closing);
    assert!(owner.shutdown_receipt().is_none());
    assert!(matches!(
        owner.try_spawn((), |_| async {}),
        Err(SpawnError::Closed)
    ));
    settle.send(()).unwrap();
    assert_eq!(handle.join().await.unwrap(), "settled cancellation");
    shutdown.await.unwrap().unwrap();
    assert!(dropped.load(Ordering::SeqCst));
    assert!(owner.shutdown_receipt().unwrap().belongs_to(&owner));
}

#[tokio::test]
async fn shutdown_timeout_retains_exclusion_and_retry_joins() {
    let owner = TaskOwner::new(1).unwrap();
    let exclusions = Exclusions::new();
    let lease = exclusions.try_acquire(["instance"], []).unwrap();
    let (release, released) = oneshot::channel();
    let handle = owner
        .try_spawn(lease, move |_| async move { released.await.unwrap() })
        .unwrap();
    let id = handle.id();
    let error = owner.shutdown(Duration::ZERO).await.unwrap_err();
    assert_eq!(error.work.running, vec![id]);
    assert!(matches!(
        exclusions.try_acquire(["instance"], []),
        Err(ExclusionError::Busy)
    ));
    assert!(owner.shutdown_receipt().is_none());
    release.send(()).unwrap();
    handle.join().await.unwrap();
    owner.shutdown(Duration::ZERO).await.unwrap();
    assert!(exclusions.try_acquire(["instance"], []).is_ok());
}

#[tokio::test]
async fn capacity_rejection_never_starts_work_and_settlement_frees_slot() {
    let owner = TaskOwner::new(1).unwrap();
    let (release, released) = oneshot::channel();
    let first = owner
        .try_spawn((), move |_| async move { released.await.unwrap() })
        .unwrap();
    let started = Arc::new(AtomicBool::new(false));
    let rejected_started = Arc::clone(&started);
    assert!(matches!(
        owner.try_spawn((), move |_| async move {
            rejected_started.store(true, Ordering::SeqCst);
        }),
        Err(SpawnError::AtCapacity)
    ));
    assert!(!started.load(Ordering::SeqCst));
    release.send(()).unwrap();
    first.join().await.unwrap();
    owner
        .try_spawn((), |_| async { 7 })
        .unwrap()
        .join()
        .await
        .unwrap();
    owner.shutdown(Duration::ZERO).await.unwrap();
}

#[tokio::test]
async fn worker_panic_preserves_guards_and_prevents_false_shutdown_success() {
    let owner = TaskOwner::new(1).unwrap();
    let exclusions = Exclusions::new();
    let lease = exclusions.try_acquire(["instance"], []).unwrap();
    let handle = owner
        .try_spawn(lease, |_| async {
            panic!("domain interrupted after an effect")
        })
        .unwrap();
    let id = handle.id();
    assert_eq!(handle.join().await, Err(TaskJoinError::Panicked));
    let error = owner.shutdown(Duration::ZERO).await.unwrap_err();
    assert_eq!(error.work.unsettled, vec![id]);
    assert!(error.work.running.is_empty());
    assert!(matches!(
        exclusions.try_acquire(["instance"], []),
        Err(ExclusionError::Busy)
    ));
    assert!(owner.shutdown_receipt().is_none());
}

#[tokio::test]
async fn busy_close_refuses_atomically_without_cancelling_running_work() {
    let owner = TaskOwner::new(1).unwrap();
    let (release, released) = oneshot::channel();
    let handle = owner
        .try_spawn((), move |cancel| async move {
            released.await.unwrap();
            cancel.is_cancelled()
        })
        .unwrap();
    assert!(owner.try_close_idle().is_err());
    assert!(!owner.status().closing);
    release.send(()).unwrap();
    assert!(!handle.join().await.unwrap());
    owner.try_close_idle().unwrap();
    assert!(matches!(
        owner.try_spawn((), |_| async {}),
        Err(SpawnError::Closed)
    ));
    let other = TaskOwner::new(1).unwrap();
    assert!(!owner.shutdown_receipt().unwrap().belongs_to(&other));
}

#[test]
fn zero_capacity_and_missing_runtime_are_explicit_errors() {
    assert!(matches!(
        TaskOwner::new(0),
        Err(SpawnError::InvalidCapacity)
    ));
    assert!(matches!(
        TaskOwner::new(1).unwrap().try_spawn((), |_| async {}),
        Err(SpawnError::NoRuntime)
    ));
}

#[tokio::test]
async fn cancellation_keeps_shared_artifacts_until_work_and_escaped_receipt_settle() {
    let owner = TaskOwner::new(1).unwrap();
    let exclusions = Exclusions::new();
    let artifacts = ArtifactKey::new("library", "managed-game-artifacts");
    let lease = exclusions
        .try_acquire_read_artifacts(["instance-a"], [artifacts.clone()])
        .unwrap();
    let receipt = lease.clone();
    let other_session = exclusions
        .try_acquire_read_artifacts(["instance-b"], [artifacts.clone()])
        .unwrap();
    let (cancel_seen, cancellation_observed) = oneshot::channel();
    let (settle, settlement) = oneshot::channel();
    let handle = owner
        .try_spawn(lease, move |cancel| async move {
            cancel.cancelled().await;
            cancel_seen.send(()).unwrap();
            settlement.await.unwrap();
        })
        .unwrap();
    let id = handle.id();
    assert!(owner.cancel(id));
    drop(handle);
    cancellation_observed.await.unwrap();
    assert!(owner.shutdown(Duration::ZERO).await.is_err());
    drop(other_session);
    assert!(matches!(
        exclusions.try_acquire(["installer"], [artifacts.clone()]),
        Err(ExclusionError::Busy)
    ));
    settle.send(()).unwrap();
    owner.shutdown(Duration::from_secs(5)).await.unwrap();
    assert!(owner.status().is_idle());
    assert!(matches!(
        exclusions.try_acquire(["installer"], [artifacts.clone()]),
        Err(ExclusionError::Busy)
    ));
    drop(receipt);
    assert!(exclusions.try_acquire(["installer"], [artifacts]).is_ok());
}

#[tokio::test]
async fn panic_preserves_read_artifacts_and_prevents_writer_admission() {
    let owner = TaskOwner::new(1).unwrap();
    let exclusions = Exclusions::new();
    let artifacts = ArtifactKey::new("library", "managed-game-artifacts");
    let lease = exclusions
        .try_acquire_read_artifacts(["instance-a"], [artifacts.clone()])
        .unwrap();
    let handle = owner
        .try_spawn(lease, |_| async { panic!("session settlement is unknown") })
        .unwrap();
    assert_eq!(handle.join().await, Err(TaskJoinError::Panicked));
    assert!(owner.shutdown(Duration::ZERO).await.is_err());
    assert!(
        exclusions
            .try_acquire_read_artifacts(["instance-b"], [artifacts.clone()])
            .is_ok()
    );
    assert!(matches!(
        exclusions.try_acquire(["installer"], [artifacts]),
        Err(ExclusionError::Busy)
    ));
}

#[test]
fn runtime_interruption_marks_even_unpolled_accepted_work_unsettled() {
    for poll_worker in [false, true] {
        let owner = TaskOwner::new(1).unwrap();
        let exclusions = Exclusions::new();
        let lease = exclusions.try_acquire(["instance"], []).unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (started, observed_start) = oneshot::channel();
        let handle = {
            let _entered = runtime.enter();
            owner
                .try_spawn(lease, move |_| async move {
                    started.send(()).unwrap();
                    std::future::pending::<()>().await;
                })
                .unwrap()
        };
        let id = handle.id();
        if poll_worker {
            runtime.block_on(observed_start).unwrap();
        }
        drop(runtime);
        assert_eq!(owner.status().unsettled, vec![id]);
        assert!(owner.status().running.is_empty());
        assert!(matches!(
            exclusions.try_acquire(["instance"], []),
            Err(ExclusionError::Busy)
        ));
        let joining_runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        assert_eq!(
            joining_runtime.block_on(handle.join()),
            Err(TaskJoinError::Interrupted)
        );
        assert!(
            joining_runtime
                .block_on(owner.shutdown(Duration::ZERO))
                .is_err()
        );
        assert!(owner.shutdown_receipt().is_none());
    }
}

#[tokio::test]
async fn completion_wakes_capacity_subscriber_even_before_it_waits() {
    let owner = TaskOwner::new(1).unwrap();
    let (release, released) = oneshot::channel();
    let handle = owner
        .try_spawn((), move |_| async move { released.await.unwrap() })
        .unwrap();
    let mut changes = owner.subscribe();
    assert!(matches!(
        owner.try_spawn((), |_| async {}),
        Err(SpawnError::AtCapacity)
    ));
    release.send(()).unwrap();
    handle.join().await.unwrap();
    changes.changed().await.unwrap();
    owner
        .try_spawn((), |_| async {})
        .unwrap()
        .join()
        .await
        .unwrap();
}

#[tokio::test]
async fn racing_admission_and_idle_close_choose_one_consistent_outcome() {
    for _ in 0..16 {
        let owner = TaskOwner::new(1).unwrap();
        let start = Arc::new(Barrier::new(3));
        let (release, released) = oneshot::channel();
        let admission = {
            let owner = owner.clone();
            let start = Arc::clone(&start);
            let runtime = tokio::runtime::Handle::current();
            std::thread::spawn(move || {
                let _entered = runtime.enter();
                start.wait();
                owner.try_spawn((), move |_| async move { released.await.unwrap() })
            })
        };
        let closure = {
            let owner = owner.clone();
            let start = Arc::clone(&start);
            std::thread::spawn(move || {
                start.wait();
                owner.try_close_idle()
            })
        };
        start.wait();
        let admission = admission.join().unwrap();
        let closure = closure.join().unwrap();
        match admission {
            Ok(handle) => {
                assert!(closure.is_err());
                assert!(!owner.status().closing);
                release.send(()).unwrap();
                handle.join().await.unwrap();
                owner.try_close_idle().unwrap();
            }
            Err(SpawnError::Closed) => {
                assert!(closure.is_ok());
                assert!(owner.status().closing);
            }
            Err(error) => panic!("unexpected admission failure: {error}"),
        }
        assert!(owner.shutdown_receipt().is_some());
    }
}
