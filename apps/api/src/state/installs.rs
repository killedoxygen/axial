use axial_config::Instance;
use axial_content::ContentKind;
use axial_minecraft::{LoaderComponentId, download::DownloadProgress};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    future::Future,
    path::PathBuf,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::sync::{Mutex as AsyncMutex, Notify, OwnedMutexGuard, RwLock, broadcast};
use tokio::task::JoinHandle;

use crate::state::{ProducerLease, contracts::OperationId};

struct InstallEntry {
    operation_id: OperationId,
    key: Option<InstallKey>,
    started_at_ms: u64,
    latest: Option<InstallProgressRecord>,
    record_events: broadcast::Sender<InstallProgressRecord>,
    done: bool,
    initializing: bool,
    initialization_reconciling: bool,
    terminalizing: bool,
    changed: Arc<Notify>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InstallInitializationStatus {
    Initialized,
    Reconciling,
    Removed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InstallAdmissionError {
    InstallIdCollision,
    OperationIdCollision,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstallSnapshot {
    pub operation_id: OperationId,
    pub latest: Option<InstallProgressRecord>,
    pub done: bool,
    pub loader_install: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstallProgressRecord {
    pub progress: DownloadProgress,
    vanilla_event_json: Option<Arc<str>>,
    loader_event_json: Option<Arc<str>>,
}

impl InstallProgressRecord {
    pub(crate) fn new(progress: DownloadProgress) -> Self {
        Self {
            progress,
            vanilla_event_json: None,
            loader_event_json: None,
        }
    }

    pub(crate) fn with_event_json(
        progress: DownloadProgress,
        vanilla_event_json: String,
        loader_event_json: String,
    ) -> Self {
        Self {
            progress,
            vanilla_event_json: Some(Arc::from(vanilla_event_json)),
            loader_event_json: Some(Arc::from(loader_event_json)),
        }
    }

    pub(crate) fn event_json(&self, loader_install: bool) -> Option<&str> {
        if loader_install {
            self.loader_event_json.as_deref()
        } else {
            self.vanilla_event_json.as_deref()
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueuedContentSelection {
    pub canonical_id: String,
    pub kind: ContentKind,
    pub version_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SetupInstanceCleanup {
    pub baseline: Option<Box<SetupInstanceBaseline>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SetupInstanceBaseline {
    pub instance: Instance,
    pub paths: Vec<SetupInstancePathSnapshot>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SetupInstancePathSnapshot {
    pub relative_path: PathBuf,
    pub kind: SetupInstancePathKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SetupInstancePathKind {
    Directory,
    File { size: u64, sha512: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContentQueueAction {
    Install {
        selections: Vec<QueuedContentSelection>,
        allow_incompatible: bool,
        setup_cleanup: Option<SetupInstanceCleanup>,
    },
    Uninstall {
        canonical_ids: Vec<String>,
    },
    Modpack {
        canonical_id: String,
        version_id: String,
        selected_file_ids: Vec<String>,
        include_overrides: bool,
        setup_cleanup: Option<SetupInstanceCleanup>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InstallQueueSpec {
    Vanilla {
        version_id: String,
    },
    Loader {
        component_id: LoaderComponentId,
        build_id: String,
        target_version_id: String,
        minecraft_version: String,
        loader_version: String,
    },
    Content {
        instance_id: String,
        label: String,
        action: ContentQueueAction,
        prerequisite_queue_id: Option<String>,
    },
}

impl InstallQueueSpec {
    pub fn vanilla(version_id: String) -> Self {
        Self::Vanilla {
            version_id: version_id.trim().to_string(),
        }
    }

    pub fn loader(
        component_id: LoaderComponentId,
        build_id: String,
        target_version_id: String,
        minecraft_version: String,
        loader_version: String,
    ) -> Self {
        Self::Loader {
            component_id,
            build_id: build_id.trim().to_string(),
            target_version_id: target_version_id.trim().to_string(),
            minecraft_version: minecraft_version.trim().to_string(),
            loader_version: loader_version.trim().to_string(),
        }
    }

    pub fn target_version_id(&self) -> &str {
        match self {
            Self::Vanilla { version_id, .. } => version_id,
            Self::Loader {
                target_version_id, ..
            } => target_version_id,
            Self::Content { instance_id, .. } => instance_id,
        }
    }

    pub fn is_loader(&self) -> bool {
        matches!(self, Self::Loader { .. })
    }

    pub fn is_content(&self) -> bool {
        matches!(self, Self::Content { .. })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueuedInstallEntry {
    pub queue_id: String,
    pub spec: InstallQueueSpec,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActiveQueuedInstallEntry {
    pub queue_id: String,
    pub install_id: Option<String>,
    /// Unix epoch milliseconds copied from the install session when this
    /// reserved queue entry is marked started. None means the queue entry has
    /// the active lane but its install session has not begun running yet.
    pub install_started_at_ms: Option<u64>,
    pub spec: InstallQueueSpec,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstallQueueSnapshot {
    pub active: Option<ActiveQueuedInstallEntry>,
    pub pending: Vec<QueuedInstallEntry>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InstallQueuePlacement {
    Back,
    Front,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InstallQueueEnqueueOutcome {
    Enqueued { queue_id: String },
    AlreadyActive { queue_id: String },
    AlreadyQueued { queue_id: String },
    MovedToFront { queue_id: String },
}

impl InstallQueueEnqueueOutcome {
    pub(crate) fn queue_id(&self) -> &str {
        match self {
            Self::Enqueued { queue_id }
            | Self::AlreadyActive { queue_id }
            | Self::AlreadyQueued { queue_id }
            | Self::MovedToFront { queue_id } => queue_id,
        }
    }
}

#[derive(Default)]
struct InstallQueueInner {
    active: Option<ActiveQueuedInstallEntry>,
    pending: VecDeque<QueuedInstallEntry>,
    completed: HashMap<String, bool>,
    completed_order: VecDeque<String>,
}

const MAX_COMPLETED_QUEUE_OUTCOMES: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq)]
enum InstallKey {
    Vanilla {
        version_id: String,
    },
    Loader {
        component_id: LoaderComponentId,
        build_id: String,
    },
}

pub struct InstallStore {
    installs: RwLock<HashMap<String, InstallEntry>>,
    queue: RwLock<InstallQueueInner>,
    // Production starters hold this through reservation and exact commit-or-discard settlement.
    queue_start_gate: Arc<AsyncMutex<()>>,
}

impl InstallStore {
    pub fn new() -> Self {
        Self {
            installs: RwLock::new(HashMap::new()),
            queue: RwLock::new(InstallQueueInner::default()),
            queue_start_gate: Arc::new(AsyncMutex::new(())),
        }
    }

    pub(crate) async fn acquire_queue_start_gate(&self) -> OwnedMutexGuard<()> {
        self.queue_start_gate.clone().lock_owned().await
    }

    pub async fn admit(
        &self,
        install_id: String,
        operation_id: OperationId,
    ) -> Result<(), InstallAdmissionError> {
        self.insert_entry(install_id, operation_id, None).await
    }

    pub async fn admit_or_existing_vanilla(
        &self,
        install_id: String,
        operation_id: OperationId,
        version_id: String,
    ) -> Result<(String, bool), InstallAdmissionError> {
        self.insert_with_identity(
            install_id,
            operation_id,
            InstallKey::Vanilla {
                version_id: version_id.trim().to_string(),
            },
        )
        .await
    }

    pub async fn admit_or_existing_loader(
        &self,
        install_id: String,
        operation_id: OperationId,
        component_id: LoaderComponentId,
        build_id: String,
    ) -> Result<(String, bool), InstallAdmissionError> {
        self.insert_with_identity(
            install_id,
            operation_id,
            InstallKey::Loader {
                component_id,
                build_id: build_id.trim().to_string(),
            },
        )
        .await
    }

    pub async fn operation_id(&self, install_id: &str) -> Option<OperationId> {
        self.installs
            .read()
            .await
            .get(install_id)
            .map(|entry| entry.operation_id.clone())
    }

    pub async fn contains_operation_id(&self, operation_id: &OperationId) -> bool {
        self.installs
            .read()
            .await
            .values()
            .any(|entry| &entry.operation_id == operation_id)
    }

    #[cfg(test)]
    pub async fn insert(&self, install_id: String) {
        let operation_id = OperationId::deterministic_test(&install_id);
        self.admit(install_id, operation_id)
            .await
            .expect("test operation id is unique");
    }

    #[cfg(test)]
    pub async fn insert_or_existing_vanilla(
        &self,
        install_id: String,
        version_id: String,
    ) -> (String, bool) {
        let operation_id = OperationId::deterministic_test(&install_id);
        self.admit_or_existing_vanilla(install_id, operation_id, version_id)
            .await
            .expect("test operation id is unique")
    }

    #[cfg(test)]
    pub async fn insert_or_existing_loader(
        &self,
        install_id: String,
        component_id: LoaderComponentId,
        build_id: String,
    ) -> (String, bool) {
        let operation_id = OperationId::deterministic_test(&install_id);
        self.admit_or_existing_loader(install_id, operation_id, component_id, build_id)
            .await
            .expect("test operation id is unique")
    }

    async fn insert_with_identity(
        &self,
        install_id: String,
        operation_id: OperationId,
        key: InstallKey,
    ) -> Result<(String, bool), InstallAdmissionError> {
        let mut installs = self.installs.write().await;
        prune_done_entries(&mut installs);
        if let Some(existing_id) = installs.iter().find_map(|(existing_id, entry)| {
            (!entry.done && entry.key.as_ref() == Some(&key)).then(|| existing_id.clone())
        }) {
            return Ok((existing_id, false));
        }
        if installs.contains_key(&install_id) {
            return Err(InstallAdmissionError::InstallIdCollision);
        }
        if installs
            .values()
            .any(|entry| entry.operation_id == operation_id)
        {
            return Err(InstallAdmissionError::OperationIdCollision);
        }

        let mut entry = new_install_entry(operation_id, Some(key));
        entry.initializing = true;
        installs.insert(install_id.clone(), entry);
        Ok((install_id, true))
    }

    pub async fn mark_initialized(&self, install_id: &str) -> bool {
        let changed = {
            let mut installs = self.installs.write().await;
            let Some(entry) = installs.get_mut(install_id) else {
                return false;
            };
            entry.initializing = false;
            entry.initialization_reconciling = false;
            entry.changed.clone()
        };
        changed.notify_waiters();
        true
    }

    pub async fn wait_until_initialized(&self, install_id: &str) -> bool {
        self.wait_for_initialization(install_id).await == InstallInitializationStatus::Initialized
    }

    pub(crate) async fn wait_for_initialization(
        &self,
        install_id: &str,
    ) -> InstallInitializationStatus {
        loop {
            let changed = {
                let installs = self.installs.read().await;
                let Some(entry) = installs.get(install_id) else {
                    return InstallInitializationStatus::Removed;
                };
                if entry.initialization_reconciling {
                    return InstallInitializationStatus::Reconciling;
                };
                if !entry.initializing {
                    return InstallInitializationStatus::Initialized;
                }
                entry.changed.clone().notified_owned()
            };
            changed.await;
        }
    }

    pub(crate) async fn mark_initialization_reconciling(&self, install_id: &str) -> bool {
        let changed = {
            let mut installs = self.installs.write().await;
            let Some(entry) = installs.get_mut(install_id) else {
                return false;
            };
            if !entry.initializing {
                return false;
            }
            entry.initialization_reconciling = true;
            entry.changed.clone()
        };
        changed.notify_waiters();
        true
    }

    async fn insert_entry(
        &self,
        install_id: String,
        operation_id: OperationId,
        key: Option<InstallKey>,
    ) -> Result<(), InstallAdmissionError> {
        let mut installs = self.installs.write().await;
        prune_done_entries(&mut installs);
        if let Some(existing) = installs.get(&install_id) {
            return if existing.operation_id == operation_id && existing.key.is_none() {
                Ok(())
            } else {
                Err(InstallAdmissionError::InstallIdCollision)
            };
        }
        if installs
            .values()
            .any(|entry| entry.operation_id == operation_id)
        {
            return Err(InstallAdmissionError::OperationIdCollision);
        }
        installs.insert(install_id, new_install_entry(operation_id, key));
        Ok(())
    }

    pub(crate) async fn emit(&self, install_id: &str, progress: DownloadProgress) {
        self.emit_record(install_id, InstallProgressRecord::new(progress))
            .await;
    }

    pub(crate) async fn emit_record(&self, install_id: &str, record: InstallProgressRecord) {
        let sender = {
            let mut installs = self.installs.write().await;
            let Some(entry) = installs.get_mut(install_id) else {
                return;
            };
            if entry.done || entry.terminalizing {
                return;
            }
            entry.done = record.progress.done;
            entry.latest = Some(record.clone());
            entry.record_events.clone()
        };
        let _ = sender.send(record);
    }

    pub async fn finish_if_active(&self, install_id: &str, mut progress: DownloadProgress) -> bool {
        progress.done = true;
        let record = InstallProgressRecord::new(progress);
        let sender = {
            let mut installs = self.installs.write().await;
            let Some(entry) = installs.get_mut(install_id) else {
                return false;
            };
            if entry.done || entry.terminalizing {
                return false;
            }

            entry.done = true;
            entry.latest = Some(record.clone());
            entry.record_events.clone()
        };
        let _ = sender.send(record);
        true
    }

    async fn reserve_terminal_if_active(&self, install_id: &str) -> bool {
        let mut installs = self.installs.write().await;
        let Some(entry) = installs.get_mut(install_id) else {
            return false;
        };
        if entry.done || entry.terminalizing {
            return false;
        }
        entry.terminalizing = true;
        true
    }

    async fn finish_reserved(&self, install_id: &str, mut progress: DownloadProgress) -> bool {
        progress.done = true;
        let record = InstallProgressRecord::new(progress);
        let sender = {
            let mut installs = self.installs.write().await;
            let Some(entry) = installs.get_mut(install_id) else {
                return false;
            };
            if entry.done || !entry.terminalizing {
                return false;
            }
            entry.terminalizing = false;
            entry.done = true;
            entry.latest = Some(record.clone());
            entry.record_events.clone()
        };
        let _ = sender.send(record);
        true
    }

    async fn cancel_reserved_terminal(&self, install_id: &str) {
        if let Some(entry) = self.installs.write().await.get_mut(install_id) {
            entry.terminalizing = false;
        }
    }

    pub async fn subscribe_records(
        &self,
        install_id: &str,
    ) -> Option<(InstallSnapshot, broadcast::Receiver<InstallProgressRecord>)> {
        let installs = self.installs.read().await;
        installs.get(install_id).map(|entry| {
            (
                InstallSnapshot {
                    operation_id: entry.operation_id.clone(),
                    latest: entry.latest.clone(),
                    done: entry.done,
                    loader_install: matches!(entry.key.as_ref(), Some(InstallKey::Loader { .. })),
                },
                entry.record_events.subscribe(),
            )
        })
    }

    pub(crate) fn spawn_tracked_worker_with_interrupt_handler_owned<F, H, HFut>(
        store: Arc<Self>,
        producer: ProducerLease,
        install_id: String,
        interrupted_progress: DownloadProgress,
        worker: F,
        on_interrupted: H,
    ) -> JoinHandle<()>
    where
        F: Future<Output = ()> + Send + 'static,
        H: FnOnce(DownloadProgress) -> HFut + Send + 'static,
        HFut: Future<Output = bool> + Send + 'static,
    {
        Self::spawn_tracked_worker_with_interrupt_progress_owned(
            store,
            producer,
            install_id,
            interrupted_progress,
            worker,
            move |fallback| async move { on_interrupted(fallback.clone()).await.then_some(fallback) },
        )
    }

    pub(crate) fn spawn_tracked_worker_with_interrupt_progress_owned<F, H, HFut>(
        store: Arc<Self>,
        producer: ProducerLease,
        install_id: String,
        interrupted_progress: DownloadProgress,
        worker: F,
        on_interrupted: H,
    ) -> JoinHandle<()>
    where
        F: Future<Output = ()> + Send + 'static,
        H: FnOnce(DownloadProgress) -> HFut + Send + 'static,
        HFut: Future<Output = Option<DownloadProgress>> + Send + 'static,
    {
        let worker = producer.claim_child().spawn_joinable(worker);
        producer.spawn_joinable(async move {
            let _ = worker.await;
            if !store.reserve_terminal_if_active(&install_id).await {
                return;
            }
            match on_interrupted(interrupted_progress).await {
                Some(progress) => {
                    let _ = store.finish_reserved(&install_id, progress).await;
                }
                None => store.cancel_reserved_terminal(&install_id).await,
            }
        })
    }

    #[cfg(test)]
    pub fn spawn_tracked_worker<F>(
        store: Arc<Self>,
        install_id: String,
        interrupted_progress: DownloadProgress,
        worker: F,
    ) -> JoinHandle<()>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let lifecycle = crate::state::AppLifecycle::new();
        let producer = lifecycle
            .try_claim_producer()
            .expect("claim test install worker");
        Self::spawn_tracked_worker_with_interrupt_handler_owned(
            store,
            producer,
            install_id,
            interrupted_progress,
            worker,
            |_| async { true },
        )
    }

    #[cfg(test)]
    pub fn spawn_tracked_worker_with_interrupt_handler<F, H, HFut>(
        store: Arc<Self>,
        install_id: String,
        interrupted_progress: DownloadProgress,
        worker: F,
        on_interrupted: H,
    ) -> JoinHandle<()>
    where
        F: Future<Output = ()> + Send + 'static,
        H: FnOnce(DownloadProgress) -> HFut + Send + 'static,
        HFut: Future<Output = bool> + Send + 'static,
    {
        let lifecycle = crate::state::AppLifecycle::new();
        let producer = lifecycle
            .try_claim_producer()
            .expect("claim test install worker");
        Self::spawn_tracked_worker_with_interrupt_handler_owned(
            store,
            producer,
            install_id,
            interrupted_progress,
            worker,
            on_interrupted,
        )
    }

    pub async fn snapshot(&self, install_id: &str) -> Option<InstallSnapshot> {
        self.installs
            .read()
            .await
            .get(install_id)
            .map(|entry| InstallSnapshot {
                operation_id: entry.operation_id.clone(),
                latest: entry.latest.clone(),
                done: entry.done,
                loader_install: matches!(entry.key.as_ref(), Some(InstallKey::Loader { .. })),
            })
    }

    pub async fn install_started_at_ms(&self, install_id: &str) -> Option<u64> {
        self.installs
            .read()
            .await
            .get(install_id)
            .map(|entry| entry.started_at_ms)
    }

    pub async fn active_vanilla_install(&self, version_id: &str) -> Option<String> {
        let version_id = version_id.trim();
        self.installs
            .read()
            .await
            .iter()
            .find_map(|(install_id, entry)| {
                let key = entry.key.as_ref()?;
                (!entry.done
                    && matches!(
                        key,
                        InstallKey::Vanilla {
                            version_id: existing_version_id
                        } if existing_version_id == version_id
                    ))
                .then(|| install_id.clone())
            })
    }

    pub async fn active_install_count(&self) -> usize {
        self.installs
            .read()
            .await
            .values()
            .filter(|entry| !entry.done)
            .count()
    }

    pub async fn enqueue_queued_install(
        &self,
        queue_id: String,
        spec: InstallQueueSpec,
        placement: InstallQueuePlacement,
    ) -> InstallQueueEnqueueOutcome {
        let mut queue = self.queue.write().await;
        if let Some(active) = queue.active.as_ref().filter(|active| active.spec == spec) {
            return InstallQueueEnqueueOutcome::AlreadyActive {
                queue_id: active.queue_id.clone(),
            };
        }

        if let Some(position) = queue.pending.iter().position(|entry| entry.spec == spec) {
            let existing_id = queue.pending[position].queue_id.clone();
            if placement == InstallQueuePlacement::Front && position > 0 {
                let entry = queue
                    .pending
                    .remove(position)
                    .expect("pending position is valid");
                queue.pending.push_front(entry);
                return InstallQueueEnqueueOutcome::MovedToFront {
                    queue_id: existing_id,
                };
            }
            return InstallQueueEnqueueOutcome::AlreadyQueued {
                queue_id: existing_id,
            };
        }

        let entry = QueuedInstallEntry {
            queue_id: queue_id.clone(),
            spec,
        };
        match placement {
            InstallQueuePlacement::Back => queue.pending.push_back(entry),
            InstallQueuePlacement::Front => queue.pending.push_front(entry),
        }
        InstallQueueEnqueueOutcome::Enqueued { queue_id }
    }

    pub async fn reserve_next_queued_install(&self) -> Option<QueuedInstallEntry> {
        let mut queue = self.queue.write().await;
        if queue.active.is_some() {
            return None;
        }
        let next = queue.pending.pop_front()?;
        queue.active = Some(ActiveQueuedInstallEntry {
            queue_id: next.queue_id.clone(),
            install_id: None,
            install_started_at_ms: None,
            spec: next.spec.clone(),
        });
        Some(next)
    }

    pub async fn mark_queued_install_started(&self, queue_id: &str, install_id: String) -> bool {
        let install_started_at_ms = self
            .install_started_at_ms(&install_id)
            .await
            .unwrap_or_else(now_unix_ms);
        let mut queue = self.queue.write().await;
        let Some(active) = queue.active.as_mut() else {
            return false;
        };
        if active.queue_id != queue_id {
            return false;
        }
        if active.install_started_at_ms.is_none() {
            active.install_started_at_ms = Some(install_started_at_ms);
        }
        active.install_id = Some(install_id);
        true
    }

    pub async fn clear_active_queued_install(
        &self,
        install_id: &str,
    ) -> Option<ActiveQueuedInstallEntry> {
        let mut queue = self.queue.write().await;
        if queue
            .active
            .as_ref()
            .and_then(|active| active.install_id.as_deref())
            != Some(install_id)
        {
            return None;
        }
        let cleared = queue.active.take();
        prune_queue_outcomes(&mut queue);
        cleared
    }

    pub async fn complete_active_queued_install(
        &self,
        install_id: &str,
        succeeded: bool,
    ) -> Option<ActiveQueuedInstallEntry> {
        let mut queue = self.queue.write().await;
        if queue
            .active
            .as_ref()
            .and_then(|active| active.install_id.as_deref())
            != Some(install_id)
        {
            return None;
        }
        let completed = queue.active.take()?;
        record_queue_outcome(&mut queue, &completed.queue_id, succeeded);
        Some(completed)
    }

    pub async fn complete_reserved_queued_install(
        &self,
        queue_id: &str,
        succeeded: bool,
    ) -> Option<ActiveQueuedInstallEntry> {
        let mut queue = self.queue.write().await;
        if queue.active.as_ref().map(|active| active.queue_id.as_str()) != Some(queue_id) {
            return None;
        }
        let completed = queue.active.take()?;
        record_queue_outcome(&mut queue, &completed.queue_id, succeeded);
        Some(completed)
    }

    pub async fn queued_install_succeeded(&self, queue_id: &str) -> Option<bool> {
        self.queue.read().await.completed.get(queue_id).copied()
    }

    pub async fn release_active_queued_install_to_front(&self, queue_id: &str) -> bool {
        let mut queue = self.queue.write().await;
        if queue.active.as_ref().map(|active| active.queue_id.as_str()) != Some(queue_id) {
            return false;
        }
        let Some(active) = queue.active.take() else {
            return false;
        };
        queue.pending.push_front(QueuedInstallEntry {
            queue_id: active.queue_id,
            spec: active.spec,
        });
        true
    }

    pub async fn discard_active_queued_install(&self, queue_id: &str) -> bool {
        let mut queue = self.queue.write().await;
        if queue.active.as_ref().map(|active| active.queue_id.as_str()) != Some(queue_id) {
            return false;
        }
        queue.active.take();
        prune_queue_outcomes(&mut queue);
        true
    }

    pub async fn remove_queued_install(&self, queue_id: &str) -> Option<QueuedInstallEntry> {
        let mut queue = self.queue.write().await;
        let position = queue
            .pending
            .iter()
            .position(|entry| entry.queue_id == queue_id)?;
        let removed = queue.pending.remove(position);
        prune_queue_outcomes(&mut queue);
        removed
    }

    pub async fn queue_snapshot(&self) -> InstallQueueSnapshot {
        let queue = self.queue.read().await;
        InstallQueueSnapshot {
            active: queue.active.clone(),
            pending: queue.pending.iter().cloned().collect(),
        }
    }

    pub async fn remove(&self, install_id: &str) {
        if let Some(entry) = self.installs.write().await.remove(install_id) {
            entry.changed.notify_waiters();
        }
    }
}

fn new_install_entry(operation_id: OperationId, key: Option<InstallKey>) -> InstallEntry {
    let (record_events, _) = broadcast::channel(256);
    InstallEntry {
        operation_id,
        key,
        started_at_ms: now_unix_ms(),
        latest: None,
        record_events,
        done: false,
        initializing: false,
        initialization_reconciling: false,
        terminalizing: false,
        changed: Arc::new(Notify::new()),
    }
}

fn prune_done_entries(installs: &mut HashMap<String, InstallEntry>) {
    installs.retain(|_, entry| !entry.done);
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or_default()
}

fn record_queue_outcome(queue: &mut InstallQueueInner, queue_id: &str, succeeded: bool) {
    queue
        .completed_order
        .retain(|completed_id| completed_id != queue_id);
    queue.completed.insert(queue_id.to_string(), succeeded);
    queue.completed_order.push_back(queue_id.to_string());
    prune_queue_outcomes(queue);
}

fn prune_queue_outcomes(queue: &mut InstallQueueInner) {
    let referenced: HashSet<String> = queue
        .active
        .iter()
        .map(|entry| &entry.spec)
        .chain(queue.pending.iter().map(|entry| &entry.spec))
        .filter_map(|spec| match spec {
            InstallQueueSpec::Content {
                prerequisite_queue_id,
                ..
            } => prerequisite_queue_id.clone(),
            _ => None,
        })
        .collect();
    while queue.completed_order.len() > MAX_COMPLETED_QUEUE_OUTCOMES {
        let Some(position) = queue
            .completed_order
            .iter()
            .position(|queue_id| !referenced.contains(queue_id))
        else {
            break;
        };
        let expired = queue
            .completed_order
            .remove(position)
            .expect("completed outcome position is valid");
        queue.completed.remove(&expired);
    }
}

impl Default for InstallStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axial_minecraft::build_id_for;

    #[test]
    fn queue_outcomes_keep_reused_ids_recent_and_unique() {
        let mut queue = InstallQueueInner::default();
        record_queue_outcome(&mut queue, "reused", false);
        for index in 0..MAX_COMPLETED_QUEUE_OUTCOMES - 1 {
            record_queue_outcome(&mut queue, &format!("older-{index}"), true);
        }

        record_queue_outcome(&mut queue, "reused", true);
        record_queue_outcome(&mut queue, "newest", true);

        assert_eq!(queue.completed.get("reused"), Some(&true));
        assert_eq!(
            queue
                .completed_order
                .iter()
                .filter(|queue_id| queue_id.as_str() == "reused")
                .count(),
            1
        );
        assert_eq!(queue.completed_order.len(), MAX_COMPLETED_QUEUE_OUTCOMES);
        assert!(!queue.completed.contains_key("older-0"));
        assert_eq!(queue.completed.get("newest"), Some(&true));
    }

    #[tokio::test]
    async fn queue_outcomes_remain_until_pending_dependents_finish() {
        let store = InstallStore::new();
        store
            .enqueue_queued_install(
                "dependent".to_string(),
                InstallQueueSpec::Content {
                    instance_id: "instance".to_string(),
                    label: "Dependent content".to_string(),
                    action: ContentQueueAction::Install {
                        selections: Vec::new(),
                        allow_incompatible: false,
                        setup_cleanup: None,
                    },
                    prerequisite_queue_id: Some("prerequisite".to_string()),
                },
                InstallQueuePlacement::Back,
            )
            .await;
        {
            let mut queue = store.queue.write().await;
            record_queue_outcome(&mut queue, "prerequisite", true);
            for index in 0..MAX_COMPLETED_QUEUE_OUTCOMES {
                record_queue_outcome(&mut queue, &format!("newer-{index}"), true);
            }
            assert_eq!(queue.completed_order.len(), MAX_COMPLETED_QUEUE_OUTCOMES);
        }

        assert_eq!(
            store.queued_install_succeeded("prerequisite").await,
            Some(true)
        );
        let dependent = store
            .reserve_next_queued_install()
            .await
            .expect("reserve dependent");
        assert_eq!(dependent.queue_id, "dependent");
        assert_eq!(
            store.queued_install_succeeded("prerequisite").await,
            Some(true),
            "the active dependent must retain its prerequisite outcome"
        );

        store
            .complete_reserved_queued_install("dependent", true)
            .await
            .expect("complete dependent");
        assert_eq!(store.queued_install_succeeded("prerequisite").await, None);
        assert_eq!(
            store.queue.read().await.completed_order.len(),
            MAX_COMPLETED_QUEUE_OUTCOMES
        );
    }

    #[tokio::test]
    async fn install_insert_or_existing_reuses_active_matching_key() {
        let store = InstallStore::new();
        let (first_id, first_inserted) = store
            .insert_or_existing_vanilla("first-install".to_string(), "1.21.5".to_string())
            .await;
        let (second_id, second_inserted) = store
            .insert_or_existing_vanilla("second-install".to_string(), "1.21.5".to_string())
            .await;

        assert_eq!(first_id, "first-install");
        assert!(first_inserted);
        assert_eq!(second_id, "first-install");
        assert!(!second_inserted);
        assert_eq!(store.active_install_count().await, 1);
        assert!(store.subscribe_records("second-install").await.is_none());
    }

    #[tokio::test]
    async fn install_insert_prunes_done_entries() {
        let store = InstallStore::new();
        store.insert("done-install".to_string()).await;
        store.insert("active-install".to_string()).await;
        store.emit("done-install", done_progress()).await;
        let (snapshot, _) = store
            .subscribe_records("done-install")
            .await
            .expect("terminal install remains subscribable until pruned");
        assert!(snapshot.done);
        assert_eq!(latest_phase(&snapshot), Some("done"));

        store.insert("fresh-install".to_string()).await;

        assert!(store.subscribe_records("done-install").await.is_none());
        assert!(store.subscribe_records("active-install").await.is_some());
        assert!(store.subscribe_records("fresh-install").await.is_some());
        assert_eq!(store.active_install_count().await, 2);
    }

    #[tokio::test]
    async fn install_insert_or_existing_prunes_done_entries_and_reuses_active_match() {
        let store = InstallStore::new();
        store
            .insert_or_existing_vanilla("done-install".to_string(), "1.21.4".to_string())
            .await;
        store
            .insert_or_existing_vanilla("active-install".to_string(), "1.21.5".to_string())
            .await;
        store.emit("done-install", done_progress()).await;
        let (snapshot, _) = store
            .subscribe_records("done-install")
            .await
            .expect("terminal install remains subscribable until pruned");
        assert!(snapshot.done);
        assert_eq!(latest_phase(&snapshot), Some("done"));

        let (install_id, inserted) = store
            .insert_or_existing_vanilla(
                "duplicate-active-install".to_string(),
                "1.21.5".to_string(),
            )
            .await;

        assert_eq!(install_id, "active-install");
        assert!(!inserted);
        assert!(store.subscribe_records("done-install").await.is_none());
        assert!(
            store
                .subscribe_records("duplicate-active-install")
                .await
                .is_none()
        );
        assert_eq!(store.active_install_count().await, 1);
    }

    #[tokio::test]
    async fn install_insert_or_existing_trims_matching_key_fields() {
        let store = InstallStore::new();
        store
            .insert_or_existing_vanilla("trimmed-install".to_string(), " 1.21.5 ".to_string())
            .await;

        let (install_id, inserted) = store
            .insert_or_existing_vanilla("duplicate-install".to_string(), "1.21.5".to_string())
            .await;

        assert_eq!(install_id, "trimmed-install");
        assert!(!inserted);
    }

    #[tokio::test]
    async fn install_insert_or_existing_allows_fresh_install_after_done() {
        let store = InstallStore::new();
        store
            .insert_or_existing_vanilla("done-install".to_string(), "1.21.5".to_string())
            .await;
        store.emit("done-install", done_progress()).await;
        let (snapshot, _) = store
            .subscribe_records("done-install")
            .await
            .expect("terminal install remains subscribable until pruned");
        assert!(snapshot.done);
        assert_eq!(latest_phase(&snapshot), Some("done"));

        let (install_id, inserted) = store
            .insert_or_existing_vanilla("fresh-install".to_string(), "1.21.5".to_string())
            .await;

        assert_eq!(install_id, "fresh-install");
        assert!(inserted);
        assert!(store.subscribe_records("done-install").await.is_none());
        assert!(store.subscribe_records("fresh-install").await.is_some());
        assert_eq!(store.active_install_count().await, 1);
    }

    #[tokio::test]
    async fn install_insert_or_existing_allows_fresh_install_after_remove() {
        let store = InstallStore::new();
        store
            .insert_or_existing_vanilla("removed-install".to_string(), "1.21.5".to_string())
            .await;
        store.remove("removed-install").await;

        let (install_id, inserted) = store
            .insert_or_existing_vanilla("fresh-install".to_string(), "1.21.5".to_string())
            .await;

        assert_eq!(install_id, "fresh-install");
        assert!(inserted);
        assert_eq!(store.active_install_count().await, 1);
    }

    #[tokio::test]
    async fn finish_if_active_marks_session_done_and_allows_fresh_retry() {
        let store = InstallStore::new();
        store
            .insert_or_existing_vanilla("interrupted-install".to_string(), "1.21.5".to_string())
            .await;

        assert!(
            store
                .finish_if_active("interrupted-install", failed_progress())
                .await
        );
        assert_eq!(store.active_install_count().await, 0);

        let (install_id, inserted) = store
            .insert_or_existing_vanilla("fresh-install".to_string(), "1.21.5".to_string())
            .await;

        assert_eq!(install_id, "fresh-install");
        assert!(inserted);
        assert!(
            store
                .subscribe_records("interrupted-install")
                .await
                .is_none()
        );
        assert_eq!(store.active_install_count().await, 1);
    }

    #[tokio::test]
    async fn finish_if_active_ignores_already_done_sessions() {
        let store = InstallStore::new();
        store.insert("done-install".to_string()).await;
        store.emit("done-install", done_progress()).await;

        assert!(
            !store
                .finish_if_active("done-install", failed_progress())
                .await
        );

        let (snapshot, _) = store
            .subscribe_records("done-install")
            .await
            .expect("done install remains until pruned");
        assert!(snapshot.done);
        assert_eq!(latest_phase(&snapshot), Some("done"));
    }

    #[tokio::test]
    async fn emit_ignores_late_progress_after_terminal_session() {
        let store = InstallStore::new();
        store.insert("done-install".to_string()).await;
        store.emit("done-install", done_progress()).await;
        store
            .emit("done-install", base_progress("libraries", false))
            .await;
        store
            .emit("done-install", base_progress("assets", true))
            .await;

        let (snapshot, _) = store
            .subscribe_records("done-install")
            .await
            .expect("done install remains until pruned");

        assert!(snapshot.done);
        assert_eq!(latest_phase(&snapshot), Some("done"));
    }

    #[tokio::test]
    async fn install_snapshot_keeps_only_latest_progress_under_many_events() {
        let store = InstallStore::new();
        store.insert("active-install".to_string()).await;

        for current in 1..=5_000 {
            store
                .emit(
                    "active-install",
                    DownloadProgress {
                        phase: "java_runtime".to_string(),
                        current,
                        total: 5_000,
                        file: Some(format!("file-{current}")),
                        error: None,
                        done: false,
                        bytes_done: Some(current as u64),
                        bytes_total: Some(5_000),
                    },
                )
                .await;
        }

        let (snapshot, _) = store
            .subscribe_records("active-install")
            .await
            .expect("active install remains subscribable");
        let latest = snapshot.latest.expect("latest progress");

        assert!(!snapshot.done);
        assert_eq!(latest.progress.phase, "java_runtime");
        assert_eq!(latest.progress.current, 5_000);
        assert_eq!(latest.progress.file.as_deref(), Some("file-5000"));
    }

    #[tokio::test]
    async fn release_active_queued_install_to_front_restores_pending_item() {
        let store = InstallStore::new();
        let spec = InstallQueueSpec::vanilla("1.21.5".to_string());
        store
            .enqueue_queued_install(
                "queue-install".to_string(),
                spec.clone(),
                InstallQueuePlacement::Back,
            )
            .await;

        let reserved = store
            .reserve_next_queued_install()
            .await
            .expect("queued install");

        assert_eq!(reserved.queue_id, "queue-install");
        assert!(
            store
                .release_active_queued_install_to_front("queue-install")
                .await
        );
        let snapshot = store.queue_snapshot().await;
        assert!(snapshot.active.is_none());
        assert_eq!(snapshot.pending.len(), 1);
        assert_eq!(snapshot.pending[0].queue_id, "queue-install");
        assert_eq!(snapshot.pending[0].spec, spec);
    }

    #[tokio::test]
    async fn mark_queued_install_started_copies_install_session_start_time() {
        let store = InstallStore::new();
        let spec = InstallQueueSpec::vanilla("1.21.5".to_string());
        store
            .insert_or_existing_vanilla("active-install".to_string(), "1.21.5".to_string())
            .await;
        let install_started_at_ms = store
            .install_started_at_ms("active-install")
            .await
            .expect("install start time");
        store
            .enqueue_queued_install(
                "queue-install".to_string(),
                spec,
                InstallQueuePlacement::Back,
            )
            .await;

        let reserved = store
            .reserve_next_queued_install()
            .await
            .expect("queued install");

        assert_eq!(reserved.queue_id, "queue-install");
        assert!(
            store
                .mark_queued_install_started("queue-install", "active-install".to_string())
                .await
        );
        let snapshot = store.queue_snapshot().await;
        let active = snapshot.active.expect("active queue entry");
        assert_eq!(active.install_id.as_deref(), Some("active-install"));
        assert_eq!(active.install_started_at_ms, Some(install_started_at_ms));
    }

    #[tokio::test]
    async fn tracked_worker_finishes_active_session_after_panic() {
        let store = Arc::new(InstallStore::new());
        store
            .insert_or_existing_vanilla("panic-install".to_string(), "1.21.5".to_string())
            .await;

        InstallStore::spawn_tracked_worker(
            Arc::clone(&store),
            "panic-install".to_string(),
            failed_progress(),
            async {
                panic!("install worker panic");
            },
        )
        .await
        .expect("tracked worker should absorb inner panic");

        assert_eq!(store.active_install_count().await, 0);
    }

    #[tokio::test]
    async fn tracked_worker_finishes_active_session_after_early_return() {
        let store = Arc::new(InstallStore::new());
        store
            .insert_or_existing_vanilla("early-install".to_string(), "1.21.5".to_string())
            .await;

        InstallStore::spawn_tracked_worker(
            Arc::clone(&store),
            "early-install".to_string(),
            failed_progress(),
            async {},
        )
        .await
        .expect("tracked worker should complete");

        assert_eq!(store.active_install_count().await, 0);
    }

    #[tokio::test]
    async fn tracked_worker_interruption_handler_runs_only_for_active_finish() {
        let store = Arc::new(InstallStore::new());
        store
            .insert_or_existing_vanilla("early-install".to_string(), "1.21.5".to_string())
            .await;
        let interrupted = Arc::new(std::sync::Mutex::new(None));
        let interrupted_capture = interrupted.clone();

        InstallStore::spawn_tracked_worker_with_interrupt_handler(
            Arc::clone(&store),
            "early-install".to_string(),
            failed_progress(),
            async {},
            move |progress| async move {
                *interrupted_capture.lock().expect("lock") = Some(progress.phase);
                true
            },
        )
        .await
        .expect("tracked worker should complete");

        assert_eq!(interrupted.lock().expect("lock").as_deref(), Some("error"));

        store.insert("done-install".to_string()).await;
        store.emit("done-install", done_progress()).await;
        let not_interrupted = Arc::new(std::sync::Mutex::new(false));
        let not_interrupted_capture = not_interrupted.clone();
        InstallStore::spawn_tracked_worker_with_interrupt_handler(
            Arc::clone(&store),
            "done-install".to_string(),
            failed_progress(),
            async {},
            move |_| async move {
                *not_interrupted_capture.lock().expect("lock") = true;
                true
            },
        )
        .await
        .expect("tracked worker should complete");

        assert!(!*not_interrupted.lock().expect("lock"));
    }

    #[tokio::test]
    async fn duplicate_install_waits_for_initialization_and_observes_removal() {
        let store = Arc::new(InstallStore::new());
        let (install_id, inserted) = store
            .insert_or_existing_vanilla("initializing-install".to_string(), "1.21.5".to_string())
            .await;
        assert!(inserted);
        let waiting_store = store.clone();
        let waiting_id = install_id.clone();
        let waiting =
            tokio::spawn(async move { waiting_store.wait_until_initialized(&waiting_id).await });
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished());
        assert!(store.mark_initialized(&install_id).await);
        assert!(waiting.await.expect("initialization waiter"));

        let (removed_id, inserted) = store
            .insert_or_existing_vanilla("removed-initialization".to_string(), "1.21.6".to_string())
            .await;
        assert!(inserted);
        let removed_store = store.clone();
        let removed_wait_id = removed_id.clone();
        let removed =
            tokio::spawn(
                async move { removed_store.wait_until_initialized(&removed_wait_id).await },
            );
        tokio::task::yield_now().await;
        assert!(!removed.is_finished());
        store.remove(&removed_id).await;
        assert!(!removed.await.expect("removed initialization waiter"));
    }

    #[tokio::test]
    async fn interrupted_status_is_published_only_after_async_handler_succeeds() {
        let store = Arc::new(InstallStore::new());
        store.insert("ordered-interruption".to_string()).await;
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let worker = InstallStore::spawn_tracked_worker_with_interrupt_handler(
            store.clone(),
            "ordered-interruption".to_string(),
            failed_progress(),
            async {},
            move |_| async move {
                let _ = entered_tx.send(());
                let _ = release_rx.await;
                true
            },
        );
        entered_rx.await.expect("interruption handler entered");
        assert!(
            !store
                .snapshot("ordered-interruption")
                .await
                .expect("active install")
                .done
        );
        let _ = release_tx.send(());
        worker.await.expect("tracked worker");
        assert!(
            store
                .snapshot("ordered-interruption")
                .await
                .expect("terminal install")
                .done
        );
    }

    #[tokio::test]
    async fn install_insert_or_existing_keeps_different_versions_independent() {
        let store = InstallStore::new();
        store
            .insert_or_existing_vanilla("first-install".to_string(), "1.21.5".to_string())
            .await;

        let (install_id, inserted) = store
            .insert_or_existing_vanilla("second-install".to_string(), "1.21.6".to_string())
            .await;

        assert_eq!(install_id, "second-install");
        assert!(inserted);
        assert_eq!(store.active_install_count().await, 2);
    }

    #[tokio::test]
    async fn install_identities_keep_vanilla_and_loader_work_independent() {
        let store = InstallStore::new();
        let build_id = build_id_for(LoaderComponentId::Fabric, "1.21.5", "0.16.10");
        store
            .insert_or_existing_vanilla("vanilla-install".to_string(), "1.21.5".to_string())
            .await;

        let (loader_id, loader_inserted) = store
            .insert_or_existing_loader(
                "loader-install".to_string(),
                LoaderComponentId::Fabric,
                build_id.clone(),
            )
            .await;
        let (duplicate_loader_id, duplicate_loader_inserted) = store
            .insert_or_existing_loader(
                "duplicate-loader-install".to_string(),
                LoaderComponentId::Fabric,
                format!(" {build_id} "),
            )
            .await;

        assert_eq!(loader_id, "loader-install");
        assert!(loader_inserted);
        assert_eq!(duplicate_loader_id, "loader-install");
        assert!(!duplicate_loader_inserted);
        assert_eq!(store.active_install_count().await, 2);
    }

    #[tokio::test]
    async fn install_queue_dedupes_and_moves_retry_to_front() {
        let store = InstallStore::new();
        let first = InstallQueueSpec::vanilla("1.21.5".to_string());
        let second = InstallQueueSpec::loader(
            LoaderComponentId::Fabric,
            build_id_for(LoaderComponentId::Fabric, "1.21.6", "0.16.10"),
            "fabric-loader-1.21.6".to_string(),
            "1.21.6".to_string(),
            "0.16.10".to_string(),
        );

        assert_eq!(
            store
                .enqueue_queued_install(
                    "queue-first".to_string(),
                    first.clone(),
                    InstallQueuePlacement::Back,
                )
                .await,
            InstallQueueEnqueueOutcome::Enqueued {
                queue_id: "queue-first".to_string()
            }
        );
        assert_eq!(
            store
                .enqueue_queued_install(
                    "queue-second".to_string(),
                    second.clone(),
                    InstallQueuePlacement::Back,
                )
                .await,
            InstallQueueEnqueueOutcome::Enqueued {
                queue_id: "queue-second".to_string()
            }
        );
        assert_eq!(
            store
                .enqueue_queued_install(
                    "queue-duplicate".to_string(),
                    first.clone(),
                    InstallQueuePlacement::Back,
                )
                .await,
            InstallQueueEnqueueOutcome::AlreadyQueued {
                queue_id: "queue-first".to_string()
            }
        );
        assert_eq!(
            store
                .enqueue_queued_install(
                    "queue-retry".to_string(),
                    second,
                    InstallQueuePlacement::Front,
                )
                .await,
            InstallQueueEnqueueOutcome::MovedToFront {
                queue_id: "queue-second".to_string()
            }
        );

        let snapshot = store.queue_snapshot().await;
        assert_eq!(snapshot.pending.len(), 2);
        assert_eq!(snapshot.pending[0].queue_id, "queue-second");
        assert_eq!(snapshot.pending[1].queue_id, "queue-first");

        let active = store
            .reserve_next_queued_install()
            .await
            .expect("first queue item");
        assert_eq!(active.queue_id, "queue-second");
        assert_eq!(
            store
                .enqueue_queued_install(
                    "queue-active-duplicate".to_string(),
                    active.spec,
                    InstallQueuePlacement::Back,
                )
                .await,
            InstallQueueEnqueueOutcome::AlreadyActive {
                queue_id: "queue-second".to_string()
            }
        );
    }

    #[tokio::test]
    async fn active_vanilla_install_finds_only_vanilla_identity_by_version() {
        let store = InstallStore::new();
        store
            .insert_or_existing_vanilla("vanilla-install".to_string(), "1.21.5".to_string())
            .await;
        store
            .insert_or_existing_loader(
                "loader-install".to_string(),
                LoaderComponentId::Fabric,
                build_id_for(LoaderComponentId::Fabric, "1.21.5", "0.16.10"),
            )
            .await;

        assert_eq!(
            store.active_vanilla_install(" 1.21.5 ").await,
            Some("vanilla-install".to_string())
        );
    }

    #[tokio::test]
    async fn active_vanilla_install_ignores_done_removed_and_failed_sessions() {
        let store = InstallStore::new();
        store
            .insert_or_existing_vanilla("done-install".to_string(), "1.21.5".to_string())
            .await;
        store.emit("done-install", done_progress()).await;
        assert_eq!(store.active_vanilla_install("1.21.5").await, None);

        store
            .insert_or_existing_vanilla("failed-install".to_string(), "1.21.5".to_string())
            .await;
        store.emit("failed-install", failed_progress()).await;
        assert_eq!(store.active_vanilla_install("1.21.5").await, None);

        store
            .insert_or_existing_vanilla("removed-install".to_string(), "1.21.5".to_string())
            .await;
        store.remove("removed-install").await;
        assert_eq!(store.active_vanilla_install("1.21.5").await, None);

        let (install_id, inserted) = store
            .insert_or_existing_vanilla("fresh-install".to_string(), "1.21.5".to_string())
            .await;

        assert_eq!(install_id, "fresh-install");
        assert!(inserted);
    }

    #[tokio::test]
    async fn launch_active_install_count_excludes_done_sessions() {
        let store = InstallStore::new();
        store.insert("active-install".to_string()).await;
        store.insert("done-install".to_string()).await;
        store
            .emit(
                "done-install",
                DownloadProgress {
                    phase: "done".to_string(),
                    current: 1,
                    total: 1,
                    file: None,
                    error: None,
                    done: true,
                    bytes_done: None,
                    bytes_total: None,
                },
            )
            .await;

        assert_eq!(store.active_install_count().await, 1);
    }

    fn done_progress() -> DownloadProgress {
        DownloadProgress {
            phase: "done".to_string(),
            current: 1,
            total: 1,
            file: None,
            error: None,
            done: true,
            bytes_done: None,
            bytes_total: None,
        }
    }

    fn failed_progress() -> DownloadProgress {
        DownloadProgress {
            phase: "error".to_string(),
            current: 0,
            total: 0,
            file: None,
            error: Some("failed".to_string()),
            done: true,
            bytes_done: None,
            bytes_total: None,
        }
    }

    fn base_progress(phase: &str, done: bool) -> DownloadProgress {
        DownloadProgress {
            phase: phase.to_string(),
            current: if done { 1 } else { 0 },
            total: if done { 1 } else { 0 },
            file: None,
            error: None,
            done,
            bytes_done: None,
            bytes_total: None,
        }
    }

    fn latest_phase(snapshot: &InstallSnapshot) -> Option<&str> {
        snapshot
            .latest
            .as_ref()
            .map(|record| record.progress.phase.as_str())
    }
}
