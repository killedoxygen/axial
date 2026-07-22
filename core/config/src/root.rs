use std::io;
use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::paths::{AppPaths, AppPathsLineage};
use axial_fs::{
    Directory, DirectoryCreateOutcome, DirectoryCreateResolution, DirectoryIdentity, LeafName,
    ResetStartOutcome, RootSession, RootSessionAcquireOutcome,
};

pub struct AppRootSession {
    paths_lineage: Arc<AppPathsLineage>,
    expected_identity: DirectoryIdentity,
    session: Mutex<Option<RootSession>>,
}

#[derive(Clone)]
pub struct PersistedStateDirectories {
    operation_journal_parent: Directory,
    guardian_failure_memory_parent: Directory,
    performance_operations: Directory,
    benchmark_suite_drivers: Directory,
}

impl PersistedStateDirectories {
    pub fn operation_journal_parent(&self) -> Directory {
        self.operation_journal_parent.clone()
    }

    pub fn guardian_failure_memory_parent(&self) -> Directory {
        self.guardian_failure_memory_parent.clone()
    }

    pub fn performance_operations(&self) -> Directory {
        self.performance_operations.clone()
    }

    pub fn benchmark_suite_drivers(&self) -> Directory {
        self.benchmark_suite_drivers.clone()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AppRootSessionReinsertErrorKind {
    Occupied,
    ForeignIdentity,
    LockPoisoned,
}

#[must_use = "a rejected root session still retains the native root authority"]
#[derive(Debug)]
pub struct AppRootSessionReinsertError {
    kind: AppRootSessionReinsertErrorKind,
    session: RootSession,
}

impl AppRootSessionReinsertError {
    pub fn kind(&self) -> AppRootSessionReinsertErrorKind {
        self.kind
    }

    pub fn into_session(self) -> RootSession {
        self.session
    }
}

impl std::fmt::Display for AppRootSessionReinsertError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self.kind {
            AppRootSessionReinsertErrorKind::Occupied => {
                "application root session is already occupied"
            }
            AppRootSessionReinsertErrorKind::ForeignIdentity => {
                "cancelled reset session has a different root identity"
            }
            AppRootSessionReinsertErrorKind::LockPoisoned => {
                "application root session lock was poisoned"
            }
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for AppRootSessionReinsertError {}

impl std::fmt::Debug for AppRootSession {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AppRootSession")
            .finish_non_exhaustive()
    }
}

impl AppRootSession {
    pub(crate) fn open(paths: &AppPaths) -> io::Result<Self> {
        let session = acquire_root_session(paths.root())?;
        Ok(Self {
            paths_lineage: Arc::clone(paths.lineage()),
            expected_identity: session.identity(),
            session: Mutex::new(Some(session)),
        })
    }

    pub fn root_directory(&self) -> io::Result<Directory> {
        self.with_session(RootSession::root)
    }

    pub fn admit_absolute_directory(&self, path: &Path) -> io::Result<Directory> {
        self.with_session(|session| session.admit_absolute_directory(path))
    }

    pub(crate) fn validate_paths(&self, paths: &AppPaths) -> io::Result<()> {
        if !Arc::ptr_eq(&self.paths_lineage, paths.lineage()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "application paths and root session have different provenance",
            ));
        }
        Ok(())
    }

    pub fn prepare_instances_directory(&self) -> io::Result<Directory> {
        self.open_or_create_fixed_directory("instances")
    }

    pub fn prepare_performance_directory(&self) -> io::Result<Directory> {
        self.open_or_create_fixed_directory("performance")
    }

    pub fn prepare_persisted_state_directories(&self) -> io::Result<PersistedStateDirectories> {
        Ok(PersistedStateDirectories {
            operation_journal_parent: self.open_or_create_fixed_relative_directory(&["state"])?,
            guardian_failure_memory_parent: self
                .open_or_create_fixed_relative_directory(&["guardian"])?,
            performance_operations: self
                .open_or_create_fixed_relative_directory(&["performance", "operations"])?,
            benchmark_suite_drivers: self
                .open_or_create_fixed_relative_directory(&["benchmarks", "suite-drivers"])?,
        })
    }

