use std::io;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::models::AppConfig;
use crate::paths::{AppPaths, AppPathsLineage};
use axial_fs::{
    Directory, DirectoryCreateOutcome, DirectoryCreateResolution, DirectoryIdentity, LeafName,
    ResetDrainAuthority, ResetDrainFailure, ResetDrainRecovery, ResetStartOutcome, RootClearFailure,
    RootClearOutcome, RootClearReceipt, RootResetAuthority, RootSession, RootSessionAcquireOutcome,
};

const RESET_SETTLEMENT_MAX_PROBES: usize = 8;
const RESET_SETTLEMENT_INITIAL_DELAY: Duration = Duration::from_millis(25);
const RESET_SETTLEMENT_MAX_DELAY: Duration = Duration::from_millis(250);

pub struct AppRootSession {
    paths_lineage: Arc<AppPathsLineage>,
    expected_identity: DirectoryIdentity,
    session: Mutex<Option<RootSession>>,
    reset_retry: Arc<Mutex<Option<AppRootResetRetry>>>,
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

#[must_use = "root reset authority must clear or explicitly preserve the owned root"]
pub struct AppRootResetAuthority {
    authority: AppRootResetAttempt,
    reset_retry: Arc<Mutex<Option<AppRootResetRetry>>>,
}

enum AppRootResetAttempt {
    Fresh(RootResetAuthority),
    Retry(RootClearFailure),
}

enum AppRootResetRetry {
    Pending(ResetDrainAuthority),
    Recovery(ResetDrainRecovery),
    Failed(ResetDrainFailure),
    Clear(RootClearFailure),
}

impl std::fmt::Debug for AppRootResetAuthority {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AppRootResetAuthority")
            .finish_non_exhaustive()
    }
}

#[must_use = "a cleared root receipt must release terminal reset ownership before relaunch"]
pub struct AppRootClearReceipt {
    receipt: RootClearReceipt,
}

impl std::fmt::Debug for AppRootClearReceipt {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AppRootClearReceipt")
            .finish_non_exhaustive()
    }
}

impl AppRootResetAuthority {
    pub fn clear_owned_root(self) -> io::Result<AppRootClearReceipt> {
        let outcome = match self.authority {
            AppRootResetAttempt::Fresh(authority) => authority.clear_root(),
            AppRootResetAttempt::Retry(failure) => failure.retry(),
        };
        match outcome {
            RootClearOutcome::Cleared(receipt) => Ok(AppRootClearReceipt { receipt }),
            RootClearOutcome::Failed(failure) => {
                let error = copy_io_error(failure.error());
                retain_reset_retry(&self.reset_retry, AppRootResetRetry::Clear(failure));
                Err(error)
            }
        }
    }
}

