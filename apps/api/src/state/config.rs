use super::contracts::{OwnershipClass, StabilizationSystem, TargetDescriptor, TargetKind};
use crate::execution::persistence::{
    AcceptedWrite, AtomicSnapshotWriter, PersistenceCoordinator, PersistenceError,
    PersistenceOwnerLease, WriteUrgency,
};
use axial_config::{AppConfig, AppPaths, CONFIG_MAX_BYTES, ConfigStore, ConfigStoreError};
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

const CONFIG_LOCK_INVARIANT: &str = "application config state lock poisoned";

struct ConfigPersistence {
    owner: PersistenceOwnerLease,
    writer: AtomicSnapshotWriter,
}

impl ConfigPersistence {
    fn claim(paths: &AppPaths) -> Result<Self, ConfigStoreError> {
        Self::claim_with_coordinator(paths, PersistenceCoordinator::global())
    }

    fn claim_with_coordinator(
        paths: &AppPaths,
        coordinator: PersistenceCoordinator,
    ) -> Result<Self, ConfigStoreError> {
        let owner = coordinator
            .claim_owner(&paths.config_file)
            .map_err(config_persistence_error)?;
        let writer = owner
            .writer(&paths.config_file, config_target())
            .map_err(config_persistence_error)?;
        Ok(Self { owner, writer })
    }
}

struct ConfigState {
    visible: AppConfig,
    retry_candidate: Option<(u64, AppConfig)>,
}

struct PendingConfigCommit {
    ticket: AcceptedWrite,
    revision: u64,
    candidate: AppConfig,
}

pub(super) type CommitObserver = Arc<dyn Fn(AppConfig, AppConfig) + Send + Sync>;

pub struct AppConfigStore {
    paths: AppPaths,
    mutation_allowed: bool,
    state: Arc<Mutex<ConfigState>>,
    mutation_gate: Arc<AsyncMutex<()>>,
    closed: AtomicBool,
    persistence: ConfigPersistence,
}

impl AppConfigStore {
    pub(crate) fn claim(source: &ConfigStore) -> Result<Self, ConfigStoreError> {
        let paths = source.paths().clone();
        let persistence = ConfigPersistence::claim(&paths)?;
        Ok(Self::from_parts(
            paths,
            source.current(),
            source.mutation_allowed(),
            persistence,
        ))
    }

    #[cfg(test)]
    pub(crate) fn claim_with_coordinator(
        source: &ConfigStore,
        coordinator: PersistenceCoordinator,
    ) -> Result<Self, ConfigStoreError> {
        let paths = source.paths().clone();
        let persistence = ConfigPersistence::claim_with_coordinator(&paths, coordinator)?;
        Ok(Self::from_parts(
            paths,
            source.current(),
            source.mutation_allowed(),
            persistence,
        ))
    }

    fn from_parts(
        paths: AppPaths,
        visible: AppConfig,
        mutation_allowed: bool,
        persistence: ConfigPersistence,
    ) -> Self {
        Self {
            paths,
            mutation_allowed,
            state: Arc::new(Mutex::new(ConfigState {
                visible,
                retry_candidate: None,
            })),
            mutation_gate: Arc::new(AsyncMutex::new(())),
            closed: AtomicBool::new(false),
            persistence,
        }
    }

    pub fn current(&self) -> AppConfig {
        self.state
            .lock()
            .expect(CONFIG_LOCK_INVARIANT)
            .visible
            .clone()
    }

    pub fn paths(&self) -> &AppPaths {
        &self.paths
    }

    pub(crate) async fn mutate<Mutation>(
        &self,
        mutation: Mutation,
        export_configured: bool,
        observer: CommitObserver,
    ) -> Result<AppConfig, ConfigStoreError>
    where
        Mutation: FnOnce(&mut AppConfig) -> Result<(), ConfigStoreError> + Send + 'static,
    {
        let gate = self.acquire_mutation().await?;
        self.mutate_with_gate(mutation, export_configured, observer, gate)
            .await
    }

    pub(crate) async fn acquire_mutation(&self) -> Result<OwnedMutexGuard<()>, ConfigStoreError> {
        let gate = self.mutation_gate.clone().lock_owned().await;
        if self.closed.load(Ordering::Acquire) {
            return Err(closed_config_error());
        }
        if !self.mutation_allowed {
            return Err(ConfigStoreError::Persistence(io::Error::new(
                io::ErrorKind::InvalidData,
                "config mutation is latched after startup admission failure",
            )));
        }
        Ok(gate)
    }

