use crate::loaders::types::LoaderError;
use crate::managed_fs::{ManagedDir, ManagedDirectoryIdentity, ManagedPersistentFile};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

const PUBLICATION_DIRECTORY: &str = ".axial-publication";
const PUBLICATION_LOCK_FILE: &str = "publication.lock";
const MAX_LIVE_PUBLICATION_ROOTS: usize = 64;
const MAX_BLOCKING_PUBLICATION_TASKS: usize = 4;
const CROSS_PROCESS_RETRY_INTERVAL: Duration = Duration::from_millis(25);

type RootMutex = tokio::sync::Mutex<()>;
type RootMutexRegistry = HashMap<ManagedDirectoryIdentity, Weak<RootMutex>>;

static ROOT_MUTEXES: OnceLock<Mutex<RootMutexRegistry>> = OnceLock::new();
static BLOCKING_PUBLICATION_TASKS: OnceLock<Arc<tokio::sync::Semaphore>> = OnceLock::new();

#[derive(Debug, thiserror::Error)]
pub(crate) enum ManagedPublicationError {
    #[error("managed publication root admission failed: {0}")]
    Admission(#[from] LoaderError),
    #[error("managed publication root capacity is exhausted")]
    RootCapacityExhausted,
    #[error("managed publication blocking task stopped unexpectedly")]
    BlockingTaskStopped,
    #[error("managed publication is changing")]
    ReadBusy,
}

pub(crate) struct ManagedRootPublicationLease {
    root: ManagedDir,
    publication_directory: ManagedDir,
    lock_file: Arc<ManagedPersistentFile>,
    _in_process_guard: tokio::sync::OwnedMutexGuard<()>,
}

pub(crate) enum ManagedRootPublicationReadLease {
    NoLane {
        root: ManagedDir,
    },
    Locked {
        root: ManagedDir,
        publication_directory: ManagedDir,
        lock_file: ManagedPersistentFile,
    },
}

impl std::fmt::Debug for ManagedRootPublicationLease {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ManagedRootPublicationLease")
            .finish_non_exhaustive()
    }
}

impl ManagedRootPublicationLease {
    pub(crate) async fn acquire(root: ManagedDir) -> Result<Self, ManagedPublicationError> {
        let identity_root = root.clone();
        let identity = run_publication_blocking(move || identity_root.identity())
            .await?
            .map_err(ManagedPublicationError::Admission)?;
        let root_mutex = root_mutex(identity)?;
        let in_process_guard = root_mutex.lock_owned().await;

        let setup_root = root.clone();
        let (publication_directory, lock_file) = run_publication_blocking(move || {
            setup_root.revalidate()?;
            let publication_directory = setup_root.open_or_create_child(PUBLICATION_DIRECTORY)?;
            let lock_file =
                publication_directory.open_or_create_persistent_file(PUBLICATION_LOCK_FILE)?;
            Ok::<_, LoaderError>((publication_directory, Arc::new(lock_file)))
        })
        .await?
        .map_err(ManagedPublicationError::Admission)?;
        loop {
            let attempt = Arc::clone(&lock_file);
            if run_publication_blocking(move || attempt.try_lock_exclusive())
                .await?
                .map_err(ManagedPublicationError::Admission)?
            {
                break;
            }
            tokio::time::sleep(CROSS_PROCESS_RETRY_INTERVAL).await;
        }
        let validation_root = root.clone();
        let validation_publication = publication_directory.clone();
        let validation_lock = Arc::clone(&lock_file);
        if let Err(error) = run_publication_blocking(move || {
            validation_root
                .revalidate()
                .and_then(|()| validation_publication.revalidate())
                .and_then(|()| validation_lock.revalidate())
        })
        .await?
        {
            let _ = lock_file.unlock();
            return Err(error.into());
        }

        Ok(Self {
            root,
            publication_directory,
            lock_file,
            _in_process_guard: in_process_guard,
        })
    }

    pub(crate) fn root(&self) -> &ManagedDir {
        &self.root
    }

    pub(crate) fn publication_directory(&self) -> &ManagedDir {
        &self.publication_directory
    }

    pub(crate) fn revalidate(&self) -> Result<(), ManagedPublicationError> {
        self.root.revalidate()?;
        self.publication_directory.revalidate()?;
        self.lock_file.revalidate()?;
        Ok(())
    }
}

impl ManagedRootPublicationReadLease {
    pub(crate) fn acquire(root: ManagedDir) -> Result<Self, ManagedPublicationError> {
        Self::try_acquire(root)?.ok_or(ManagedPublicationError::ReadBusy)
    }

