use super::contracts::{OwnershipClass, StabilizationSystem, TargetDescriptor, TargetKind};
use crate::execution::file::{DeleteFileRequest, delete_launcher_managed_file};
use crate::execution::persistence::{
    AcceptedWrite, AtomicSnapshotWriter, PersistenceCoordinator, PersistenceError,
    PersistenceOwnerLease, WriteUrgency,
};
use axial_config::{AppPaths, is_canonical_instance_id};
use axial_minecraft::known_good::{
    KnownGoodArtifactKind, KnownGoodIntegrity, KnownGoodInventory, KnownGoodRelativePath,
    KnownGoodRoot, MAX_KNOWN_GOOD_ENTRIES, MAX_KNOWN_GOOD_PATH_SEGMENT_BYTES,
    MAX_KNOWN_GOOD_RELATIVE_PATH_BYTES,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap};
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};
#[cfg(test)]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex, Weak};
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard, RwLock as AsyncRwLock};

const KNOWN_GOOD_SCHEMA: &str = "axial.state.known_good_inventory.v4";
const KNOWN_GOOD_DIR: &str = "known-good";
const MAX_KNOWN_GOOD_SNAPSHOT_BYTES: u64 = 256 << 20;
const MAX_KNOWN_GOOD_CLEANUP_OBLIGATIONS: usize = 4_096;
const STORE_LOCK_INVARIANT: &str =
    "known-good inventory store lock poisoned; cache settlement may be inconsistent";

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct KnownGoodSnapshot {
    schema: String,
    instance_id: String,
    version_id: String,
    entries: Vec<KnownGoodEntrySnapshot>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct KnownGoodEntrySnapshot {
    root: KnownGoodRootSnapshot,
    path: String,
    kind: KnownGoodArtifactKindSnapshot,
    integrity: KnownGoodIntegritySnapshot,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum KnownGoodRootSnapshot {
    Versions,
    Libraries,
    Assets,
    ManagedRuntime { component: String },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum KnownGoodArtifactKindSnapshot {
    VersionMetadata,
    ClientJar,
    Library,
    NativeLibrary,
    AssetIndex,
    AssetObject,
    LogConfig,
    RuntimeManifestProof,
    RuntimeReadyMarker,
    RuntimeFile,
    RuntimeExecutable,
    RuntimeDirectory,
    RuntimeLink,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum KnownGoodIntegritySnapshot {
    Sha1 { digest: String, size: u64 },
    ExactBytes { digest: String, size: u64 },
    Directory,
    LinkTarget { target: String },
}

#[derive(Clone)]
struct PendingSnapshot {
    revision: u64,
    snapshot: KnownGoodSnapshot,
    failed: bool,
}

#[derive(Default)]
struct StoreState {
    writers: HashMap<String, AtomicSnapshotWriter>,
    pending: HashMap<String, PendingSnapshot>,
}

#[derive(Default)]
struct CleanupState {
    reserved: BTreeSet<String>,
    committed: BTreeSet<String>,
}

pub(super) struct KnownGoodRetirementReservation {
    store: Arc<KnownGoodInventoryStore>,
    instance_id: String,
    armed: bool,
}

impl KnownGoodRetirementReservation {
    pub(super) async fn commit(mut self) -> io::Result<()> {
        {
            let mut cleanup = self.store.cleanup.lock().expect(STORE_LOCK_INVARIANT);
            cleanup.reserved.remove(&self.instance_id);
            cleanup.committed.insert(self.instance_id.clone());
        }
        self.armed = false;
        self.store.retry_retirement(&self.instance_id).await
    }
}

impl Drop for KnownGoodRetirementReservation {
    fn drop(&mut self) {
        if self.armed {
            self.store
                .cleanup
                .lock()
                .expect(STORE_LOCK_INVARIANT)
                .reserved
                .remove(&self.instance_id);
        }
    }
}

struct ActiveInventory<T> {
    version_id: String,
    created_at: String,
    library_root: PathBuf,
    inventory: Arc<T>,
}

struct ActiveInventories<T> {
    by_instance: HashMap<String, ActiveInventory<T>>,
}

impl<T> Default for ActiveInventories<T> {
    fn default() -> Self {
        Self {
            by_instance: HashMap::new(),
        }
    }
}

impl<T> ActiveInventories<T> {
    fn activate_validated(
        &mut self,
        snapshot: &KnownGoodSnapshot,
        instance_id: &str,
        version_id: &str,
        created_at: &str,
        library_root: PathBuf,
        inventory: Arc<T>,
    ) -> io::Result<()> {
        snapshot.validate()?;
        self.activate(instance_id, version_id, created_at, library_root, inventory);
        Ok(())
    }

    fn activate(
        &mut self,
        instance_id: &str,
        version_id: &str,
        created_at: &str,
        library_root: PathBuf,
        inventory: Arc<T>,
    ) {
        self.by_instance.insert(
            instance_id.to_string(),
            ActiveInventory {
                version_id: version_id.to_string(),
                created_at: created_at.to_string(),
                library_root,
                inventory,
            },
        );
    }

    fn get(
        &self,
        instance_id: &str,
        version_id: &str,
        created_at: &str,
        library_root: &Path,
    ) -> Option<Arc<T>> {
        let active = self.by_instance.get(instance_id)?;
        (active.version_id == version_id
            && active.created_at == created_at
            && active.library_root == library_root)
            .then(|| active.inventory.clone())
    }

    fn remove(&mut self, instance_id: &str) {
        self.by_instance.remove(instance_id);
    }

    fn remove_exact(
        &mut self,
        instance_id: &str,
        version_id: &str,
        created_at: &str,
        library_root: &Path,
    ) {
        if self.by_instance.get(instance_id).is_some_and(|active| {
            active.version_id == version_id
                && active.created_at == created_at
                && active.library_root == library_root
        }) {
            self.by_instance.remove(instance_id);
        }
    }

    fn remove_incarnation(&mut self, instance_id: &str, version_id: &str, created_at: &str) {
        if self.by_instance.get(instance_id).is_some_and(|active| {
            active.version_id == version_id && active.created_at == created_at
        }) {
            self.by_instance.remove(instance_id);
        }
    }

    fn clear(&mut self) {
        self.by_instance.clear();
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
enum StorePhase {
    Running = 0,
    Closing = 1,
    Closed = 2,
}

struct CloseTransition {
    phase: Arc<AtomicU8>,
    armed: bool,
}

impl CloseTransition {
    fn new(phase: Arc<AtomicU8>) -> Self {
        Self { phase, armed: true }
    }

    fn finish(mut self, phase: StorePhase) {
        self.phase.store(phase as u8, Ordering::Release);
        self.armed = false;
    }
}

impl Drop for CloseTransition {
    fn drop(&mut self) {
        if self.armed {
            self.phase
                .store(StorePhase::Running as u8, Ordering::Release);
        }
    }
}

pub(super) struct KnownGoodInventoryStore {
    root: PathBuf,
    owner: PersistenceOwnerLease,
    state: Arc<Mutex<StoreState>>,
    cleanup: Arc<Mutex<CleanupState>>,
    active: Mutex<ActiveInventories<KnownGoodInventory>>,
    gates: Mutex<HashMap<String, Weak<AsyncMutex<()>>>>,
    lifecycle: Arc<AsyncRwLock<()>>,
    close_gate: AsyncMutex<()>,
    phase: Arc<AtomicU8>,
    #[cfg(test)]
    retirement_delete_failures: AtomicUsize,
}

impl KnownGoodInventoryStore {
    pub(super) fn claim(paths: &AppPaths) -> io::Result<Self> {
        let root = paths.config_dir.join("state").join(KNOWN_GOOD_DIR);
        Self::claim_root_with_coordinator(root, PersistenceCoordinator::global())
    }

    #[cfg(test)]
    fn claim_with_coordinator(
        paths: &AppPaths,
        coordinator: PersistenceCoordinator,
    ) -> io::Result<Self> {
        let root = paths.config_dir.join("state").join(KNOWN_GOOD_DIR);
        Self::claim_root_with_coordinator(root, coordinator)
    }

    fn claim_root_with_coordinator(
        root: PathBuf,
        coordinator: PersistenceCoordinator,
    ) -> io::Result<Self> {
        let owner = coordinator.claim_owner(&root).map_err(io::Error::from)?;
        Ok(Self {
            root,
            owner,
            state: Arc::new(Mutex::new(StoreState::default())),
            cleanup: Arc::new(Mutex::new(CleanupState::default())),
            active: Mutex::new(ActiveInventories::default()),
            gates: Mutex::new(HashMap::new()),
            lifecycle: Arc::new(AsyncRwLock::new(())),
            close_gate: AsyncMutex::new(()),
            phase: Arc::new(AtomicU8::new(StorePhase::Running as u8)),
            #[cfg(test)]
            retirement_delete_failures: AtomicUsize::new(0),
        })
    }

    pub(super) async fn reconcile(
        &self,
        instance_id: &str,
        version_id: &str,
        created_at: &str,
        library_root: &Path,
        inventory: Arc<KnownGoodInventory>,
    ) -> io::Result<()> {
        validate_identity(instance_id, version_id)?;
        let library_root = normalize_library_root(library_root)?;
        let _lifecycle = self.lifecycle.clone().read_owned().await;
        if self.phase() != StorePhase::Running {
            return Err(closed_error());
        }
        let _instance = self.instance_gate(instance_id).await;
        if self.phase() != StorePhase::Running {
            return Err(closed_error());
        }

        let snapshot = snapshot_from_inventory(instance_id, version_id, &inventory);
        snapshot.validate()?;
        let path = self.snapshot_path(instance_id);
        let read_path = path.clone();
        let persisted = tokio::task::spawn_blocking(move || read_snapshot(&read_path))
            .await
            .unwrap_or(Ok(None))
            .unwrap_or(None);

        self.reconcile_persistence(instance_id, path, persisted.as_ref(), snapshot.clone())?;
        self.active
            .lock()
            .expect(STORE_LOCK_INVARIANT)
            .activate_validated(
                &snapshot,
                instance_id,
                version_id,
                created_at,
                library_root,
                inventory,
            )?;
        Ok(())
    }

    pub(super) fn active_inventory(
        &self,
        instance_id: &str,
        version_id: &str,
        created_at: &str,
        library_root: &Path,
    ) -> Option<Arc<KnownGoodInventory>> {
        if !is_canonical_instance_id(instance_id) {
            return None;
        }
        let library_root = normalize_library_root(library_root).ok()?;
        self.active.lock().expect(STORE_LOCK_INVARIANT).get(
            instance_id,
            version_id,
            created_at,
            &library_root,
        )
    }

    #[cfg(test)]
    pub(super) fn activate_for_test(
        &self,
        instance_id: &str,
        version_id: &str,
        created_at: &str,
        library_root: &Path,
        inventory: Arc<KnownGoodInventory>,
    ) -> io::Result<()> {
        validate_identity(instance_id, version_id)?;
        let library_root = normalize_library_root(library_root)?;
        self.active.lock().expect(STORE_LOCK_INVARIANT).activate(
            instance_id,
            version_id,
            created_at,
            library_root,
            inventory,
        );
        Ok(())
    }

    pub(super) fn deactivate_exact(
        &self,
        instance_id: &str,
        version_id: &str,
        created_at: &str,
        library_root: &Path,
    ) {
        let normalized_root = normalize_library_root(library_root).ok();
        let mut active = self.active.lock().expect(STORE_LOCK_INVARIANT);
        deactivate_active_inventory(
            &mut active,
            instance_id,
            version_id,
            created_at,
            normalized_root.as_deref(),
        );
    }

    pub(super) fn clear_active(&self) {
        self.active.lock().expect(STORE_LOCK_INVARIANT).clear();
    }

    pub(super) fn reserve_retirement(
        self: &Arc<Self>,
        instance_id: &str,
    ) -> io::Result<KnownGoodRetirementReservation> {
        if !is_canonical_instance_id(instance_id) {
            return Err(invalid_snapshot("invalid known-good instance identity"));
        }
        if self.phase() != StorePhase::Running {
            return Err(closed_error());
        }
        let mut cleanup = self.cleanup.lock().expect(STORE_LOCK_INVARIANT);
        if cleanup.reserved.contains(instance_id) || cleanup.committed.contains(instance_id) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "known-good retirement already has an owner",
            ));
        }
        if cleanup.reserved.len() + cleanup.committed.len() >= MAX_KNOWN_GOOD_CLEANUP_OBLIGATIONS {
            return Err(invalid_snapshot(
                "known-good cleanup obligation capacity is exhausted",
            ));
        }
        cleanup.reserved.insert(instance_id.to_string());
        drop(cleanup);
        Ok(KnownGoodRetirementReservation {
            store: self.clone(),
            instance_id: instance_id.to_string(),
            armed: true,
        })
    }

    pub(super) fn discover_absent_snapshot_obligations(
        &self,
        registered_instances: impl IntoIterator<Item = String>,
    ) -> io::Result<()> {
        let registered = registered_instances.into_iter().collect::<BTreeSet<_>>();
        let entries = match fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        let mut absent = BTreeSet::new();
        for (index, entry) in entries.enumerate() {
            if index >= MAX_KNOWN_GOOD_CLEANUP_OBLIGATIONS {
                return Err(invalid_snapshot(
                    "known-good cleanup obligation capacity is exhausted",
                ));
            }
            let entry = entry?;
            let Some(file_name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            let Some(instance_id) = file_name.strip_suffix(".json") else {
                continue;
            };
            if is_canonical_instance_id(instance_id) && !registered.contains(instance_id) {
                absent.insert(instance_id.to_string());
            }
        }
        let mut cleanup = self.cleanup.lock().expect(STORE_LOCK_INVARIANT);
        absent.retain(|instance_id| {
            !cleanup.reserved.contains(instance_id) && !cleanup.committed.contains(instance_id)
        });
        if cleanup.reserved.len() + cleanup.committed.len() + absent.len()
            > MAX_KNOWN_GOOD_CLEANUP_OBLIGATIONS
        {
            return Err(invalid_snapshot(
                "known-good cleanup obligation capacity is exhausted",
            ));
        }
        cleanup.committed.extend(absent);
        Ok(())
    }

    async fn retry_retirement(&self, instance_id: &str) -> io::Result<()> {
        let _lifecycle = self.lifecycle.clone().read_owned().await;
        if self.phase() != StorePhase::Running {
            return Err(closed_error());
        }
        let _instance = self.instance_gate(instance_id).await;
        if self.phase() != StorePhase::Running {
            return Err(closed_error());
        }

        self.active
            .lock()
            .expect(STORE_LOCK_INVARIANT)
            .remove(instance_id);
        let writer = self
            .state
            .lock()
            .expect(STORE_LOCK_INVARIANT)
            .writers
            .get(instance_id)
            .cloned();
        if let Some(writer) = writer {
            writer.settle().await.map_err(io::Error::from)?;
        }
        {
            let mut state = self.state.lock().expect(STORE_LOCK_INVARIANT);
            state.pending.remove(instance_id);
            state.writers.remove(instance_id);
        }

        let path = self.snapshot_path(instance_id);
        let target = known_good_target(instance_id);
        #[cfg(test)]
        if self
            .retirement_delete_failures
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |failures| {
                (failures > 0).then(|| failures - 1)
            })
            .is_ok()
        {
            return Err(io::Error::other(
                "injected known-good retirement delete failure",
            ));
        }
        tokio::task::spawn_blocking(move || {
            delete_launcher_managed_file(DeleteFileRequest::new(target, &path))
                .map(|_| ())
                .map_err(io::Error::from)
        })
        .await
        .map_err(|_| io::Error::other("known-good retirement owner stopped"))??;
        self.cleanup
            .lock()
            .expect(STORE_LOCK_INVARIANT)
            .committed
            .remove(instance_id);
        Ok(())
    }

    pub(super) async fn retry_retirements(&self) -> io::Result<()> {
        let instance_ids = self
            .cleanup
            .lock()
            .expect(STORE_LOCK_INVARIANT)
            .committed
            .iter()
            .cloned()
            .collect::<Vec<_>>();
        let mut first_error = None;
        for instance_id in instance_ids {
            if let Err(error) = self.retry_retirement(&instance_id).await
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    #[cfg(test)]
    fn fail_retirement_deletes(&self, failures: usize) {
        self.retirement_delete_failures
            .store(failures, Ordering::SeqCst);
    }

    #[cfg(test)]
    fn pending_retirement_ids(&self) -> Vec<String> {
        self.cleanup
            .lock()
            .expect(STORE_LOCK_INVARIANT)
            .committed
            .iter()
            .cloned()
            .collect()
    }

    pub(super) async fn close(&self) -> io::Result<()> {
        let _close = self.close_gate.lock().await;
        if self.phase() == StorePhase::Closed {
            return Ok(());
        }
        self.retry_retirements().await?;
        let _lifecycle = self.lifecycle.write().await;
        self.phase
            .store(StorePhase::Closing as u8, Ordering::Release);
        self.clear_active();
        let transition = CloseTransition::new(self.phase.clone());
        let result = match self.settle_writers().await {
            Ok(()) => self.owner.close().await.map_err(io::Error::from),
            Err(error) => Err(error),
        };
        if result.is_ok() {
            transition.finish(StorePhase::Closed);
        }
        result
    }

    fn accept_snapshot(
        &self,
        instance_id: &str,
        path: PathBuf,
        snapshot: KnownGoodSnapshot,
    ) -> io::Result<()> {
        let (writer, is_new) = {
            let state = self.state.lock().expect(STORE_LOCK_INVARIANT);
            match state.writers.get(instance_id) {
                Some(writer) => (writer.clone(), false),
                None => {
                    let writer = self
                        .owner
                        .writer(&path, known_good_target(instance_id))
                        .map_err(io::Error::from)?;
                    (writer, true)
                }
            }
        };

        let ticket = writer
            .accept(snapshot.clone(), WriteUrgency::Immediate, encode_snapshot)
            .map_err(io::Error::from)?;
        if is_new {
            self.state
                .lock()
                .expect(STORE_LOCK_INVARIANT)
                .writers
                .insert(instance_id.to_string(), writer);
        }
        self.track_ticket(instance_id, snapshot, ticket);
        Ok(())
    }

    async fn settle_writers(&self) -> io::Result<()> {
        let writers = self
            .state
            .lock()
            .expect(STORE_LOCK_INVARIANT)
            .writers
            .iter()
            .map(|(instance_id, writer)| (instance_id.clone(), writer.clone()))
            .collect::<Vec<_>>();
        for (instance_id, writer) in writers {
            match writer.settle().await {
                Ok(revision) => self.clear_committed_pending(&instance_id, revision.get()),
                Err(PersistenceError::RetryUnavailable) => {
                    let snapshot = self
                        .state
                        .lock()
                        .expect(STORE_LOCK_INVARIANT)
                        .pending
                        .get(&instance_id)
                        .map(|pending| pending.snapshot.clone())
                        .ok_or_else(|| io::Error::from(PersistenceError::RetryUnavailable))?;
                    self.accept_snapshot(&instance_id, self.snapshot_path(&instance_id), snapshot)?;
                    let writer = self
                        .state
                        .lock()
                        .expect(STORE_LOCK_INVARIANT)
                        .writers
                        .get(&instance_id)
                        .cloned()
                        .ok_or_else(closed_error)?;
                    let revision = writer.settle().await.map_err(io::Error::from)?;
                    self.clear_committed_pending(&instance_id, revision.get());
                }
                Err(error) => return Err(io::Error::from(error)),
            }
        }
        Ok(())
    }

    fn clear_committed_pending(&self, instance_id: &str, committed_revision: u64) {
        let mut state = self.state.lock().expect(STORE_LOCK_INVARIANT);
        if state
            .pending
            .get(instance_id)
            .is_some_and(|pending| pending.revision <= committed_revision)
        {
            state.pending.remove(instance_id);
        }
    }

    fn reconcile_persistence(
        &self,
        instance_id: &str,
        path: PathBuf,
        persisted: Option<&KnownGoodSnapshot>,
        snapshot: KnownGoodSnapshot,
    ) -> io::Result<()> {
        let pending = self
            .state
            .lock()
            .expect(STORE_LOCK_INVARIANT)
            .pending
            .get(instance_id)
            .cloned();
        if let Some(pending) = pending {
            if pending.snapshot == snapshot {
                if pending.failed && !self.retry_snapshot(instance_id, &snapshot)? {
                    self.accept_snapshot(instance_id, path, snapshot)?;
                }
                return Ok(());
            }
            return self.accept_snapshot(instance_id, path, snapshot);
        }
        if persisted == Some(&snapshot) {
            return Ok(());
        }
        self.accept_snapshot(instance_id, path, snapshot)
    }

    fn retry_snapshot(&self, instance_id: &str, snapshot: &KnownGoodSnapshot) -> io::Result<bool> {
        let retry = {
            let state = self.state.lock().expect(STORE_LOCK_INVARIANT);
            let Some(pending) = state.pending.get(instance_id) else {
                return Ok(false);
            };
            if !pending.failed || pending.snapshot != *snapshot {
                return Ok(false);
            }
            let writer = state
                .writers
                .get(instance_id)
                .cloned()
                .ok_or_else(closed_error)?;
            (pending.revision, writer)
        };

        let ticket = match retry.1.retry() {
            Ok(ticket) => ticket,
            Err(PersistenceError::RetryUnavailable) => return Ok(false),
            Err(error) => return Err(io::Error::from(error)),
        };
        assert_eq!(
            ticket.revision().get(),
            retry.0,
            "known-good retry revision diverged from retained candidate"
        );
        self.track_ticket(instance_id, snapshot.clone(), ticket);
        Ok(true)
    }

    fn track_ticket(&self, instance_id: &str, snapshot: KnownGoodSnapshot, ticket: AcceptedWrite) {
        let revision = ticket.revision().get();
        self.state
            .lock()
            .expect(STORE_LOCK_INVARIANT)
            .pending
            .insert(
                instance_id.to_string(),
                PendingSnapshot {
                    revision,
                    snapshot: snapshot.clone(),
                    failed: false,
                },
            );
        let state = self.state.clone();
        let instance_id = instance_id.to_string();
        ticket.observe(move |result| {
            let mut state = state.lock().expect(STORE_LOCK_INVARIANT);
            let Some(pending) = state.pending.get_mut(&instance_id) else {
                return;
            };
            if pending.revision != revision || pending.snapshot != snapshot {
                return;
            }
            if result.is_ok() {
                state.pending.remove(&instance_id);
            } else {
                pending.failed = true;
            }
        });
    }

    async fn instance_gate(&self, instance_id: &str) -> OwnedMutexGuard<()> {
        let gate = {
            let mut gates = self.gates.lock().expect(STORE_LOCK_INVARIANT);
            gates.retain(|_, gate| gate.strong_count() > 0);
            match gates.get(instance_id).and_then(Weak::upgrade) {
                Some(gate) => gate,
                None => {
                    let gate = Arc::new(AsyncMutex::new(()));
                    gates.insert(instance_id.to_string(), Arc::downgrade(&gate));
                    gate
                }
            }
        };
        gate.lock_owned().await
    }

    fn snapshot_path(&self, instance_id: &str) -> PathBuf {
        self.root.join(format!("{instance_id}.json"))
    }

    fn phase(&self) -> StorePhase {
        match self.phase.load(Ordering::Acquire) {
            value if value == StorePhase::Running as u8 => StorePhase::Running,
            value if value == StorePhase::Closing as u8 => StorePhase::Closing,
            _ => StorePhase::Closed,
        }
    }

    #[cfg(test)]
    async fn reconcile_snapshot(&self, snapshot: KnownGoodSnapshot) -> io::Result<()> {
        validate_identity(&snapshot.instance_id, &snapshot.version_id)?;
        snapshot.validate()?;
        let _lifecycle = self.lifecycle.clone().read_owned().await;
        let _instance = self.instance_gate(&snapshot.instance_id).await;
        let path = self.snapshot_path(&snapshot.instance_id);
        let read_path = path.clone();
        let persisted = tokio::task::spawn_blocking(move || read_snapshot(&read_path))
            .await
            .unwrap_or(Ok(None))?;
        let instance_id = snapshot.instance_id.clone();
        self.reconcile_persistence(&instance_id, path, persisted.as_ref(), snapshot)
    }

    #[cfg(test)]
    async fn flush_for_test(&self) -> io::Result<()> {
        self.owner.flush().await.map_err(io::Error::from)?;

        // Persistence can finish before ticket observers clear the store's pending marker.
        let writers = self
            .state
            .lock()
            .expect(STORE_LOCK_INVARIANT)
            .writers
            .iter()
            .map(|(instance_id, writer)| (instance_id.clone(), writer.clone()))
            .collect::<Vec<_>>();
        for (instance_id, writer) in writers {
            let revision = writer.flush().await.map_err(io::Error::from)?;
            self.clear_committed_pending(&instance_id, revision.get());
        }
        Ok(())
    }
}

fn snapshot_from_inventory(
    instance_id: &str,
    version_id: &str,
    inventory: &KnownGoodInventory,
) -> KnownGoodSnapshot {
    KnownGoodSnapshot {
        schema: KNOWN_GOOD_SCHEMA.to_string(),
        instance_id: instance_id.to_string(),
        version_id: version_id.to_string(),
        entries: inventory
            .entries()
            .iter()
            .map(|entry| KnownGoodEntrySnapshot {
                root: match entry.root() {
                    KnownGoodRoot::Versions => KnownGoodRootSnapshot::Versions,
                    KnownGoodRoot::Libraries => KnownGoodRootSnapshot::Libraries,
                    KnownGoodRoot::Assets => KnownGoodRootSnapshot::Assets,
                    KnownGoodRoot::ManagedRuntime { component } => {
                        KnownGoodRootSnapshot::ManagedRuntime {
                            component: component.as_str().to_string(),
                        }
                    }
                },
                path: entry.path().as_str().to_string(),
                kind: entry.kind().into(),
                integrity: entry.integrity().into(),
            })
            .collect(),
    }
}

impl From<KnownGoodArtifactKind> for KnownGoodArtifactKindSnapshot {
    fn from(value: KnownGoodArtifactKind) -> Self {
        match value {
            KnownGoodArtifactKind::VersionMetadata => Self::VersionMetadata,
            KnownGoodArtifactKind::ClientJar => Self::ClientJar,
            KnownGoodArtifactKind::Library => Self::Library,
            KnownGoodArtifactKind::NativeLibrary => Self::NativeLibrary,
            KnownGoodArtifactKind::AssetIndex => Self::AssetIndex,
            KnownGoodArtifactKind::AssetObject => Self::AssetObject,
            KnownGoodArtifactKind::LogConfig => Self::LogConfig,
            KnownGoodArtifactKind::RuntimeManifestProof => Self::RuntimeManifestProof,
            KnownGoodArtifactKind::RuntimeReadyMarker => Self::RuntimeReadyMarker,
            KnownGoodArtifactKind::RuntimeFile => Self::RuntimeFile,
            KnownGoodArtifactKind::RuntimeExecutable => Self::RuntimeExecutable,
            KnownGoodArtifactKind::RuntimeDirectory => Self::RuntimeDirectory,
            KnownGoodArtifactKind::RuntimeLink => Self::RuntimeLink,
        }
    }
}

impl From<&KnownGoodIntegrity> for KnownGoodIntegritySnapshot {
    fn from(value: &KnownGoodIntegrity) -> Self {
        match value {
            KnownGoodIntegrity::Sha1 { digest, size } => Self::Sha1 {
                digest: digest.as_str().to_string(),
                size: *size,
            },
            KnownGoodIntegrity::ExactBytes { digest, size } => Self::ExactBytes {
                digest: digest.as_str().to_string(),
                size: *size,
            },
            KnownGoodIntegrity::Directory => Self::Directory,
            KnownGoodIntegrity::LinkTarget(target) => Self::LinkTarget {
                target: target.as_str().to_string(),
            },
        }
    }
}

impl KnownGoodSnapshot {
    fn validate(&self) -> io::Result<()> {
        if self.schema != KNOWN_GOOD_SCHEMA {
            return Err(invalid_snapshot("unsupported known-good snapshot schema"));
        }
        validate_identity(&self.instance_id, &self.version_id)?;
        if self.entries.is_empty() || self.entries.len() > MAX_KNOWN_GOOD_ENTRIES {
            return Err(invalid_snapshot(
                "known-good snapshot contains an invalid entry count",
            ));
        }
        let mut previous: Option<(&str, &str, &str)> = None;
        for entry in &self.entries {
            entry.validate()?;
            let key = entry.sort_key();
            if previous.is_some_and(|previous| previous >= key) {
                return Err(invalid_snapshot(
                    "known-good snapshot entries are not strictly ordered",
                ));
            }
            previous = Some(key);
        }
        Ok(())
    }
}

impl KnownGoodEntrySnapshot {
    fn validate(&self) -> io::Result<()> {
        KnownGoodRelativePath::new(&self.path)
            .map_err(|_| invalid_snapshot("known-good snapshot contains an unsafe path"))?;
        if let KnownGoodRootSnapshot::ManagedRuntime { component } = &self.root {
            validate_safe_segment(component, "runtime component")?;
        }
        if !root_kind_compatible(&self.root, self.kind)
            || !integrity_kind_compatible(self.kind, &self.integrity)
        {
            return Err(invalid_snapshot(
                "known-good snapshot contains an invalid artifact contract",
            ));
        }
        match &self.integrity {
            KnownGoodIntegritySnapshot::Sha1 { digest, .. }
            | KnownGoodIntegritySnapshot::ExactBytes { digest, .. } => validate_digest(digest)?,
            KnownGoodIntegritySnapshot::LinkTarget { target } => {
                validate_link_target(&self.path, target)?;
            }
            KnownGoodIntegritySnapshot::Directory => {}
        }
        Ok(())
    }

    fn sort_key(&self) -> (&str, &str, &str) {
        match &self.root {
            KnownGoodRootSnapshot::Versions => ("versions", "", self.path.as_str()),
            KnownGoodRootSnapshot::Libraries => ("libraries", "", self.path.as_str()),
            KnownGoodRootSnapshot::Assets => ("assets", "", self.path.as_str()),
            KnownGoodRootSnapshot::ManagedRuntime { component } => {
                ("managed_runtime", component.as_str(), self.path.as_str())
            }
        }
    }
}

fn root_kind_compatible(root: &KnownGoodRootSnapshot, kind: KnownGoodArtifactKindSnapshot) -> bool {
    match root {
        KnownGoodRootSnapshot::Versions => matches!(
            kind,
            KnownGoodArtifactKindSnapshot::VersionMetadata
                | KnownGoodArtifactKindSnapshot::ClientJar
        ),
        KnownGoodRootSnapshot::Libraries => matches!(
            kind,
            KnownGoodArtifactKindSnapshot::Library | KnownGoodArtifactKindSnapshot::NativeLibrary
        ),
        KnownGoodRootSnapshot::Assets => matches!(
            kind,
            KnownGoodArtifactKindSnapshot::AssetIndex
                | KnownGoodArtifactKindSnapshot::AssetObject
                | KnownGoodArtifactKindSnapshot::LogConfig
        ),
        KnownGoodRootSnapshot::ManagedRuntime { .. } => matches!(
            kind,
            KnownGoodArtifactKindSnapshot::RuntimeManifestProof
                | KnownGoodArtifactKindSnapshot::RuntimeReadyMarker
                | KnownGoodArtifactKindSnapshot::RuntimeFile
                | KnownGoodArtifactKindSnapshot::RuntimeExecutable
                | KnownGoodArtifactKindSnapshot::RuntimeDirectory
                | KnownGoodArtifactKindSnapshot::RuntimeLink
        ),
    }
}

fn integrity_kind_compatible(
    kind: KnownGoodArtifactKindSnapshot,
    integrity: &KnownGoodIntegritySnapshot,
) -> bool {
    match kind {
        KnownGoodArtifactKindSnapshot::VersionMetadata => matches!(
            integrity,
            KnownGoodIntegritySnapshot::Sha1 { .. } | KnownGoodIntegritySnapshot::ExactBytes { .. }
        ),
        KnownGoodArtifactKindSnapshot::ClientJar
        | KnownGoodArtifactKindSnapshot::AssetIndex
        | KnownGoodArtifactKindSnapshot::AssetObject
        | KnownGoodArtifactKindSnapshot::LogConfig
        | KnownGoodArtifactKindSnapshot::RuntimeFile
        | KnownGoodArtifactKindSnapshot::RuntimeExecutable => {
            matches!(integrity, KnownGoodIntegritySnapshot::Sha1 { .. })
        }
        KnownGoodArtifactKindSnapshot::Library | KnownGoodArtifactKindSnapshot::NativeLibrary => {
            matches!(integrity, KnownGoodIntegritySnapshot::Sha1 { .. })
        }
        KnownGoodArtifactKindSnapshot::RuntimeManifestProof
        | KnownGoodArtifactKindSnapshot::RuntimeReadyMarker => {
            matches!(integrity, KnownGoodIntegritySnapshot::ExactBytes { .. })
        }
        KnownGoodArtifactKindSnapshot::RuntimeDirectory => {
            matches!(integrity, KnownGoodIntegritySnapshot::Directory)
        }
        KnownGoodArtifactKindSnapshot::RuntimeLink => {
            matches!(integrity, KnownGoodIntegritySnapshot::LinkTarget { .. })
        }
    }
}

fn read_snapshot(path: &Path) -> io::Result<Option<KnownGoodSnapshot>> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Ok(None),
    };
    let mut bytes = Vec::new();
    if file
        .take(MAX_KNOWN_GOOD_SNAPSHOT_BYTES + 1)
        .read_to_end(&mut bytes)
        .is_err()
    {
        return Ok(None);
    }
    if bytes.len() as u64 > MAX_KNOWN_GOOD_SNAPSHOT_BYTES {
        return Ok(None);
    }
    let Ok(snapshot) = serde_json::from_slice::<KnownGoodSnapshot>(&bytes) else {
        return Ok(None);
    };
    if snapshot.validate().is_err() {
        return Ok(None);
    }
    Ok(Some(snapshot))
}

fn encode_snapshot(snapshot: KnownGoodSnapshot) -> io::Result<Vec<u8>> {
    snapshot.validate()?;
    let bytes = serde_json::to_vec(&snapshot)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if bytes.len() as u64 > MAX_KNOWN_GOOD_SNAPSHOT_BYTES {
        return Err(invalid_snapshot("known-good snapshot is too large"));
    }
    Ok(bytes)
}

fn validate_identity(instance_id: &str, version_id: &str) -> io::Result<()> {
    if !is_canonical_instance_id(instance_id) {
        return Err(invalid_snapshot("invalid known-good instance identity"));
    }
    validate_safe_segment(version_id, "version identity")
}

fn validate_safe_segment(value: &str, label: &str) -> io::Result<()> {
    let path = KnownGoodRelativePath::new(value)
        .map_err(|_| invalid_snapshot(format!("invalid known-good {label}")))?;
    if path.as_str().contains('/') || value.len() > MAX_KNOWN_GOOD_PATH_SEGMENT_BYTES {
        return Err(invalid_snapshot(format!("invalid known-good {label}")));
    }
    Ok(())
}

fn validate_digest(value: &str) -> io::Result<()> {
    if value.len() != 40
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(invalid_snapshot("invalid known-good SHA-1 digest"));
    }
    Ok(())
}

fn validate_link_target(link_path: &str, target: &str) -> io::Result<()> {
    if target.is_empty()
        || target.len() > MAX_KNOWN_GOOD_RELATIVE_PATH_BYTES
        || target.starts_with('/')
        || target.starts_with('\\')
        || target.chars().any(char::is_control)
    {
        return Err(invalid_snapshot("unsafe known-good link target"));
    }
    let mut resolved = link_path.split('/').collect::<Vec<_>>();
    resolved.pop();
    for segment in target.split('/') {
        match segment {
            ".." => {
                if resolved.pop().is_none() {
                    return Err(invalid_snapshot("escaping known-good link target"));
                }
            }
            "" | "." => return Err(invalid_snapshot("unsafe known-good link target")),
            value => {
                validate_safe_segment(value, "link target segment")?;
                resolved.push(value);
            }
        }
    }
    Ok(())
}

fn known_good_target(instance_id: &str) -> TargetDescriptor {
    TargetDescriptor::new(
        StabilizationSystem::State,
        TargetKind::Instance,
        instance_id,
        OwnershipClass::LauncherManaged,
    )
}

pub(super) fn normalize_library_root(path: &Path) -> io::Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                let _ = normalized.pop();
            }
            Component::Normal(value) => normalized.push(value),
        }
    }
    let canonical = fs::canonicalize(normalized)?;
    let metadata = fs::symlink_metadata(&canonical)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "known-good library root must resolve to an existing directory",
        ));
    }
    Ok(canonical)
}

