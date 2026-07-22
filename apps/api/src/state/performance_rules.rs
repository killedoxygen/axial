use super::performance_managed::{
    AppManagedCompositionAdmission, ManagedCompositionAdmissionError, ManagedCompositionCloseError,
    ManagedCompositionOwner, ManagedCompositionRetirement, managed_authority_claim_error,
};
use crate::execution::anchored_record::{AnchoredRecordDirectory, AnchoredRecordObservation};
use crate::execution::persistence::{
    AcceptedWrite, AtomicSnapshotWriter, PersistenceCoordinator, PersistenceError,
    PersistenceOwnerLease, WriteUrgency,
};
use axial_performance::{
    CompositionPlan, HardwareProfile, PerformanceManager, PerformanceRulesAuthority,
    PerformanceRulesStatus, RULES_CACHE_MAX_BYTES, ResolutionRequest, RulesCacheStartupSource,
    RulesRefreshError, VerifiedRemoteRules,
};
use axial_config::AppRootSession;
use std::io;
#[cfg(test)]
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

const PERFORMANCE_RULES_LOCK_INVARIANT: &str =
    "performance rules persistence lock poisoned; active rules may diverge from persistence";

struct RulesPersistence {
    owner: PersistenceOwnerLease,
    writer: AtomicSnapshotWriter,
}

impl RulesPersistence {
    fn claim(
        directory: AnchoredRecordDirectory,
    ) -> Result<Self, RulesRefreshError> {
        Self::claim_with_coordinator(directory, PersistenceCoordinator::global())
    }

    fn claim_with_coordinator(
        directory: AnchoredRecordDirectory,
        coordinator: PersistenceCoordinator,
    ) -> Result<Self, RulesRefreshError> {
        let record = directory
            .target(std::ffi::OsStr::new("rules-cache.json"), RULES_CACHE_MAX_BYTES)
            .map_err(rules_persistence_error)?;
        let owner = coordinator
            .claim_record(record.clone())
            .map_err(rules_persistence_error)?;
        let writer = owner
            .writer(record)
            .map_err(rules_persistence_error)?;
        Ok(Self { owner, writer })
    }
}

struct RulesState {
    retry_candidate: Option<(u64, VerifiedRemoteRules)>,
}

struct PendingRulesCommit {
    ticket: AcceptedWrite,
    revision: u64,
    candidate: VerifiedRemoteRules,
}

pub struct AppPerformanceStore {
    manager: Arc<PerformanceManager>,
    authority: PerformanceRulesAuthority,
    mutation_allowed: bool,
    state: Arc<Mutex<RulesState>>,
    mutation_gate: Arc<AsyncMutex<()>>,
    closed: AtomicBool,
    persistence: RulesPersistence,
    managed: ManagedCompositionOwner,
    _root_session: Arc<AppRootSession>,
}

impl AppPerformanceStore {
    pub(super) fn claim(
        manager: Arc<PerformanceManager>,
        persistence_directory: AnchoredRecordDirectory,
        root_session: Arc<AppRootSession>,
        instance_lifecycle: super::instance_lifecycle::InstanceLifecycleGates,
        managed_artifact_epoch: super::managed_artifact_epoch::ManagedArtifactMutationEpochCoordinator,
    ) -> Result<Self, RulesRefreshError> {
        let authority = manager
            .claim_rules_authority()
            .map_err(RulesRefreshError::Cache)?;
        let mutation_allowed = authority.mutation_allowed();
        admit_rules_source(&authority, &persistence_directory)?;
        let managed = ManagedCompositionOwner::claim(
            manager
                .claim_managed_authority(
                    root_session
                        .prepare_instances_directory()
                        .map_err(RulesRefreshError::Cache)?,
                )
                .map_err(managed_authority_claim_error)?,
            instance_lifecycle,
            managed_artifact_epoch,
        );
        Ok(Self {
            manager,
            authority,
            mutation_allowed,
            state: Arc::new(Mutex::new(RulesState {
                retry_candidate: None,
            })),
            mutation_gate: Arc::new(AsyncMutex::new(())),
            closed: AtomicBool::new(false),
            persistence: RulesPersistence::claim(persistence_directory)?,
            managed,
            _root_session: root_session,
        })
    }