    pub fn reset_preflight(&self) -> io::Result<()> {
        self.with_session(RootSession::validate_reset_preflight)
    }

    pub fn begin_reset(&self) -> io::Result<ResetStartOutcome> {
        let session = self
            .session
            .lock()
            .map_err(|_| io::Error::other("application root session lock was poisoned"))?
            .take()
            .ok_or_else(|| io::Error::other("application root session is unavailable"))?;
        Ok(session.begin_reset())
    }

    pub fn restore_cancelled_reset(
        &self,
        session: RootSession,
    ) -> Result<(), AppRootSessionReinsertError> {
        if session.identity() != self.expected_identity {
            return Err(AppRootSessionReinsertError {
                kind: AppRootSessionReinsertErrorKind::ForeignIdentity,
                session,
            });
        }
        let mut current = match self.session.lock() {
            Ok(current) => current,
            Err(_) => {
                return Err(AppRootSessionReinsertError {
                    kind: AppRootSessionReinsertErrorKind::LockPoisoned,
                    session,
                });
            }
        };
        if current.is_some() {
            return Err(AppRootSessionReinsertError {
                kind: AppRootSessionReinsertErrorKind::Occupied,
                session,
            });
        }
        *current = Some(session);
        Ok(())
    }

    fn with_session<T>(
        &self,
        operation: impl FnOnce(&RootSession) -> io::Result<T>,
    ) -> io::Result<T> {
        let session = self
            .session
            .lock()
            .map_err(|_| io::Error::other("application root session lock was poisoned"))?;
        operation(
            session
                .as_ref()
                .ok_or_else(|| io::Error::other("application root session is unavailable"))?,
        )
    }

    fn open_or_create_fixed_directory(&self, fixed_name: &'static str) -> io::Result<Directory> {
        self.open_or_create_fixed_relative_directory(&[fixed_name])
    }

