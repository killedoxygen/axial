use crate::execution::anchored_record::{AnchoredRecordDirectory, AnchoredRecordObservation};
use crate::execution::persistence::{
    AcceptedWrite, AtomicSnapshotWriter, PersistenceCoordinator, PersistenceError,
    PersistenceOwnerLease, WriteUrgency,
};
use axial_config::{
    AppConfig, AppPaths, CONFIG_MAX_BYTES, ConfigStore, ConfigStoreError,
    StartupFileProvenance,
};
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

const CONFIG_LOCK_INVARIANT: &str = "application config state lock poisoned";

struct ConfigPersistence {
    owner: PersistenceOwnerLease,
    writer: AtomicSnapshotWriter,
}

impl ConfigPersistence {
    fn claim(
        directory: AnchoredRecordDirectory,
    ) -> Result<Self, ConfigStoreError> {
        Self::claim_with_coordinator(directory, PersistenceCoordinator::global())
    }

    fn claim_with_coordinator(
        directory: AnchoredRecordDirectory,
        coordinator: PersistenceCoordinator,
    ) -> Result<Self, ConfigStoreError> {
        let record = directory
            .target(std::ffi::OsStr::new("config.json"), CONFIG_MAX_BYTES)
            .map_err(ConfigStoreError::Persistence)?;
        let owner = coordinator
            .claim_record(record.clone())
            .map_err(config_persistence_error)?;
        let writer = owner
            .writer(record)
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ConfigCommitAdmissionContext {
    NewCandidate,
    RetainedRetry,
}

pub(super) type ConfigCommitAdmissionFuture<Admission> = Pin<
    Box<dyn Future<Output = Result<Option<Admission>, ConfigStoreError>> + Send + 'static>,
>;

pub(super) trait ConfigCommitAdmission: Send + 'static {
    type Committed;

    fn commit(self) -> Self::Committed;
}

impl ConfigCommitAdmission for () {
    type Committed = ();

    fn commit(self) -> Self::Committed {}
}

pub struct AppConfigStore {
    paths: AppPaths,
    mutation_allowed: bool,
    state: Arc<Mutex<ConfigState>>,
    mutation_gate: Arc<AsyncMutex<()>>,
    closed: AtomicBool,
    persistence: ConfigPersistence,
}

impl AppConfigStore {
    pub(crate) fn claim(
        source: &ConfigStore,
        directory: AnchoredRecordDirectory,
    ) -> Result<Self, ConfigStoreError> {
        let paths = source.paths().clone();
        admit_config_source(source, &directory)?;
        let persistence = ConfigPersistence::claim(directory)?;
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
        let root_session = Arc::clone(source.root_session());
        let directory = AnchoredRecordDirectory::from_directory(
            root_session.clone(),
            root_session.root_directory().map_err(ConfigStoreError::Root)?,
        );
        admit_config_source(source, &directory)?;
        let persistence = ConfigPersistence::claim_with_coordinator(directory, coordinator)?;
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
        self.mutate_with_gate_admitted(
            mutation,
            export_configured,
            observer,
            |_, _, _| Box::pin(async { Ok(None::<()>) }),
            gate,
        )
        .await
    }

    pub(super) async fn mutate_with_gate_admitted<Mutation, Admit, Admission>(
        &self,
        mutation: Mutation,
        export_configured: bool,
        observer: CommitObserver,
        admit: Admit,
        gate: OwnedMutexGuard<()>,
    ) -> Result<AppConfig, ConfigStoreError>
    where
        Mutation: FnOnce(&mut AppConfig) -> Result<(), ConfigStoreError> + Send + 'static,
        Admit: Fn(
                ConfigCommitAdmissionContext,
                AppConfig,
                AppConfig,
            ) -> ConfigCommitAdmissionFuture<Admission>
            + Send
            + Sync,
        Admission: ConfigCommitAdmission,
    {
        let gate = self.reconcile_retry(gate, observer.clone(), &admit).await?;
        let mut candidate = self.current();
        mutation(&mut candidate)?;
        let mut candidate = candidate.normalized()?;
        ensure_telemetry_install_id(&mut candidate, export_configured);
        let candidate = candidate.normalized()?;
        if candidate == self.current() {
            drop(gate);
            return Ok(candidate);
        }
        self.commit(candidate, gate, observer, &admit).await
    }