    fn try_acquire(root: ManagedDir) -> Result<Option<Self>, ManagedPublicationError> {
        root.revalidate()?;
        if !root.has_portably_exact_child_name(PUBLICATION_DIRECTORY)? {
            return Ok(Some(Self::NoLane { root }));
        }

        let publication_directory = root.open_child(PUBLICATION_DIRECTORY)?;
        if !publication_directory.has_portably_exact_child_name(PUBLICATION_LOCK_FILE)? {
            return Ok(None);
        }
        let lock_file = publication_directory.open_persistent_file(PUBLICATION_LOCK_FILE)?;
        if !lock_file.try_lock_shared()? {
            return Ok(None);
        }
        if let Err(error) = root
            .revalidate()
            .and_then(|()| publication_directory.revalidate())
            .and_then(|()| lock_file.revalidate())
        {
            let _ = lock_file.unlock();
            return Err(error.into());
        }

        Ok(Some(Self::Locked {
            root,
            publication_directory,
            lock_file,
        }))
    }

    pub(crate) fn revalidate(&self) -> Result<(), ManagedPublicationError> {
        match self {
            Self::NoLane { root } => {
                root.revalidate()?;
                if root.has_portably_exact_child_name(PUBLICATION_DIRECTORY)? {
                    return Err(ManagedPublicationError::ReadBusy);
                }
            }
            Self::Locked {
                root,
                publication_directory,
                lock_file,
            } => {
                root.revalidate()?;
                publication_directory.revalidate()?;
                lock_file.revalidate()?;
            }
        }
        Ok(())
    }

    pub(crate) fn root_identity(
        &self,
    ) -> Result<ManagedDirectoryIdentity, ManagedPublicationError> {
        let root = match self {
            Self::NoLane { root } | Self::Locked { root, .. } => root,
        };
        Ok(root.identity()?)
    }
}

pub(crate) async fn run_publication_blocking<F, R>(work: F) -> Result<R, ManagedPublicationError>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    let semaphore = BLOCKING_PUBLICATION_TASKS
        .get_or_init(|| Arc::new(tokio::sync::Semaphore::new(MAX_BLOCKING_PUBLICATION_TASKS)));
    let permit = Arc::clone(semaphore)
        .acquire_owned()
        .await
        .map_err(|_| ManagedPublicationError::BlockingTaskStopped)?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        work()
    })
    .await
    .map_err(|_| ManagedPublicationError::BlockingTaskStopped)
}

impl Drop for ManagedRootPublicationLease {
    fn drop(&mut self) {
        let _ = self.lock_file.unlock();
    }
}

impl Drop for ManagedRootPublicationReadLease {
    fn drop(&mut self) {
        if let Self::Locked { lock_file, .. } = self {
            let _ = lock_file.unlock();
        }
    }
}

fn root_mutex(
    identity: ManagedDirectoryIdentity,
) -> Result<Arc<RootMutex>, ManagedPublicationError> {
    let registry = ROOT_MUTEXES.get_or_init(|| Mutex::new(HashMap::new()));
    let mut registry = registry
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    registry.retain(|_, root_mutex| root_mutex.strong_count() > 0);
    if let Some(root_mutex) = registry.get(&identity).and_then(Weak::upgrade) {
        return Ok(root_mutex);
    }
    if registry.len() >= MAX_LIVE_PUBLICATION_ROOTS {
        return Err(ManagedPublicationError::RootCapacityExhausted);
    }
    let root_mutex = Arc::new(RootMutex::new(()));
    registry.insert(identity, Arc::downgrade(&root_mutex));
    Ok(root_mutex)
}

#[cfg(test)]
mod tests {
    use super::{
        ManagedPublicationError, ManagedRootPublicationLease, ManagedRootPublicationReadLease,
        PUBLICATION_DIRECTORY, PUBLICATION_LOCK_FILE,
    };
    use crate::managed_fs::ManagedDir;
    use std::ffi::OsString;
    use std::fs;
    use std::time::Duration;
    use tempfile::TempDir;

