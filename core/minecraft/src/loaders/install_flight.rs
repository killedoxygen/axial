use crate::loaders::types::LoaderError;
use crate::managed_fs::ManagedDir;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};

const MAX_LIVE_LOADER_INSTALL_FLIGHTS: usize = 64;

type InstallMutex = tokio::sync::Mutex<()>;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct InstallFlightKey {
    namespace: PathBuf,
    version_id: String,
}

struct InstallFlightRegistry {
    flights: Mutex<HashMap<InstallFlightKey, Weak<InstallMutex>>>,
    max_live: usize,
}

static INSTALL_FLIGHTS: OnceLock<InstallFlightRegistry> = OnceLock::new();

pub(super) struct InstallFlightGuard {
    root: ManagedDir,
    _flight: tokio::sync::OwnedMutexGuard<()>,
}

pub(super) async fn acquire(
    library_dir: &Path,
    version_id: &str,
) -> Result<InstallFlightGuard, LoaderError> {
    acquire_with_wait_observer(library_dir, version_id, || {}).await
}

async fn acquire_with_wait_observer(
    library_dir: &Path,
    version_id: &str,
    before_wait: impl FnOnce(),
) -> Result<InstallFlightGuard, LoaderError> {
    let root = ManagedDir::open_root(library_dir)?;
    let namespace = std::fs::canonicalize(library_dir)?;
    root.revalidate()?;
    let key = InstallFlightKey {
        namespace,
        version_id: version_id.to_string(),
    };
    let flight = INSTALL_FLIGHTS
        .get_or_init(|| InstallFlightRegistry::new(MAX_LIVE_LOADER_INSTALL_FLIGHTS))
        .flight(key)?;
    before_wait();
    let flight = flight.lock_owned().await;
    root.revalidate()?;
    Ok(InstallFlightGuard {
        root,
        _flight: flight,
    })
}

impl InstallFlightGuard {
    pub(super) fn revalidate(&self) -> Result<(), LoaderError> {
        self.root.revalidate()
    }
}

impl InstallFlightRegistry {
    fn new(max_live: usize) -> Self {
        Self {
            flights: Mutex::new(HashMap::new()),
            max_live,
        }
    }

    fn flight(&self, key: InstallFlightKey) -> Result<Arc<InstallMutex>, LoaderError> {
        let mut flights = self
            .flights
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        flights.retain(|_, flight| flight.strong_count() > 0);
        if let Some(flight) = flights.get(&key).and_then(Weak::upgrade) {
            return Ok(flight);
        }
        if flights.len() >= self.max_live {
            return Err(LoaderError::InstallExecutionFailed(
                "loader install flight capacity is exhausted".to_string(),
            ));
        }
        let flight = Arc::new(InstallMutex::new(()));
        flights.insert(key, Arc::downgrade(&flight));
        Ok(flight)
    }
}

#[cfg(test)]
mod tests {
    use super::{InstallFlightKey, InstallFlightRegistry, acquire, acquire_with_wait_observer};
    use crate::loaders::types::LoaderError;
    use std::fs;
    use std::path::Path;
    use std::sync::Arc;
    #[cfg(unix)]
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;
    use tempfile::TempDir;
    #[cfg(unix)]
    use tokio::sync::oneshot;

    #[tokio::test]
    async fn same_root_alias_and_child_share_one_flight() {
        let temporary = library_root("same-child-alias");
        let root = temporary.path().join("library");
        let first = acquire(&root, "child").await.expect("first flight");
        let alias = root.join(".");
        let waiter = tokio::spawn(async move {
            let _flight = acquire(&alias, "child").await.expect("alias flight");
        });

        tokio::task::yield_now().await;
        assert!(!waiter.is_finished());
        drop(first);
        tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .expect("alias waiter settles")
            .expect("alias waiter owner");
    }