impl AppRootClearReceipt {
    pub fn release(self) -> Result<(), Self> {
        match self.receipt.release() {
            Ok(()) => Ok(()),
            Err(receipt) => Err(Self { receipt }),
        }
    }
}

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
            reset_retry: Arc::new(Mutex::new(None)),
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

    pub fn reset_preflight(&self, paths: &AppPaths, config: &AppConfig) -> io::Result<()> {
        self.validate_paths(paths)?;
        let external_library = reset_library_preflight(paths, config)?;
        if reset_retry_is_retained(&self.reset_retry) {
            return Ok(());
        }
        self.with_session(|session| {
            session.validate_reset_preflight()?;
            if let Some(external_library) = external_library {
                session.validate_absolute_directory_outside_root(external_library)?;
            }
            Ok(())
        })
    }

    pub async fn begin_reset(self: &Arc<Self>) -> io::Result<AppRootResetAuthority> {
        let retry = take_reset_retry(&self.reset_retry);
        if retry.is_some() {
            let session = match self.session.lock() {
                Ok(session) => session,
                Err(_) => std::process::abort(),
            };
            if session.is_some() {
                std::process::abort();
            }
            drop(session);
        }
        let mut probes = 0;
        let mut outcome = match retry {
            Some(AppRootResetRetry::Clear(failure)) => {
                return Ok(AppRootResetAuthority {
                    authority: AppRootResetAttempt::Retry(failure),
                    reset_retry: Arc::clone(&self.reset_retry),
                });
            }
            Some(AppRootResetRetry::Pending(drain)) => ResetStartOutcome::Pending(drain),
            Some(AppRootResetRetry::Recovery(recovery)) => {
                ResetStartOutcome::Recovery { recovery }
            }
            Some(AppRootResetRetry::Failed(failure)) => {
                tokio::time::sleep(reset_settlement_probe_delay(probes)).await;
                probes += 1;
                failure.retry()
            }
            None => {
                let session = self
                    .session
                    .lock()
                    .map_err(|_| io::Error::other("application root session lock was poisoned"))?
                    .take()
                    .ok_or_else(|| io::Error::other("application root session is unavailable"))?;
                session.begin_reset()
            }
        };
        loop {
            outcome = match outcome {
                ResetStartOutcome::Ready(authority) => {
                    return Ok(AppRootResetAuthority {
                        authority: AppRootResetAttempt::Fresh(authority),
                        reset_retry: Arc::clone(&self.reset_retry),
                    });
                }
                ResetStartOutcome::Pending(drain)
                    if probes == RESET_SETTLEMENT_MAX_PROBES =>
                {
                    retain_reset_retry(&self.reset_retry, AppRootResetRetry::Pending(drain));
                    return Err(reset_settlement_would_block());
                }
                ResetStartOutcome::Pending(drain) => {
                    tokio::time::sleep(reset_settlement_probe_delay(probes)).await;
                    probes += 1;
                    drain.try_settle()
                }
                ResetStartOutcome::Recovery { recovery }
                    if probes == RESET_SETTLEMENT_MAX_PROBES =>
                {
                    retain_reset_retry(&self.reset_retry, AppRootResetRetry::Recovery(recovery));
                    return Err(reset_settlement_would_block());
                }
                ResetStartOutcome::Recovery { recovery }
                    if recovery.file_count() > 0 || recovery.directory_count() > 0 =>
                {
                    tokio::time::sleep(reset_settlement_probe_delay(probes)).await;
                    probes += 1;
                    recovery.remove_all()
                }
                ResetStartOutcome::Recovery { recovery } => {
                    tokio::time::sleep(reset_settlement_probe_delay(probes)).await;
                    probes += 1;
                    match recovery.acknowledge_external() {
                        ResetStartOutcome::Recovery { recovery } => {
                            recovery.defer_managed_reset()
                        }
                        outcome => outcome,
                    }
                }
                ResetStartOutcome::Refused(failure) => {
                    let error = copy_io_error(failure.error());
                    self.restore_cancelled_reset(failure.cancel_reset());
                    return Err(error);
                }
                ResetStartOutcome::Failed(failure) => {
                    let error = copy_io_error(failure.error());
                    retain_reset_retry(&self.reset_retry, AppRootResetRetry::Failed(failure));
                    return Err(error);
                }
            };
        }
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

    fn restore_cancelled_reset(&self, session: RootSession) {
        if session.identity() != self.expected_identity {
            std::process::abort();
        }
        let mut current = match self.session.lock() {
            Ok(current) => current,
            Err(_) => std::process::abort(),
        };
        if current.is_some() {
            std::process::abort();
        }
        *current = Some(session);
    }
}

fn copy_io_error(error: &io::Error) -> io::Error {
    io::Error::new(error.kind(), error.to_string())
}

fn reset_settlement_probe_delay(probe: usize) -> Duration {
    debug_assert!(probe < RESET_SETTLEMENT_MAX_PROBES);
    let multiplier = 1_u32 << u32::try_from(probe).expect("reset probe index fits u32");
    RESET_SETTLEMENT_INITIAL_DELAY
        .saturating_mul(multiplier)
        .min(RESET_SETTLEMENT_MAX_DELAY)
}

fn reset_settlement_would_block() -> io::Error {
    io::Error::new(
        io::ErrorKind::WouldBlock,
        "application root reset remains unsettled; retry the same terminal intent",
    )
}

fn reset_retry_is_retained(reset_retry: &Mutex<Option<AppRootResetRetry>>) -> bool {
    match reset_retry.lock() {
        Ok(reset_retry) => reset_retry.is_some(),
        Err(_) => std::process::abort(),
    }
}

fn take_reset_retry(
    reset_retry: &Mutex<Option<AppRootResetRetry>>,
) -> Option<AppRootResetRetry> {
    match reset_retry.lock() {
        Ok(mut reset_retry) => reset_retry.take(),
        Err(_) => std::process::abort(),
    }
}

fn retain_reset_retry(
    reset_retry: &Mutex<Option<AppRootResetRetry>>,
    retry: AppRootResetRetry,
) {
    let mut reset_retry = match reset_retry.lock() {
        Ok(reset_retry) => reset_retry,
        Err(_) => std::process::abort(),
    };
    if reset_retry.is_some() {
        std::process::abort();
    }
    *reset_retry = Some(retry);
}