    fn open_or_create_fixed_relative_directory(
        &self,
        fixed_path: &[&'static str],
    ) -> io::Result<Directory> {
        let mut directory = self.root_directory()?;
        for &fixed_name in fixed_path {
            directory = open_or_create_fixed_child(directory, fixed_name)?;
        }
        Ok(directory)
    }
}

fn open_or_create_fixed_child(
    parent: Directory,
    fixed_name: &'static str,
) -> io::Result<Directory> {
    let name = LeafName::new(fixed_name).expect("fixed app directory leaf is valid");
    match parent.open_directory(&name) {
        Ok(directory) => return Ok(directory),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    match parent.create_directory(&name) {
        DirectoryCreateOutcome::Created(directory) => Ok(directory),
        DirectoryCreateOutcome::NoEffect(error)
            if error.kind() == io::ErrorKind::AlreadyExists =>
        {
            parent.open_directory(&name)
        }
        DirectoryCreateOutcome::NoEffect(error) => Err(error),
        DirectoryCreateOutcome::CreatedUnclassified {
            error,
            preservation,
        } => {
            let message = error.to_string();
            if preservation.acknowledge_preserved().is_err() {
                std::process::abort();
            }
            Err(io::Error::new(error.kind(), message))
        }
        DirectoryCreateOutcome::AppliedUnverified(obligation) => {
            match obligation.reconcile() {
                DirectoryCreateResolution::Created(directory) => Ok(directory),
                DirectoryCreateResolution::Indeterminate(_) => std::process::abort(),
            }
        }
    }
}

fn acquire_root_session(path: &Path) -> io::Result<RootSession> {
    match RootSession::acquire(path) {
        RootSessionAcquireOutcome::Acquired(session) => Ok(session),
        RootSessionAcquireOutcome::NoEffect(error) => Err(io::Error::other(error.to_string())),
        RootSessionAcquireOutcome::AppliedUnverified(obligation) => {
            let message = obligation.error().to_string();
            match obligation.reconcile() {
                RootSessionAcquireOutcome::Acquired(session) => Ok(session),
                RootSessionAcquireOutcome::NoEffect(error) => {
                    Err(io::Error::other(error.to_string()))
                }
                RootSessionAcquireOutcome::AppliedUnverified(obligation) => {
                    match obligation.cleanup() {
                        Ok(()) => Err(io::Error::other(message)),
                        Err(obligation) => match obligation.acknowledge_preserved() {
                            Ok(()) => Err(io::Error::other(message)),
                            Err(_) => std::process::abort(),
                        },
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TestRoot {
        root: PathBuf,
        paths: AppPaths,
    }

    impl TestRoot {
        fn new(name: &str) -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock after unix epoch")
                .as_nanos();
            let root = std::env::temp_dir().join(format!(
                "axial-root-session-{name}-{}-{nonce}",
                std::process::id()
            ));
            let paths = AppPaths::from_root(root.clone()).expect("absolute test app root");
            Self { root, paths }
        }

        fn paths(&self) -> AppPaths {
            self.paths.clone()
        }
    }

    impl Drop for TestRoot {
        fn drop(&mut self) {
            if let Err(error) = std::fs::remove_dir_all(&self.root)
                && error.kind() != io::ErrorKind::NotFound
            {
                if std::thread::panicking() {
                    eprintln!("failed to clean root-session test root during panic: {error}");
                } else {
                    panic!("failed to clean root-session test root: {error}");
                }
            }
        }
    }

    fn take_session(root: &AppRootSession) -> RootSession {
        root.session
            .lock()
            .expect("root session lock")
            .take()
            .expect("retained root session")
    }

    #[test]
    fn restores_a_cancelled_session_with_the_expected_identity() {
        let test_root = TestRoot::new("restore");
        let paths = test_root.paths();
        let root = paths.open_root_session().expect("open root session");
        let session = take_session(&root);

        root.restore_cancelled_reset(session)
            .expect("restore matching session");
        root.root_directory().expect("restored root directory");

        drop(root);
    }

    #[test]
    fn rejects_a_cancelled_session_with_a_foreign_identity() {
        let test_root = TestRoot::new("foreign-owner");
        let paths = test_root.paths();
        let foreign_test_root = TestRoot::new("foreign-session");
        let foreign_paths = foreign_test_root.paths();
        let root = paths.open_root_session().expect("open root session");
        let foreign = foreign_paths
            .open_root_session()
            .expect("open foreign root session");
        let error = root
            .restore_cancelled_reset(take_session(&foreign))
            .expect_err("foreign session must reject");

        assert_eq!(
            error.kind(),
            AppRootSessionReinsertErrorKind::ForeignIdentity
        );
        drop(error.into_session());
        drop((root, foreign));
    }

    #[test]
    fn rejects_reinsertion_when_the_session_slot_is_occupied() {
        let occupied_test_root = TestRoot::new("occupied-owner");
        let occupied_paths = occupied_test_root.paths();
        let candidate_test_root = TestRoot::new("occupied-candidate");
        let candidate_paths = candidate_test_root.paths();
        let occupied = occupied_paths
            .open_root_session()
            .expect("open occupied root session");
        let candidate = candidate_paths
            .open_root_session()
            .expect("open candidate root session");
        let occupied_session = take_session(&occupied);
        let candidate_session = take_session(&candidate);
        let root = AppRootSession {
            paths_lineage: Arc::clone(candidate_paths.lineage()),
            expected_identity: candidate_session.identity(),
            session: Mutex::new(Some(occupied_session)),
        };

        let error = root
            .restore_cancelled_reset(candidate_session)
            .expect_err("occupied slot must reject");
        assert_eq!(error.kind(), AppRootSessionReinsertErrorKind::Occupied);
        drop(error.into_session());

        drop((root, occupied, candidate));
    }
}