    #[tokio::test]
    async fn same_root_aliases_share_in_process_exclusion() {
        let temporary = library_root("same-root-alias");
        let root = temporary.path().join("library");
        let first =
            ManagedRootPublicationLease::acquire(ManagedDir::open_root(&root).expect("first root"))
                .await
                .expect("first lease");
        let alias = ManagedDir::open_root(&root.join(".")).expect("root alias");
        let waiter = tokio::spawn(ManagedRootPublicationLease::acquire(alias));

        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!waiter.is_finished());
        drop(first);
        waiter.await.expect("waiter task").expect("alias lease");
    }

    #[tokio::test]
    async fn different_roots_do_not_block_each_other() {
        let first_temporary = library_root("different-root-first");
        let second_temporary = library_root("different-root-second");
        let first = ManagedRootPublicationLease::acquire(
            ManagedDir::open_root(&first_temporary.path().join("library")).expect("first root"),
        )
        .await
        .expect("first lease");

        let second = tokio::time::timeout(
            Duration::from_millis(100),
            ManagedRootPublicationLease::acquire(
                ManagedDir::open_root(&second_temporary.path().join("library"))
                    .expect("second root"),
            ),
        )
        .await
        .expect("second root did not wait")
        .expect("second lease");

        drop((first, second));
    }

    #[test]
    fn reader_without_a_publication_lane_does_not_create_metadata() {
        let temporary = library_root("reader-no-lane");
        let root = temporary.path().join("library");
        let reader = ManagedRootPublicationReadLease::acquire(
            ManagedDir::open_root(&root).expect("managed root"),
        )
        .expect("reader admission");

        assert!(!root.join(PUBLICATION_DIRECTORY).exists());
        reader.revalidate().expect("stable missing lane");
    }

    #[tokio::test]
    async fn publication_lane_appearing_during_an_unlocked_read_invalidates_it() {
        let temporary = library_root("reader-lane-race");
        let root = temporary.path().join("library");
        let reader = ManagedRootPublicationReadLease::acquire(
            ManagedDir::open_root(&root).expect("reader root"),
        )
        .expect("reader admission");
        let writer = ManagedRootPublicationLease::acquire(
            ManagedDir::open_root(&root).expect("writer root"),
        )
        .await
        .expect("writer admission");

        assert!(matches!(
            reader.revalidate(),
            Err(ManagedPublicationError::ReadBusy)
        ));
        drop(writer);
    }

    #[tokio::test]
    async fn existing_publication_lane_excludes_writer_while_reader_is_live() {
        let temporary = library_root("reader-shared-lock");
        let root = temporary.path().join("library");
        ManagedRootPublicationLease::acquire(
            ManagedDir::open_root(&root).expect("initial writer root"),
        )
        .await
        .expect("initial writer admission");
        let reader = ManagedRootPublicationReadLease::acquire(
            ManagedDir::open_root(&root).expect("reader root"),
        )
        .expect("reader admission");
        let waiter = tokio::spawn(ManagedRootPublicationLease::acquire(
            ManagedDir::open_root(&root).expect("waiting writer root"),
        ));

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!waiter.is_finished());
        reader.revalidate().expect("live reader");
        drop(reader);
        tokio::time::timeout(Duration::from_millis(200), waiter)
            .await
            .expect("writer unblocked")
            .expect("writer task")
            .expect("writer admission");
    }

    #[tokio::test]
    async fn file_substitution_for_publication_directory_is_rejected() {
        let temporary = library_root("file-substitution");
        let root = temporary.path().join("library");
        fs::write(root.join(PUBLICATION_DIRECTORY), b"not a directory")
            .expect("publication substitution");

        let error = ManagedRootPublicationLease::acquire(
            ManagedDir::open_root(&root).expect("managed root"),
        )
        .await
        .expect_err("file substitution must fail closed");
        assert!(matches!(error, ManagedPublicationError::Admission(_)));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlink_substitution_for_lock_file_is_rejected() {
        use std::os::unix::fs::symlink;

        let temporary = library_root("symlink-substitution");
        let root = temporary.path().join("library");
        let publication = root.join(PUBLICATION_DIRECTORY);
        fs::create_dir(&publication).expect("publication directory");
        fs::write(root.join("outside-lock"), b"").expect("outside lock");
        symlink(
            root.join("outside-lock"),
            publication.join(PUBLICATION_LOCK_FILE),
        )
        .expect("lock symlink");

        let error = ManagedRootPublicationLease::acquire(
            ManagedDir::open_root(&root).expect("managed root"),
        )
        .await
        .expect_err("symlink substitution must fail closed");
        assert!(matches!(error, ManagedPublicationError::Admission(_)));
    }

    #[tokio::test]
    async fn locked_file_substitution_is_denied_or_detected_by_revalidation() {
        let temporary = library_root("lock-file-replacement");
        let root = temporary.path().join("library");
        let lease = ManagedRootPublicationLease::acquire(
            ManagedDir::open_root(&root).expect("managed root"),
        )
        .await
        .expect("publication lease");
        let publication = root.join(PUBLICATION_DIRECTORY);
        let replacement = fs::rename(
            publication.join(PUBLICATION_LOCK_FILE),
            publication.join("displaced.lock"),
        );
        match replacement {
            Ok(()) => {
                fs::write(publication.join(PUBLICATION_LOCK_FILE), b"")
                    .expect("replacement lock file");
                assert!(matches!(
                    lease.revalidate(),
                    Err(ManagedPublicationError::Admission(_))
                ));
            }
            Err(_) => lease.revalidate().expect("locked identity remains exact"),
        }
    }

    #[tokio::test]
    async fn cancelling_waiter_releases_its_root_mutex_reference() {
        let temporary = library_root("cancelled-waiter");
        let root = temporary.path().join("library");
        let first =
            ManagedRootPublicationLease::acquire(ManagedDir::open_root(&root).expect("first root"))
                .await
                .expect("first lease");
        let waiting_root = ManagedDir::open_root(&root).expect("waiting root");
        let waiter = tokio::spawn(ManagedRootPublicationLease::acquire(waiting_root));
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!waiter.is_finished());
        waiter.abort();
        assert!(waiter.await.expect_err("cancelled waiter").is_cancelled());
        drop(first);

        tokio::time::timeout(
            Duration::from_millis(100),
            ManagedRootPublicationLease::acquire(ManagedDir::open_root(&root).expect("final root")),
        )
        .await
        .expect("cancelled waiter released exclusion")
        .expect("final lease");
    }

    #[tokio::test]
    async fn cancelling_cross_process_waiter_releases_in_process_exclusion() {
        let temporary = library_root("cancelled-cross-process-waiter");
        let root = temporary.path().join("library");
        let managed_root = ManagedDir::open_root(&root).expect("managed root");
        let publication = managed_root
            .open_or_create_child(PUBLICATION_DIRECTORY)
            .expect("publication directory");
        let external_lock = publication
            .open_or_create_persistent_file(PUBLICATION_LOCK_FILE)
            .expect("external lock file");
        assert!(external_lock.try_lock_exclusive().expect("external lock"));

        let waiter = tokio::spawn(ManagedRootPublicationLease::acquire(
            ManagedDir::open_root(&root).expect("waiting root"),
        ));
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!waiter.is_finished());
        waiter.abort();
        assert!(waiter.await.expect_err("cancelled waiter").is_cancelled());
        external_lock.unlock().expect("release external lock");

        tokio::time::timeout(
            Duration::from_millis(100),
            ManagedRootPublicationLease::acquire(ManagedDir::open_root(&root).expect("final root")),
        )
        .await
        .expect("cancelled cross-process waiter released exclusion")
        .expect("final lease");
    }

    #[tokio::test]
    async fn lock_lane_is_fixed_and_persistent() {
        let temporary = library_root("persistent-lane");
        let root = temporary.path().join("library");
        let lease =
            ManagedRootPublicationLease::acquire(ManagedDir::open_root(&root).expect("first root"))
                .await
                .expect("first lease");
        lease.revalidate().expect("live lease");
        assert_eq!(
            lease.root().identity().expect("lease root identity"),
            ManagedDir::open_root(&root)
                .expect("reopened root")
                .identity()
                .expect("reopened root identity")
        );
        assert_eq!(
            lease.publication_directory().path(),
            root.join(PUBLICATION_DIRECTORY)
        );
        drop(lease);

        let entries = fs::read_dir(root.join(PUBLICATION_DIRECTORY))
            .expect("persistent publication directory")
            .map(|entry| entry.expect("publication entry").file_name())
            .collect::<Vec<_>>();
        assert_eq!(entries, [OsString::from(PUBLICATION_LOCK_FILE)]);

        ManagedRootPublicationLease::acquire(ManagedDir::open_root(&root).expect("second root"))
            .await
            .expect("persistent lane reacquired");
        assert!(
            root.join(PUBLICATION_DIRECTORY)
                .join(PUBLICATION_LOCK_FILE)
                .is_file()
        );
    }

    fn library_root(label: &str) -> TempDir {
        let temporary = tempfile::Builder::new()
            .prefix(&format!("axial-managed-publication-{label}-"))
            .tempdir()
            .expect("temporary root");
        fs::create_dir(temporary.path().join("library")).expect("library root");
        temporary
    }
}