    #[cfg(test)]
    pub(crate) fn claim_with_coordinator(
        manager: Arc<PerformanceManager>,
        performance_dir: &Path,
        root_session: Arc<AppRootSession>,
        coordinator: PersistenceCoordinator,
    ) -> Result<Self, RulesRefreshError> {
        std::fs::create_dir_all(performance_dir).map_err(RulesRefreshError::Cache)?;
        let persistence_directory = AnchoredRecordDirectory::for_test_directory(performance_dir)
            .map_err(RulesRefreshError::Cache)?;
        let authority = manager
            .claim_rules_authority()
            .map_err(RulesRefreshError::Cache)?;
        let mutation_allowed = authority.mutation_allowed();
        admit_rules_source(&authority, &persistence_directory)?;
        let managed = ManagedCompositionOwner::claim(
            manager
                .claim_managed_authority(
                    root_session
                        .prepare_instances_directory()
                        .map_err(RulesRefreshError::Cache)?,
                )
                .map_err(managed_authority_claim_error)?,
            super::instance_lifecycle::InstanceLifecycleGates::default(),
            super::managed_artifact_epoch::ManagedArtifactMutationEpochCoordinator::default(),
        );
        Ok(Self {
            manager,
            authority,
            mutation_allowed,
            state: Arc::new(Mutex::new(RulesState {
                retry_candidate: None,
            })),
            mutation_gate: Arc::new(AsyncMutex::new(())),
            closed: AtomicBool::new(false),
            persistence: RulesPersistence::claim_with_coordinator(
                persistence_directory,
                coordinator,
            )?,
            managed,
            _root_session: root_session,
        })
    }

    pub fn get_plan(&self, request: ResolutionRequest) -> CompositionPlan {
        self.manager.get_plan(request)
    }

    pub fn rules_status(&self) -> PerformanceRulesStatus {
        self.manager.rules_status()
    }

    pub fn remote_refresh_enabled(&self) -> bool {
        self.manager.remote_refresh_enabled()
    }

    pub fn hardware(&self) -> HardwareProfile {
        self.manager.hardware()
    }

    pub(crate) async fn admit_managed(
        &self,
        instance_id: &str,
        instance_lifecycle: super::InstanceLifecycleLease,
        recovery_allowed: bool,
    ) -> Result<AppManagedCompositionAdmission, ManagedCompositionAdmissionError> {
        self.managed
            .admit(instance_id, instance_lifecycle, recovery_allowed)
            .await
    }

    pub(crate) async fn close_managed(&self) -> Result<(), ManagedCompositionCloseError> {
        self.managed.close().await
    }

    pub(crate) async fn retire_managed(
        &self,
        instance_id: &str,
        instance_lifecycle: super::InstanceLifecycleLease,
    ) -> Result<ManagedCompositionRetirement, ManagedCompositionAdmissionError> {
        self.managed.retire(instance_id, instance_lifecycle).await
    }

    pub(crate) async fn acquire_refresh(&self) -> Result<OwnedMutexGuard<()>, RulesRefreshError> {
        let gate = self.mutation_gate.clone().lock_owned().await;
        if self.closed.load(Ordering::Acquire) {
            return Err(closed_rules_error());
        }
        if !self.mutation_allowed {
            return Err(RulesRefreshError::Cache(io::Error::new(
                io::ErrorKind::InvalidData,
                "performance rules mutation is latched after startup admission failure",
            )));
        }
        Ok(gate)
    }

    pub(crate) async fn refresh_with_gate(
        &self,
        gate: OwnedMutexGuard<()>,
    ) -> Result<PerformanceRulesStatus, RulesRefreshError> {
        let gate = self.reconcile_retry(gate).await?;
        let candidate = match self.authority.fetch_remote_rules().await {
            Ok(candidate) => candidate,
            Err(error) => {
                self.authority.record_refresh_warning(
                    axial_performance::remote_rules_refresh_warning("rejected", &error),
                );
                drop(gate);
                return Err(error);
            }
        };
        self.commit(candidate, gate).await
    }

    pub(crate) async fn close(&self) -> Result<(), RulesRefreshError> {
        let gate = self.mutation_gate.clone().lock_owned().await;
        if self.closed.load(Ordering::Acquire) {
            return Ok(());
        }
        let _gate = self.reconcile_retry(gate).await?;
        self.persistence
            .owner
            .close()
            .await
            .map_err(rules_persistence_error)?;
        self.closed.store(true, Ordering::Release);
        Ok(())
    }

    async fn reconcile_retry(
        &self,
        gate: OwnedMutexGuard<()>,
    ) -> Result<OwnedMutexGuard<()>, RulesRefreshError> {
        let retained = self
            .state
            .lock()
            .expect(PERFORMANCE_RULES_LOCK_INVARIANT)
            .retry_candidate
            .clone();
        let Some((candidate_revision, candidate)) = retained else {
            return Ok(gate);
        };
        let ticket = self
            .persistence
            .writer
            .retry()
            .map_err(rules_persistence_error)?;
        let revision = ticket.revision().get();
        assert_eq!(
            revision, candidate_revision,
            "performance rules retry revision diverged from retained candidate"
        );
        self.await_commit(
            PendingRulesCommit {
                ticket,
                revision,
                candidate,
            },
            gate,
        )
        .await
        .map(|(_, gate)| gate)
    }