fn reset_library_preflight<'a>(
    paths: &AppPaths,
    config: &'a AppConfig,
) -> io::Result<Option<&'a Path>> {
    let configured_library = config.library_dir.trim();
    match config.library_mode.as_str() {
        "managed" => {
            if !configured_library.is_empty()
                && Path::new(configured_library) != paths.library_dir()
            {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "managed library is outside its application-owned location",
                ));
            }
            Ok(None)
        }
        "existing" if configured_library.is_empty() => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "existing library location is empty",
        )),
        "existing" => Ok(Some(Path::new(configured_library))),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "library ownership mode is invalid",
        )),
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
    #[cfg(unix)]
    use axial_fs::DirectoryParkOutcome;
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

    #[tokio::test]
    async fn terminal_reset_clears_owned_children_before_releasing_authority() {
        let test_root = TestRoot::new("terminal-reset");
        let paths = test_root.paths();
        let root = Arc::new(paths.open_root_session().expect("open root session"));
        let marker = test_root.root.join("state.json");
        std::fs::write(&marker, b"state").expect("write managed marker");

        root.reset_preflight(&paths, &AppConfig::default())
            .expect("reset preflight");
        let authority = root.begin_reset().await.expect("settled reset authority");
        let receipt = authority.clear_owned_root().expect("clear owned root");
        receipt.release().expect("release reset authority");

        assert!(!marker.exists());
        assert!(root.root_directory().is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn failed_clear_retains_the_exact_authority_for_retry() {
        let test_root = TestRoot::new("terminal-reset-retry");
        let paths = test_root.paths();
        let root = Arc::new(paths.open_root_session().expect("open root session"));
        let marker = test_root.root.join("state.json");
        let parked = test_root.root.with_extension("parked");
        std::fs::write(&marker, b"state").expect("write managed marker");

        root.reset_preflight(&paths, &AppConfig::default())
            .expect("initial reset preflight");
        let authority = root.begin_reset().await.expect("initial reset authority");
        std::fs::rename(&test_root.root, &parked).expect("park exact root");
        std::fs::create_dir(&test_root.root).expect("create replacement root");

        authority
            .clear_owned_root()
            .expect_err("replacement root must reject");
        assert!(root.root_directory().is_err());

        std::fs::remove_dir(&test_root.root).expect("remove replacement root");
        std::fs::rename(&parked, &test_root.root).expect("restore exact root");
        root.reset_preflight(&paths, &AppConfig::default())
            .expect("retained retry owns physical preflight");
        let authority = root.begin_reset().await.expect("retained reset retry");
        let receipt = authority.clear_owned_root().expect("retry clear");
        receipt.release().expect("release retried reset authority");

        assert!(!marker.exists());
    }

    #[tokio::test]
    async fn active_reader_refuses_reset_and_restores_the_live_session() {
        let test_root = TestRoot::new("terminal-reset-active-reader");
        let paths = test_root.paths();
        let root = Arc::new(paths.open_root_session().expect("open root session"));
        let marker = test_root.root.join("state.json");
        std::fs::write(&marker, b"state").expect("write managed marker");
        let root_directory = root.root_directory().expect("root directory");
        let marker_file = root_directory
            .open_file(&LeafName::new("state.json").expect("marker leaf"))
            .expect("marker capability");
        let reader = marker_file.reader(16).expect("active marker reader");

        root.reset_preflight(&paths, &AppConfig::default())
            .expect("initial reset preflight");
        assert_eq!(
            root.begin_reset()
                .await
                .expect_err("active reader must refuse reset")
                .kind(),
            io::ErrorKind::WouldBlock
        );
        root.root_directory()
            .expect("reset refusal must restore the live root session");

        drop(reader);
        root.reset_preflight(&paths, &AppConfig::default())
            .expect("retry reset preflight");
        let authority = root.begin_reset().await.expect("retry reset authority");
        let receipt = authority.clear_owned_root().expect("clear after retry");
        receipt.release().expect("release reset authority");

        assert!(!marker.exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unsettled_recovery_is_bounded_and_retries_the_exact_authority() {
        let test_root = TestRoot::new("terminal-reset-recovery-retry");
        let paths = test_root.paths();
        let root = Arc::new(paths.open_root_session().expect("open root session"));
        let root_directory = root.root_directory().expect("root directory");
        let managed_name = LeafName::new("managed").expect("managed leaf");
        let park_name = LeafName::new("managed.parked").expect("park leaf");
        std::fs::create_dir(test_root.root.join("managed")).expect("managed directory");
        let managed = root_directory
            .open_directory(&managed_name)
            .expect("managed directory capability");
        let parked = match managed.park_as(park_name) {
            DirectoryParkOutcome::Parked(parked) => parked,
            DirectoryParkOutcome::NoEffect { error, .. } => {
                panic!("directory park had no effect: {error}")
            }
            DirectoryParkOutcome::AppliedUnverified(obligation) => {
                panic!("directory park was unverified: {}", obligation.error())
            }
        };
        drop(parked);

        let parked_path = test_root.root.join("managed.parked");
        let displaced_path = test_root.root.join("managed.displaced");
        std::fs::rename(&parked_path, &displaced_path).expect("displace exact parked directory");
        std::fs::create_dir(&parked_path).expect("create replacement parked directory");

        root.reset_preflight(&paths, &AppConfig::default())
            .expect("initial reset preflight");
        assert_eq!(
            root.begin_reset()
                .await
                .expect_err("unsettled recovery must exhaust its bounded probe budget")
                .kind(),
            io::ErrorKind::WouldBlock
        );
        assert!(root.root_directory().is_err());

        std::fs::remove_dir(&parked_path).expect("remove replacement parked directory");
        std::fs::rename(&displaced_path, &parked_path).expect("restore exact parked directory");
        root.reset_preflight(&paths, &AppConfig::default())
            .expect("retained recovery owns reset preflight");
        let authority = root.begin_reset().await.expect("settled recovery retry");
        let receipt = authority.clear_owned_root().expect("clear recovered root");
        receipt.release().expect("release recovered reset authority");

        assert!(!parked_path.exists());
    }

    #[test]
    fn reset_preflight_rejects_user_library_inside_owned_root() {
        let test_root = TestRoot::new("nested-existing-library");
        let paths = test_root.paths();
        let root = Arc::new(paths.open_root_session().expect("open root session"));
        let nested = test_root.root.join("user-library");
        std::fs::create_dir(&nested).expect("nested user library");
        let config = AppConfig {
            library_dir: nested.to_string_lossy().into_owned(),
            library_mode: "existing".to_string(),
            ..AppConfig::default()
        };

        assert_eq!(
            root.reset_preflight(&paths, &config)
                .expect_err("nested user library must reject")
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        assert!(nested.exists());
    }

    #[tokio::test]
    async fn reset_preserves_external_existing_library() {
        let test_root = TestRoot::new("external-existing-library");
        let paths = test_root.paths();
        let root = Arc::new(paths.open_root_session().expect("open root session"));
        let external = test_root.root.with_extension("external-library");
        let external_marker = external.join("user-owned.bin");
        std::fs::create_dir(&external).expect("external user library");
        std::fs::write(&external_marker, b"user-owned").expect("external marker");
        let config = AppConfig {
            library_dir: external.to_string_lossy().into_owned(),
            library_mode: "existing".to_string(),
            ..AppConfig::default()
        };

        root.reset_preflight(&paths, &config)
            .expect("external library preflight");
        let authority = root.begin_reset().await.expect("settled reset authority");
        let receipt = authority.clear_owned_root().expect("clear owned root");
        receipt.release().expect("release reset authority");

        assert_eq!(
            std::fs::read(&external_marker).expect("external marker survives"),
            b"user-owned"
        );
        std::fs::remove_dir_all(external).expect("cleanup external library");
    }

    #[test]
    fn reset_preflight_rejects_invalid_library_ownership_shapes() {
        let test_root = TestRoot::new("invalid-library-ownership");
        let paths = test_root.paths();
        let root = Arc::new(paths.open_root_session().expect("open root session"));
        let mismatched_managed = AppConfig {
            library_dir: test_root.root.join("other").to_string_lossy().into_owned(),
            ..AppConfig::default()
        };
        let empty_existing = AppConfig {
            library_mode: "existing".to_string(),
            ..AppConfig::default()
        };
        let unknown = AppConfig {
            library_mode: "legacy".to_string(),
            ..AppConfig::default()
        };

        assert!(root.reset_preflight(&paths, &mismatched_managed).is_err());
        assert!(root.reset_preflight(&paths, &empty_existing).is_err());
        assert!(root.reset_preflight(&paths, &unknown).is_err());
    }
}