    pub(crate) async fn mutate_with_gate<Mutation>(
        &self,
        mutation: Mutation,
        export_configured: bool,
        observer: CommitObserver,
        gate: OwnedMutexGuard<()>,
    ) -> Result<AppConfig, ConfigStoreError>
    where
        Mutation: FnOnce(&mut AppConfig) -> Result<(), ConfigStoreError> + Send + 'static,
    {
        let gate = self.reconcile_retry(gate, observer.clone()).await?;
        let mut candidate = self.current();
        mutation(&mut candidate)?;
        let mut candidate = candidate.normalized()?;
        ensure_telemetry_install_id(&mut candidate, export_configured);
        let candidate = candidate.normalized()?;
        if candidate == self.current() {
            drop(gate);
            return Ok(candidate);
        }
        self.commit(candidate, gate, observer).await
    }

    pub(crate) async fn close(&self, observer: CommitObserver) -> Result<(), ConfigStoreError> {
        let gate = self.mutation_gate.clone().lock_owned().await;
        if self.closed.load(Ordering::Acquire) {
            return Ok(());
        }
        let _gate = self.reconcile_retry(gate, observer).await?;
        self.persistence
            .owner
            .close()
            .await
            .map_err(config_persistence_error)?;
        self.closed.store(true, Ordering::Release);
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn replace_for_test(&self, next: AppConfig) -> Result<(), ConfigStoreError> {
        self.state.lock().expect(CONFIG_LOCK_INVARIANT).visible = next.normalized()?;
        Ok(())
    }

    async fn reconcile_retry(
        &self,
        gate: OwnedMutexGuard<()>,
        observer: CommitObserver,
    ) -> Result<OwnedMutexGuard<()>, ConfigStoreError> {
        let retained = self
            .state
            .lock()
            .expect(CONFIG_LOCK_INVARIANT)
            .retry_candidate
            .clone();
        let Some((candidate_revision, candidate)) = retained else {
            return Ok(gate);
        };
        let ticket = self
            .persistence
            .writer
            .retry()
            .map_err(config_persistence_error)?;
        let revision = ticket.revision().get();
        assert_eq!(
            revision, candidate_revision,
            "application config retry revision diverged from retained candidate"
        );
        self.await_commit(
            PendingConfigCommit {
                ticket,
                revision,
                candidate,
            },
            gate,
            observer,
        )
        .await
        .map(|(_, gate)| gate)
    }

    async fn commit(
        &self,
        candidate: AppConfig,
        gate: OwnedMutexGuard<()>,
        observer: CommitObserver,
    ) -> Result<AppConfig, ConfigStoreError> {
        let (candidate, encoded) = encode_config(candidate).await?;
        let ticket = self
            .persistence
            .writer
            .accept(encoded, WriteUrgency::Immediate, Ok)
            .map_err(config_persistence_error)?;
        let revision = ticket.revision().get();
        let (candidate, gate) = self
            .await_commit(
                PendingConfigCommit {
                    ticket,
                    revision,
                    candidate,
                },
                gate,
                observer,
            )
            .await?;
        drop(gate);
        Ok(candidate)
    }

    async fn await_commit(
        &self,
        commit: PendingConfigCommit,
        gate: OwnedMutexGuard<()>,
        observer: CommitObserver,
    ) -> Result<(AppConfig, OwnedMutexGuard<()>), ConfigStoreError> {
        let state = self.state.clone();
        let (completed_tx, completed_rx) = tokio::sync::oneshot::channel();
        commit.ticket.observe(move |result| {
            let result = match result {
                Ok(_) => {
                    let (previous, current) = {
                        let mut state = state.lock().expect(CONFIG_LOCK_INVARIANT);
                        let previous = state.visible.clone();
                        state.visible = commit.candidate.clone();
                        if state
                            .retry_candidate
                            .as_ref()
                            .is_some_and(|(_, candidate)| candidate == &commit.candidate)
                        {
                            state.retry_candidate = None;
                        }
                        (previous, state.visible.clone())
                    };
                    observer(previous, current.clone());
                    Ok(current)
                }
                Err(error) => {
                    if matches!(error, PersistenceError::Write { .. }) {
                        state.lock().expect(CONFIG_LOCK_INVARIANT).retry_candidate =
                            Some((commit.revision, commit.candidate));
                    }
                    Err(config_persistence_error(error))
                }
            };
            let _ = completed_tx.send((result, gate));
        });
        let (result, gate) = completed_rx.await.map_err(|_| {
            ConfigStoreError::Persistence(io::Error::other(
                "application config commit observer stopped before reporting completion",
            ))
        })?;
        result.map(|config| (config, gate))
    }
}

impl crate::observability::telemetry::TelemetryConfigSource for AppConfigStore {
    fn current(&self) -> AppConfig {
        AppConfigStore::current(self)
    }
}

fn ensure_telemetry_install_id(config: &mut AppConfig, export_configured: bool) {
    if config.telemetry_enabled
        && export_configured
        && config.telemetry_install_id.trim().is_empty()
    {
        config.telemetry_install_id = uuid::Uuid::new_v4().to_string();
    }
}

async fn encode_config(config: AppConfig) -> Result<(AppConfig, Vec<u8>), ConfigStoreError> {
    tokio::task::spawn_blocking(move || {
        let encoded = serde_json::to_vec_pretty(&config).map_err(|error| {
            ConfigStoreError::Persistence(io::Error::new(io::ErrorKind::InvalidData, error))
        })?;
        if encoded.len() as u64 > CONFIG_MAX_BYTES {
            return Err(ConfigStoreError::TooLarge {
                max_bytes: CONFIG_MAX_BYTES,
            });
        }
        Ok((config, encoded))
    })
    .await
    .map_err(|error| {
        ConfigStoreError::Persistence(io::Error::other(format!(
            "application config encoder stopped: {error}"
        )))
    })?
}

fn config_target() -> TargetDescriptor {
    TargetDescriptor::new(
        StabilizationSystem::State,
        TargetKind::Config,
        "launcher_config",
        OwnershipClass::LauncherManaged,
    )
}

fn config_persistence_error(error: impl Into<io::Error>) -> ConfigStoreError {
    ConfigStoreError::Persistence(error.into())
}

fn closed_config_error() -> ConfigStoreError {
    ConfigStoreError::Persistence(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "application config persistence is closed",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::persistence::AtomicWriteBackend;
    use crate::state::{AppState, AppStateInit, InstallStore, SessionStore};
    use axial_config::{InstanceRegistrySnapshot, InstanceStore};
    use axial_performance::PerformanceManager;
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Condvar, Mutex};
    use std::time::Duration;
    use tokio::sync::Notify;

    struct RecordingBackend {
        attempts: AtomicUsize,
        failures: AtomicUsize,
        started: Notify,
        gate: Mutex<Option<Arc<WriteGate>>>,
        attempted: Mutex<Vec<Vec<u8>>>,
        committed: Mutex<Vec<Vec<u8>>>,
    }

    struct WriteGate {
        released: Mutex<bool>,
        changed: Condvar,
    }

    impl RecordingBackend {
        fn new(failures: usize) -> Arc<Self> {
            Arc::new(Self {
                attempts: AtomicUsize::new(0),
                failures: AtomicUsize::new(failures),
                started: Notify::new(),
                gate: Mutex::new(None),
                attempted: Mutex::new(Vec::new()),
                committed: Mutex::new(Vec::new()),
            })
        }

        fn gate_next(&self) -> Arc<WriteGate> {
            let gate = Arc::new(WriteGate {
                released: Mutex::new(false),
                changed: Condvar::new(),
            });
            *self.gate.lock().expect("backend gate lock") = Some(gate.clone());
            gate
        }

        async fn wait_for_attempt(&self, expected: usize) {
            loop {
                let started = self.started.notified();
                if self.attempts.load(Ordering::SeqCst) >= expected {
                    return;
                }
                started.await;
            }
        }
    }

    impl AtomicWriteBackend for RecordingBackend {
        fn write(
            &self,
            _target: &TargetDescriptor,
            _destination: &Path,
            contents: &[u8],
        ) -> io::Result<()> {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            self.attempted
                .lock()
                .expect("attempted config lock")
                .push(contents.to_vec());
            self.started.notify_one();
            if let Some(gate) = self.gate.lock().expect("backend gate lock").take() {
                gate.wait();
            }
            if self
                .failures
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |failures| {
                    (failures > 0).then(|| failures - 1)
                })
                .is_ok()
            {
                return Err(io::Error::other("injected config write failure"));
            }
            self.committed
                .lock()
                .expect("committed config lock")
                .push(contents.to_vec());
            Ok(())
        }
    }