    async fn commit(
        &self,
        candidate: VerifiedRemoteRules,
        gate: OwnedMutexGuard<()>,
    ) -> Result<PerformanceRulesStatus, RulesRefreshError> {
        let (candidate, encoded) = encode_rules(candidate).await?;
        let ticket = self
            .persistence
            .writer
            .accept(encoded, WriteUrgency::Immediate, Ok)
            .map_err(rules_persistence_error)?;
        let revision = ticket.revision().get();
        let (status, gate) = self
            .await_commit(
                PendingRulesCommit {
                    ticket,
                    revision,
                    candidate,
                },
                gate,
            )
            .await?;
        drop(gate);
        Ok(status)
    }

    async fn await_commit(
        &self,
        commit: PendingRulesCommit,
        gate: OwnedMutexGuard<()>,
    ) -> Result<(PerformanceRulesStatus, OwnedMutexGuard<()>), RulesRefreshError> {
        let state = self.state.clone();
        let authority = self.authority.clone();
        let (completed_tx, completed_rx) = tokio::sync::oneshot::channel();
        commit.ticket.observe_async(move |persisted| async move {
            let settlement = authority
                .settle_remote_rules(
                    commit.candidate.clone(),
                    async move { persisted.map(|_| ()) },
                )
                .await;
            let result = match settlement {
                Ok(status) => {
                    let mut state = state.lock().expect(PERFORMANCE_RULES_LOCK_INVARIANT);
                    if state
                        .retry_candidate
                        .as_ref()
                        .is_some_and(|(_, candidate)| {
                            candidate.snapshot() == commit.candidate.snapshot()
                        })
                    {
                        state.retry_candidate = None;
                    }
                    Ok(status)
                }
                Err(error) => {
                    if matches!(error, PersistenceError::Write { .. }) {
                        state
                            .lock()
                            .expect(PERFORMANCE_RULES_LOCK_INVARIANT)
                            .retry_candidate = Some((commit.revision, commit.candidate));
                    }
                    Err(rules_persistence_error(error))
                }
            };
            let _ = completed_tx.send((result, gate));
        });
        let (result, gate) = completed_rx.await.map_err(|_| {
            RulesRefreshError::Cache(io::Error::other(
                "performance rules commit observer stopped before reporting completion",
            ))
        })?;
        result.map(|status| (status, gate))
    }
}

async fn encode_rules(
    candidate: VerifiedRemoteRules,
) -> Result<(VerifiedRemoteRules, Vec<u8>), RulesRefreshError> {
    tokio::task::spawn_blocking(move || {
        let encoded = candidate.snapshot().encode()?;
        Ok((candidate, encoded))
    })
    .await
    .map_err(|error| {
        RulesRefreshError::Cache(io::Error::other(format!(
            "performance rules encoder stopped: {error}"
        )))
    })?
}

fn admit_rules_source(
    authority: &PerformanceRulesAuthority,
    directory: &AnchoredRecordDirectory,
) -> Result<(), RulesRefreshError> {
    if !authority.mutation_allowed() {
        return Ok(());
    }
    let expected = match authority.startup_source() {
        RulesCacheStartupSource::Accepted(bytes) => bytes,
        RulesCacheStartupSource::Missing => {
            return match directory.read(
                std::ffi::OsStr::new("rules-cache.json"),
                RULES_CACHE_MAX_BYTES,
            ) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
                Ok(_) => Err(RulesRefreshError::Cache(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "performance rules cache appeared after startup admission",
                ))),
                Err(error) => Err(RulesRefreshError::Cache(error)),
            };
        }
        RulesCacheStartupSource::Synthetic => return Ok(()),
        RulesCacheStartupSource::Rejected => {
            return Err(RulesRefreshError::Cache(io::Error::new(
                io::ErrorKind::InvalidData,
                "rejected performance rules source allowed mutation",
            )));
        }
    };
    let observation = match directory.read(
        std::ffi::OsStr::new("rules-cache.json"),
        RULES_CACHE_MAX_BYTES,
    ) {
        Ok(observation @ AnchoredRecordObservation::Bytes { .. }) => observation,
        Ok(AnchoredRecordObservation::Oversized { .. }) => {
            return Err(RulesRefreshError::Cache(io::Error::new(
                io::ErrorKind::InvalidData,
                "performance rules cache exceeds its byte bound",
            )));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(RulesRefreshError::Cache(io::Error::new(
                io::ErrorKind::NotFound,
                "performance rules cache generation changed during startup",
            )));
        }
        Err(error) => return Err(RulesRefreshError::Cache(error)),
    };
    let bytes = observation
        .bytes()
        .expect("bounded performance rules observation has bytes");
    if bytes != expected || !authority.matches_loaded_cache(bytes) {
        return Err(RulesRefreshError::Cache(io::Error::new(
            io::ErrorKind::InvalidData,
            "performance rules cache generation changed during startup",
        )));
    }
    observation
        .admit(RULES_CACHE_MAX_BYTES)
        .map_err(RulesRefreshError::Cache)
}

