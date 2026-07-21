use super::contracts::{OwnershipClass, StabilizationSystem, TargetDescriptor, TargetKind};
use super::performance_managed::{
    AppManagedCompositionAdmission, ManagedCompositionAdmissionError, ManagedCompositionCloseError,
    ManagedCompositionOwner, ManagedCompositionRetirement, managed_authority_claim_error,
};
use crate::execution::persistence::{
    AcceptedWrite, AtomicSnapshotWriter, PersistenceCoordinator, PersistenceError,
    PersistenceOwnerLease, WriteUrgency,
};
use axial_performance::{
    CompositionPlan, HardwareProfile, PerformanceManager, PerformanceRulesAuthority,
    PerformanceRulesStatus, ResolutionRequest, RulesRefreshError, VerifiedRemoteRules,
    rules_cache_path,
};
use std::io;
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
    fn claim(performance_dir: &Path) -> Result<Self, RulesRefreshError> {
        Self::claim_with_coordinator(performance_dir, PersistenceCoordinator::global())
    }

    fn claim_with_coordinator(
        performance_dir: &Path,
        coordinator: PersistenceCoordinator,
    ) -> Result<Self, RulesRefreshError> {
        let path = rules_cache_path(performance_dir);
        let owner = coordinator
            .claim_owner(&path)
            .map_err(rules_persistence_error)?;
        let writer = owner
            .writer(&path, rules_cache_target())
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
}

impl AppPerformanceStore {
    pub(super) fn claim(
        manager: Arc<PerformanceManager>,
        performance_dir: &Path,
        instances_root: &Path,
        instance_lifecycle: super::instance_lifecycle::InstanceLifecycleGates,
        managed_artifact_epoch: super::managed_artifact_epoch::ManagedArtifactMutationEpochCoordinator,
    ) -> Result<Self, RulesRefreshError> {
        let authority = manager
            .claim_rules_authority(performance_dir)
            .map_err(RulesRefreshError::Cache)?;
        let mutation_allowed = authority.mutation_allowed();
        let managed = ManagedCompositionOwner::claim(
            manager
                .claim_managed_authority(instances_root)
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
            persistence: RulesPersistence::claim(performance_dir)?,
            managed,
        })
    }

    #[cfg(test)]
    pub(crate) fn claim_with_coordinator(
        manager: Arc<PerformanceManager>,
        performance_dir: &Path,
        instances_root: &Path,
        coordinator: PersistenceCoordinator,
    ) -> Result<Self, RulesRefreshError> {
        let authority = manager
            .claim_rules_authority(performance_dir)
            .map_err(RulesRefreshError::Cache)?;
        let mutation_allowed = authority.mutation_allowed();
        let managed = ManagedCompositionOwner::claim(
            manager
                .claim_managed_authority(instances_root)
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
            persistence: RulesPersistence::claim_with_coordinator(performance_dir, coordinator)?,
            managed,
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
    ) -> Result<ManagedCompositionRetirement, ManagedCompositionAdmissionError> {
        self.managed.retire(instance_id).await
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

fn rules_cache_target() -> TargetDescriptor {
    TargetDescriptor::new(
        StabilizationSystem::Performance,
        TargetKind::Config,
        "performance_rules_cache",
        OwnershipClass::LauncherManaged,
    )
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

    impl AtomicWriteBackend for NoopBackend {
        fn write(
            &self,
            _target: &TargetDescriptor,
            _destination: &Path,
            _contents: &[u8],
        ) -> io::Result<()> {
            Ok(())
        }
    }

    impl AtomicWriteBackend for FailOnceBackend {
        fn write(
            &self,
            _target: &TargetDescriptor,
            _destination: &Path,
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
            _target: &TargetDescriptor,
            _destination: &Path,
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
        let coordinator = test_coordinator();
        let first = AppPerformanceStore::claim_with_coordinator(
            Arc::new(PerformanceManager::load_for_startup(&root).expect("first manager")),
            &root,
            &root.join("instances"),
            coordinator.clone(),
        )
        .expect("first rules owner");

        let second = AppPerformanceStore::claim_with_coordinator(
            Arc::new(PerformanceManager::load_for_startup(&root).expect("second manager")),
            &root,
            &root.join("instances"),
            coordinator,
        );

        assert!(matches!(second, Err(RulesRefreshError::Cache(_))));
        first.close().await.expect("close first owner");
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn authority_rejects_unadmitted_or_mismatched_cache_paths() {
        let admitted = test_root("authority-admitted-path");
        let different = test_root("authority-different-path");
        let unbound = AppPerformanceStore::claim_with_coordinator(
            Arc::new(PerformanceManager::new().expect("unbound manager")),
            &admitted,
            &admitted.join("instances"),
            test_coordinator(),
        );
        assert!(matches!(unbound, Err(RulesRefreshError::Cache(_))));

        let different_cache = rules_cache_path(&different);
        std::fs::create_dir_all(different_cache.parent().expect("different cache parent"))
            .expect("create different cache parent");
        let rejected_bytes = b"{unadmitted cache bytes";
        std::fs::write(&different_cache, rejected_bytes).expect("seed unadmitted cache");
        let backend = Arc::new(FailOnceBackend {
            failures: AtomicUsize::new(0),
            attempted: Mutex::new(Vec::new()),
        });
        let coordinator = PersistenceCoordinator::for_test(
            backend.clone(),
            Duration::from_millis(1),
            Duration::from_millis(5),
        );
        let mismatched = AppPerformanceStore::claim_with_coordinator(
            Arc::new(PerformanceManager::load_for_startup(&admitted).expect("admitted manager")),
            &different,
            &different.join("instances"),
            coordinator,
        );
        assert!(matches!(mismatched, Err(RulesRefreshError::Cache(_))));
        assert!(
            backend
                .attempted
                .lock()
                .expect("attempted rules lock")
                .is_empty()
        );
        assert_eq!(
            std::fs::read(&different_cache).expect("read unadmitted cache"),
            rejected_bytes
        );
        let _ = std::fs::remove_dir_all(admitted);
        let _ = std::fs::remove_dir_all(different);
    }

    #[tokio::test]
    async fn invalid_startup_bytes_latch_refresh_without_rewrite() {
        let root = test_root("startup-latch");
        let cache_path = rules_cache_path(&root);
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
            &root.join("instances"),
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
        let cache_path = rules_cache_path(&root);
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
            &root.join("instances"),
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
        std::fs::remove_dir_all(&root).expect("remove performance directory");
        std::fs::write(&root, b"owned parent bytes").expect("seed performance path file");
        let manager = Arc::new(
            PerformanceManager::load_for_startup_with_remote_url(&root, None)
                .expect("manager falls back to built-in rules"),
        );
        let store = AppPerformanceStore::claim_with_coordinator(
            manager,
            &root,
            &root.join("instances"),
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
        std::fs::remove_dir_all(&root).expect("remove performance directory");
        symlink(&outside, &root).expect("symlink performance directory");
        let manager = Arc::new(
            PerformanceManager::load_for_startup_with_remote_url(&root, None)
                .expect("manager falls back to built-in rules"),
        );
        let store = AppPerformanceStore::claim_with_coordinator(
            manager,
            &root,
            &root.join("instances"),
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
            &root.join("instances"),
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
                &root.join("instances"),
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
            &root.join("instances"),
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