    #[cfg(test)]
    pub(crate) async fn close(&self, observer: CommitObserver) -> Result<(), ConfigStoreError> {
        self.close_admitted(observer, |_, _, _| {
            Box::pin(async { Ok(None::<()>) })
        })
        .await
    }

    pub(super) async fn close_admitted<Admit, Admission>(
        &self,
        observer: CommitObserver,
        admit: Admit,
    ) -> Result<(), ConfigStoreError>
    where
        Admit: Fn(
                ConfigCommitAdmissionContext,
                AppConfig,
                AppConfig,
            ) -> ConfigCommitAdmissionFuture<Admission>
            + Send
            + Sync,
        Admission: ConfigCommitAdmission,
    {
        let gate = self.mutation_gate.clone().lock_owned().await;
        if self.closed.load(Ordering::Acquire) {
            return Ok(());
        }
        let _gate = self.reconcile_retry(gate, observer, &admit).await?;
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

    async fn reconcile_retry<Admit, Admission>(
        &self,
        gate: OwnedMutexGuard<()>,
        observer: CommitObserver,
        admit: &Admit,
    ) -> Result<OwnedMutexGuard<()>, ConfigStoreError>
    where
        Admit: Fn(
                ConfigCommitAdmissionContext,
                AppConfig,
                AppConfig,
            ) -> ConfigCommitAdmissionFuture<Admission>
            + Send
            + Sync,
        Admission: ConfigCommitAdmission,
    {
        let retained = self
            .state
            .lock()
            .expect(CONFIG_LOCK_INVARIANT)
            .retry_candidate
            .clone();
        let Some((candidate_revision, candidate)) = retained else {
            return Ok(gate);
        };
        let current = self.current();
        let admission = admit(
            ConfigCommitAdmissionContext::RetainedRetry,
            current,
            candidate.clone(),
        )
        .await?;
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
            admission,
        )
        .await
        .map(|(_, gate)| gate)
    }

    async fn commit<Admit, Admission>(
        &self,
        candidate: AppConfig,
        gate: OwnedMutexGuard<()>,
        observer: CommitObserver,
        admit: &Admit,
    ) -> Result<AppConfig, ConfigStoreError>
    where
        Admit: Fn(
                ConfigCommitAdmissionContext,
                AppConfig,
                AppConfig,
            ) -> ConfigCommitAdmissionFuture<Admission>
            + Send
            + Sync,
        Admission: ConfigCommitAdmission,
    {
        let (candidate, encoded) = encode_config(candidate).await?;
        let current = self.current();
        let admission = admit(
            ConfigCommitAdmissionContext::NewCandidate,
            current,
            candidate.clone(),
        )
        .await?;
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
                admission,
            )
            .await?;
        drop(gate);
        Ok(candidate)
    }

