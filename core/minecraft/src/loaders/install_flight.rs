use crate::loaders::types::LoaderError;
use crate::managed_fs::{ManagedDir, ManagedLibraryOperation};
use crate::portable_path::PortableFileName;

const MAX_LIVE_LOADER_INSTALL_FLIGHTS: usize = 64;

pub(super) struct InstallFlightGuard {
    root: ManagedDir,
    _flight: tokio::sync::OwnedMutexGuard<()>,
}

pub(super) async fn acquire(
    library_root: &ManagedLibraryOperation,
    version_id: &str,
) -> Result<InstallFlightGuard, LoaderError> {
    library_root.revalidate().map_err(LoaderError::Io)?;
    let root = library_root.managed_directory()?;
    let version_id = PortableFileName::new_exact(version_id).map_err(|_| {
        LoaderError::InstallExecutionFailed(
            "loader install flight version id is not portable".to_string(),
        )
    })?;
    let flight = root.install_flight(version_id.key(), MAX_LIVE_LOADER_INSTALL_FLIGHTS)?;
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

#[cfg(test)]
mod tests {
    use super::acquire;
    use crate::loaders::types::LoaderError;
    use crate::managed_fs::{ManagedLibraryOperation, ManagedLibraryRoot};
    use crate::portable_path::PortableFileName;
    use std::fs;
    use std::sync::Arc;
    use std::time::Duration;
    use tempfile::TempDir;

    #[tokio::test]
    async fn same_generation_and_child_share_one_flight() {
        let temporary = library_root("same-child-alias");
        let root = temporary.path().join("library");
        let library = managed_library(&root);
        let first = acquire(&library, "child").await.expect("first flight");
        let alias = library.clone();
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
        let first_library = managed_library(&first_root);
        let second_library = managed_library(&second_root);
        let first = acquire(&first_library, "child-a")
            .await
            .expect("first flight");

        let other_child =
            tokio::time::timeout(Duration::from_secs(1), acquire(&first_library, "child-b"))
                .await
                .expect("different child does not wait")
                .expect("different child flight");
        let other_root =
            tokio::time::timeout(Duration::from_secs(1), acquire(&second_library, "child-a"))
                .await
                .expect("different root does not wait")
                .expect("different root flight");

        drop((first, other_child, other_root));
    }

    #[tokio::test]
    async fn canceled_waiter_does_not_retain_a_flight() {
        let temporary = library_root("canceled-waiter");
        let root = temporary.path().join("library");
        let library = Arc::new(managed_library(&root));
        let first = acquire(&library, "child").await.expect("first flight");
        let waiter_root = Arc::clone(&library);
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

        tokio::time::timeout(Duration::from_secs(1), acquire(&library, "child"))
            .await
            .expect("fresh acquisition does not wait")
            .expect("fresh flight");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn replacement_generation_does_not_share_the_retiring_flight() {
        let temporary = library_root("replaced-root");
        let root = temporary.path().join("library");
        let parked = temporary.path().join("parked-library");
        let retiring = managed_library(&root);
        let first = acquire(&retiring, "child").await.expect("admitted flight");
        fs::rename(&root, &parked).expect("park admitted root");
        fs::create_dir(&root).expect("replacement root");
        let replacement = managed_library(&root);
        let second = tokio::time::timeout(
            Duration::from_secs(1),
            acquire(&replacement, "child"),
        )
        .await
        .expect("replacement generation does not wait")
        .expect("replacement flight");

        assert!(matches!(
            first.revalidate(),
            Err(LoaderError::Io(_) | LoaderError::Verify(_))
        ));
        second.revalidate().expect("replacement root remains exact");
        drop((first, second));
    }

    #[test]
    fn registry_is_bounded_and_evicts_dead_flights() {
        let temporary = library_root("bounded-registry");
        let root = managed_library(&temporary.path().join("library"))
            .managed_directory()
            .expect("managed test root");
        let first = root
            .install_flight(key("child-a"), 2)
            .expect("first flight");
        let second = root
            .install_flight(key("child-b"), 2)
            .expect("second flight");

        assert!(matches!(
            root.install_flight(key("child-c"), 2),
            Err(LoaderError::InstallExecutionFailed(message))
                if message == "loader install flight capacity is exhausted"
        ));

        drop(first);
        let third = root
            .install_flight(key("child-c"), 2)
            .expect("dead flight is evicted");
        assert!(!Arc::ptr_eq(&second, &third));
    }

    #[test]
    fn portable_version_aliases_share_one_flight() {
        let temporary = library_root("portable-version-alias");
        let root = managed_library(&temporary.path().join("library"))
            .managed_directory()
            .expect("managed test root");

        let first = root
            .install_flight(key("Stra\u{df}e"), 2)
            .expect("first flight");
        let alias = root
            .install_flight(key("STRASSE"), 2)
            .expect("alias flight");

        assert!(Arc::ptr_eq(&first, &alias));
    }

    fn key(child: &str) -> crate::portable_path::PortablePathKey {
        PortableFileName::new(child)
            .expect("portable test version id")
            .key()
    }

    fn managed_library(path: &std::path::Path) -> ManagedLibraryOperation {
        let root = ManagedLibraryRoot::open_for_test(path).expect("managed library root");
        root.try_acquire().expect("managed library operation")
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
