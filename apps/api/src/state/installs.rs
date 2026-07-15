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
use tokio::sync::{RwLock, broadcast};
use tokio::task::JoinHandle;

struct InstallEntry {
    key: Option<InstallKey>,
    started_at_ms: u64,
    latest: Option<InstallProgressRecord>,
    events: broadcast::Sender<DownloadProgress>,
    record_events: broadcast::Sender<InstallProgressRecord>,
    finishing: bool,
    done: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstallSnapshot {
    pub latest: Option<InstallProgressRecord>,
    pub done: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstallProgressRecord {
    pub progress: DownloadProgress,
    vanilla_event_json: Option<Arc<str>>,
    loader_event_json: Option<Arc<str>>,
}

impl InstallProgressRecord {
    pub fn new(progress: DownloadProgress) -> Self {
        Self {
            progress,
            vanilla_event_json: None,
            loader_event_json: None,
        }
    }

    pub fn with_event_json(
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

    pub fn event_json(&self, loader_install: bool) -> Option<&str> {
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
        selected_paths: Vec<String>,
        include_overrides: bool,
        setup_cleanup: Option<SetupInstanceCleanup>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InstallQueueSpec {
    Vanilla {
        version_id: String,
        manifest_url: String,
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
    pub fn vanilla(version_id: String, manifest_url: String) -> Self {
        Self::Vanilla {
            version_id: version_id.trim().to_string(),
            manifest_url: manifest_url.trim().to_string(),
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

#[derive(Default)]
struct InstallQueueInner {
    active: Option<ActiveQueuedInstallEntry>,
    pending: VecDeque<QueuedInstallEntry>,
    completed: HashMap<String, bool>,
    completed_order: VecDeque<String>,
}

const MAX_COMPLETED_QUEUE_OUTCOMES: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq)]
struct InstallKey {
    scope: String,
    version_id: String,
    manifest_url: String,
}

impl InstallKey {
    fn new(scope: String, version_id: String, manifest_url: String) -> Self {
        Self {
            scope: scope.trim().to_string(),
            version_id: version_id.trim().to_string(),
            manifest_url: manifest_url.trim().to_string(),
        }
    }
}

pub struct InstallStore {
    installs: RwLock<HashMap<String, InstallEntry>>,
    queue: RwLock<InstallQueueInner>,
}

impl InstallStore {
    pub fn new() -> Self {
        Self {
            installs: RwLock::new(HashMap::new()),
            queue: RwLock::new(InstallQueueInner::default()),
        }
    }

    pub async fn insert(&self, install_id: String) {
        self.insert_entry(install_id, None).await;
    }

    pub async fn insert_or_existing_active(
        &self,
        install_id: String,
        version_id: String,
        manifest_url: String,
    ) -> (String, bool) {
        self.insert_or_existing_active_scoped(
            "vanilla".to_string(),
            install_id,
            version_id,
            manifest_url,
        )
        .await
    }

    pub async fn insert_or_existing_active_scoped(
        &self,
        scope: String,
        install_id: String,
        version_id: String,
        manifest_url: String,
    ) -> (String, bool) {
        let key = InstallKey::new(scope, version_id, manifest_url);
        let mut installs = self.installs.write().await;
        prune_done_entries(&mut installs);
        if let Some(existing_id) = installs.iter().find_map(|(existing_id, entry)| {
            (!entry.done && entry.key.as_ref() == Some(&key)).then(|| existing_id.clone())
        }) {
            return (existing_id, false);
        }

        installs.insert(install_id.clone(), new_install_entry(Some(key)));
        (install_id, true)
    }

    async fn insert_entry(&self, install_id: String, key: Option<InstallKey>) {
        let mut installs = self.installs.write().await;
        prune_done_entries(&mut installs);
        installs.insert(install_id, new_install_entry(key));
    }

    pub async fn emit(&self, install_id: &str, progress: DownloadProgress) {
        self.emit_record(install_id, InstallProgressRecord::new(progress))
            .await;
    }

    pub async fn emit_record(&self, install_id: &str, record: InstallProgressRecord) {
        let senders = {
            let mut installs = self.installs.write().await;
            let Some(entry) = installs.get_mut(install_id) else {
                return;
            };
            if entry.done || entry.finishing {
                return;
            }
            entry.done = record.progress.done;
            entry.latest = Some(record.clone());
            (entry.events.clone(), entry.record_events.clone())
        };
        let _ = senders.0.send(record.progress.clone());
        let _ = senders.1.send(record);
    }

    pub async fn finish_if_active(&self, install_id: &str, mut progress: DownloadProgress) -> bool {
        progress.done = true;
        let record = InstallProgressRecord::new(progress);
        let senders = {
            let mut installs = self.installs.write().await;
            let Some(entry) = installs.get_mut(install_id) else {
                return false;
            };
            if entry.done || entry.finishing {
                return false;
            }

            entry.done = true;
            entry.latest = Some(record.clone());
            (entry.events.clone(), entry.record_events.clone())
        };
        let _ = senders.0.send(record.progress.clone());
        let _ = senders.1.send(record);
        true
    }

    pub async fn subscribe(
        &self,
        install_id: &str,
    ) -> Option<(
        Vec<DownloadProgress>,
        broadcast::Receiver<DownloadProgress>,
        bool,
    )> {
        let installs = self.installs.read().await;
        installs.get(install_id).map(|entry| {
            (
                entry
                    .latest
                    .as_ref()
                    .map(|record| vec![record.progress.clone()])
                    .unwrap_or_default(),
                entry.events.subscribe(),
                entry.done,
            )
        })
    }

    pub async fn subscribe_records(
        &self,
        install_id: &str,
    ) -> Option<(InstallSnapshot, broadcast::Receiver<InstallProgressRecord>)> {
        let installs = self.installs.read().await;
        installs.get(install_id).map(|entry| {
            (
                InstallSnapshot {
                    latest: entry.latest.clone(),
                    done: entry.done,
                },
                entry.record_events.subscribe(),
            )
        })
    }

    pub fn spawn_tracked_worker<F>(
        store: Arc<Self>,
        install_id: String,
        interrupted_progress: DownloadProgress,
        worker: F,
    ) -> JoinHandle<()>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        Self::spawn_tracked_worker_with_interrupt_handler(
            store,
            install_id,
            interrupted_progress,
            worker,
            |_| {},
        )
    }

    pub fn spawn_tracked_worker_with_interrupt_handler<F, H>(
        store: Arc<Self>,
        install_id: String,
        interrupted_progress: DownloadProgress,
        worker: F,
        on_interrupted: H,
    ) -> JoinHandle<()>
    where
        F: Future<Output = ()> + Send + 'static,
        H: FnOnce(DownloadProgress) + Send + 'static,
    {
        tokio::spawn(async move {
            let _ = tokio::spawn(worker).await;
            if store
                .finish_if_active(&install_id, interrupted_progress.clone())
                .await
            {
                on_interrupted(interrupted_progress);
            }
        })
    }

    /// Run asynchronous interruption cleanup before publishing the terminal
    /// progress it determines. Claiming the entry first prevents a late worker
    /// event from racing cleanup or changing its outcome.
    pub fn spawn_tracked_worker_with_async_interrupt_handler<F, H, HF>(
        store: Arc<Self>,
        install_id: String,
        fallback_progress: DownloadProgress,
        worker: F,
        on_interrupted: H,
    ) -> JoinHandle<()>
    where
        F: Future<Output = ()> + Send + 'static,
        H: FnOnce(DownloadProgress) -> HF + Send + 'static,
        HF: Future<Output = DownloadProgress> + Send + 'static,
    {
        tokio::spawn(async move {
            let _ = tokio::spawn(worker).await;
            if !store.claim_interrupted_finish(&install_id).await {
                return;
            }
            let fallback = fallback_progress.clone();
            let progress = tokio::spawn(on_interrupted(fallback_progress))
                .await
                .unwrap_or(fallback);
            store
                .finish_claimed_interruption(&install_id, progress)
                .await;
        })
    }

    async fn claim_interrupted_finish(&self, install_id: &str) -> bool {
        let mut installs = self.installs.write().await;
        let Some(entry) = installs.get_mut(install_id) else {
            return false;
        };
        if entry.done || entry.finishing {
            return false;
        }
        entry.finishing = true;
        true
    }

    async fn finish_claimed_interruption(&self, install_id: &str, mut progress: DownloadProgress) {
        progress.done = true;
        let record = InstallProgressRecord::new(progress);
        let senders = {
            let mut installs = self.installs.write().await;
            let Some(entry) = installs.get_mut(install_id) else {
                return;
            };
            if !entry.finishing || entry.done {
                return;
            }
            entry.finishing = false;
            entry.done = true;
            entry.latest = Some(record.clone());
            (entry.events.clone(), entry.record_events.clone())
        };
        let _ = senders.0.send(record.progress.clone());
        let _ = senders.1.send(record);
    }

    pub async fn snapshot(&self, install_id: &str) -> Option<InstallSnapshot> {
        self.installs
            .read()
            .await
            .get(install_id)
            .map(|entry| InstallSnapshot {
                latest: entry.latest.clone(),
                done: entry.done,
            })
    }

    pub async fn install_started_at_ms(&self, install_id: &str) -> Option<u64> {
        self.installs
            .read()
            .await
            .get(install_id)
            .map(|entry| entry.started_at_ms)
    }

    pub async fn active_install_for_scope_and_version(
        &self,
        scope: &str,
        version_id: &str,
    ) -> Option<String> {
        let scope = scope.trim();
        let version_id = version_id.trim();
        self.installs
            .read()
            .await
            .iter()
            .find_map(|(install_id, entry)| {
                let key = entry.key.as_ref()?;
                (!entry.done && key.scope == scope && key.version_id == version_id)
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
        self.installs.write().await.remove(install_id);
    }

    pub async fn clear(&self) {
        self.installs.write().await.clear();
        *self.queue.write().await = InstallQueueInner::default();
    }
}

fn new_install_entry(key: Option<InstallKey>) -> InstallEntry {
    let (events, _) = broadcast::channel(256);
    let (record_events, _) = broadcast::channel(256);
    InstallEntry {
        key,
        started_at_ms: now_unix_ms(),
        latest: None,
        events,
        record_events,
        finishing: false,
        done: false,
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
            .insert_or_existing_active(
                "first-install".to_string(),
                "1.21.5".to_string(),
                String::new(),
            )
            .await;
        let (second_id, second_inserted) = store
            .insert_or_existing_active(
                "second-install".to_string(),
                "1.21.5".to_string(),
                String::new(),
            )
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
            .insert_or_existing_active(
                "done-install".to_string(),
                "1.21.4".to_string(),
                String::new(),
            )
            .await;
        store
            .insert_or_existing_active(
                "active-install".to_string(),
                "1.21.5".to_string(),
                String::new(),
            )
            .await;
        store.emit("done-install", done_progress()).await;
        let (snapshot, _) = store
            .subscribe_records("done-install")
            .await
            .expect("terminal install remains subscribable until pruned");
        assert!(snapshot.done);
        assert_eq!(latest_phase(&snapshot), Some("done"));

        let (install_id, inserted) = store
            .insert_or_existing_active(
                "duplicate-active-install".to_string(),
                "1.21.5".to_string(),
                String::new(),
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
            .insert_or_existing_active(
                "trimmed-install".to_string(),
                " 1.21.5 ".to_string(),
                " https://example.invalid/manifest.json ".to_string(),
            )
            .await;

        let (install_id, inserted) = store
            .insert_or_existing_active(
                "duplicate-install".to_string(),
                "1.21.5".to_string(),
                "https://example.invalid/manifest.json".to_string(),
            )
            .await;

        assert_eq!(install_id, "trimmed-install");
        assert!(!inserted);
    }

    #[tokio::test]
    async fn install_insert_or_existing_allows_fresh_install_after_done() {
        let store = InstallStore::new();
        store
            .insert_or_existing_active(
                "done-install".to_string(),
                "1.21.5".to_string(),
                String::new(),
            )
            .await;
        store.emit("done-install", done_progress()).await;
        let (snapshot, _) = store
            .subscribe_records("done-install")
            .await
            .expect("terminal install remains subscribable until pruned");
        assert!(snapshot.done);
        assert_eq!(latest_phase(&snapshot), Some("done"));

        let (install_id, inserted) = store
            .insert_or_existing_active(
                "fresh-install".to_string(),
                "1.21.5".to_string(),
                String::new(),
            )
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
            .insert_or_existing_active(
                "removed-install".to_string(),
                "1.21.5".to_string(),
                String::new(),
            )
            .await;
        store.remove("removed-install").await;

        let (install_id, inserted) = store
            .insert_or_existing_active(
                "fresh-install".to_string(),
                "1.21.5".to_string(),
                String::new(),
            )
            .await;

        assert_eq!(install_id, "fresh-install");
        assert!(inserted);
        assert_eq!(store.active_install_count().await, 1);
    }

    #[tokio::test]
    async fn finish_if_active_marks_session_done_and_allows_fresh_retry() {
        let store = InstallStore::new();
        store
            .insert_or_existing_active(
                "interrupted-install".to_string(),
                "1.21.5".to_string(),
                String::new(),
            )
            .await;

        assert!(
            store
                .finish_if_active("interrupted-install", failed_progress())
                .await
        );
        assert_eq!(store.active_install_count().await, 0);

        let (install_id, inserted) = store
            .insert_or_existing_active(
                "fresh-install".to_string(),
                "1.21.5".to_string(),
                String::new(),
            )
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
        let spec = InstallQueueSpec::vanilla("1.21.5".to_string(), String::new());
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
        let spec = InstallQueueSpec::vanilla("1.21.5".to_string(), String::new());
        store
            .insert_or_existing_active(
                "active-install".to_string(),
                "1.21.5".to_string(),
                String::new(),
            )
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
            .insert_or_existing_active(
                "panic-install".to_string(),
                "1.21.5".to_string(),
                String::new(),
            )
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
            .insert_or_existing_active(
                "early-install".to_string(),
                "1.21.5".to_string(),
                String::new(),
            )
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
            .insert_or_existing_active(
                "early-install".to_string(),
                "1.21.5".to_string(),
                String::new(),
            )
            .await;
        let interrupted = Arc::new(std::sync::Mutex::new(None));
        let interrupted_capture = interrupted.clone();

        InstallStore::spawn_tracked_worker_with_interrupt_handler(
            Arc::clone(&store),
            "early-install".to_string(),
            failed_progress(),
            async {},
            move |progress| {
                *interrupted_capture.lock().expect("lock") = Some(progress.phase);
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
            move |_| {
                *not_interrupted_capture.lock().expect("lock") = true;
            },
        )
        .await
        .expect("tracked worker should complete");

        assert!(!*not_interrupted.lock().expect("lock"));
    }

    #[tokio::test]
    async fn install_insert_or_existing_keeps_different_versions_independent() {
        let store = InstallStore::new();
        store
            .insert_or_existing_active(
                "first-install".to_string(),
                "1.21.5".to_string(),
                String::new(),
            )
            .await;

        let (install_id, inserted) = store
            .insert_or_existing_active(
                "second-install".to_string(),
                "1.21.6".to_string(),
                String::new(),
            )
            .await;

        assert_eq!(install_id, "second-install");
        assert!(inserted);
        assert_eq!(store.active_install_count().await, 2);
    }

    #[tokio::test]
    async fn install_insert_or_existing_keeps_manifest_urls_independent() {
        let store = InstallStore::new();
        store
            .insert_or_existing_active(
                "normal-install".to_string(),
                "1.21.5".to_string(),
                String::new(),
            )
            .await;

        let (explicit_id, explicit_inserted) = store
            .insert_or_existing_active(
                "explicit-install".to_string(),
                "1.21.5".to_string(),
                "https://example.invalid/manifest.json".to_string(),
            )
            .await;
        let (duplicate_explicit_id, duplicate_explicit_inserted) = store
            .insert_or_existing_active(
                "duplicate-explicit-install".to_string(),
                "1.21.5".to_string(),
                "https://example.invalid/manifest.json".to_string(),
            )
            .await;

        assert_eq!(explicit_id, "explicit-install");
        assert!(explicit_inserted);
        assert_eq!(duplicate_explicit_id, "explicit-install");
        assert!(!duplicate_explicit_inserted);
        assert_eq!(store.active_install_count().await, 2);
    }

    #[tokio::test]
    async fn install_insert_or_existing_keeps_scopes_independent() {
        let store = InstallStore::new();
        store
            .insert_or_existing_active(
                "vanilla-install".to_string(),
                "1.21.5".to_string(),
                String::new(),
            )
            .await;

        let (loader_id, loader_inserted) = store
            .insert_or_existing_active_scoped(
                "loader".to_string(),
                "loader-install".to_string(),
                "1.21.5".to_string(),
                String::new(),
            )
            .await;
        let (duplicate_loader_id, duplicate_loader_inserted) = store
            .insert_or_existing_active_scoped(
                " loader ".to_string(),
                "duplicate-loader-install".to_string(),
                "1.21.5".to_string(),
                String::new(),
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
        let first = InstallQueueSpec::vanilla("1.21.5".to_string(), String::new());
        let second = InstallQueueSpec::loader(
            LoaderComponentId::Fabric,
            "fabric:1.21.6:0.16.10".to_string(),
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
    async fn active_install_for_scope_and_version_finds_active_vanilla_by_version() {
        let store = InstallStore::new();
        store
            .insert_or_existing_active(
                "vanilla-install".to_string(),
                "1.21.5".to_string(),
                "https://example.invalid/manifest.json".to_string(),
            )
            .await;
        store
            .insert_or_existing_active_scoped(
                "loader".to_string(),
                "loader-install".to_string(),
                "1.21.5".to_string(),
                String::new(),
            )
            .await;

        assert_eq!(
            store
                .active_install_for_scope_and_version(" vanilla ", " 1.21.5 ")
                .await,
            Some("vanilla-install".to_string())
        );
    }

    #[tokio::test]
    async fn active_install_for_scope_and_version_ignores_done_removed_and_failed_sessions() {
        let store = InstallStore::new();
        store
            .insert_or_existing_active(
                "done-install".to_string(),
                "1.21.5".to_string(),
                String::new(),
            )
            .await;
        store.emit("done-install", done_progress()).await;
        assert_eq!(
            store
                .active_install_for_scope_and_version("vanilla", "1.21.5")
                .await,
            None
        );

        store
            .insert_or_existing_active(
                "failed-install".to_string(),
                "1.21.5".to_string(),
                String::new(),
            )
            .await;
        store.emit("failed-install", failed_progress()).await;
        assert_eq!(
            store
                .active_install_for_scope_and_version("vanilla", "1.21.5")
                .await,
            None
        );

        store
            .insert_or_existing_active(
                "removed-install".to_string(),
                "1.21.5".to_string(),
                String::new(),
            )
            .await;
        store.remove("removed-install").await;
        assert_eq!(
            store
                .active_install_for_scope_and_version("vanilla", "1.21.5")
                .await,
            None
        );

        let (install_id, inserted) = store
            .insert_or_existing_active(
                "fresh-install".to_string(),
                "1.21.5".to_string(),
                String::new(),
            )
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