fn rules_persistence_error(error: impl Into<io::Error>) -> RulesRefreshError {
    RulesRefreshError::Cache(error.into())
}

fn closed_rules_error() -> RulesRefreshError {
    RulesRefreshError::Cache(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "performance rules persistence is closed",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::persistence::AtomicWriteBackend;
    use ed25519_dalek::{Signer, SigningKey};
    use std::sync::Condvar;
    use std::sync::atomic::AtomicUsize;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    struct NoopBackend;

    struct FailOnceBackend {
        failures: AtomicUsize,
        attempted: Mutex<Vec<Vec<u8>>>,
    }

    struct GatedBackend {
        started: tokio::sync::Notify,
        attempts: AtomicUsize,
        gate: Mutex<bool>,
        released: Condvar,
    }

    struct TestManagedAuthority {
        root: std::path::PathBuf,
        root_session: Option<Arc<AppRootSession>>,
    }

    impl Drop for TestManagedAuthority {
        fn drop(&mut self) {
            drop(self.root_session.take());
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    impl AtomicWriteBackend for NoopBackend {
        fn write(
            &self,
            _destination: &crate::execution::anchored_record::AnchoredRecordTarget,
            _effects: &axial_fs::EffectOwner,
            _contents: &[u8],
        ) -> io::Result<()> {
            Ok(())
        }
    }

    impl AtomicWriteBackend for FailOnceBackend {
        fn write(
            &self,
            _destination: &crate::execution::anchored_record::AnchoredRecordTarget,
            _effects: &axial_fs::EffectOwner,
            contents: &[u8],
        ) -> io::Result<()> {
            self.attempted
                .lock()
                .expect("attempted rules lock")
                .push(contents.to_vec());
            if self
                .failures
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |failures| {
                    (failures > 0).then(|| failures - 1)
                })
                .is_ok()
            {
                return Err(io::Error::other("injected rules write failure"));
            }
            Ok(())
        }
    }

    impl AtomicWriteBackend for GatedBackend {
        fn write(
            &self,
            _destination: &crate::execution::anchored_record::AnchoredRecordTarget,
            _effects: &axial_fs::EffectOwner,
            _contents: &[u8],
        ) -> io::Result<()> {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            self.started.notify_one();
            let mut released = self.gate.lock().expect("rules write gate lock");
            while !*released {
                released = self.released.wait(released).expect("rules write gate wait");
            }
            Ok(())
        }
    }

    impl GatedBackend {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                started: tokio::sync::Notify::new(),
                attempts: AtomicUsize::new(0),
                gate: Mutex::new(false),
                released: Condvar::new(),
            })
        }

        async fn wait_started(&self) {
            loop {
                let started = self.started.notified();
                if self.attempts.load(Ordering::SeqCst) > 0 {
                    return;
                }
                started.await;
            }
        }

        fn release(&self) {
            *self.gate.lock().expect("rules write gate lock") = true;
            self.released.notify_all();
        }
    }

    #[tokio::test]
    async fn duplicate_rules_cache_owner_is_rejected() {
        let root = test_root("duplicate-owner");
        let managed = test_managed_authority("duplicate-owner");
        let coordinator = test_coordinator();
        let first = AppPerformanceStore::claim_with_coordinator(
            Arc::new(PerformanceManager::load_for_startup(&root).expect("first manager")),
            &root,
            Arc::clone(managed.root_session.as_ref().expect("managed root session")),
            coordinator.clone(),
        )
        .expect("first rules owner");

        let second = AppPerformanceStore::claim_with_coordinator(
            Arc::new(PerformanceManager::load_for_startup(&root).expect("second manager")),
            &root,
            Arc::clone(managed.root_session.as_ref().expect("managed root session")),
            coordinator,
        );

        assert!(matches!(second, Err(RulesRefreshError::Cache(_))));
        first.close().await.expect("close first owner");
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn synthetic_source_is_explicit_and_missing_source_rejects_appeared_cache() {
        let synthetic_root = test_root("authority-synthetic");
        let synthetic_managed = test_managed_authority("authority-synthetic");
        let synthetic = AppPerformanceStore::claim_with_coordinator(
            Arc::new(PerformanceManager::new().expect("synthetic manager")),
            &synthetic_root,
            Arc::clone(
                synthetic_managed
                    .root_session
                    .as_ref()
                    .expect("managed root session"),
            ),
            test_coordinator(),
        )
        .expect("synthetic source deliberately bypasses startup file admission");
        synthetic.close().await.expect("close synthetic store");

        let missing_root = test_root("authority-missing-appeared");
        let missing_manager = Arc::new(
            PerformanceManager::load_for_startup(&missing_root).expect("missing cache manager"),
        );
        std::fs::create_dir_all(&missing_root).expect("create performance directory");
        let appeared = missing_root.join("rules-cache.json");
        std::fs::write(&appeared, b"{appeared cache bytes").expect("seed appeared cache");
        let missing_managed = test_managed_authority("authority-missing-appeared");
        let result = AppPerformanceStore::claim_with_coordinator(
            missing_manager,
            &missing_root,
            Arc::clone(
                missing_managed
                    .root_session
                    .as_ref()
                    .expect("managed root session"),
            ),
            test_coordinator(),
        );
        assert!(matches!(result, Err(RulesRefreshError::Cache(_))));
        assert_eq!(
            std::fs::read(&appeared).expect("read appeared cache"),
            b"{appeared cache bytes"
        );
        let _ = std::fs::remove_dir_all(synthetic_root);
        let _ = std::fs::remove_dir_all(missing_root);
    }

    #[test]
    fn startup_rules_provenance_requires_the_exact_loaded_generation() {
        let root = test_root("startup-provenance-accepted");
        let cache_path = root.join("rules-cache.json");
        let manifest = axial_performance::builtin_manifest().expect("builtin manifest");
        let signing_key = SigningKey::from_bytes(&[31_u8; 32]);
        let payload = axial_performance::canonical_manifest_payload(&manifest)
            .expect("canonical manifest payload");
        let snapshot = axial_performance::RulesCacheSnapshot {
            rule_source: axial_performance::RuleSource::Remote,
            rule_channel: axial_performance::RuleChannel::Remote,
            schema_version: manifest.schema_version,
            generated_at: manifest.generated_at.clone(),
            validation: axial_performance::RulesValidation::Valid,
            updated_at: "2026-07-22T08:00:00Z".to_string(),
            manifest,
            signature: axial_performance::RulesSignatureMetadata {
                signature: hex::encode(signing_key.sign(&payload).to_bytes()),
                key_id: Some("startup-provenance-test".to_string()),
            },
        };
        let bytes = snapshot.encode().expect("encode rules cache");
        std::fs::write(&cache_path, &bytes).expect("seed rules cache");
        let manager = Arc::new(
            PerformanceManager::load_for_startup_with_remote_url_and_public_key(
                &root,
                Some("https://rules.example.test/current.json".to_string()),
                Some(hex::encode(signing_key.verifying_key().to_bytes())),
            )
            .expect("load accepted rules cache"),
        );
        let authority = manager.claim_rules_authority().expect("rules authority");
        let directory = AnchoredRecordDirectory::for_test_directory(&root)
            .expect("rules directory authority");
        admit_rules_source(&authority, &directory).expect("unchanged cache is admitted");

        let mut changed = bytes.clone();
        changed.push(b'\n');
        std::fs::write(&cache_path, &changed).expect("replace rules cache bytes");
        assert!(admit_rules_source(&authority, &directory).is_err());
        std::fs::remove_file(&cache_path).expect("remove startup rules cache");
        assert!(admit_rules_source(&authority, &directory).is_err());

        let missing_root = test_root("startup-provenance-missing");
        let missing_manager = Arc::new(
            PerformanceManager::load_for_startup_with_remote_url_and_public_key(
                &missing_root,
                Some("https://rules.example.test/current.json".to_string()),
                Some(hex::encode(signing_key.verifying_key().to_bytes())),
            )
            .expect("load missing rules cache"),
        );
        let missing_authority = missing_manager
            .claim_rules_authority()
            .expect("missing rules authority");
        let missing_directory = AnchoredRecordDirectory::for_test_directory(&missing_root)
            .expect("missing rules directory authority");
        admit_rules_source(&missing_authority, &missing_directory)
            .expect("unchanged absence is admitted");
        std::fs::write(missing_root.join("rules-cache.json"), &bytes)
            .expect("make rules cache appear");
        assert!(admit_rules_source(&missing_authority, &missing_directory).is_err());

        drop((directory, missing_directory));
        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(missing_root);
    }

    #[tokio::test]
    async fn invalid_startup_bytes_latch_refresh_without_rewrite() {
        let root = test_root("startup-latch");
        let managed = test_managed_authority("startup-latch");
        let cache_path = root.join("rules-cache.json");
        std::fs::create_dir_all(cache_path.parent().expect("cache parent"))
            .expect("create cache parent");
        std::fs::write(&cache_path, b"{invalid rules cache").expect("seed invalid cache");
        let manager = Arc::new(
            PerformanceManager::load_for_startup_with_remote_url(
                &root,
                Some("https://rules.example.test/current.json".to_string()),
            )
            .expect("manager falls back to built-in rules"),
        );
        let store = AppPerformanceStore::claim_with_coordinator(
            manager,
            &root,
            Arc::clone(managed.root_session.as_ref().expect("managed root session")),
            test_coordinator(),
        )
        .expect("claim latched rules owner");

        assert!(matches!(
            store.acquire_refresh().await,
            Err(RulesRefreshError::Cache(_))
        ));
        assert_eq!(
            std::fs::read(&cache_path).expect("read rejected bytes"),
            b"{invalid rules cache"
        );
        store.close().await.expect("close latched owner");
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn hostile_bounded_manifest_is_rejected_and_latches_refresh() {
        let root = test_root("hostile-manifest-latch");
        let managed = test_managed_authority("hostile-manifest-latch");
        let mut hostile = axial_performance::builtin_manifest().expect("builtin manifest");
        hostile.compositions[0].description = "x".repeat(2048);
        let signing_key = SigningKey::from_bytes(&[19_u8; 32]);
        let payload = axial_performance::canonical_manifest_payload(&hostile)
            .expect("canonical hostile payload");
        let signature = axial_performance::RulesSignatureMetadata {
            signature: hex::encode(signing_key.sign(&payload).to_bytes()),
            key_id: Some("hostile-test".to_string()),
        };
        let snapshot = axial_performance::RulesCacheSnapshot {
            rule_source: axial_performance::RuleSource::Remote,
            rule_channel: axial_performance::RuleChannel::Remote,
            schema_version: hostile.schema_version,
            generated_at: hostile.generated_at.clone(),
            validation: axial_performance::RulesValidation::Valid,
            updated_at: "2026-07-11T08:30:00Z".to_string(),
            manifest: hostile,
            signature,
        };
        let cache_path = root.join("rules-cache.json");
        std::fs::create_dir_all(cache_path.parent().expect("cache parent"))
            .expect("create cache parent");
        let encoded = snapshot.encode().expect("encode hostile cache");
        std::fs::write(&cache_path, &encoded).expect("seed hostile cache");
        let manager = Arc::new(
            PerformanceManager::load_for_startup_with_remote_url_and_public_key(
                &root,
                Some("https://rules.example.test/current.json".to_string()),
                Some(hex::encode(signing_key.verifying_key().to_bytes())),
            )
            .expect("manager falls back to built-in rules"),
        );
        let store = AppPerformanceStore::claim_with_coordinator(
            manager,
            &root,
            Arc::clone(managed.root_session.as_ref().expect("managed root session")),
            test_coordinator(),
        )
        .expect("claim latched rules owner");

        assert!(matches!(
            store.acquire_refresh().await,
            Err(RulesRefreshError::Cache(_))
        ));
        assert_eq!(
            std::fs::read(&cache_path).expect("read hostile bytes"),
            encoded
        );
        store.close().await.expect("close latched owner");
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn non_directory_cache_parent_latches_refresh_without_replacement() {
        let root = test_root("cache-parent-file");
        let managed = test_managed_authority("cache-parent-file");
        std::fs::remove_dir_all(&root).expect("remove performance directory");
        std::fs::write(&root, b"owned parent bytes").expect("seed performance path file");
        let manager = Arc::new(
            PerformanceManager::load_for_startup_with_remote_url(&root, None)
                .expect("manager falls back to built-in rules"),
        );
        let store = AppPerformanceStore::claim_with_coordinator(
            manager,
            &root,
            Arc::clone(managed.root_session.as_ref().expect("managed root session")),
            test_coordinator(),
        )
        .expect("claim latched rules owner");

        assert!(matches!(
            store.acquire_refresh().await,
            Err(RulesRefreshError::Cache(_))
        ));
        assert_eq!(
            std::fs::read(&root).expect("read parent bytes"),
            b"owned parent bytes"
        );
        store.close().await.expect("close latched owner");
        let _ = std::fs::remove_file(root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlinked_cache_parent_latches_refresh_without_following_link() {
        use std::os::unix::fs::symlink;

        let root = test_root("cache-parent-symlink");
        let outside = test_root("cache-parent-symlink-target");
        let managed = test_managed_authority("cache-parent-symlink");
        std::fs::remove_dir_all(&root).expect("remove performance directory");
        symlink(&outside, &root).expect("symlink performance directory");
        let manager = Arc::new(
            PerformanceManager::load_for_startup_with_remote_url(&root, None)
                .expect("manager falls back to built-in rules"),
        );
        let store = AppPerformanceStore::claim_with_coordinator(
            manager,
            &root,
            Arc::clone(managed.root_session.as_ref().expect("managed root session")),
            test_coordinator(),
        )
        .expect("claim latched rules owner");

        assert!(matches!(
            store.acquire_refresh().await,
            Err(RulesRefreshError::Cache(_))
        ));
        assert!(!outside.join("rules-cache.json").exists());
        store.close().await.expect("close latched owner");
        let _ = std::fs::remove_file(root);
        let _ = std::fs::remove_dir_all(outside);
    }

    #[tokio::test]
    async fn close_retries_exact_failed_bytes_before_publishing_rules() {
        let root = test_root("retry-before-publish");
        let managed = test_managed_authority("retry-before-publish");
        let mut remote = axial_performance::builtin_manifest().expect("builtin manifest");
        remote.generated_at = "2026-07-11T08:00:00Z".to_string();
        let signing_key = SigningKey::from_bytes(&[17_u8; 32]);
        let payload = axial_performance::canonical_manifest_payload(&remote)
            .expect("canonical manifest payload");
        let signature = hex::encode(signing_key.sign(&payload).to_bytes());
        let remote_url = spawn_rules_server(remote.clone(), signature).await;
        let manager = Arc::new(
            PerformanceManager::load_for_startup_with_remote_url_and_public_key(
                &root,
                Some(remote_url),
                Some(hex::encode(signing_key.verifying_key().to_bytes())),
            )
            .expect("performance manager"),
        );
        let backend = Arc::new(FailOnceBackend {
            failures: AtomicUsize::new(1),
            attempted: Mutex::new(Vec::new()),
        });
        let coordinator = PersistenceCoordinator::for_test(
            backend.clone(),
            Duration::from_millis(1),
            Duration::from_millis(5),
        );
        let store = AppPerformanceStore::claim_with_coordinator(
            manager,
            &root,
            Arc::clone(managed.root_session.as_ref().expect("managed root session")),
            coordinator,
        )
        .expect("rules owner");
        let before = store.rules_status();

        let gate = store.acquire_refresh().await.expect("refresh gate");
        assert!(matches!(
            store.refresh_with_gate(gate).await,
            Err(RulesRefreshError::Cache(_))
        ));
        assert_eq!(store.rules_status().generated_at, before.generated_at);

        store.close().await.expect("close retries retained bytes");
        store.close().await.expect("close is idempotent");
        assert!(matches!(
            store.acquire_refresh().await,
            Err(RulesRefreshError::Cache(_))
        ));
        assert_eq!(store.rules_status().generated_at, remote.generated_at);
        let attempted = backend.attempted.lock().expect("attempted rules lock");
        assert_eq!(attempted.len(), 2);
        assert_eq!(attempted[0], attempted[1]);
        drop(attempted);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn accepted_refresh_publishes_only_after_commit_and_survives_cancellation() {
        let root = test_root("persist-before-publish");
        let managed = test_managed_authority("persist-before-publish");
        let mut remote = axial_performance::builtin_manifest().expect("builtin manifest");
        remote.generated_at = "2026-07-11T09:00:00Z".to_string();
        let signing_key = SigningKey::from_bytes(&[23_u8; 32]);
        let payload = axial_performance::canonical_manifest_payload(&remote)
            .expect("canonical manifest payload");
        let remote_url = spawn_rules_server(
            remote.clone(),
            hex::encode(signing_key.sign(&payload).to_bytes()),
        )
        .await;
        let manager = Arc::new(
            PerformanceManager::load_for_startup_with_remote_url_and_public_key(
                &root,
                Some(remote_url),
                Some(hex::encode(signing_key.verifying_key().to_bytes())),
            )
            .expect("performance manager"),
        );
        let backend = GatedBackend::new();
        let coordinator = PersistenceCoordinator::for_test(
            backend.clone(),
            Duration::from_millis(1),
            Duration::from_millis(5),
        );
        let store = Arc::new(
            AppPerformanceStore::claim_with_coordinator(
                manager,
                &root,
                Arc::clone(managed.root_session.as_ref().expect("managed root session")),
                coordinator,
            )
            .expect("rules owner"),
        );
        let before = store.rules_status();
        let refresh_store = store.clone();
        let refresh = tokio::spawn(async move {
            let gate = refresh_store.acquire_refresh().await.expect("refresh gate");
            refresh_store.refresh_with_gate(gate).await
        });
        backend.wait_started().await;

        assert_eq!(store.rules_status().generated_at, before.generated_at);
        refresh.abort();
        assert!(
            refresh
                .await
                .expect_err("cancel refresh waiter")
                .is_cancelled()
        );
        backend.release();
        store
            .close()
            .await
            .expect("close waits for accepted commit");
        assert_eq!(store.rules_status().generated_at, remote.generated_at);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn failed_exact_bytes_commit_before_successor_refresh() {
        let root = test_root("retry-before-successor");
        let managed = test_managed_authority("retry-before-successor");
        let mut first = axial_performance::builtin_manifest().expect("first manifest");
        first.generated_at = "2026-07-11T10:00:00Z".to_string();
        let mut second = first.clone();
        second.generated_at = "2026-07-11T11:00:00Z".to_string();
        let signing_key = SigningKey::from_bytes(&[29_u8; 32]);
        let remote_url =
            spawn_rules_sequence_server(vec![first.clone(), second.clone()], signing_key.clone())
                .await;
        let manager = Arc::new(
            PerformanceManager::load_for_startup_with_remote_url_and_public_key(
                &root,
                Some(remote_url),
                Some(hex::encode(signing_key.verifying_key().to_bytes())),
            )
            .expect("performance manager"),
        );
        let backend = Arc::new(FailOnceBackend {
            failures: AtomicUsize::new(1),
            attempted: Mutex::new(Vec::new()),
        });
        let coordinator = PersistenceCoordinator::for_test(
            backend.clone(),
            Duration::from_millis(1),
            Duration::from_millis(5),
        );
        let store = AppPerformanceStore::claim_with_coordinator(
            manager,
            &root,
            Arc::clone(managed.root_session.as_ref().expect("managed root session")),
            coordinator,
        )
        .expect("rules owner");

        let first_gate = store.acquire_refresh().await.expect("first refresh gate");
        assert!(matches!(
            store.refresh_with_gate(first_gate).await,
            Err(RulesRefreshError::Cache(_))
        ));
        let second_gate = store.acquire_refresh().await.expect("second refresh gate");
        store
            .refresh_with_gate(second_gate)
            .await
            .expect("successor refresh");

        assert_eq!(store.rules_status().generated_at, second.generated_at);
        {
            let attempted = backend.attempted.lock().expect("attempted rules lock");
            assert_eq!(attempted.len(), 3);
            assert_eq!(attempted[0], attempted[1]);
            assert_ne!(attempted[1], attempted[2]);
        }
        store.close().await.expect("close rules owner");
        let _ = std::fs::remove_dir_all(root);
    }

    fn test_coordinator() -> PersistenceCoordinator {
        PersistenceCoordinator::for_test(
            Arc::new(NoopBackend),
            Duration::from_millis(1),
            Duration::from_millis(5),
        )
    }

    fn test_managed_authority(name: &str) -> TestManagedAuthority {
        let root = test_root(&format!("{name}-managed-root"));
        let paths = axial_config::AppPaths::from_root(root.clone()).expect("managed app paths");
        let root_session = crate::state::test_root_session(&paths);
        TestManagedAuthority {
            root,
            root_session: Some(root_session),
        }
    }

    fn test_root(name: &str) -> std::path::PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "axial-api-performance-rules-{name}-{}-{nonce}",
            std::process::id()
        ))
    }

    async fn spawn_rules_server(
        manifest: axial_performance::Manifest,
        signature: String,
    ) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind rules server");
        let addr = listener.local_addr().expect("rules server address");
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept rules request");
            let mut request = [0_u8; 2048];
            let _ = tokio::io::AsyncReadExt::read(&mut stream, &mut request).await;
            let body = serde_json::to_vec(&manifest).expect("serialize remote manifest");
            let headers = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nx-axial-rules-signature-ed25519: {}\r\nConnection: close\r\n\r\n",
                body.len(),
                signature
            );
            tokio::io::AsyncWriteExt::write_all(&mut stream, headers.as_bytes())
                .await
                .expect("write rules headers");
            tokio::io::AsyncWriteExt::write_all(&mut stream, &body)
                .await
                .expect("write rules body");
        });
        format!("http://{addr}/rules.json")
    }

    async fn spawn_rules_sequence_server(
        manifests: Vec<axial_performance::Manifest>,
        signing_key: SigningKey,
    ) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind rules sequence server");
        let addr = listener.local_addr().expect("rules sequence address");
        tokio::spawn(async move {
            for manifest in manifests {
                let (mut stream, _) = listener.accept().await.expect("accept rules request");
                let mut request = [0_u8; 2048];
                let _ = tokio::io::AsyncReadExt::read(&mut stream, &mut request).await;
                let payload = axial_performance::canonical_manifest_payload(&manifest)
                    .expect("canonical manifest payload");
                let signature = hex::encode(signing_key.sign(&payload).to_bytes());
                let body = serde_json::to_vec(&manifest).expect("serialize remote manifest");
                let headers = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nx-axial-rules-signature-ed25519: {}\r\nConnection: close\r\n\r\n",
                    body.len(),
                    signature
                );
                tokio::io::AsyncWriteExt::write_all(&mut stream, headers.as_bytes())
                    .await
                    .expect("write rules headers");
                tokio::io::AsyncWriteExt::write_all(&mut stream, &body)
                    .await
                    .expect("write rules body");
            }
        });
        format!("http://{addr}/rules.json")
    }
}