    #[tokio::test]
    async fn different_children_and_roots_remain_independent() {
        let first_temporary = library_root("independent-first");
        let second_temporary = library_root("independent-second");
        let first_root = first_temporary.path().join("library");
        let second_root = second_temporary.path().join("library");
        let first = acquire(&first_root, "child-a").await.expect("first flight");

        let other_child =
            tokio::time::timeout(Duration::from_secs(1), acquire(&first_root, "child-b"))
                .await
                .expect("different child does not wait")
                .expect("different child flight");
        let other_root =
            tokio::time::timeout(Duration::from_secs(1), acquire(&second_root, "child-a"))
                .await
                .expect("different root does not wait")
                .expect("different root flight");

        drop((first, other_child, other_root));
    }

    #[tokio::test]
    async fn canceled_waiter_does_not_retain_a_flight() {
        let temporary = library_root("canceled-waiter");
        let root = temporary.path().join("library");
        let first = acquire(&root, "child").await.expect("first flight");
        let waiter_root = root.clone();
        let waiter = tokio::spawn(async move {
            acquire(&waiter_root, "child")
                .await
                .expect("waiting flight")
        });

        tokio::task::yield_now().await;
        assert!(!waiter.is_finished());
        waiter.abort();
        assert!(waiter.await.is_err(), "waiter is canceled");
        drop(first);

        tokio::time::timeout(Duration::from_secs(1), acquire(&root, "child"))
            .await
            .expect("fresh acquisition does not wait")
            .expect("fresh flight");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn root_replacement_cannot_admit_a_second_same_child_owner() {
        let temporary = library_root("replaced-root");
        let root = temporary.path().join("library");
        let parked = temporary.path().join("parked-library");
        let first = acquire(&root, "child").await.expect("admitted flight");
        fs::rename(&root, &parked).expect("park admitted root");
        fs::create_dir(&root).expect("replacement root");
        let second_entered = Arc::new(AtomicBool::new(false));
        let waiter_entered = Arc::clone(&second_entered);
        let waiter_root = root.clone();
        let (waiting_tx, waiting_rx) = oneshot::channel();
        let waiter = tokio::spawn(async move {
            let flight = acquire_with_wait_observer(&waiter_root, "child", || {
                let _ = waiting_tx.send(());
            })
            .await
            .expect("replacement flight");
            waiter_entered.store(true, Ordering::SeqCst);
            flight
        });

        tokio::time::timeout(Duration::from_secs(1), waiting_rx)
            .await
            .expect("replacement waiter reaches namespace gate")
            .expect("replacement waiter reports gate wait");
        tokio::task::yield_now().await;
        assert!(!second_entered.load(Ordering::SeqCst));
        assert!(!waiter.is_finished());

        assert!(matches!(
            first.revalidate(),
            Err(LoaderError::Io(_) | LoaderError::Verify(_))
        ));

        drop(first);
        let second = tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .expect("replacement waiter settles")
            .expect("replacement waiter owner");
        assert!(second_entered.load(Ordering::SeqCst));
        second.revalidate().expect("replacement root remains exact");
    }

    #[test]
    fn registry_is_bounded_and_evicts_dead_flights() {
        let temporary = library_root("bounded-registry");
        let namespace =
            fs::canonicalize(temporary.path().join("library")).expect("canonical namespace");
        let registry = InstallFlightRegistry::new(2);
        let first = registry
            .flight(key(&namespace, "child-a"))
            .expect("first flight");
        let second = registry
            .flight(key(&namespace, "child-b"))
            .expect("second flight");

        assert!(matches!(
            registry.flight(key(&namespace, "child-c")),
            Err(LoaderError::InstallExecutionFailed(message))
                if message == "loader install flight capacity is exhausted"
        ));

        drop(first);
        let third = registry
            .flight(key(&namespace, "child-c"))
            .expect("dead flight is evicted");
        assert!(!Arc::ptr_eq(&second, &third));
    }

    fn key(namespace: &Path, child: &str) -> InstallFlightKey {
        InstallFlightKey {
            namespace: namespace.to_path_buf(),
            version_id: child.to_string(),
        }
    }

    fn library_root(name: &str) -> TempDir {
        let temporary = tempfile::Builder::new()
            .prefix(&format!("axial-loader-flight-{name}-"))
            .tempdir()
            .expect("temporary root");
        fs::create_dir(temporary.path().join("library")).expect("library root");
        temporary
    }
}