    impl WriteGate {
        fn wait(&self) {
            let mut released = self.released.lock().expect("write gate lock");
            while !*released {
                released = self.changed.wait(released).expect("wait on write gate");
            }
        }

        fn release(&self) {
            *self.released.lock().expect("write gate lock") = true;
            self.changed.notify_all();
        }
    }

    #[tokio::test]
    async fn concurrent_mutations_derive_from_latest_committed_config() {
        let (store, backend) = test_store("concurrent", 0);
        let first = {
            let store = store.clone();
            tokio::spawn(async move {
                store
                    .mutate(
                        |config| {
                            config.theme = "night".to_string();
                            Ok(())
                        },
                        false,
                        no_op_observer(),
                    )
                    .await
            })
        };
        let second = {
            let store = store.clone();
            tokio::spawn(async move {
                store
                    .mutate(
                        |config| {
                            config.music_volume = Some(37);
                            Ok(())
                        },
                        false,
                        no_op_observer(),
                    )
                    .await
            })
        };

        first
            .await
            .expect("first mutation task")
            .expect("first mutation");
        second
            .await
            .expect("second mutation task")
            .expect("second mutation");
        let current = store.current();
        assert_eq!(current.theme, "night");
        assert_eq!(current.music_volume, Some(37));
        assert_eq!(backend.attempts.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn accepted_commit_survives_waiter_cancellation() {
        let (store, backend) = test_store("cancellation", 0);
        let gate = backend.gate_next();
        let first_store = store.clone();
        let first = tokio::spawn(async move {
            first_store
                .mutate(
                    |config| {
                        config.theme = "owned".to_string();
                        Ok(())
                    },
                    false,
                    no_op_observer(),
                )
                .await
        });
        backend.wait_for_attempt(1).await;
        first.abort();
        assert!(first.await.expect_err("cancel waiter").is_cancelled());
        gate.release();

        store
            .mutate(
                |config| {
                    config.music_track = 4;
                    Ok(())
                },
                false,
                no_op_observer(),
            )
            .await
            .expect("successor waits for owned commit");
        assert_eq!(store.current().theme, "owned");
        assert_eq!(store.current().music_track, 4);
    }

    #[tokio::test]
    async fn cancellation_before_app_state_admission_drops_mutation_before_close() {
        let state = test_app_state("pre-admission-cancellation");
        let gate = state
            .config()
            .acquire_mutation()
            .await
            .expect("hold config mutation gate");
        let waiting_state = state.clone();
        let waiting = tokio::spawn(async move {
            waiting_state
                .mutate_config(|config| {
                    config.theme = "must-not-commit".to_string();
                    Ok(())
                })
                .await
        });
        tokio::task::yield_now().await;
        waiting.abort();
        assert!(
            waiting
                .await
                .expect_err("cancel waiting mutation")
                .is_cancelled()
        );
        drop(gate);

        state
            .close_config()
            .await
            .expect("close after canceled pre-admission mutation");
        assert!(state.config().current().theme.is_empty());
    }

    #[tokio::test]
    async fn failed_exact_bytes_commit_before_successor_derivation() {
        let (store, backend) = test_store("retry", 1);
        let first = store
            .mutate(
                |config| {
                    config.theme = "retained".to_string();
                    Ok(())
                },
                false,
                no_op_observer(),
            )
            .await;
        assert!(matches!(first, Err(ConfigStoreError::Persistence(_))));
        assert!(store.current().theme.is_empty());

        store
            .mutate(
                |config| {
                    config.music_volume = Some(19);
                    Ok(())
                },
                false,
                no_op_observer(),
            )
            .await
            .expect("successor reconciles retained bytes");

        let attempted = backend.attempted.lock().expect("attempted config lock");
        assert_eq!(attempted.len(), 3);
        assert_eq!(attempted[0], attempted[1]);
        assert_ne!(attempted[1], attempted[2]);
        drop(attempted);

        let committed = backend.committed.lock().expect("committed config lock");
        assert_eq!(committed.len(), 2);
        let retained: AppConfig = serde_json::from_slice(&committed[0]).expect("retained config");
        let successor: AppConfig = serde_json::from_slice(&committed[1]).expect("successor config");
        assert_eq!(retained.theme, "retained");
        assert_eq!(retained.music_volume, None);
        assert_eq!(successor.theme, "retained");
        assert_eq!(successor.music_volume, Some(19));
    }

    #[tokio::test]
    async fn close_retries_retained_bytes_before_rejecting_later_mutations() {
        let (store, backend) = test_store("close-retry", 1);
        let first = store
            .mutate(
                |config| {
                    config.theme = "retained-for-close".to_string();
                    Ok(())
                },
                false,
                no_op_observer(),
            )
            .await;
        assert!(matches!(first, Err(ConfigStoreError::Persistence(_))));

        store
            .close(no_op_observer())
            .await
            .expect("close retries retained config");
        assert_eq!(store.current().theme, "retained-for-close");
        assert_eq!(backend.attempts.load(Ordering::SeqCst), 2);

        let after_close = store
            .mutate(
                |config| {
                    config.music_track = 9;
                    Ok(())
                },
                false,
                no_op_observer(),
            )
            .await;
        assert!(matches!(after_close, Err(ConfigStoreError::Persistence(_))));
        assert_eq!(store.current().music_track, 0);
    }

    #[tokio::test]
    async fn concurrent_telemetry_identity_creation_commits_one_configured_identity() {
        let (store, backend) = test_store("telemetry-id", 0);
        store
            .mutate(
                |config| {
                    config.telemetry_enabled = true;
                    Ok(())
                },
                false,
                no_op_observer(),
            )
            .await
            .expect("enable keyless telemetry");
        assert!(store.current().telemetry_install_id.is_empty());

        let first = {
            let store = store.clone();
            tokio::spawn(async move { store.mutate(|_| Ok(()), true, no_op_observer()).await })
        };
        let second = {
            let store = store.clone();
            tokio::spawn(async move { store.mutate(|_| Ok(()), true, no_op_observer()).await })
        };
        first
            .await
            .expect("first identity task")
            .expect("first identity mutation");
        second
            .await
            .expect("second identity task")
            .expect("second identity mutation");

        let identity = store.current().telemetry_install_id;
        assert_eq!(identity.len(), 36);
        assert_eq!(backend.attempts.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn oversized_candidate_is_rejected_before_acceptance() {
        let (store, backend) = test_store("oversized", 0);
        let result = store
            .mutate(
                |config| {
                    config.theme = "x".repeat(CONFIG_MAX_BYTES as usize);
                    Ok(())
                },
                false,
                no_op_observer(),
            )
            .await;

        assert!(matches!(result, Err(ConfigStoreError::TooLarge { .. })));
        assert_eq!(backend.attempts.load(Ordering::SeqCst), 0);
        assert_eq!(store.current(), AppConfig::default());

        store
            .mutate(
                |config| {
                    config.theme = "night".to_string();
                    Ok(())
                },
                false,
                no_op_observer(),
            )
            .await
            .expect("unaccepted oversized candidate must not latch retry");
        assert_eq!(backend.attempts.load(Ordering::SeqCst), 1);
    }

    fn test_store(name: &str, failures: usize) -> (Arc<AppConfigStore>, Arc<RecordingBackend>) {
        let paths = test_paths(name);
        let source = ConfigStore::from_config(paths, AppConfig::default()).expect("config source");
        let backend = RecordingBackend::new(failures);
        let coordinator =
            PersistenceCoordinator::for_test(backend.clone(), Duration::ZERO, Duration::ZERO);
        let store = AppConfigStore::claim_with_coordinator(&source, coordinator)
            .expect("claim config store");
        (Arc::new(store), backend)
    }

    fn test_app_state(name: &str) -> AppState {
        let paths = test_paths(name);
        let config = Arc::new(
            ConfigStore::from_config(paths.clone(), AppConfig::default()).expect("config source"),
        );
        AppState::new(AppStateInit {
            app_name: "Axial".to_string(),
            version: "test".to_string(),
            instances: Arc::new(
                InstanceStore::from_snapshot(paths.clone(), InstanceRegistrySnapshot::default())
                    .expect("instance store"),
            ),
            installs: Arc::new(InstallStore::new()),
            sessions: Arc::new(SessionStore::new()),
            performance: Arc::new(PerformanceManager::new().expect("performance manager")),
            config,
            startup_warnings: Vec::new(),
            frontend_dir: paths.config_dir.join("frontend"),
        })
    }

    fn no_op_observer() -> CommitObserver {
        Arc::new(|_, _| {})
    }

    fn test_paths(name: &str) -> AppPaths {
        let root =
            std::env::temp_dir().join(format!("axial-config-state-{name}-{}", std::process::id()));
        let config_dir = root.join("config");
        AppPaths {
            config_file: config_dir.join("config.json"),
            instances_file: config_dir.join("instances.json"),
            instances_dir: root.join("instances"),
            music_dir: root.join("music"),
            library_dir: root.join("library"),
            config_dir,
        }
    }
}