    async fn await_commit<Admission>(
        &self,
        commit: PendingConfigCommit,
        gate: OwnedMutexGuard<()>,
        observer: CommitObserver,
        admission: Option<Admission>,
    ) -> Result<(AppConfig, OwnedMutexGuard<()>), ConfigStoreError>
    where
        Admission: ConfigCommitAdmission,
    {
        let state = self.state.clone();
        let (completed_tx, completed_rx) = tokio::sync::oneshot::channel();
        commit.ticket.observe(move |result| {
            let mut admission = admission;
            let result = match result {
                Ok(_) => {
                    let committed_admission =
                        admission.take().map(ConfigCommitAdmission::commit);
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
                    drop(committed_admission);
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

fn admit_config_source(
    source: &ConfigStore,
    directory: &AnchoredRecordDirectory,
) -> Result<(), ConfigStoreError> {
    if !source.mutation_allowed() {
        return Ok(());
    }
    let expected = match source.startup_source() {
        StartupFileProvenance::Accepted(bytes) => bytes,
        StartupFileProvenance::Missing => {
            return match directory.read(std::ffi::OsStr::new("config.json"), CONFIG_MAX_BYTES) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
                Ok(_) => Err(ConfigStoreError::Read(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "config appeared after startup admission",
                ))),
                Err(error) => Err(ConfigStoreError::Read(error)),
            };
        }
        StartupFileProvenance::Synthetic => return Ok(()),
        StartupFileProvenance::Rejected => {
            return Err(ConfigStoreError::Read(io::Error::new(
                io::ErrorKind::InvalidData,
                "rejected config source allowed mutation",
            )));
        }
    };
    let observation = match directory.read(std::ffi::OsStr::new("config.json"), CONFIG_MAX_BYTES) {
        Ok(observation @ AnchoredRecordObservation::Bytes { .. }) => observation,
        Ok(AnchoredRecordObservation::Oversized { .. }) => {
            return Err(ConfigStoreError::TooLarge {
                max_bytes: CONFIG_MAX_BYTES,
            });
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(ConfigStoreError::Read(io::Error::new(
                io::ErrorKind::NotFound,
                "config generation changed during startup",
            )));
        }
        Err(error) => return Err(ConfigStoreError::Read(error)),
    };
    let bytes = observation
        .bytes()
        .expect("bounded config observation has bytes");
    if bytes != expected {
        return Err(ConfigStoreError::Read(io::Error::new(
            io::ErrorKind::InvalidData,
            "config generation changed during startup",
        )));
    }
    let decoded = serde_json::from_slice::<AppConfig>(bytes)?.normalized()?;
    if decoded != source.current() {
        return Err(ConfigStoreError::Read(io::Error::new(
            io::ErrorKind::InvalidData,
            "config generation changed during startup",
        )));
    }
    observation
        .admit(CONFIG_MAX_BYTES)
        .map_err(ConfigStoreError::Persistence)
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
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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

    struct AdmissionProbe {
        active: Arc<AtomicBool>,
        committed: Arc<AtomicBool>,
    }

    impl ConfigCommitAdmission for AdmissionProbe {
        type Committed = Self;

        fn commit(self) -> Self::Committed {
            self.committed.store(true, Ordering::Release);
            self
        }
    }

    impl Drop for AdmissionProbe {
        fn drop(&mut self) {
            self.active.store(false, Ordering::Release);
        }
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
            _destination: &crate::execution::anchored_record::AnchoredRecordTarget,
            _effects: &axial_fs::EffectOwner,
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
    async fn update_preserves_disabled_guardian_mode() {
        let (store, backend) = test_store("disabled-guardian-mode", 0);

        store
            .mutate(
                |config| {
                    config.guardian_mode = "disabled".to_string();
                    Ok(())
                },
                false,
                no_op_observer(),
            )
            .await
            .expect("persist disabled Guardian mode");

        assert_eq!(store.current().guardian_mode, "disabled");
        assert_eq!(backend.attempts.load(Ordering::SeqCst), 1);
        let committed = backend.committed.lock().expect("committed config lock");
        let persisted: AppConfig = serde_json::from_slice(&committed[0]).expect("persisted config");
        assert_eq!(persisted.guardian_mode, "disabled");
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
    async fn precommit_admission_is_retained_until_the_physical_commit_settles() {
        let (store, backend) = test_store("precommit-admission", 0);
        let write_gate = backend.gate_next();
        let admission_active = Arc::new(AtomicBool::new(false));
        let admission_committed = Arc::new(AtomicBool::new(false));
        let task_active = admission_active.clone();
        let task_committed = admission_committed.clone();
        let candidate_library = store.paths().library_dir().to_string_lossy().into_owned();
        let expected_library = candidate_library.clone();
        let task_store = store.clone();
        let commit = tokio::spawn(async move {
            let gate = task_store
                .acquire_mutation()
                .await
                .expect("config mutation gate");
            task_store
                .mutate_with_gate_admitted(
                    move |config| {
                        config.library_dir = candidate_library;
                        Ok(())
                    },
                    false,
                    no_op_observer(),
                    move |context, previous, candidate| {
                        let task_active = task_active.clone();
                        let task_committed = task_committed.clone();
                        let expected_library = expected_library.clone();
                        Box::pin(async move {
                            assert_eq!(context, ConfigCommitAdmissionContext::NewCandidate);
                            assert!(previous.library_dir.is_empty());
                            assert_eq!(candidate.library_dir, expected_library);
                            assert!(
                                task_active
                                    .compare_exchange(
                                        false,
                                        true,
                                        Ordering::AcqRel,
                                        Ordering::Acquire,
                                    )
                                    .is_ok(),
                                "one precommit admission per physical write"
                            );
                            Ok(Some(AdmissionProbe {
                                active: task_active,
                                committed: task_committed,
                            }))
                        })
                    },
                    gate,
                )
                .await
        });

        backend.wait_for_attempt(1).await;
        assert!(
            admission_active.load(Ordering::Acquire),
            "admission must precede persistence acceptance and remain active while blocked"
        );
        write_gate.release();
        commit
            .await
            .expect("config commit task")
            .expect("admitted config commit");
        assert!(admission_committed.load(Ordering::Acquire));
        assert!(!admission_active.load(Ordering::Acquire));
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
    async fn retained_identity_retry_reacquires_admission_before_publication() {
        let (store, backend) = test_store("retained-identity-admission", 1);
        let managed_library_dir = store.paths().library_dir().to_string_lossy().into_owned();
        let first_gate = store
            .acquire_mutation()
            .await
            .expect("initial config mutation gate");
        let first = store
            .mutate_with_gate(
                move |config| {
                    config.library_dir = managed_library_dir;
                    Ok(())
                },
                false,
                no_op_observer(),
                first_gate,
            )
            .await;
        assert!(matches!(first, Err(ConfigStoreError::Persistence(_))));

        let contexts = Arc::new(Mutex::new(Vec::new()));
        let observed_contexts = contexts.clone();
        let admission_active = Arc::new(AtomicBool::new(false));
        let admission_committed = Arc::new(AtomicBool::new(false));
        let admitted_active = admission_active.clone();
        let admitted_committed = admission_committed.clone();
        let observer_active = admission_active.clone();
        let observer_committed = admission_committed.clone();
        let observer: CommitObserver = Arc::new(move |previous, current| {
            if previous.library_mode != current.library_mode {
                assert!(
                    observer_active.load(Ordering::Acquire),
                    "the retained identity admission must cover observer publication"
                );
                assert!(
                    observer_committed.load(Ordering::Acquire),
                    "the retained identity admission must commit before observer publication"
                );
            }
        });
        let second_gate = store
            .acquire_mutation()
            .await
            .expect("successor config mutation gate");
        store
            .mutate_with_gate_admitted(
                |config| {
                    config.theme = "after-retained-identity".to_string();
                    Ok(())
                },
                false,
                observer,
                move |context, previous, candidate| {
                    let observed_contexts = observed_contexts.clone();
                    let admitted_active = admitted_active.clone();
                    let admitted_committed = admitted_committed.clone();
                    Box::pin(async move {
                        let identity_changed = previous.library_dir != candidate.library_dir
                            || previous.library_mode != candidate.library_mode;
                        observed_contexts
                            .lock()
                            .expect("admission context lock")
                            .push((context, identity_changed));
                        if !identity_changed {
                            return Ok(None);
                        }
                        admitted_active.store(true, Ordering::Release);
                        Ok(Some(AdmissionProbe {
                            active: admitted_active,
                            committed: admitted_committed,
                        }))
                    })
                },
                second_gate,
            )
            .await
            .expect("successor reconciles retained identity bytes");

        assert_eq!(
            *contexts.lock().expect("admission context lock"),
            vec![
                (ConfigCommitAdmissionContext::RetainedRetry, true),
                (ConfigCommitAdmissionContext::NewCandidate, false),
            ]
        );
        assert!(!admission_active.load(Ordering::Acquire));
        assert!(admission_committed.load(Ordering::Acquire));
        assert_eq!(
            store.current().library_dir,
            store.paths().library_dir().to_string_lossy()
        );
        assert_eq!(store.current().theme, "after-retained-identity");
        assert_eq!(backend.attempts.load(Ordering::SeqCst), 3);
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
        let root_session = crate::state::test_root_session(&paths);
        let source = ConfigStore::from_config(paths, root_session, AppConfig::default())
            .expect("config source");
        let backend = RecordingBackend::new(failures);
        let coordinator =
            PersistenceCoordinator::for_test(backend.clone(), Duration::ZERO, Duration::ZERO);
        let store = AppConfigStore::claim_with_coordinator(&source, coordinator)
            .expect("claim config store");
        (Arc::new(store), backend)
    }

    #[test]
    fn startup_config_provenance_requires_the_exact_loaded_generation() {
        let paths = test_paths("startup-provenance-accepted");
        std::fs::create_dir_all(paths.config_file().parent().expect("config parent"))
            .expect("create config root");
        let bytes = serde_json::to_vec_pretty(&AppConfig::default()).expect("encode config");
        std::fs::write(paths.config_file(), &bytes).expect("seed config");
        let root_session = crate::state::test_root_session(&paths);
        let source = ConfigStore::load_from(paths.clone(), root_session.clone())
            .expect("load accepted config");
        let directory = AnchoredRecordDirectory::from_directory(
            root_session.clone(),
            root_session.root_directory().expect("root directory"),
        );
        admit_config_source(&source, &directory).expect("unchanged config is admitted");

        let replaced_paths = test_paths("startup-provenance-replaced");
        std::fs::create_dir_all(
            replaced_paths
                .config_file()
                .parent()
                .expect("config parent"),
        )
        .expect("create replaced root");
        std::fs::write(replaced_paths.config_file(), &bytes).expect("seed replaced config");
        let replaced_root = crate::state::test_root_session(&replaced_paths);
        let replaced_source = ConfigStore::load_from(
            replaced_paths.clone(),
            replaced_root.clone(),
        )
        .expect("load replaced source");
        let mut changed = bytes.clone();
        changed.push(b'\n');
        std::fs::write(replaced_paths.config_file(), &changed).expect("replace config bytes");
        let replaced_directory = AnchoredRecordDirectory::from_directory(
            replaced_root.clone(),
            replaced_root.root_directory().expect("replaced root directory"),
        );
        assert!(admit_config_source(&replaced_source, &replaced_directory).is_err());

        let removed_paths = test_paths("startup-provenance-removed");
        std::fs::create_dir_all(
            removed_paths
                .config_file()
                .parent()
                .expect("config parent"),
        )
        .expect("create removed root");
        std::fs::write(removed_paths.config_file(), &bytes).expect("seed removed config");
        let removed_root = crate::state::test_root_session(&removed_paths);
        let removed_source = ConfigStore::load_from(
            removed_paths.clone(),
            removed_root.clone(),
        )
        .expect("load removed source");
        std::fs::remove_file(removed_paths.config_file()).expect("remove startup config");
        let removed_directory = AnchoredRecordDirectory::from_directory(
            removed_root.clone(),
            removed_root.root_directory().expect("removed root directory"),
        );
        assert!(admit_config_source(&removed_source, &removed_directory).is_err());

        let missing_paths = test_paths("startup-provenance-missing");
        std::fs::create_dir_all(
            missing_paths
                .config_file()
                .parent()
                .expect("config parent"),
        )
        .expect("create missing root");
        let missing_root = crate::state::test_root_session(&missing_paths);
        let missing_source = ConfigStore::load_from(
            missing_paths.clone(),
            missing_root.clone(),
        )
        .expect("load missing source");
        let missing_directory = AnchoredRecordDirectory::from_directory(
            missing_root.clone(),
            missing_root.root_directory().expect("missing root directory"),
        );
        admit_config_source(&missing_source, &missing_directory)
            .expect("unchanged absence is admitted");
        std::fs::write(missing_paths.config_file(), &bytes).expect("make config appear");
        assert!(admit_config_source(&missing_source, &missing_directory).is_err());

        drop((
            directory,
            replaced_directory,
            removed_directory,
            missing_directory,
        ));
        let _ = std::fs::remove_dir_all(paths.config_file().parent().expect("config root"));
        let _ = std::fs::remove_dir_all(
            replaced_paths.config_file().parent().expect("config root"),
        );
        let _ = std::fs::remove_dir_all(
            removed_paths.config_file().parent().expect("config root"),
        );
        let _ = std::fs::remove_dir_all(
            missing_paths.config_file().parent().expect("config root"),
        );
    }

    fn test_app_state(name: &str) -> AppState {
        let paths = test_paths(name);
        let root_session = crate::state::test_root_session(&paths);
        let config = Arc::new(
            ConfigStore::from_config(
                paths.clone(),
                Arc::clone(&root_session),
                AppConfig::default(),
            )
            .expect("config source"),
        );
        AppState::new(AppStateInit {
            app_name: "Axial".to_string(),
            version: "test".to_string(),
            instances: Arc::new(
                InstanceStore::from_snapshot(
                    paths.clone(),
                    root_session,
                    InstanceRegistrySnapshot::default(),
                )
                .expect("instance store"),
            ),
            installs: Arc::new(InstallStore::new()),
            sessions: Arc::new(SessionStore::new()),
            performance: Arc::new(
                PerformanceManager::load_for_startup(paths.performance_dir())
                    .expect("performance manager"),
            ),
            config,
            startup_warnings: Vec::new(),
        })
    }

    fn no_op_observer() -> CommitObserver {
        Arc::new(|_, _| {})
    }

    fn test_paths(name: &str) -> AppPaths {
        let root =
            std::env::temp_dir().join(format!("axial-config-state-{name}-{}", std::process::id()));
        AppPaths::from_root(root.to_path_buf()).expect("absolute test app root")
    }
}