fn deactivate_active_inventory<T>(
    active: &mut ActiveInventories<T>,
    instance_id: &str,
    version_id: &str,
    created_at: &str,
    normalized_root: Option<&Path>,
) {
    match normalized_root {
        Some(library_root) => {
            active.remove_exact(instance_id, version_id, created_at, library_root)
        }
        None => active.remove_incarnation(instance_id, version_id, created_at),
    }
}

fn invalid_snapshot(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn closed_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::NotConnected,
        "known-good inventory persistence is closed",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::persistence::AtomicWriteBackend;
    use std::sync::Condvar;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    struct FileBackend {
        attempts: AtomicUsize,
        failures: AtomicUsize,
        block_next: AtomicBool,
        block_state: Mutex<BlockState>,
        block_changed: Condvar,
    }

    #[derive(Default)]
    struct BlockState {
        entered: bool,
        released: bool,
    }

    #[cfg(unix)]
    #[test]
    fn normalized_library_root_changes_when_a_root_symlink_is_retargeted() {
        use std::os::unix::fs::symlink;

        let (root, _) = paths("root-symlink-retarget");
        let first = root.join("first");
        let second = root.join("second");
        let linked = root.join("library-link");
        fs::create_dir_all(&first).expect("first root");
        fs::create_dir_all(&second).expect("second root");
        symlink(&first, &linked).expect("first symlink");
        let first_identity = normalize_library_root(&linked).expect("first identity");

        fs::remove_file(&linked).expect("remove first symlink");
        symlink(&second, &linked).expect("second symlink");
        let second_identity = normalize_library_root(&linked).expect("second identity");

        assert_eq!(
            first_identity,
            fs::canonicalize(&first).expect("canonical first")
        );
        assert_eq!(
            second_identity,
            fs::canonicalize(&second).expect("canonical second")
        );
        assert_ne!(first_identity, second_identity);
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn duplicate_claim_fails_until_the_owned_store_closes() {
        let (root, paths) = paths("duplicate-owner");
        let backend = FileBackend::new(0);
        let coordinator = PersistenceCoordinator::for_test(
            backend,
            Duration::from_millis(1),
            Duration::from_millis(2),
        );
        let first = KnownGoodInventoryStore::claim_with_coordinator(&paths, coordinator.clone())
            .expect("first owner");

        let duplicate =
            KnownGoodInventoryStore::claim_with_coordinator(&paths, coordinator.clone())
                .err()
                .expect("duplicate owner must fail");
        assert_eq!(duplicate.kind(), io::ErrorKind::AlreadyExists);

        first.close().await.expect("close first owner");
        KnownGoodInventoryStore::claim_with_coordinator(&paths, coordinator)
            .expect("closed owner root is reclaimable");
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn conflicting_snapshot_lane_rejects_reconciliation_without_pending_state() {
        let (root, paths) = paths("snapshot-lane-conflict");
        let backend = FileBackend::new(0);
        let coordinator = PersistenceCoordinator::for_test(
            backend,
            Duration::from_millis(1),
            Duration::from_millis(2),
        );
        let store = KnownGoodInventoryStore::claim_with_coordinator(&paths, coordinator.clone())
            .expect("known-good owner");
        fs::create_dir_all(&store.root).expect("known-good directory");
        let current = snapshot("0000000000000010", "1.21.5");
        let path = store.snapshot_path(&current.instance_id);
        let conflicting_owner = coordinator.claim_owner(&path).expect("conflicting owner");
        let conflicting_writer = conflicting_owner
            .writer(&path, known_good_target(&current.instance_id))
            .expect("conflicting writer");

        let error = store
            .reconcile_snapshot(current)
            .await
            .expect_err("physical lane collision must fail reconciliation");
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        {
            let state = store.state.lock().expect(STORE_LOCK_INVARIANT);
            assert!(state.pending.is_empty());
            assert!(state.writers.is_empty());
        }

        drop(conflicting_writer);
        conflicting_owner
            .close()
            .await
            .expect("close conflicting owner");
        store.close().await.expect("close known-good owner");
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn failed_new_writer_admission_leaves_no_store_or_live_state() {
        let (root, paths) = paths("writer-admission-failure");
        let backend = FileBackend::new(0);
        let store = store(&paths, backend);
        let current = snapshot("0000000000000011", "1.21.5");
        let path = store.snapshot_path(&current.instance_id);
        let exhausted_writer = store
            .owner
            .writer(&path, known_good_target(&current.instance_id))
            .expect("unpublished writer");
        exhausted_writer.exhaust_revisions_for_test();

        let error = store
            .reconcile_snapshot(current)
            .await
            .expect_err("revision exhaustion must reject persistence admission");
        assert_eq!(error.kind(), io::ErrorKind::Other);
        {
            let state = store.state.lock().expect(STORE_LOCK_INVARIANT);
            assert!(state.pending.is_empty());
            assert!(state.writers.is_empty());
        }
        assert!(
            store
                .active
                .lock()
                .expect(STORE_LOCK_INVARIANT)
                .by_instance
                .is_empty()
        );

        drop(exhausted_writer);
        store.close().await.expect("close known-good owner");
        let _ = fs::remove_dir_all(root);
    }

    impl FileBackend {
        fn new(failures: usize) -> Arc<Self> {
            Arc::new(Self {
                attempts: AtomicUsize::new(0),
                failures: AtomicUsize::new(failures),
                block_next: AtomicBool::new(false),
                block_state: Mutex::new(BlockState::default()),
                block_changed: Condvar::new(),
            })
        }

        fn block_next_write(&self) {
            let mut state = self.block_state.lock().expect("block state");
            state.entered = false;
            state.released = false;
            self.block_next.store(true, Ordering::SeqCst);
        }

        async fn wait_until_blocked(&self) {
            for _ in 0..100 {
                if self.block_state.lock().expect("block state").entered {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            panic!("write did not block");
        }

        fn release_blocked_write(&self) {
            let mut state = self.block_state.lock().expect("block state");
            state.released = true;
            self.block_changed.notify_all();
        }
    }

    impl AtomicWriteBackend for FileBackend {
        fn write(
            &self,
            _target: &TargetDescriptor,
            destination: &Path,
            contents: &[u8],
        ) -> io::Result<()> {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            if self.block_next.swap(false, Ordering::SeqCst) {
                let mut state = self.block_state.lock().expect("block state");
                state.entered = true;
                self.block_changed.notify_all();
                state = self
                    .block_changed
                    .wait_while(state, |state| !state.released)
                    .expect("wait to release blocked write");
                state.entered = false;
            }
            if self
                .failures
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |failures| {
                    (failures > 0).then(|| failures - 1)
                })
                .is_ok()
            {
                return Err(io::Error::other("injected known-good write failure"));
            }
            fs::write(destination, contents)
        }
    }

    #[test]
    fn strict_schema_rejects_unknown_fields_and_invalid_contracts() {
        assert_eq!(KNOWN_GOOD_SCHEMA, "axial.state.known_good_inventory.v4");
        let mut value =
            serde_json::to_value(snapshot("0000000000000001", "1.21.5")).expect("snapshot value");
        value["extra"] = serde_json::json!(true);
        assert!(serde_json::from_value::<KnownGoodSnapshot>(value).is_err());

        let mut invalid = snapshot("0000000000000001", "1.21.5");
        invalid.entries[0].root = KnownGoodRootSnapshot::Assets;
        assert!(invalid.validate().is_err());

        let mut duplicate = snapshot("0000000000000001", "1.21.5");
        duplicate.entries.push(duplicate.entries[0].clone());
        assert!(duplicate.validate().is_err());

        for legacy in [
            "axial.state.known_good_inventory.v2",
            "axial.state.known_good_inventory.v3",
        ] {
            let mut snapshot = snapshot("0000000000000001", "1.21.5");
            snapshot.schema = legacy.to_string();
            assert!(snapshot.validate().is_err());
        }

        for size in [serde_json::Value::Null, serde_json::json!(-1)] {
            let mut invalid_size = serde_json::to_value(snapshot("0000000000000001", "1.21.5"))
                .expect("snapshot value");
            invalid_size["entries"][0]["integrity"]["size"] = size;
            assert!(serde_json::from_value::<KnownGoodSnapshot>(invalid_size).is_err());
        }
        let mut missing_size =
            serde_json::to_value(snapshot("0000000000000001", "1.21.5")).expect("snapshot value");
        missing_size["entries"][0]["integrity"]
            .as_object_mut()
            .expect("integrity object")
            .remove("size");
        assert!(serde_json::from_value::<KnownGoodSnapshot>(missing_size).is_err());

        let mut structural =
            serde_json::to_value(snapshot("0000000000000001", "1.21.5")).expect("snapshot value");
        structural["entries"][0]["kind"] = serde_json::json!("library");
        structural["entries"][0]["root"] = serde_json::json!({ "kind": "libraries" });
        structural["entries"][0]["integrity"] =
            serde_json::json!({ "kind": "structural_jar", "size": 42 });
        assert!(serde_json::from_value::<KnownGoodSnapshot>(structural).is_err());
    }

    #[test]
    fn active_authority_requires_exact_incarnation_and_replaces_only_its_instance() {
        let root = PathBuf::from("/library");
        let other_root = PathBuf::from("/other-library");
        let first = Arc::new(1_u8);
        let replacement = Arc::new(2_u8);
        let unrelated = Arc::new(3_u8);
        let mut active = ActiveInventories::default();

        active.activate(
            "0000000000000001",
            "1.21.5",
            "created-1",
            root.clone(),
            first,
        );
        active.activate(
            "0000000000000002",
            "1.21.6",
            "created-2",
            root.clone(),
            unrelated.clone(),
        );
        assert!(
            active
                .get("0000000000000001", "1.21.4", "created-1", &root)
                .is_none()
        );
        assert!(
            active
                .get("0000000000000001", "1.21.5", "created-other", &root)
                .is_none()
        );
        assert!(
            active
                .get("0000000000000001", "1.21.5", "created-1", &other_root,)
                .is_none()
        );

        active.activate(
            "0000000000000001",
            "1.21.7",
            "created-1",
            other_root.clone(),
            replacement.clone(),
        );
        assert!(
            active
                .get("0000000000000001", "1.21.5", "created-1", &root)
                .is_none()
        );
        assert!(Arc::ptr_eq(
            &active
                .get("0000000000000001", "1.21.7", "created-1", &other_root,)
                .expect("replacement authority"),
            &replacement
        ));
        assert!(Arc::ptr_eq(
            &active
                .get("0000000000000002", "1.21.6", "created-2", &root)
                .expect("unrelated authority"),
            &unrelated
        ));
    }

    #[test]
    fn same_id_version_and_root_recreation_cannot_inherit_or_lose_replacement_authority() {
        let instance_id = "0000000000000001";
        let version_id = "1.21.5";
        let root = PathBuf::from("/library");
        let replacement = Arc::new(2_u8);
        let mut active = ActiveInventories::default();

        active.activate(
            instance_id,
            version_id,
            "old-created-at",
            root.clone(),
            Arc::new(1_u8),
        );
        active.activate(
            instance_id,
            version_id,
            "new-created-at",
            root.clone(),
            replacement.clone(),
        );

        assert!(
            active
                .get(instance_id, version_id, "old-created-at", &root)
                .is_none(),
            "the old incarnation must not use replacement authority as a live fast path"
        );
        active.remove_exact(instance_id, version_id, "old-created-at", &root);
        assert!(Arc::ptr_eq(
            &active
                .get(instance_id, version_id, "new-created-at", &root)
                .expect("replacement authority survives stale cleanup"),
            &replacement
        ));
    }

    #[test]
    fn incompatible_snapshot_is_rejected_before_authority_activation() {
        let instance_id = "0000000000000001";
        let version_id = "1.21.5";
        let root = PathBuf::from("/library");
        let mut incompatible = snapshot(instance_id, version_id);
        incompatible.entries[0].integrity = KnownGoodIntegritySnapshot::ExactBytes {
            digest: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
            size: 42,
        };
        let mut active = ActiveInventories::default();

        let error = active
            .activate_validated(
                &incompatible,
                instance_id,
                version_id,
                "created-1",
                root.clone(),
                Arc::new(1_u8),
            )
            .expect_err("incompatible producer contract must fail closed");

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(
            active
                .get(instance_id, version_id, "created-1", &root)
                .is_none()
        );
    }

    #[test]
    fn retirement_exact_removal_and_clear_are_fail_closed() {
        let root = PathBuf::from("/library");
        let mut active = ActiveInventories::default();
        active.activate(
            "0000000000000001",
            "1.21.5",
            "created-1",
            root.clone(),
            Arc::new(1_u8),
        );
        active.activate(
            "0000000000000002",
            "1.21.6",
            "created-2",
            root.clone(),
            Arc::new(2_u8),
        );

        active.remove_exact("0000000000000001", "1.21.4", "created-1", &root);
        assert!(
            active
                .get("0000000000000001", "1.21.5", "created-1", &root)
                .is_some()
        );
        active.remove("0000000000000001");
        assert!(
            active
                .get("0000000000000001", "1.21.5", "created-1", &root)
                .is_none()
        );
        assert!(
            active
                .get("0000000000000002", "1.21.6", "created-2", &root)
                .is_some()
        );

        active.clear();
        assert!(
            active
                .get("0000000000000002", "1.21.6", "created-2", &root)
                .is_none()
        );
    }

    #[test]
    fn deactivation_with_unresolvable_root_removes_only_the_exact_live_identity() {
        let unique = format!(
            "axial-missing-known-good-root-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        );
        let bound_root = std::env::temp_dir().join(format!("{unique}-bound"));
        let missing_root = std::env::temp_dir().join(format!("{unique}-missing"));
        let mut active = ActiveInventories::default();
        active.activate(
            "0000000000000001",
            "1.21.5",
            "created-1",
            bound_root.clone(),
            Arc::new(1_u8),
        );
        active.activate(
            "0000000000000002",
            "1.21.6",
            "created-2",
            bound_root.clone(),
            Arc::new(2_u8),
        );

        assert!(normalize_library_root(&missing_root).is_err());
        deactivate_active_inventory(&mut active, "0000000000000001", "1.21.4", "created-1", None);
        assert!(
            active
                .get("0000000000000001", "1.21.5", "created-1", &bound_root,)
                .is_some()
        );

        deactivate_active_inventory(&mut active, "0000000000000001", "1.21.5", "created-1", None);
        assert!(
            active
                .get("0000000000000001", "1.21.5", "created-1", &bound_root,)
                .is_none()
        );
        assert!(
            active
                .get("0000000000000002", "1.21.6", "created-2", &bound_root,)
                .is_some()
        );
    }

    #[tokio::test]
    async fn missing_corrupt_v3_and_stale_snapshots_are_replaced_without_warning_state() {
        let (root, paths) = paths("replace");
        let backend = FileBackend::new(0);
        let store = store(&paths, backend.clone());
        let current = snapshot("0000000000000001", "1.21.5");
        store
            .reconcile_snapshot(current.clone())
            .await
            .expect("reconcile missing snapshot");
        store.flush_for_test().await.expect("flush missing rebuild");

        let path = store.snapshot_path(&current.instance_id);
        fs::write(&path, b"{not-json").expect("corrupt snapshot");
        store
            .reconcile_snapshot(current.clone())
            .await
            .expect("reconcile corrupt snapshot");
        store.flush_for_test().await.expect("flush corrupt rebuild");

        let mut v3 = current.clone();
        v3.schema = "axial.state.known_good_inventory.v3".to_string();
        fs::write(&path, serde_json::to_vec(&v3).expect("v3 bytes")).expect("v3 snapshot");
        assert_eq!(read_snapshot(&path).expect("read v3 snapshot"), None);
        store
            .reconcile_snapshot(current.clone())
            .await
            .expect("reconcile v3 snapshot");
        store.flush_for_test().await.expect("flush v3 rebuild");

        let stale = snapshot(&current.instance_id, "1.21.4");
        fs::write(&path, encode_snapshot(stale).expect("stale bytes")).expect("stale snapshot");
        store
            .reconcile_snapshot(current.clone())
            .await
            .expect("reconcile stale snapshot");
        store.flush_for_test().await.expect("flush stale rebuild");
        assert_eq!(read_snapshot(&path).expect("read rebuilt"), Some(current));
        assert_eq!(backend.attempts.load(Ordering::SeqCst), 4);
        drop(store);
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn equal_snapshot_does_not_schedule_a_write() {
        let (root, paths) = paths("equal");
        let backend = FileBackend::new(0);
        let store = store(&paths, backend.clone());
        let current = snapshot("0000000000000002", "1.21.5");
        let path = store.snapshot_path(&current.instance_id);
        fs::write(
            &path,
            encode_snapshot(current.clone()).expect("snapshot bytes"),
        )
        .expect("seed snapshot");

        store
            .reconcile_snapshot(current)
            .await
            .expect("reconcile equal snapshot");

        assert_eq!(backend.attempts.load(Ordering::SeqCst), 0);
        drop(store);
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn persisted_snapshot_never_hydrates_runtime_authority() {
        let (root, paths) = paths("disk-is-not-authority");
        let backend = FileBackend::new(0);
        let current = snapshot("0000000000000002", "1.21.5");
        let snapshot_root = paths.config_dir.join("state").join(KNOWN_GOOD_DIR);
        fs::create_dir_all(&snapshot_root).expect("snapshot root");
        let path = snapshot_root.join(format!("{}.json", current.instance_id));
        let bytes = encode_snapshot(current).expect("snapshot bytes");
        fs::write(&path, &bytes).expect("seed snapshot");

        let store = store(&paths, backend.clone());
        assert_eq!(backend.attempts.load(Ordering::SeqCst), 0);
        assert_eq!(fs::read(path).expect("read untouched snapshot"), bytes);
        assert!(
            store
                .active_inventory(
                    "0000000000000002",
                    "1.21.5",
                    "created-2",
                    &paths.library_dir,
                )
                .is_none()
        );
        drop(store);
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn equal_disk_truth_supersedes_a_different_pending_candidate() {
        let (root, paths) = paths("equal-supersedes-pending");
        let backend = FileBackend::new(0);
        let store = store(&paths, backend.clone());
        let current = snapshot("0000000000000004", "1.21.5");
        let stale = snapshot(&current.instance_id, "1.21.4");
        let path = store.snapshot_path(&current.instance_id);
        fs::write(
            &path,
            encode_snapshot(current.clone()).expect("current snapshot bytes"),
        )
        .expect("seed current snapshot");
        backend.block_next_write();
        store
            .reconcile_snapshot(stale)
            .await
            .expect("accept stale in-flight candidate");
        backend.wait_until_blocked().await;

        store
            .reconcile_snapshot(current.clone())
            .await
            .expect("current truth supersedes pending candidate");
        backend.release_blocked_write();
        store.flush_for_test().await.expect("flush successor");

        assert_eq!(backend.attempts.load(Ordering::SeqCst), 2);
        assert_eq!(read_snapshot(&path).expect("read current"), Some(current));
        drop(store);
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn failed_cache_write_is_retried_without_rejecting_fresh_truth() {
        let (root, paths) = paths("retry");
        let backend = FileBackend::new(1);
        let store = store(&paths, backend.clone());
        let current = snapshot("0000000000000003", "1.21.5");
        store
            .reconcile_snapshot(current.clone())
            .await
            .expect("fresh truth remains admitted");
        assert!(store.flush_for_test().await.is_err());
        wait_for_failed_pending(&store, &current.instance_id).await;
        let writer = store
            .state
            .lock()
            .expect(STORE_LOCK_INVARIANT)
            .writers
            .get(&current.instance_id)
            .cloned()
            .expect("retained writer");
        let failed_revision = writer.latest_revision();

        store
            .reconcile_snapshot(current.clone())
            .await
            .expect("retry fresh truth");
        assert_eq!(writer.latest_revision(), failed_revision);
        store.flush_for_test().await.expect("retry persists");

        assert_eq!(
            read_snapshot(&store.snapshot_path(&current.instance_id)).expect("read retry"),
            Some(current)
        );
        assert_eq!(backend.attempts.load(Ordering::SeqCst), 2);
        drop(store);
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn retirement_settles_pending_work_and_deletes_only_the_exact_snapshot() {
        let (root, paths) = paths("retire");
        let backend = FileBackend::new(1);
        let store = Arc::new(store(&paths, backend.clone()));
        let current = snapshot("0000000000000005", "1.21.5");
        let path = store.snapshot_path(&current.instance_id);
        let sibling = store.snapshot_path("0000000000000006");
        fs::write(&sibling, b"sibling").expect("seed sibling");

        store
            .reconcile_snapshot(current.clone())
            .await
            .expect("admit truth before retirement");
        assert!(store.flush_for_test().await.is_err());
        wait_for_failed_pending(&store, &current.instance_id).await;
        store
            .reserve_retirement(&current.instance_id)
            .expect("reserve exact retirement")
            .commit()
            .await
            .expect("settle and retire exact snapshot");

        assert_eq!(backend.attempts.load(Ordering::SeqCst), 2);
        assert!(!path.exists());
        assert_eq!(fs::read(&sibling).expect("read sibling"), b"sibling");
        let state = store.state.lock().expect(STORE_LOCK_INVARIANT);
        assert!(!state.pending.contains_key(&current.instance_id));
        assert!(!state.writers.contains_key(&current.instance_id));
        drop(state);
        drop(store);
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn committed_retirement_delete_failure_is_retained_and_retried_on_close() {
        let (root, paths) = paths("retirement-delete-retry");
        let backend = FileBackend::new(0);
        let store = Arc::new(store(&paths, backend));
        let instance_id = "0000000000000041";
        fs::create_dir_all(&store.root).expect("known-good directory");
        let path = store.snapshot_path(instance_id);
        fs::write(
            &path,
            encode_snapshot(snapshot(instance_id, "1.21.5")).expect("snapshot"),
        )
        .expect("seed snapshot");
        store.fail_retirement_deletes(1);

        let retirement = store
            .reserve_retirement(instance_id)
            .expect("reserve retirement");
        retirement
            .commit()
            .await
            .expect_err("first delete must fail");
        assert_eq!(
            store.pending_retirement_ids(),
            vec![instance_id.to_string()]
        );
        assert!(path.is_file());

        store.close().await.expect("close retries retirement");
        assert!(store.pending_retirement_ids().is_empty());
        assert!(!path.exists());
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn uncommitted_retirement_reservation_leaves_known_good_untouched() {
        let (root, paths) = paths("retirement-reservation-compensation");
        let backend = FileBackend::new(0);
        let store = Arc::new(store(&paths, backend));
        let instance_id = "0000000000000040";
        fs::create_dir_all(&store.root).expect("known-good directory");
        let path = store.snapshot_path(instance_id);
        fs::write(
            &path,
            encode_snapshot(snapshot(instance_id, "1.21.5")).expect("snapshot"),
        )
        .expect("seed snapshot");

        let retirement = store
            .reserve_retirement(instance_id)
            .expect("reserve retirement");
        drop(retirement);

        assert!(store.pending_retirement_ids().is_empty());
        assert!(path.is_file());
        store.close().await.expect("close untouched store");
        assert!(path.is_file());
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn duplicate_retirement_reservation_cannot_disarm_the_exact_owner() {
        let (root, paths) = paths("retirement-reservation-owner");
        let store = Arc::new(store(&paths, FileBackend::new(0)));
        let instance_id = "0000000000000041";

        let retirement = store
            .reserve_retirement(instance_id)
            .expect("reserve exact retirement");
        let error = store
            .reserve_retirement(instance_id)
            .err()
            .expect("duplicate reservation must be rejected");
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(
            store.cleanup.lock().expect(STORE_LOCK_INVARIANT).reserved,
            BTreeSet::from([instance_id.to_string()])
        );

        drop(retirement);
        assert!(store.pending_retirement_ids().is_empty());
        drop(
            store
                .reserve_retirement(instance_id)
                .expect("released retirement can be reserved again"),
        );
        store.close().await.expect("close retirement store");
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn committed_retirement_writer_failure_retains_exact_cleanup_obligation() {
        let (root, paths) = paths("retirement-writer-retry");
        let backend = FileBackend::new(2);
        let store = Arc::new(store(&paths, backend));
        let instance_id = "0000000000000042";
        store
            .reconcile_snapshot(snapshot(instance_id, "1.21.5"))
            .await
            .expect("accept pending snapshot");
        wait_for_failed_pending(&store, instance_id).await;

        let retirement = store
            .reserve_retirement(instance_id)
            .expect("reserve retirement");
        retirement
            .commit()
            .await
            .expect_err("settlement retry must fail");
        assert_eq!(
            store.pending_retirement_ids(),
            vec![instance_id.to_string()]
        );

        store
            .close()
            .await
            .expect("close retries writer and cleanup");
        assert!(store.pending_retirement_ids().is_empty());
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn restart_discovers_only_absent_instance_snapshot_cleanup() {
        let (root, paths) = paths("restart-retirement-discovery");
        let backend = FileBackend::new(0);
        let store = Arc::new(store(&paths, backend));
        let absent = "0000000000000043";
        let registered = "0000000000000044";
        fs::create_dir_all(&store.root).expect("known-good directory");
        let absent_path = store.snapshot_path(absent);
        let registered_path = store.snapshot_path(registered);
        fs::write(
            &absent_path,
            encode_snapshot(snapshot(absent, "1.21.5")).expect("absent snapshot"),
        )
        .expect("seed absent snapshot");
        fs::write(
            &registered_path,
            encode_snapshot(snapshot(registered, "1.21.5")).expect("registered snapshot"),
        )
        .expect("seed registered snapshot");

        store
            .discover_absent_snapshot_obligations([registered.to_string()])
            .expect("discover absent snapshot");
        assert_eq!(store.pending_retirement_ids(), vec![absent.to_string()]);

        store.close().await.expect("close discovered cleanup");
        assert!(!absent_path.exists());
        assert!(registered_path.is_file());
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn cancelled_close_restores_running_admission() {
        let (root, paths) = paths("cancel-close");
        let backend = FileBackend::new(0);
        let store = Arc::new(store(&paths, backend.clone()));
        let current = snapshot("0000000000000007", "1.21.5");
        backend.block_next_write();
        store
            .reconcile_snapshot(current.clone())
            .await
            .expect("accept blocked snapshot");
        backend.wait_until_blocked().await;

        let closing_store = store.clone();
        let closing = tokio::spawn(async move { closing_store.close().await });
        wait_for_phase(&store, StorePhase::Closing).await;
        closing.abort();
        assert!(
            closing
                .await
                .expect_err("close task must be cancelled")
                .is_cancelled()
        );
        assert_eq!(store.phase(), StorePhase::Running);

        backend.release_blocked_write();
        store
            .flush_for_test()
            .await
            .expect("flush after cancellation");
        let successor = snapshot(&current.instance_id, "1.21.6");
        store
            .reconcile_snapshot(successor.clone())
            .await
            .expect("admit after cancelled close");
        store.flush_for_test().await.expect("flush successor");
        assert_eq!(
            read_snapshot(&store.snapshot_path(&current.instance_id)).expect("read successor"),
            Some(successor)
        );
        drop(store);
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn failed_close_retries_the_same_revision_on_the_next_close() {
        let (root, paths) = paths("retry-close");
        let backend = FileBackend::new(2);
        let store = store(&paths, backend.clone());
        let current = snapshot("0000000000000008", "1.21.5");
        store
            .reconcile_snapshot(current.clone())
            .await
            .expect("accept snapshot before close");
        let writer = store
            .state
            .lock()
            .expect(STORE_LOCK_INVARIANT)
            .writers
            .get(&current.instance_id)
            .cloned()
            .expect("retained writer");
        let accepted_revision = writer.latest_revision();

        assert!(store.close().await.is_err());
        assert_eq!(store.phase(), StorePhase::Running);
        assert_eq!(writer.latest_revision(), accepted_revision);
        store.close().await.expect("retry close");

        assert_eq!(store.phase(), StorePhase::Closed);
        assert_eq!(writer.latest_revision(), accepted_revision);
        assert_eq!(backend.attempts.load(Ordering::SeqCst), 3);
        assert_eq!(
            read_snapshot(&store.snapshot_path(&current.instance_id))
                .expect("read closed snapshot"),
            Some(current)
        );
        drop(store);
        let _ = fs::remove_dir_all(root);
    }

    async fn wait_for_failed_pending(store: &KnownGoodInventoryStore, instance_id: &str) {
        for _ in 0..100 {
            if store
                .state
                .lock()
                .expect(STORE_LOCK_INVARIANT)
                .pending
                .get(instance_id)
                .is_some_and(|pending| pending.failed)
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        panic!("known-good failure observer did not retain the failed candidate");
    }

    async fn wait_for_phase(store: &KnownGoodInventoryStore, expected: StorePhase) {
        for _ in 0..100 {
            if store.phase() == expected {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("known-good store did not enter {expected:?}");
    }

    fn snapshot(instance_id: &str, version_id: &str) -> KnownGoodSnapshot {
        KnownGoodSnapshot {
            schema: KNOWN_GOOD_SCHEMA.to_string(),
            instance_id: instance_id.to_string(),
            version_id: version_id.to_string(),
            entries: vec![KnownGoodEntrySnapshot {
                root: KnownGoodRootSnapshot::Versions,
                path: format!("{version_id}/{version_id}.jar"),
                kind: KnownGoodArtifactKindSnapshot::ClientJar,
                integrity: KnownGoodIntegritySnapshot::Sha1 {
                    digest: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
                    size: 42,
                },
            }],
        }
    }

    fn store(paths: &AppPaths, backend: Arc<FileBackend>) -> KnownGoodInventoryStore {
        let store = KnownGoodInventoryStore::claim_with_coordinator(
            paths,
            PersistenceCoordinator::for_test(
                backend,
                Duration::from_millis(1),
                Duration::from_millis(2),
            ),
        )
        .expect("known-good store");
        fs::create_dir_all(&store.root).expect("known-good test directory");
        store
    }

    fn paths(name: &str) -> (PathBuf, AppPaths) {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "axial-known-good-{name}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&root).expect("test root");
        (
            root.clone(),
            AppPaths {
                config_dir: root.clone(),
                config_file: root.join("config.json"),
                instances_file: root.join("instances.json"),
                instances_dir: root.join("instances"),
                music_dir: root.join("music"),
                library_dir: root.join("library"),
            },
        )
    }
}
