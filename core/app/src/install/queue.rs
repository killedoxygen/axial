//! Durable install admission, verified activation and publication acknowledgement.
//! The queue never aborts an accepted installer future. The shared task owner
//! retains its generation and exclusion independently of disposable HTTP waiters.

pub use super::artifacts::InstalledVersionReceipt;
use super::{
    artifacts::{self, ActivatedVersion, InstallReceiptState, LoaderBaseState, VersionInspection},
    model::*,
};
use crate::{
    content::{
        catalog::ContentService,
        install::{ContentMutations, MutationError},
        model::{CanonicalId, ContentKind},
    },
    instances::{model::InstanceId, setup::SetupWork},
    library::{GenerationPin, LibraryLifecycle},
    public::OperationId,
    storage::{
        MetadataStore, Migration, StorageError,
        rusqlite::{self, OptionalExtension, params},
    },
    tasks::{
        ArtifactKey, CancellationToken, ExclusionLease, Exclusions, ShutdownReceipt, TaskHandle,
        TaskOwner,
    },
    telemetry::{Telemetry, TelemetryErrorKind, TelemetryEvent},
};
#[cfg(feature = "test-support")]
pub use axial_minecraft::download::InstallTestEndpoints;
use axial_minecraft::known_good::KnownGoodActivationSource;
use axial_minecraft::loaders::{
    LoaderBuildRecord, LoaderInstallBaseContinuation, LoaderInstallError,
    LoaderInstallPublicationOutcome,
};
use axial_minecraft::{
    DownloadError, DownloadProgress, KnownGoodActivationRejected, LoaderComponentId,
    ManagedInstallAcknowledgementOutcome, ManagedInstallActivationContractId,
    ManagedInstallDurableOutcome, ManagedRuntimeCache,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::{sync::watch, task::JoinSet};

pub const MIGRATION: Migration = Migration {
    id: "install.v1",
    sql: "
CREATE TABLE install_queue (
 id TEXT PRIMARY KEY, operation_id TEXT NOT NULL, library_id TEXT NOT NULL,
 request_json TEXT NOT NULL, target_json TEXT NOT NULL, status_json TEXT NOT NULL,
 phase TEXT NOT NULL CHECK(phase IN ('queued','running','settlement_required','terminal')),
 accepted_at INTEGER NOT NULL, checkpoint_json TEXT
);
CREATE TABLE installed_versions (
 library_id TEXT NOT NULL, version_id TEXT NOT NULL, contract_id TEXT NOT NULL,
 inventory_json TEXT NOT NULL, install_id TEXT NOT NULL,
 state TEXT NOT NULL CHECK(state IN ('activating','ready')),
 PRIMARY KEY(library_id, version_id)
);",
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PublicationCheckpoint {
    kind: CheckpointKind,
    version_id: String,
    evidence_id: String,
    contract_id: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
enum CheckpointKind {
    Final,
    Base,
    RolledBack,
}

enum RetainedInstall {
    Retry,
    Content(Option<(InstallOutcome, Option<InstallError>)>),
    Publication(axial_minecraft::download::ManagedInstallPublicationRecovery),
    LoaderPublication(axial_minecraft::loaders::LoaderInstallPublicationRecovery),
    Outcome(ManagedInstallDurableOutcome),
    Acknowledgement {
        recovery: axial_minecraft::ManagedInstallAcknowledgementRecovery,
        continuation: Option<LoaderInstallBaseContinuation>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum InstallError {
    #[error("The install request is invalid.")]
    InvalidRequest,
    #[error("The install was not found.")]
    NotFound,
    #[error("The install queue is full. Wait for an installation to finish.")]
    AtCapacity,
    #[error("The library is unavailable.")]
    LibraryUnavailable,
    #[error("The library is in use. Wait for its current operation to finish.")]
    Busy,
    #[error("Installation metadata could not be saved.")]
    Storage,
    #[error(
        "The version is not fully installed or its files changed. Install it before launching."
    )]
    NotReady,
    #[error("Client game files are missing. Install this version before launching.")]
    ClientJarMissing,
    #[error("Client game files are corrupt. Repair this version before launching.")]
    ClientJarCorrupt,
    #[error("Installed version metadata is missing. Install this version before launching.")]
    VersionJsonMissing,
    #[error("Required libraries are missing. Install this version before launching.")]
    LibrariesMissing,
    #[error("Required libraries are corrupt. Repair this version before launching.")]
    LibrariesCorrupt,
    #[error("Asset index is missing. Install this version before launching.")]
    AssetIndexMissing,
    #[error("Asset index is corrupt. Repair this version before launching.")]
    AssetIndexCorrupt,
    #[error("The selected loader build is unavailable.")]
    LoaderUnavailable,
    #[error("Content installation is unavailable.")]
    ContentUnavailable,
    #[error(
        "Content changed or conflicts with existing files. Review the selection and try again."
    )]
    ContentConflict,
    #[error("Content installation was interrupted. Review installed content before retrying.")]
    ContentInterrupted,
    #[error(
        "This installation requires publication settlement before another operation can begin."
    )]
    SettlementRequired,
    #[error("Installations are unavailable while the application is shutting down.")]
    Closed,
    #[error("The installation failed. Check your connection and try again.")]
    Failed,
}
impl From<StorageError> for InstallError {
    fn from(_: StorageError) -> Self {
        Self::Storage
    }
}
impl From<rusqlite::Error> for InstallError {
    fn from(_: rusqlite::Error) -> Self {
        Self::Storage
    }
}

#[derive(Clone)]
pub struct InstallQueue {
    inner: Arc<Inner>,
    telemetry: Option<Arc<Telemetry>>,
    content: Option<(Arc<ContentService>, Arc<ContentMutations>)>,
    #[cfg(feature = "test-support")]
    test_endpoints: Option<InstallTestEndpoints>,
}
struct Inner {
    storage: Arc<MetadataStore>,
    library: LibraryLifecycle,
    exclusions: Exclusions,
    owner: TaskOwner,
    runtime: ManagedRuntimeCache,
    state: Mutex<State>,
    latest: watch::Sender<InstallQueueStateResponse>,
    events_closed: AtomicBool,
    scheduling: AtomicBool,
    observers: Mutex<Observers>,
    observer_join: tokio::sync::Mutex<()>,
}
struct Observers {
    closed: bool,
    tasks: JoinSet<()>,
}
struct State {
    epoch: String,
    registry_revision: u64,
    revision: u64,
    queued: VecDeque<String>,
    active: Option<String>,
    entries: BTreeMap<String, Entry>,
    latest_failure: Option<InstallQueueFailureViewModel>,
    closed: bool,
}
struct Entry {
    request: InstallQueueRequest,
    item: InstallQueueInstallItemViewModel,
    status: InstallStatusResponse,
    pin: Option<GenerationPin>,
    started_at: Option<u64>,
    cancel: CancellationToken,
    // Recovery values retain publication authority; they must not be silently
    // discarded when a request, subscriber, or installer waiter goes away.
    retained: Option<RetainedInstall>,
    recovery_lease: Option<ExclusionLease>,
    recovery_running: bool,
    setup: Option<SetupWork>,
}

fn retained_retry_eligible(entry: &Entry) -> bool {
    !matches!(entry.request, InstallQueueRequest::Content { .. })
        && !entry.status.done
        && entry.status.view_model.phase_id == "settlement_required"
        && entry.pin.is_some()
        && entry.recovery_lease.is_some()
        && !entry.recovery_running
}

impl InstallQueue {
    pub fn new(
        storage: Arc<MetadataStore>,
        library: LibraryLifecycle,
        exclusions: Exclusions,
        owner: TaskOwner,
        runtime: ManagedRuntimeCache,
    ) -> Result<Self, InstallError> {
        let mut state = State {
            epoch: uuid::Uuid::new_v4().to_string(),
            registry_revision: 0,
            revision: 1,
            queued: VecDeque::new(),
            active: None,
            entries: BTreeMap::new(),
            latest_failure: None,
            closed: false,
        };
        let pending: Vec<(String, String, String, String, String, String)> = storage.read(|db| {
            let mut statement = db.prepare("SELECT id, library_id, request_json, target_json, status_json, phase FROM install_queue WHERE phase != 'terminal' ORDER BY accepted_at, rowid LIMIT 129")?;
            statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?)))?
                .collect::<Result<Vec<_>, _>>().map_err(InstallError::from)
        })?;
        if pending.len() > 128 {
            return Err(InstallError::AtCapacity);
        }
        for (id, library_id, request, item, status, phase) in pending {
            let request: InstallQueueRequest =
                serde_json::from_str(&request).map_err(|_| InstallError::Storage)?;
            let item = serde_json::from_str(&item).map_err(|_| InstallError::Storage)?;
            let mut status: InstallStatusResponse =
                serde_json::from_str(&status).map_err(|_| InstallError::Storage)?;
            let pin = library
                .admit()
                .ok()
                .filter(|pin| pin.library_id().to_string() == library_id);
            let recovery_lease = if phase == "queued" {
                state.queued.push_back(id.clone());
                None
            } else {
                status.view_model = settlement_progress();
                status.done = false;
                status.outcome = None;
                status.allowed_actions.clear();
                state.active = Some(id.clone());
                let lease = exclusions
                    .try_acquire(
                        std::iter::empty::<String>(),
                        install_artifacts(&request, &library_id),
                    )
                    .map_err(|_| InstallError::Busy)?;
                Some(lease)
            };
            state.entries.insert(
                id,
                Entry {
                    request,
                    item,
                    status,
                    pin,
                    started_at: None,
                    cancel: CancellationToken::new(),
                    retained: None,
                    recovery_lease,
                    recovery_running: false,
                    setup: None,
                },
            );
        }
        let (latest, _) = watch::channel(project(&state));
        Ok(Self {
            inner: Arc::new(Inner {
                storage,
                library,
                exclusions,
                owner,
                runtime,
                state: Mutex::new(state),
                latest,
                events_closed: AtomicBool::new(false),
                scheduling: AtomicBool::new(false),
                observers: Mutex::new(Observers {
                    closed: false,
                    tasks: JoinSet::new(),
                }),
                observer_join: tokio::sync::Mutex::new(()),
            }),
            telemetry: None,
            content: None,
            #[cfg(feature = "test-support")]
            test_endpoints: None,
        })
    }

    /// Attach before sharing the queue. Consent and event bounds remain owned
    /// by telemetry; installation only reports a settled, persisted failure.
    pub fn with_telemetry(mut self, telemetry: Arc<Telemetry>) -> Self {
        self.telemetry = Some(telemetry);
        self
    }

    pub fn with_content(
        mut self,
        content: Arc<ContentService>,
        mutations: Arc<ContentMutations>,
    ) -> Self {
        self.content = Some((content, mutations));
        self
    }

    /// Configure the real downloader's loopback providers before sharing this
    /// queue. This acceptance-only seam cannot provide readiness or receipts.
    #[cfg(feature = "test-support")]
    pub fn with_test_endpoints(mut self, endpoints: InstallTestEndpoints) -> Self {
        self.test_endpoints = Some(endpoints);
        self
    }

    pub fn runtime_cache(&self) -> &ManagedRuntimeCache {
        &self.inner.runtime
    }
    pub fn library(&self) -> &LibraryLifecycle {
        &self.inner.library
    }
    pub fn close_admission(&self) {
        let mut state = self.inner.state.lock().expect("install queue lock");
        state.closed = true;
        publish(&self.inner, &mut state);
    }
    pub fn has_unsettled_effects(&self) -> bool {
        self.inner
            .state
            .lock()
            .expect("install queue lock")
            .active
            .is_some()
    }
    pub fn events_closed(&self) -> bool {
        self.inner.events_closed.load(Ordering::Acquire)
    }
    pub fn close_events(&self) {
        self.inner.events_closed.store(true, Ordering::Release);
        self.inner.latest.send_replace(self.snapshot());
    }

    /// Join notification and scheduling waiters after accepted work has joined.
    /// Cancellation leaves their handles here for the next shutdown caller.
    pub async fn join_observers(&self) -> Result<(), InstallError> {
        let _joining = self.inner.observer_join.lock().await;
        {
            let state = self.inner.state.lock().expect("install queue lock");
            if !state.closed || self.inner.owner.shutdown_receipt().is_none() {
                return Err(InstallError::Busy);
            }
            self.inner
                .observers
                .lock()
                .expect("install observers lock")
                .closed = true;
        }
        std::future::poll_fn(|context| {
            let mut observers = self.inner.observers.lock().expect("install observers lock");
            loop {
                match observers.tasks.poll_join_next(context) {
                    std::task::Poll::Ready(Some(result)) => observer_finished(result),
                    std::task::Poll::Ready(None) => return std::task::Poll::Ready(()),
                    std::task::Poll::Pending => return std::task::Poll::Pending,
                }
            }
        })
        .await;
        Ok(())
    }

    // Retain this guard through work acceptance and waiter registration so a
    // just-completed worker cannot be missed by shutdown.
    fn observers(&self) -> Result<std::sync::MutexGuard<'_, Observers>, InstallError> {
        let mut observers = self.inner.observers.lock().expect("install observers lock");
        if observers.closed {
            return Err(InstallError::Closed);
        }
        while let Some(result) = observers.tasks.try_join_next() {
            observer_finished(result);
        }
        Ok(observers)
    }
    /// Call after the shared task owner joins. Queued intents remain durable for
    /// restart; their pins can be released because they never began effects.
    pub fn shutdown_queued(&self) -> Result<(), InstallError> {
        let mut state = self.inner.state.lock().expect("install queue lock");
        state.closed = true;
        if state.active.is_some() {
            return Err(InstallError::SettlementRequired);
        }
        let retained = state
            .entries
            .values_mut()
            .map(|entry| (entry.pin.take(), entry.setup.take()))
            .collect::<Vec<_>>();
        publish(&self.inner, &mut state);
        drop(state);
        drop(retained);
        Ok(())
    }

    /// Preserve an interrupted Content queue item without acknowledging its
    /// receipt or changing its status. Native installation recovery stays owned.
    pub fn preserve_shutdown(&self, receipt: &ShutdownReceipt) -> Result<(), InstallError> {
        if !receipt.belongs_to(&self.inner.owner) {
            return Err(InstallError::Busy);
        }
        let active = {
            let state = self.inner.state.lock().expect("install queue lock");
            let observers = self.inner.observers.lock().expect("install observers lock");
            if !state.closed || !observers.closed || !observers.tasks.is_empty() {
                return Err(InstallError::Busy);
            }
            preserved_content_instance(&state)?
        };
        let Some(instance) = active else {
            return self.shutdown_queued();
        };
        let (_, mutations) = self
            .content
            .as_ref()
            .ok_or(InstallError::ContentUnavailable)?;
        if !crate::content::install::has_pending(&self.inner.storage, &instance)?
            || crate::performance::mutation::has_pending(&self.inner.storage, &instance)?
        {
            return Err(InstallError::SettlementRequired);
        }
        mutations
            .release_shutdown_admissions(receipt)
            .map_err(|_| InstallError::SettlementRequired)?;
        let mut state = self.inner.state.lock().expect("install queue lock");
        if preserved_content_instance(&state)?.as_ref() != Some(&instance) {
            return Err(InstallError::SettlementRequired);
        }
        let admissions = state
            .entries
            .values_mut()
            .map(|entry| {
                (
                    entry.pin.take(),
                    entry.setup.take(),
                    entry.recovery_lease.take(),
                )
            })
            .collect::<Vec<_>>();
        drop(state);
        drop(admissions);
        Ok(())
    }
    pub fn snapshot(&self) -> InstallQueueStateResponse {
        project(&self.inner.state.lock().expect("install queue lock"))
    }
    pub(crate) fn active_count(&self) -> usize {
        let state = self.inner.state.lock().expect("install queue lock");
        state
            .entries
            .iter()
            .filter(|(id, entry)| !entry.status.done && !state.queued.contains(id))
            .count()
    }
    pub(crate) fn invalidate_registry(&self) {
        let mut state = self.inner.state.lock().expect("install queue lock");
        state.registry_revision = state.registry_revision.saturating_add(1);
        publish(&self.inner, &mut state);
    }
    pub fn subscribe(
        &self,
    ) -> (
        InstallQueueStateResponse,
        watch::Receiver<InstallQueueStateResponse>,
    ) {
        let state = self.inner.state.lock().expect("install queue lock");
        (project(&state), self.inner.latest.subscribe())
    }
    pub fn status(&self, id: &str) -> Result<InstallStatusResponse, InstallError> {
        if let Some(status) = self
            .inner
            .state
            .lock()
            .expect("install queue lock")
            .entries
            .get(id)
            .map(|entry| entry.status.clone())
        {
            return Ok(status);
        }
        let encoded: Option<String> = self.inner.storage.read(|db| {
            db.query_row(
                "SELECT status_json FROM install_queue WHERE id=?1",
                [id],
                |row| row.get(0),
            )
            .optional()
            .map_err(InstallError::from)
        })?;
        serde_json::from_str(&encoded.ok_or(InstallError::NotFound)?)
            .map_err(|_| InstallError::Storage)
    }

    pub async fn resolve_target(
        &self,
        request: &InstallQueueRequest,
    ) -> Result<InstallQueueInstallItemViewModel, InstallError> {
        match request {
            InstallQueueRequest::Vanilla { version_id } => {
                axial_minecraft::portable_path::PortableFileName::new_exact(version_id)
                    .map_err(|_| InstallError::InvalidRequest)?;
                Ok(InstallQueueInstallItemViewModel {
                    version_id: version_id.clone(),
                    loader: None,
                    content: None,
                })
            }
            InstallQueueRequest::Loader {
                component_id,
                build_id,
            } => {
                let record = resolve_loader(*component_id, build_id).await?;
                Ok(item_for_loader(&record))
            }
            InstallQueueRequest::Content {
                instance_id,
                label,
                action,
            } => {
                validate_content_request(label, action)?;
                let (_, mutations) = self
                    .content
                    .as_ref()
                    .ok_or(InstallError::ContentUnavailable)?;
                let id: InstanceId = instance_id
                    .parse()
                    .map_err(|_| InstallError::InvalidRequest)?;
                let record = mutations
                    .directories()
                    .registry()
                    .get_live(&id)
                    .map_err(|_| InstallError::NotFound)?;
                Ok(InstallQueueInstallItemViewModel {
                    version_id: record.instance.version_id,
                    loader: None,
                    content: Some(InstallQueueContentItemViewModel {
                        instance_id: instance_id.clone(),
                        label: label.clone(),
                        action: action.clone(),
                    }),
                })
            }
        }
    }

    pub async fn enqueue(
        &self,
        request: InstallQueueRequest,
    ) -> Result<InstallQueueStateResponse, InstallError> {
        self.enqueue_with_placement(request, None, false).await
    }

    pub(crate) async fn enqueue_creation(
        &self,
        request: InstallQueueRequest,
        version_id: &str,
        admission: &crate::instances::create::Admission,
    ) -> Result<InstallQueueStateResponse, InstallError> {
        let item = self.resolve_target(&request).await?;
        if item.version_id != version_id {
            return Err(InstallError::InvalidRequest);
        }
        let pin = admission.generation().clone();
        let operation = pin
            .managed_library()
            .map_err(|_| InstallError::LibraryUnavailable)?;
        let guard = axial_minecraft::VersionBundleReadGuard::acquire(&operation)
            .map_err(|_| InstallError::LibraryUnavailable)?;
        admission
            .validate()
            .map_err(|_| InstallError::LibraryUnavailable)?;
        let result = self.enqueue_resolved(request, item, pin, None, false);
        drop(guard);
        drop(operation);
        self.resume_queued();
        result
    }

    pub async fn retry(
        &self,
        request: InstallQueueRequest,
    ) -> Result<InstallQueueStateResponse, InstallError> {
        let retained = {
            let state = self.inner.state.lock().expect("install queue lock");
            state
                .active
                .as_ref()
                .filter(|id| {
                    state.entries.get(*id).is_some_and(|entry| {
                        entry.request == request
                            && !matches!(request, InstallQueueRequest::Content { .. })
                            && entry.status.view_model.phase_id == "settlement_required"
                    })
                })
                .cloned()
        };
        if let Some(id) = retained {
            return self.retry_retained(&id, &request).await;
        }
        let content = matches!(request, InstallQueueRequest::Content { .. });
        let response = self.enqueue_with_placement(request, None, true).await?;
        if content
            && response
                .started_install
                .as_ref()
                .is_some_and(|started| started.view_model.phase_id == "settlement_required")
        {
            self.recover_install(&response.started_install.as_ref().unwrap().install_id)
                .await?;
            let mut recovered = self.snapshot();
            recovered.started_install = response.started_install.and_then(|started| {
                self.status(&started.install_id)
                    .ok()
                    .map(|status| start_response(&status))
            });
            return Ok(recovered);
        }
        Ok(response)
    }

    /// Retry only the caller's exact retained operation, never a replacement
    /// with the same version or loader request.
    pub async fn retry_retained(
        &self,
        expected_id: &str,
        request: &InstallQueueRequest,
    ) -> Result<InstallQueueStateResponse, InstallError> {
        {
            let state = self.inner.state.lock().expect("install queue lock");
            if state.closed || self.inner.owner.status().closing {
                return Err(InstallError::Closed);
            }
            let entry = state
                .entries
                .get(expected_id)
                .ok_or(InstallError::NotFound)?;
            if &entry.request != request || matches!(request, InstallQueueRequest::Content { .. }) {
                return Err(InstallError::InvalidRequest);
            }
            if state.active.as_deref() != Some(expected_id) || !retained_retry_eligible(entry) {
                return Err(InstallError::Busy);
            }
        }
        self.recover_install(expected_id).await?;
        let mut response = self.snapshot();
        response.started_install = Some(start_response(&self.status(expected_id)?));
        Ok(response)
    }

    pub(crate) fn has_setup_work(&self, id: &InstanceId) -> bool {
        self.inner
            .state
            .lock()
            .expect("install queue lock")
            .entries
            .values()
            .any(|entry| {
                !entry.status.done
                    && entry
                        .setup
                        .as_ref()
                        .is_some_and(|work| &work.instance().record().instance.id == id)
            })
    }

    pub(crate) async fn resume_attached_setup(
        &self,
        id: &InstanceId,
    ) -> Result<Option<InstallQueueStateResponse>, InstallError> {
        self.resume_attached_setup_with_placement(id, false).await
    }

    pub(crate) async fn retry_attached_setup(
        &self,
        id: &InstanceId,
    ) -> Result<Option<InstallQueueStateResponse>, InstallError> {
        self.resume_attached_setup_with_placement(id, true).await
    }

    async fn resume_attached_setup_with_placement(
        &self,
        id: &InstanceId,
        front: bool,
    ) -> Result<Option<InstallQueueStateResponse>, InstallError> {
        let work = self
            .inner
            .state
            .lock()
            .expect("install queue lock")
            .entries
            .values()
            .filter(|entry| !entry.status.done)
            .filter_map(|entry| entry.setup.as_ref())
            .find(|work| &work.instance().record().instance.id == id)
            .cloned();
        match work {
            Some(work) => self
                .enqueue_setup_with_placement(work.request(), work.prerequisite(), work, front)
                .await
                .map(Some),
            None => Ok(None),
        }
    }

    pub(crate) async fn enqueue_setup_content(
        &self,
        request: InstallQueueRequest,
        prerequisite: InstallQueueRequest,
        work: SetupWork,
    ) -> Result<InstallQueueStateResponse, InstallError> {
        self.enqueue_setup_with_placement(request, prerequisite, work, false)
            .await
    }

    pub(crate) async fn retry_setup_content(
        &self,
        request: InstallQueueRequest,
        prerequisite: InstallQueueRequest,
        work: SetupWork,
    ) -> Result<InstallQueueStateResponse, InstallError> {
        self.enqueue_setup_with_placement(request, prerequisite, work, true)
            .await
    }

    async fn enqueue_setup_with_placement(
        &self,
        request: InstallQueueRequest,
        prerequisite: InstallQueueRequest,
        work: SetupWork,
        front: bool,
    ) -> Result<InstallQueueStateResponse, InstallError> {
        if request != work.request() || prerequisite != work.prerequisite() {
            return Err(InstallError::InvalidRequest);
        }
        let prerequisite_item = if self
            .ready_version(
                work.instance().generation(),
                &work.instance().record().instance.version_id,
            )
            .await
            .is_err()
        {
            Some(self.resolve_target(&prerequisite).await?)
        } else {
            None
        };
        let item = self.resolve_target(&request).await?;
        let pin = work.instance().generation().clone();
        let response = {
            let mut state = self.inner.state.lock().expect("install queue lock");
            let existing_content = state
                .entries
                .values()
                .any(|entry| !entry.status.done && entry.request == request);
            let result = if front {
                self.admit_locked(&mut state, request, item, pin.clone(), Some(work), true)
                    .and_then(|id| {
                        if let Some(item) = prerequisite_item {
                            self.admit_locked(&mut state, prerequisite, item, pin, None, true)?;
                        }
                        Ok(id)
                    })
            } else {
                let prerequisite = match prerequisite_item {
                    Some(item) => self
                        .admit_locked(
                            &mut state,
                            prerequisite,
                            item,
                            pin.clone(),
                            None,
                            existing_content,
                        )
                        .map(|_| ()),
                    None => Ok(()),
                };
                prerequisite.and_then(|()| {
                    self.admit_locked(&mut state, request, item, pin, Some(work), false)
                })
            };
            publish(&self.inner, &mut state);
            result.map(|id| {
                project_with_started(
                    &state,
                    state
                        .entries
                        .get(&id)
                        .map(|entry| start_response(&entry.status)),
                )
            })
        };
        self.resume_queued();
        let response = response?;
        let recover = response
            .started_install
            .as_ref()
            .is_some_and(|started| started.view_model.phase_id == "settlement_required");
        if recover {
            self.recover_install(&response.started_install.as_ref().unwrap().install_id)
                .await?;
            let mut recovered = self.snapshot();
            recovered.started_install = response.started_install.and_then(|started| {
                self.status(&started.install_id)
                    .ok()
                    .map(|status| start_response(&status))
            });
            return Ok(recovered);
        }
        Ok(response)
    }

    async fn enqueue_with_placement(
        &self,
        request: InstallQueueRequest,
        setup: Option<SetupWork>,
        front: bool,
    ) -> Result<InstallQueueStateResponse, InstallError> {
        let item = self.resolve_target(&request).await?;
        let pin = match setup.as_ref() {
            Some(work) => work.instance().generation().clone(),
            None => self
                .inner
                .library
                .admit()
                .map_err(|_| InstallError::LibraryUnavailable)?,
        };
        let result = self.enqueue_resolved(request, item, pin, setup, front);
        self.resume_queued();
        result
    }

    fn enqueue_resolved(
        &self,
        request: InstallQueueRequest,
        item: InstallQueueInstallItemViewModel,
        pin: GenerationPin,
        setup: Option<SetupWork>,
        front: bool,
    ) -> Result<InstallQueueStateResponse, InstallError> {
        let mut state = self.inner.state.lock().expect("install queue lock");
        let result = self.admit_locked(&mut state, request, item, pin, setup, front);
        publish(&self.inner, &mut state);
        result.map(|id| {
            project_with_started(
                &state,
                state
                    .entries
                    .get(&id)
                    .map(|entry| start_response(&entry.status)),
            )
        })
    }

    fn admit_locked(
        &self,
        state: &mut State,
        request: InstallQueueRequest,
        item: InstallQueueInstallItemViewModel,
        pin: GenerationPin,
        setup: Option<SetupWork>,
        front: bool,
    ) -> Result<String, InstallError> {
        let library_id = pin.library_id().to_string();
        if let InstallQueueRequest::Content { instance_id, .. } = &request {
            let (_, mutations) = self
                .content
                .as_ref()
                .ok_or(InstallError::ContentUnavailable)?;
            let record = mutations
                .directories()
                .registry()
                .get_live(
                    &instance_id
                        .parse()
                        .map_err(|_| InstallError::InvalidRequest)?,
                )
                .map_err(|_| InstallError::NotFound)?;
            if record.library_id != library_id {
                return Err(InstallError::LibraryUnavailable);
            }
        }
        let id = uuid::Uuid::new_v4().to_string();
        let operation_id = OperationId::new();
        if state.closed || self.inner.owner.status().closing {
            return Err(InstallError::Closed);
        }
        if let Some(existing_id) = state
            .entries
            .iter()
            .find(|(_, entry)| !entry.status.done && entry.request == request)
            .map(|(id, _)| id.clone())
        {
            let existing = state.entries.get(&existing_id).expect("existing install");
            if existing.item != item
                || !existing.pin.as_ref().is_some_and(|existing| {
                    existing.library_id() == pin.library_id()
                        && existing.generation() == pin.generation()
                })
            {
                return Err(InstallError::LibraryUnavailable);
            }
            if front && state.queued.iter().any(|id| id == &existing_id) {
                self.inner.storage.transaction(|db| {
                    db.execute("UPDATE install_queue SET accepted_at=(SELECT MIN(accepted_at)-1 FROM install_queue WHERE phase='queued') WHERE id=?1", [&existing_id])?;
                    Ok::<_, InstallError>(())
                })?;
                state.queued.retain(|id| id != &existing_id);
                state.queued.push_front(existing_id.clone());
            }
            let existing = state
                .entries
                .get_mut(&existing_id)
                .expect("existing install");
            if setup.is_some() {
                existing.setup = setup;
            }
            return Ok(existing_id);
        }
        if state.queued.len() + usize::from(state.active.is_some()) >= 128 {
            return Err(InstallError::AtCapacity);
        }
        let status = InstallStatusResponse {
            revision: 1,
            queue_id: id.clone(),
            outcome: None,
            allowed_actions: vec![InstallActionViewModel::enabled(
                "remove_from_queue",
                "Remove from queue",
            )],
            install_id: id.clone(),
            operation_id,
            done: false,
            progress: Vec::new(),
            view_model: InstallProgressViewModel {
                phase_id: "queued".into(),
                label: "Queued for installation".into(),
                ..InstallProgressViewModel::starting()
            },
            failure_view_model: None,
            failure_point: None,
        };
        let request_json = serde_json::to_string(&request).map_err(|_| InstallError::Storage)?;
        let item_json = serde_json::to_string(&item).map_err(|_| InstallError::Storage)?;
        let status_json = serde_json::to_string(&status).map_err(|_| InstallError::Storage)?;
        self.inner.storage.transaction(|db| {
            let accepted_at = if front {
                db.query_row("SELECT COALESCE(MIN(accepted_at)-1,?1) FROM install_queue WHERE phase='queued'", [now_ms() as i64], |row| row.get::<_, i64>(0))?
            } else { now_ms() as i64 };
            db.execute("INSERT INTO install_queue(id,operation_id,library_id,request_json,target_json,status_json,phase,accepted_at) VALUES(?1,?2,?3,?4,?5,?6,'queued',?7)",
                params![id, status.operation_id.as_str(), library_id, request_json, item_json, status_json, accepted_at])?;
            Ok::<_, InstallError>(())
        })?;
        if front {
            state.queued.push_front(id.clone());
        } else {
            state.queued.push_back(id.clone());
        }
        state.entries.insert(
            id.clone(),
            Entry {
                request,
                item,
                status,
                pin: Some(pin),
                started_at: None,
                cancel: CancellationToken::new(),
                retained: None,
                recovery_lease: None,
                recovery_running: false,
                setup,
            },
        );
        Ok(id)
    }

    pub async fn start_vanilla(
        &self,
        version_id: &str,
    ) -> Result<InstallStartResponse, InstallError> {
        self.enqueue(InstallQueueRequest::Vanilla {
            version_id: version_id.into(),
        })
        .await?
        .started_install
        .ok_or(InstallError::Failed)
    }

    pub async fn remove(&self, id: &str) -> Result<InstallQueueStateResponse, InstallError> {
        let setup = {
            let state = self.inner.state.lock().expect("install queue lock");
            if state.active.as_deref() == Some(id) {
                return Err(InstallError::Busy);
            }
            if !state.queued.iter().any(|queued| queued == id) {
                return Err(InstallError::NotFound);
            }
            state.entries.get(id).and_then(|entry| entry.setup.clone())
        };
        let Some(work) = setup else {
            return self.remove_queued(id);
        };
        let operation_id = uuid::Uuid::parse_str(id).map_err(|_| InstallError::InvalidRequest)?;
        let queue = self.clone();
        let id = id.to_owned();
        self.inner
            .owner
            .try_spawn(work.clone(), move |_| async move {
                queue.remove_queued(&id)?;
                let result = work.remove(operation_id).await;
                let mut state = queue.inner.state.lock().expect("install queue lock");
                state.registry_revision = state.registry_revision.saturating_add(1);
                publish(&queue.inner, &mut state);
                let mut response = project(&state);
                match result {
                    Ok(removed) => response.removed_instance_id = removed.map(|id| id.to_string()),
                    Err(_) => {
                        response.notice = Some(InstallQueueNoticeViewModel {
                            state_id: "removed_cleanup_pending".into(),
                            tone: "warn".into(),
                            message: "Removed from queue. Instance cleanup is incomplete.".into(),
                            detail: Some("The instance may need deletion to be resumed.".into()),
                        })
                    }
                }
                Ok(response)
            })
            .map_err(|_| InstallError::Busy)?
            .join()
            .await
            .map_err(|_| InstallError::SettlementRequired)?
    }

    fn remove_queued(&self, id: &str) -> Result<InstallQueueStateResponse, InstallError> {
        let mut state = self.inner.state.lock().expect("install queue lock");
        if state.active.as_deref() == Some(id) {
            return Err(InstallError::Busy);
        }
        let position = state
            .queued
            .iter()
            .position(|queued| queued == id)
            .ok_or(InstallError::NotFound)?;
        let entry = state.entries.get_mut(id).ok_or(InstallError::NotFound)?;
        let mut status = entry.status.clone();
        finish_status(&mut status, InstallOutcome::Removed, None);
        persist_status(&self.inner.storage, &status, "terminal")?;
        entry.status = status;
        entry.pin = None;
        entry.setup = None;
        state.queued.remove(position);
        state.registry_revision = state.registry_revision.saturating_add(1);
        publish(&self.inner, &mut state);
        Ok(project(&state))
    }

    /// Only cancellation before the retained materializer starts can be accepted.
    /// Once it owns effects, completion is joined and publication is settled.
    pub fn cancel(&self, id: &str) -> Result<InstallStatusResponse, InstallError> {
        let state = self.inner.state.lock().expect("install queue lock");
        let entry = state.entries.get(id).ok_or(InstallError::NotFound)?;
        if matches!(entry.request, InstallQueueRequest::Content { .. })
            || entry.status.view_model.phase_id != "starting"
        {
            return Err(InstallError::Busy);
        }
        entry.cancel.cancel();
        Ok(entry.status.clone())
    }

    /// Resume retained native proofs or reconstruct their exact activation
    /// contract after restart. An ambiguous witness keeps the library fenced.
    pub async fn recover_interrupted(&self) -> Result<(), InstallError> {
        let id = self
            .inner
            .state
            .lock()
            .expect("install queue lock")
            .active
            .clone();
        match id {
            Some(id) => self.recover_install(&id).await,
            None => Ok(()),
        }
    }

    async fn recover_install(&self, id: &str) -> Result<(), InstallError> {
        let content = {
            let state = self.inner.state.lock().expect("install queue lock");
            let entry = state.entries.get(id).ok_or(InstallError::NotFound)?;
            matches!(entry.request, InstallQueueRequest::Content { .. })
        };
        if content {
            return self.recover_content(id).await;
        }
        let queue = self.clone();
        self.recover_interrupted_with(
            id,
            |version, expected, recorded| async move {
                axial_minecraft::reconstruct_known_good(&version, &expected, recorded)
                    .await
                    .map_err(|_| InstallError::SettlementRequired)
            },
            move |operation, continuation, id| async move {
                super::processors::continue_loader_install(&operation, continuation, move |event| {
                    queue.progress(&id, event)
                })
                .await
            },
        )
        .await
    }

    async fn recover_interrupted_with<Reconstruct, Reconstruction, Continue, Continuation>(
        &self,
        expected_id: &str,
        reconstruct: Reconstruct,
        continue_loader: Continue,
    ) -> Result<(), InstallError>
    where
        Reconstruct: FnOnce(
                String,
                ManagedInstallActivationContractId,
                Option<axial_minecraft::RecordedVersionMetadata>,
            ) -> Reconstruction
            + Send
            + 'static,
        Reconstruction: std::future::Future<
                Output = Result<axial_minecraft::KnownGoodReconstructionReceipt, InstallError>,
            > + Send,
        Continue: FnOnce(
                axial_minecraft::managed_path::ManagedLibraryOperation,
                LoaderInstallBaseContinuation,
                String,
            ) -> Continuation
            + Send
            + 'static,
        Continuation: std::future::Future<
                Output = Result<axial_minecraft::KnownGoodInstallReceipt, LoaderInstallError>,
            > + Send,
    {
        let id = expected_id.to_owned();
        let (pin, lease, item) = {
            let mut state = self.inner.state.lock().expect("install queue lock");
            let active = state.active.as_deref() == Some(expected_id);
            let entry = state
                .entries
                .get_mut(expected_id)
                .ok_or(InstallError::NotFound)?;
            if entry.status.done || entry.status.view_model.phase_id == "queued" {
                return Ok(());
            }
            if !active || entry.status.view_model.phase_id != "settlement_required" {
                return Err(InstallError::Busy);
            }
            if entry.recovery_running {
                return Err(InstallError::Busy);
            }
            let pin = entry.pin.clone().ok_or(InstallError::LibraryUnavailable)?;
            let lease = entry
                .recovery_lease
                .clone()
                .ok_or(InstallError::SettlementRequired)?;
            let item = entry.item.clone();
            entry.recovery_running = true;
            publish(&self.inner, &mut state);
            (pin, lease, item)
        };
        let operation = match pin.managed_library() {
            Ok(operation) => operation,
            Err(_) => {
                self.recovery_finished(&id, false);
                return Err(InstallError::LibraryUnavailable);
            }
        };
        let queue = self.clone();
        let worker_id = id.clone();
        let receiver = (|| -> Result<_, InstallError> {
            let mut observers = self.observers()?;
            let task = self
                .inner
                .owner
                .try_spawn(
                    (pin.clone(), lease, operation.clone()),
                    move |_| async move {
                        let retained = queue
                            .inner
                            .state
                            .lock()
                            .expect("install queue lock")
                            .entries
                            .get_mut(&worker_id)
                            .and_then(|entry| entry.retained.take());
                        let result = queue
                            .recover_publication(
                                &worker_id,
                                &pin,
                                &operation,
                                &item,
                                retained,
                                reconstruct,
                            )
                            .await;
                        let result = match result {
                            Ok(RecoveryAction::Requeue) => {
                                return queue.requeue_recovered(&worker_id);
                            }
                            Ok(RecoveryAction::Complete) => Ok(()),
                            Ok(RecoveryAction::Continue(continuation)) => {
                                match continue_loader(
                                    operation.clone(),
                                    continuation,
                                    worker_id.clone(),
                                )
                                .await
                                {
                                    Ok(receipt) => {
                                        queue
                                            .settle_receipt(
                                                &worker_id,
                                                &pin,
                                                operation.clone(),
                                                receipt,
                                            )
                                            .await
                                    }
                                    Err(LoaderInstallError::PublicationIndeterminate(recovery)) => {
                                        Err(WorkFailure::Unsettled(
                                            RetainedInstall::LoaderPublication(recovery),
                                        ))
                                    }
                                    Err(error) => {
                                        log_loader_failure(&error);
                                        queue
                                            .classify_failure(
                                                &worker_id,
                                                &operation,
                                                &item.version_id,
                                                error,
                                            )
                                            .await
                                    }
                                }
                            }
                            Err(failure) => Err(failure),
                        };
                        match result {
                            Ok(())
                                if queue.complete(&worker_id, InstallOutcome::Succeeded, None) =>
                            {
                                Ok(())
                            }
                            Err(WorkFailure::Failed(error))
                                if queue.complete(
                                    &worker_id,
                                    InstallOutcome::Failed,
                                    Some(error),
                                ) =>
                            {
                                Ok(())
                            }
                            Err(WorkFailure::Unsettled(retained)) => {
                                queue.retain_unsettled(&worker_id, retained);
                                Err(InstallError::SettlementRequired)
                            }
                            _ => {
                                queue.retain_unsettled(&worker_id, RetainedInstall::Retry);
                                Err(InstallError::Storage)
                            }
                        }
                    },
                )
                .map_err(|_| InstallError::Busy)?;
            let queue = self.clone();
            let id = id.clone();
            let (sender, receiver) = tokio::sync::oneshot::channel();
            // Joining and rescheduling survive the disposable recovery
            // caller, just as they do for an ordinary install worker.
            observers.tasks.spawn(async move {
                let result = task
                    .join()
                    .await
                    .unwrap_or(Err(InstallError::SettlementRequired));
                queue.recovery_finished(&id, result.is_ok());
                drop(queue);
                let _ = sender.send(result);
            });
            Ok(receiver)
        })();
        match receiver {
            Ok(receiver) => receiver
                .await
                .unwrap_or(Err(InstallError::SettlementRequired)),
            Err(error) => {
                self.recovery_finished(&id, false);
                Err(error)
            }
        }
    }

    fn recovery_finished(&self, id: &str, settled: bool) {
        let mut state = self.inner.state.lock().expect("install queue lock");
        let mut setup_changed = false;
        if let Some(entry) = state.entries.get_mut(id) {
            entry.recovery_running = false;
            if !settled && !entry.status.done {
                setup_changed = entry.setup.is_some()
                    && entry.status.view_model.phase_id != "settlement_required";
                entry.status.view_model = settlement_progress();
                entry.status.allowed_actions.clear();
                entry.status.revision = entry.status.revision.saturating_add(1);
                let _ = persist_status(&self.inner.storage, &entry.status, "settlement_required");
            }
        }
        if setup_changed || settled {
            state.registry_revision = state.registry_revision.saturating_add(1);
        }
        publish(&self.inner, &mut state);
        drop(state);
        if settled {
            self.resume_queued();
        }
    }

    fn requeue_recovered(&self, id: &str) -> Result<(), InstallError> {
        let mut state = self.inner.state.lock().expect("install queue lock");
        let entry = state.entries.get_mut(id).ok_or(InstallError::NotFound)?;
        let mut status = entry.status.clone();
        status.view_model.phase_id = "queued".into();
        status.view_model.label = "Queued after restart".into();
        status.allowed_actions = vec![InstallActionViewModel::enabled(
            "remove_from_queue",
            "Remove from queue",
        )];
        persist_status(&self.inner.storage, &status, "queued")?;
        entry.status = status;
        entry.recovery_lease = None;
        entry.retained = None;
        state.active = None;
        state.queued.push_front(id.to_owned());
        publish(&self.inner, &mut state);
        Ok(())
    }

    async fn recover_publication<Reconstruct, Reconstruction>(
        &self,
        id: &str,
        pin: &GenerationPin,
        operation: &axial_minecraft::managed_path::ManagedLibraryOperation,
        item: &InstallQueueInstallItemViewModel,
        retained: Option<RetainedInstall>,
        reconstruct: Reconstruct,
    ) -> Result<RecoveryAction, WorkFailure>
    where
        Reconstruct: FnOnce(
            String,
            ManagedInstallActivationContractId,
            Option<axial_minecraft::RecordedVersionMetadata>,
        ) -> Reconstruction,
        Reconstruction: std::future::Future<
                Output = Result<axial_minecraft::KnownGoodReconstructionReceipt, InstallError>,
            >,
    {
        let checkpoint = (|| -> Result<Option<PublicationCheckpoint>, InstallError> {
            let encoded: Option<String> = self.inner.storage.read(|db| {
                db.query_row(
                    "SELECT checkpoint_json FROM install_queue WHERE id=?1",
                    [id],
                    |row| row.get(0),
                )
                .map_err(InstallError::from)
            })?;
            let checkpoint: Option<PublicationCheckpoint> = encoded
                .as_deref()
                .map(|encoded| {
                    serde_json::from_str(encoded).map_err(|_| InstallError::SettlementRequired)
                })
                .transpose()?;
            if let Some(checkpoint) = &checkpoint {
                checkpoint.verify(operation, item)?;
            }
            Ok(checkpoint)
        })();
        let checkpoint = match checkpoint {
            Ok(checkpoint) => checkpoint,
            Err(error) => {
                tracing::warn!(?error, "Installation settlement requires retry.");
                return Err(WorkFailure::Unsettled(
                    retained.unwrap_or(RetainedInstall::Retry),
                ));
            }
        };
        let outcome = match retained {
            Some(RetainedInstall::Publication(recovery)) => match recovery.retry().await {
                Ok(receipt) => {
                    return self
                        .settle_receipt(id, pin, operation.clone(), receipt)
                        .await
                        .map(|()| RecoveryAction::Complete);
                }
                Err(DownloadError::PublicationIndeterminate(recovery)) => {
                    return Err(WorkFailure::Unsettled(RetainedInstall::Publication(
                        recovery,
                    )));
                }
                Err(error) => {
                    return self
                        .classify_failure(id, operation, &item.version_id, error)
                        .await
                        .map(|()| RecoveryAction::Complete);
                }
            },
            Some(RetainedInstall::LoaderPublication(recovery)) => match recovery.retry().await {
                Ok(outcome) => {
                    return self
                        .settle_loader(id, pin, operation.clone(), &item.version_id, outcome)
                        .await
                        .map(|()| RecoveryAction::Complete);
                }
                Err(LoaderInstallError::PublicationIndeterminate(recovery)) => {
                    return Err(WorkFailure::Unsettled(RetainedInstall::LoaderPublication(
                        recovery,
                    )));
                }
                Err(error) => {
                    log_loader_failure(&error);
                    return self
                        .classify_failure(id, operation, &item.version_id, error)
                        .await
                        .map(|()| RecoveryAction::Complete);
                }
            },
            Some(RetainedInstall::Acknowledgement {
                recovery,
                continuation,
            }) => match recovery.retry().await {
                ManagedInstallAcknowledgementOutcome::Indeterminate(recovery) => {
                    return Err(WorkFailure::Unsettled(RetainedInstall::Acknowledgement {
                        recovery,
                        continuation,
                    }));
                }
                ManagedInstallAcknowledgementOutcome::Acknowledged => {
                    if let Some(continuation) = continuation {
                        let checkpoint = checkpoint
                            .as_ref()
                            .ok_or(InstallError::SettlementRequired)?;
                        self.mark_ready(pin, id, checkpoint)?;
                        return Ok(RecoveryAction::Continue(continuation));
                    }
                    ManagedInstallDurableOutcome::NoEffect
                }
            },
            Some(RetainedInstall::Outcome(ManagedInstallDurableOutcome::Indeterminate(
                recovery,
            ))) => recovery.retry().await,
            Some(RetainedInstall::Outcome(
                outcome @ (ManagedInstallDurableOutcome::Committed(_)
                | ManagedInstallDurableOutcome::RolledBack { .. }),
            )) => outcome,
            _ => {
                let candidates = match &item.loader {
                    Some(loader) => axial_minecraft::ManagedInstallPublicationCandidates::pair(
                        item.version_id.clone(),
                        loader.minecraft_version.clone(),
                    ),
                    None => axial_minecraft::ManagedInstallPublicationCandidates::one(
                        item.version_id.clone(),
                    ),
                }
                .map_err(|_| InstallError::InvalidRequest)?;
                axial_minecraft::classify_managed_install_publication_candidates(
                    operation.clone(),
                    candidates,
                )
                .await
            }
        };
        let (checkpoint, evidence) = match outcome {
            ManagedInstallDurableOutcome::NoEffect => match checkpoint {
                Some(checkpoint) if checkpoint.kind == CheckpointKind::RolledBack => {
                    return Err(WorkFailure::Failed(InstallError::Failed));
                }
                Some(checkpoint) => (checkpoint, None),
                None => {
                    let has_activation: bool = self.inner.storage.read(|db| db.query_row(
                        "SELECT EXISTS(SELECT 1 FROM installed_versions WHERE library_id=?1 AND install_id=?2)",
                        params![pin.library_id().to_string(), id], |row| row.get(0)).map_err(InstallError::from))?;
                    return if has_activation {
                        Err(InstallError::SettlementRequired.into())
                    } else {
                        Ok(RecoveryAction::Requeue)
                    };
                }
            },
            ManagedInstallDurableOutcome::Committed(evidence) => {
                let kind = if evidence.version_id() == item.version_id {
                    CheckpointKind::Final
                } else if item
                    .loader
                    .as_ref()
                    .is_some_and(|loader| loader.minecraft_version == evidence.version_id())
                {
                    CheckpointKind::Base
                } else {
                    return Err(WorkFailure::Unsettled(RetainedInstall::Outcome(
                        ManagedInstallDurableOutcome::Committed(evidence),
                    )));
                };
                let observed = PublicationCheckpoint {
                    kind,
                    version_id: evidence.version_id().to_owned(),
                    evidence_id: evidence.id().as_str().to_owned(),
                    contract_id: evidence
                        .committed_activation_contract_id()
                        .map(|contract| contract.as_str().to_owned()),
                };
                if let Err(error) = observed.verify(operation, item) {
                    tracing::warn!(?error, "Installation settlement requires retry.");
                    return Err(WorkFailure::Unsettled(RetainedInstall::Outcome(
                        ManagedInstallDurableOutcome::Committed(evidence),
                    )));
                }
                if checkpoint.as_ref().is_some_and(|previous| {
                    !(previous.kind == CheckpointKind::Base
                        && observed.kind == CheckpointKind::Final)
                        && (previous.kind != observed.kind
                            || previous.evidence_id != observed.evidence_id
                            || previous.contract_id != observed.contract_id)
                }) {
                    return Err(WorkFailure::Unsettled(RetainedInstall::Outcome(
                        ManagedInstallDurableOutcome::Committed(evidence),
                    )));
                }
                (observed, Some(evidence))
            }
            ManagedInstallDurableOutcome::RolledBack { evidence, effect } => {
                if checkpoint.as_ref().is_some_and(|previous| {
                    !(previous.kind == CheckpointKind::Base
                        && evidence.id().matches_version_id(&item.version_id))
                        && (previous.kind != CheckpointKind::RolledBack
                            || previous.evidence_id != evidence.id().as_str())
                }) {
                    return Err(WorkFailure::Unsettled(RetainedInstall::Outcome(
                        ManagedInstallDurableOutcome::RolledBack { evidence, effect },
                    )));
                }
                let version_id = if evidence.id().matches_version_id(&item.version_id) {
                    item.version_id.clone()
                } else {
                    item.loader
                        .as_ref()
                        .ok_or(InstallError::SettlementRequired)?
                        .minecraft_version
                        .clone()
                };
                let checkpoint = PublicationCheckpoint {
                    kind: CheckpointKind::RolledBack,
                    version_id,
                    evidence_id: evidence.id().as_str().to_owned(),
                    contract_id: None,
                };
                checkpoint.verify(operation, item)?;
                self.persist_checkpoint(id, &checkpoint)?;
                return match evidence.acknowledge().await {
                    ManagedInstallAcknowledgementOutcome::Acknowledged => {
                        Err(WorkFailure::Failed(InstallError::Failed))
                    }
                    ManagedInstallAcknowledgementOutcome::Indeterminate(recovery) => {
                        Err(WorkFailure::Unsettled(RetainedInstall::Acknowledgement {
                            recovery,
                            continuation: None,
                        }))
                    }
                };
            }
            outcome => return Err(WorkFailure::Unsettled(RetainedInstall::Outcome(outcome))),
        };
        let contract = checkpoint
            .contract_id
            .as_deref()
            .and_then(|value| ManagedInstallActivationContractId::parse(value).ok());
        let Some(contract) = contract else {
            tracing::warn!(error = ?InstallError::SettlementRequired, "Installation settlement requires retry.");
            return Err(WorkFailure::Unsettled(match evidence {
                Some(evidence) => {
                    RetainedInstall::Outcome(ManagedInstallDurableOutcome::Committed(evidence))
                }
                None => RetainedInstall::Retry,
            }));
        };
        let recorded = if let Some(committed) = &evidence {
            match committed.read_recorded_metadata().await {
                Ok(recorded) => Some(recorded),
                Err(_) => {
                    return Err(WorkFailure::Unsettled(match evidence {
                        Some(evidence) => RetainedInstall::Outcome(
                            ManagedInstallDurableOutcome::Committed(evidence),
                        ),
                        None => RetainedInstall::Retry,
                    }));
                }
            }
        } else {
            let metadata = (|| -> Result<Option<super::artifacts::ActivatedFile>, InstallError> {
                let encoded: Option<Option<String>> = self.inner.storage.read(|db| {
                    db.query_row(
                        "SELECT CASE WHEN length(CAST(inventory_json AS BLOB))<=134217728 THEN inventory_json END FROM installed_versions WHERE library_id=?1 AND version_id=?2 AND contract_id=?3 AND install_id=?4 AND state IN ('activating','ready')",
                        params![pin.library_id().to_string(), checkpoint.version_id, contract.as_str(), id],
                        |row| row.get(0),
                    ).optional().map_err(InstallError::from)
                })?;
                let Some(encoded) = encoded else {
                    return Ok(None);
                };
                let registered: ActivatedVersion =
                    serde_json::from_str(&encoded.ok_or(InstallError::SettlementRequired)?)
                        .map_err(|_| InstallError::SettlementRequired)?;
                if registered.version_id != checkpoint.version_id
                    || registered.contract_id != contract.as_str()
                    || registered.files.is_empty()
                    || registered.files.len() > 1_000_000
                {
                    return Err(InstallError::SettlementRequired);
                }
                let path = format!("versions/{0}/{0}.json", checkpoint.version_id);
                let mut matching = registered
                    .files
                    .into_iter()
                    .filter(|file| file.path == path);
                let metadata = matching.next().ok_or(InstallError::SettlementRequired)?;
                if matching.next().is_some() {
                    return Err(InstallError::SettlementRequired);
                }
                Ok(Some(metadata))
            })();
            match metadata {
                Ok(Some(metadata)) => {
                    match axial_minecraft::RecordedVersionMetadata::read_registered(
                        operation.clone(),
                        &checkpoint.version_id,
                        &contract,
                        &metadata.sha1,
                        metadata.size,
                    )
                    .await
                    {
                        Ok(recorded) => recorded,
                        Err(_) => return Err(WorkFailure::Unsettled(RetainedInstall::Retry)),
                    }
                }
                Ok(None) => None,
                Err(_) => return Err(WorkFailure::Unsettled(RetainedInstall::Retry)),
            }
        };
        let receipt =
            match reconstruct(checkpoint.version_id.clone(), contract.clone(), recorded).await {
                Ok(receipt) => receipt,
                Err(_) => {
                    return Err(WorkFailure::Unsettled(match evidence {
                        Some(evidence) => RetainedInstall::Outcome(
                            ManagedInstallDurableOutcome::Committed(evidence),
                        ),
                        None => RetainedInstall::Retry,
                    }));
                }
            };
        let storage = self.inner.storage.clone();
        let install_id = id.to_owned();
        let activation_pin = pin.clone();
        let activation_checkpoint = checkpoint.clone();
        let activate = move |source| {
            persist_activation(
                storage,
                activation_pin,
                install_id,
                Arc::new(source),
                activation_checkpoint,
            )
        };
        let (continuation, acknowledgement) = if checkpoint.kind == CheckpointKind::Base {
            let commit = match axial_minecraft::loaders::resume_install_build_after_base(
                &item.version_id,
                receipt,
            ) {
                Ok(commit) => commit,
                Err(_) => {
                    tracing::warn!(error = ?InstallError::SettlementRequired, "Installation settlement requires retry.");
                    return Err(WorkFailure::Unsettled(match evidence {
                        Some(evidence) => RetainedInstall::Outcome(
                            ManagedInstallDurableOutcome::Committed(evidence),
                        ),
                        None => RetainedInstall::Retry,
                    }));
                }
            };
            match evidence {
                Some(evidence) => {
                    let verified =
                        evidence
                            .verify_loader_base_commit(commit)
                            .map_err(|failure| {
                                let (evidence, _) = failure.into_parts();
                                WorkFailure::Unsettled(RetainedInstall::Outcome(
                                    ManagedInstallDurableOutcome::Committed(evidence),
                                ))
                            })?;
                    let (continuation, acknowledgement) = verified
                        .activate_with(activate)
                        .await
                        .map_err(base_activation_error)?;
                    (Some(continuation), Some(acknowledgement))
                }
                None => {
                    let verified = axial_minecraft::verify_managed_install_loader_base_checkpoint(
                        &contract, commit,
                    )
                    .map_err(|_| InstallError::SettlementRequired)?;
                    (
                        Some(
                            verified
                                .activate_with(activate)
                                .await
                                .map_err(base_activation_error)?,
                        ),
                        None,
                    )
                }
            }
        } else {
            match evidence {
                Some(evidence) => {
                    let verified =
                        evidence
                            .verify_reconstruction_receipt(receipt)
                            .map_err(|failure| {
                                let (evidence, _) = failure.into_parts();
                                WorkFailure::Unsettled(RetainedInstall::Outcome(
                                    ManagedInstallDurableOutcome::Committed(evidence),
                                ))
                            })?;
                    (
                        None,
                        Some(
                            verified
                                .activate_with(activate)
                                .await
                                .map_err(|_| InstallError::Storage)?,
                        ),
                    )
                }
                None => {
                    let verified =
                        axial_minecraft::verify_managed_install_reconstruction_checkpoint(
                            &contract, receipt,
                        )
                        .map_err(|_| InstallError::SettlementRequired)?;
                    verified
                        .activate_with(activate)
                        .await
                        .map_err(|_| InstallError::Storage)?;
                    (None, None)
                }
            }
        };
        if let Some(acknowledgement) = acknowledgement {
            if let ManagedInstallAcknowledgementOutcome::Indeterminate(recovery) =
                acknowledgement.acknowledge().await
            {
                return Err(WorkFailure::Unsettled(RetainedInstall::Acknowledgement {
                    recovery,
                    continuation,
                }));
            }
        }
        self.mark_ready(pin, id, &checkpoint)?;
        Ok(match continuation {
            Some(continuation) => RecoveryAction::Continue(continuation),
            None => RecoveryAction::Complete,
        })
    }

    fn persist_checkpoint(
        &self,
        id: &str,
        checkpoint: &PublicationCheckpoint,
    ) -> Result<(), InstallError> {
        let encoded = serde_json::to_string(checkpoint).map_err(|_| InstallError::Storage)?;
        self.inner.storage.transaction(|db| {
            let changed = db.execute(
                "UPDATE install_queue SET checkpoint_json=?1 WHERE id=?2",
                params![encoded, id],
            )?;
            if changed != 1 {
                return Err(InstallError::Storage);
            }
            Ok::<_, InstallError>(())
        })
    }

    /// May be called after startup to resume only entries that never began effects.
    pub fn resume_queued(&self) {
        let mut changes = self.inner.owner.subscribe();
        self.start_next();
        let needs_scheduler = {
            let state = self.inner.state.lock().expect("install queue lock");
            !state.closed && state.active.is_none() && !state.queued.is_empty()
        };
        if !needs_scheduler || self.inner.scheduling.swap(true, Ordering::AcqRel) {
            return;
        }
        let queue = self.clone();
        let mut queue_changes = self.inner.latest.subscribe();
        let Ok(mut observers) = self.observers() else {
            self.inner.scheduling.store(false, Ordering::Release);
            return;
        };
        observers.tasks.spawn(async move {
            loop {
                {
                    let state = queue.inner.state.lock().expect("install queue lock");
                    if state.closed
                        || queue.inner.owner.status().closing
                        || state.active.is_some()
                        || state.queued.is_empty()
                    {
                        break;
                    }
                }
                tokio::select! {
                    change = changes.changed() => if change.is_err() { break; },
                    change = queue_changes.changed() => if change.is_err() { break; },
                }
                queue.start_next();
            }
            queue.inner.scheduling.store(false, Ordering::Release);
        });
    }

    /// Display metadata only. Selected installs still require `ready_version`
    /// to verify their exact files before reuse or launch.
    pub(crate) fn ready_version_ids(
        &self,
        pin: &GenerationPin,
    ) -> Result<BTreeSet<String>, InstallError> {
        const MAX_VERSION_IDS: usize = 4096;
        pin.revalidate()
            .map_err(|_| InstallError::LibraryUnavailable)?;
        self.inner.storage.read(|db| {
            let mut statement = db.prepare(
                "SELECT version_id FROM installed_versions WHERE library_id=?1 AND state='ready' ORDER BY version_id LIMIT ?2",
            )?;
            let mut rows = statement.query(params![pin.library_id().to_string(), MAX_VERSION_IDS + 1])?;
            let mut ids = BTreeSet::new();
            while let Some(row) = rows.next()? {
                let id: String = row.get(0)?;
                if ids.len() == MAX_VERSION_IDS || axial_minecraft::portable_path::PortableFileName::new_exact(&id).is_err() {
                    return Err(InstallError::Storage);
                }
                ids.insert(id);
            }
            Ok(ids)
        })
    }

    pub async fn ready_version(
        &self,
        pin: &GenerationPin,
        version_id: &str,
    ) -> Result<InstalledVersionReceipt, InstallError> {
        self.inspect_version(pin, version_id, false)
            .await?
            .into_ready()
    }

    pub(crate) async fn inspect_version(
        &self,
        pin: &GenerationPin,
        version_id: &str,
        diagnostics: bool,
    ) -> Result<VersionInspection, InstallError> {
        let library_id = pin.library_id().to_string();
        let version_id = version_id.to_owned();
        let storage = self.inner.storage.clone();
        let pin = pin.clone();
        tokio::task::spawn_blocking(move || {
            let record: Option<String> = storage.read(|db| {
                db.query_row("SELECT inventory_json FROM installed_versions WHERE library_id=?1 AND version_id=?2 AND state='ready'", params![library_id, version_id], |row| row.get(0))
                    .optional().map_err(InstallError::from)
            })?;
            let record = record.ok_or(InstallError::NotReady)?;
            if record.len() > 128 << 20 { return Err(InstallError::NotReady); }
            let activated: ActivatedVersion = serde_json::from_str(&record).map_err(|_| InstallError::NotReady)?;
            if activated.version_id != version_id { return Err(InstallError::NotReady); }
            activated.inspect(pin, diagnostics)
        }).await.map_err(|_| InstallError::NotReady)?
    }

    fn start_next(&self) {
        let mut state = self.inner.state.lock().expect("install queue lock");
        if state.closed || state.active.is_some() || self.inner.owner.status().closing {
            return;
        }
        let Ok(mut observers) = self.observers() else {
            return;
        };
        let Some(id) = state.queued.front().cloned() else {
            return;
        };
        let entry = state.entries.get_mut(&id).expect("queued install exists");
        if let InstallQueueRequest::Content { instance_id, .. } = &entry.request {
            if self.content.is_none() {
                return;
            }
            let Ok(instance_id) = instance_id.parse::<InstanceId>() else {
                return;
            };
            if entry.setup.is_none()
                && crate::instances::setup::has_pending(&self.inner.storage, &instance_id)
                    .unwrap_or(true)
            {
                return;
            }
        }
        let Some(pin) = entry.pin.clone() else {
            return;
        };
        let Ok(lease) = self.inner.exclusions.try_acquire(
            std::iter::empty::<String>(),
            install_artifacts(&entry.request, &pin.library_id().to_string()),
        ) else {
            return;
        };
        let Ok(operation) = pin.managed_library() else {
            return;
        };
        let request = entry.request.clone();
        let cancel = entry.cancel.clone();
        let setup = entry.setup.clone();
        entry.status.view_model = InstallProgressViewModel::starting();
        entry.status.allowed_actions = if matches!(request, InstallQueueRequest::Content { .. }) {
            Vec::new()
        } else {
            vec![InstallActionViewModel::enabled("cancel", "Cancel install")]
        };
        entry.started_at = Some(now_ms());
        if persist_status(&self.inner.storage, &entry.status, "running").is_err() {
            return;
        }
        state.active = Some(id.clone());
        state.queued.pop_front();
        let queue = self.clone();
        let worker_id = id.clone();
        let retained = (pin.clone(), lease.clone(), operation.clone(), setup);
        let rejected_retention = (pin.clone(), lease.clone(), operation.clone());
        let result = self
            .inner
            .owner
            .try_spawn(retained, move |owner_cancel| async move {
                queue
                    .run(
                        worker_id,
                        pin,
                        lease,
                        operation,
                        request,
                        cancel,
                        owner_cancel,
                    )
                    .await;
            });
        if result.is_err() {
            let entry = state.entries.get_mut(&id).expect("install exists");
            let mut waiting = entry.status.clone();
            waiting.view_model.phase_id = "queued".into();
            waiting.view_model.label = "Waiting for an install slot".into();
            waiting.allowed_actions = vec![InstallActionViewModel::enabled(
                "remove_from_queue",
                "Remove from queue",
            )];
            if persist_status(&self.inner.storage, &waiting, "queued").is_ok() {
                entry.status = waiting;
                entry.started_at = None;
                state.active = None;
                state.queued.push_front(id);
            } else {
                entry.status.view_model = settlement_progress();
                entry.status.allowed_actions.clear();
                entry.recovery_lease = Some(rejected_retention.1);
                entry.retained = Some(
                    if matches!(entry.request, InstallQueueRequest::Content { .. }) {
                        RetainedInstall::Content(Some((
                            InstallOutcome::Failed,
                            Some(InstallError::Storage),
                        )))
                    } else {
                        RetainedInstall::Retry
                    },
                );
            }
        } else if let Ok(handle) = result {
            let queue = self.clone();
            // This waiter owns no effects. Every new worker is accepted by the
            // task owner only after the previous one has fully joined.
            observers.tasks.spawn(async move {
                queue.join_worker(&id, handle).await;
            });
        }
        publish(&self.inner, &mut state);
    }

    async fn join_worker(&self, id: &str, handle: TaskHandle<()>) {
        if handle.join().await.is_ok() {
            if self.status(id).is_ok_and(|status| status.done) {
                self.invalidate_registry();
            }
            self.resume_queued();
        }
    }

    async fn run(
        &self,
        id: String,
        pin: GenerationPin,
        lease: ExclusionLease,
        operation: axial_minecraft::managed_path::ManagedLibraryOperation,
        request: InstallQueueRequest,
        cancel: CancellationToken,
        owner_cancel: CancellationToken,
    ) {
        let content_request = matches!(request, InstallQueueRequest::Content { .. });
        if cancel.is_cancelled() || owner_cancel.is_cancelled() {
            if !self.complete(&id, InstallOutcome::Cancelled, None) {
                self.retain_work(
                    &id,
                    lease,
                    if content_request {
                        RetainedInstall::Content(Some((InstallOutcome::Cancelled, None)))
                    } else {
                        RetainedInstall::Retry
                    },
                );
            }
            return;
        }
        self.progress(
            &id,
            DownloadProgress {
                phase: if content_request {
                    "content_planning"
                } else {
                    "version_json"
                }
                .into(),
                current: 0,
                total: 1,
                file: None,
                error: None,
                done: false,
                bytes_done: None,
                bytes_total: None,
            },
        );
        let queue = self.clone();
        let progress_id = id.clone();
        let progress = move |event| queue.progress(&progress_id, event);
        let result = match request {
            InstallQueueRequest::Vanilla { version_id } => {
                #[cfg(feature = "test-support")]
                let installed = super::vanilla::install_with_test_endpoints(
                    &operation,
                    self.inner.runtime.clone(),
                    &version_id,
                    self.test_endpoints.clone(),
                    progress,
                )
                .await;
                #[cfg(not(feature = "test-support"))]
                let installed = super::vanilla::install(
                    &operation,
                    self.inner.runtime.clone(),
                    &version_id,
                    progress,
                )
                .await;
                match installed {
                    Ok(receipt) => {
                        self.settle_receipt(&id, &pin, operation.clone(), receipt)
                            .await
                    }
                    Err(DownloadError::PublicationIndeterminate(recovery)) => Err(
                        WorkFailure::Unsettled(RetainedInstall::Publication(recovery)),
                    ),
                    Err(error) => {
                        let diagnostic = download_failure_diagnostic(&error);
                        tracing::warn!(
                            install_id = %id,
                            category = diagnostic.category,
                            file_failure_class = ?diagnostic.file_failure_class,
                            io_kind = ?diagnostic.io_kind,
                            raw_os_error = diagnostic.raw_os_error,
                            http_status = diagnostic.http_status,
                            is_timeout = diagnostic.is_timeout,
                            is_connect = diagnostic.is_connect,
                            is_decode = diagnostic.is_decode,
                            runtime_source_kind = diagnostic.runtime_source_kind,
                            "Vanilla installer failed; classifying publication state"
                        );
                        self.classify_failure(&id, &operation, &version_id, error)
                            .await
                    }
                }
            }
            InstallQueueRequest::Loader {
                component_id,
                build_id,
            } => match resolve_loader(component_id, &build_id).await {
                Err(error) => Err(WorkFailure::Failed(error)),
                Ok(record) => {
                    let version_id = record.version_id.clone();
                    match axial_minecraft::loaders::install_build(
                        &operation,
                        self.inner.runtime.clone(),
                        record,
                        progress,
                    )
                    .await
                    {
                        Ok(outcome) => {
                            self.settle_loader(&id, &pin, operation.clone(), &version_id, outcome)
                                .await
                        }
                        Err(LoaderInstallError::PublicationIndeterminate(recovery)) => Err(
                            WorkFailure::Unsettled(RetainedInstall::LoaderPublication(recovery)),
                        ),
                        Err(error) => {
                            log_loader_failure(&error);
                            self.classify_failure(&id, &operation, &version_id, error)
                                .await
                        }
                    }
                }
            },
            InstallQueueRequest::Content {
                instance_id,
                action,
                ..
            } => {
                let result = self
                    .run_content(&id, &instance_id, action, &owner_cancel)
                    .await;
                return self.finish_content(&id, lease, result);
            }
        };
        let completed = match result {
            Ok(()) => self.complete(&id, InstallOutcome::Succeeded, None),
            Err(WorkFailure::Failed(error)) => {
                self.complete(&id, InstallOutcome::Failed, Some(error))
            }
            Err(WorkFailure::Unsettled(recovery)) => {
                self.retain_work(&id, lease, recovery);
                return;
            }
        };
        if !completed {
            self.retain_work(&id, lease, RetainedInstall::Retry);
        }
    }

    async fn run_content(
        &self,
        id: &str,
        instance_id: &str,
        action: InstallQueueContentActionRequest,
        cancel: &CancellationToken,
    ) -> Result<InstallOutcome, WorkFailure> {
        let (service, mutations) = self
            .content
            .as_ref()
            .ok_or(WorkFailure::Failed(InstallError::ContentUnavailable))?;
        let instance_id: InstanceId = instance_id
            .parse()
            .map_err(|_| WorkFailure::Failed(InstallError::InvalidRequest))?;
        let (setup, version_id) = {
            let state = self.inner.state.lock().expect("install queue lock");
            let entry = state
                .entries
                .get(id)
                .ok_or(WorkFailure::Failed(InstallError::NotFound))?;
            (entry.setup.clone(), entry.item.version_id.clone())
        };
        let record = mutations
            .directories()
            .registry()
            .get_live(&instance_id)
            .map_err(|_| WorkFailure::Failed(InstallError::NotFound))?;
        if record.instance.version_id != version_id {
            return Err(WorkFailure::Failed(InstallError::ContentConflict));
        }
        let progress = self.content_progress(id);
        if let Some(work) = setup {
            self.ready_version(
                work.instance().generation(),
                &work.instance().record().instance.version_id,
            )
            .await
            .map_err(|_| WorkFailure::Failed(InstallError::NotReady))?;
            return match work.execute(cancel, progress).await {
                Ok(()) => Ok(InstallOutcome::Succeeded),
                Err(crate::instances::model::InstanceError::Cancelled) => {
                    Ok(InstallOutcome::Cancelled)
                }
                Err(_)
                    if crate::content::install::has_pending(&self.inner.storage, &instance_id)
                        .unwrap_or(true) =>
                {
                    Err(WorkFailure::Unsettled(RetainedInstall::Content(None)))
                }
                Err(_) => Err(WorkFailure::Failed(InstallError::ContentConflict)),
            };
        }
        let mutations = mutations.as_ref().clone().with_progress(progress);
        let service = service.with_cancellation(cancel.clone());
        let result: Result<(), MutationError> = async {
            let task = match action {
                InstallQueueContentActionRequest::Install {
                    selections,
                    allow_incompatible,
                } => {
                    let admitted = mutations
                        .directories()
                        .admit(&instance_id)
                        .map_err(|_| MutationError::Unavailable)?;
                    if admitted.record().instance.version_id != version_id {
                        return Err(MutationError::Changed);
                    }
                    let plan = mutations
                        .plan_admitted(&service, &admitted, &selections)
                        .await?;
                    mutations.install_admitted(admitted, plan, allow_incompatible)?
                }
                InstallQueueContentActionRequest::Uninstall { canonical_ids } => mutations
                    .remove_many(
                        &instance_id,
                        &canonical_ids
                            .into_iter()
                            .map(CanonicalId)
                            .collect::<Vec<_>>(),
                    )?,
                InstallQueueContentActionRequest::Modpack {
                    canonical_id,
                    version_id,
                    selected_file_ids,
                    include_overrides,
                } => {
                    let canonical_id = CanonicalId(canonical_id);
                    if selected_file_ids.is_empty() {
                        let pack = mutations
                            .resolve_pack(&service, &canonical_id, Some(&version_id))
                            .await?;
                        mutations.install_pack(&service, &instance_id, pack, include_overrides)?
                    } else {
                        mutations
                            .install_selected_pack(
                                &service,
                                &instance_id,
                                &canonical_id,
                                Some(&version_id),
                                &selected_file_ids,
                            )
                            .await?
                    }
                }
            };
            task.join().await.map_err(|_| MutationError::Pending)??;
            Ok(())
        }
        .await;
        self.content_outcome(&instance_id, result, cancel)
    }

    fn content_progress(&self, id: &str) -> Arc<dyn Fn(DownloadProgress) + Send + Sync> {
        let queue = self.clone();
        let id = id.to_owned();
        Arc::new(move |event| queue.progress(&id, event))
    }

    fn content_outcome(
        &self,
        instance_id: &InstanceId,
        result: Result<(), MutationError>,
        cancel: &CancellationToken,
    ) -> Result<InstallOutcome, WorkFailure> {
        match result {
            Ok(()) => Ok(InstallOutcome::Succeeded),
            Err(MutationError::Pending) => {
                Err(WorkFailure::Unsettled(RetainedInstall::Content(None)))
            }
            Err(_)
                if crate::content::install::has_pending(&self.inner.storage, instance_id)
                    .unwrap_or(true) =>
            {
                Err(WorkFailure::Unsettled(RetainedInstall::Content(None)))
            }
            Err(MutationError::Cancelled) => Ok(InstallOutcome::Cancelled),
            Err(_) if cancel.is_cancelled() => Ok(InstallOutcome::Cancelled),
            Err(MutationError::Changed | MutationError::Conflict | MutationError::Required) => {
                Err(WorkFailure::Failed(InstallError::ContentConflict))
            }
            Err(_) => Err(WorkFailure::Failed(InstallError::Failed)),
        }
    }

    fn finish_content(
        &self,
        id: &str,
        lease: ExclusionLease,
        result: Result<InstallOutcome, WorkFailure>,
    ) {
        let (outcome, error) = match result {
            Ok(outcome) => (outcome, None),
            Err(WorkFailure::Failed(error)) => (InstallOutcome::Failed, Some(error)),
            Err(WorkFailure::Unsettled(retained)) => {
                self.retain_work(id, lease, retained);
                return;
            }
        };
        if !self.complete(id, outcome, error) {
            self.retain_work(id, lease, RetainedInstall::Content(Some((outcome, error))));
        }
    }

    async fn recover_content(&self, id: &str) -> Result<(), InstallError> {
        let (_, mutations) = self
            .content
            .as_ref()
            .ok_or(InstallError::ContentUnavailable)?;
        let (pin, lease, request, setup, settled, retained) = {
            let mut state = self.inner.state.lock().expect("install queue lock");
            let entry = state.entries.get_mut(id).ok_or(InstallError::NotFound)?;
            if entry.recovery_running {
                return Err(InstallError::Busy);
            }
            let pin = entry.pin.clone().ok_or(InstallError::LibraryUnavailable)?;
            let lease = entry
                .recovery_lease
                .clone()
                .ok_or(InstallError::SettlementRequired)?;
            let settled = match entry.retained {
                Some(RetainedInstall::Content(outcome)) => outcome,
                _ => None,
            };
            (
                pin,
                lease,
                entry.request.clone(),
                entry.setup.clone(),
                settled,
                entry.retained.is_some(),
            )
        };
        let InstallQueueRequest::Content {
            instance_id,
            action,
            ..
        } = request
        else {
            return Err(InstallError::InvalidRequest);
        };
        let instance: InstanceId = instance_id
            .parse()
            .map_err(|_| InstallError::InvalidRequest)?;
        if setup.is_none() && crate::instances::setup::has_pending(&self.inner.storage, &instance)?
        {
            return Err(InstallError::SettlementRequired);
        }
        {
            let mut state = self.inner.state.lock().expect("install queue lock");
            let entry = state.entries.get_mut(id).ok_or(InstallError::NotFound)?;
            if entry.recovery_running {
                return Err(InstallError::Busy);
            }
            let setup_changed =
                entry.setup.is_some() && entry.status.view_model.phase_id == "settlement_required";
            entry.recovery_running = true;
            entry.status.view_model = InstallProgressViewModel::starting();
            entry.status.revision = entry.status.revision.saturating_add(1);
            if setup_changed {
                state.registry_revision = state.registry_revision.saturating_add(1);
            }
            publish(&self.inner, &mut state);
        }
        let mutations = mutations
            .as_ref()
            .clone()
            .with_progress(self.content_progress(id));
        let queue = self.clone();
        let worker_id = id.to_owned();
        let receiver = (|| -> Result<_, InstallError> {
            let mut observers = self.observers()?;
            let task = self
                .inner
                .owner
                .try_spawn(
                    (pin, lease.clone(), setup.clone()),
                    move |cancel| async move {
                        if let Some((outcome, error)) = settled {
                            queue.finish_content(
                                &worker_id,
                                lease,
                                match error {
                                    Some(error) => Err(WorkFailure::Failed(error)),
                                    None => Ok(outcome),
                                },
                            );
                            return;
                        }
                        let pending =
                            crate::content::install::has_pending(&queue.inner.storage, &instance)
                                .unwrap_or(true);
                        let result = if pending {
                            let result = match mutations.resume(&instance) {
                                Ok(task) => task
                                    .join()
                                    .await
                                    .map_err(|_| MutationError::Pending)
                                    .and_then(|result| result)
                                    .map(|_| ()),
                                Err(error) => Err(error),
                            };
                            let result = queue.content_outcome(&instance, result, &cancel);
                            if matches!(result, Ok(InstallOutcome::Succeeded)) && setup.is_some() {
                                queue
                                    .run_content(&worker_id, &instance_id, action, &cancel)
                                    .await
                            } else {
                                result
                            }
                        } else if setup.is_some() {
                            queue
                                .run_content(&worker_id, &instance_id, action, &cancel)
                                .await
                        } else if retained {
                            Err(WorkFailure::Unsettled(RetainedInstall::Content(None)))
                        } else {
                            Err(WorkFailure::Failed(InstallError::ContentInterrupted))
                        };
                        queue.finish_content(&worker_id, lease, result);
                    },
                )
                .map_err(|_| InstallError::Closed)?;
            let queue = self.clone();
            let id = id.to_owned();
            let (sender, receiver) = tokio::sync::oneshot::channel();
            observers.tasks.spawn(async move {
                let joined = task.join().await.is_ok();
                let settled = joined && queue.status(&id).is_ok_and(|status| status.done);
                queue.recovery_finished(&id, settled);
                drop(queue);
                let _ = sender.send(if settled {
                    Ok(())
                } else {
                    Err(InstallError::SettlementRequired)
                });
            });
            Ok(receiver)
        })();
        match receiver {
            Ok(receiver) => receiver
                .await
                .map_err(|_| InstallError::SettlementRequired)?,
            Err(error) => {
                self.recovery_finished(id, false);
                Err(error)
            }
        }
    }

    async fn classify_failure<E: Send + 'static>(
        &self,
        id: &str,
        operation: &axial_minecraft::managed_path::ManagedLibraryOperation,
        version_id: &str,
        _error: E,
    ) -> Result<(), WorkFailure> {
        match axial_minecraft::classify_managed_install_publication(
            operation.clone(),
            version_id.to_owned(),
        )
        .await
        {
            axial_minecraft::ManagedInstallDurableOutcome::NoEffect => {
                Err(WorkFailure::Failed(InstallError::Failed))
            }
            ManagedInstallDurableOutcome::RolledBack { evidence, .. } => {
                let checkpoint = PublicationCheckpoint {
                    kind: CheckpointKind::RolledBack,
                    version_id: version_id.to_owned(),
                    evidence_id: evidence.id().as_str().to_owned(),
                    contract_id: None,
                };
                self.persist_checkpoint(id, &checkpoint)?;
                match evidence.acknowledge().await {
                    ManagedInstallAcknowledgementOutcome::Acknowledged => {
                        Err(WorkFailure::Failed(InstallError::Failed))
                    }
                    ManagedInstallAcknowledgementOutcome::Indeterminate(recovery) => {
                        Err(WorkFailure::Unsettled(RetainedInstall::Acknowledgement {
                            recovery,
                            continuation: None,
                        }))
                    }
                }
            }
            outcome => Err(WorkFailure::Unsettled(RetainedInstall::Outcome(outcome))),
        }
    }

    async fn settle_receipt(
        &self,
        id: &str,
        pin: &GenerationPin,
        operation: axial_minecraft::managed_path::ManagedLibraryOperation,
        receipt: axial_minecraft::KnownGoodInstallReceipt,
    ) -> Result<(), WorkFailure> {
        let version_id = receipt.version_id().to_owned();
        match artifacts::inspect_install_receipt(operation, receipt).await {
            InstallReceiptState::AwaitingActivation { verified, evidence } => {
                let checkpoint = PublicationCheckpoint {
                    kind: CheckpointKind::Final,
                    version_id,
                    evidence_id: evidence,
                    contract_id: Some(verified.activation_contract_id().as_str().to_owned()),
                };
                let storage = self.inner.storage.clone();
                let install_id = id.to_owned();
                let activation_pin = pin.clone();
                let activation_checkpoint = checkpoint.clone();
                let ack = verified
                    .activate_with(move |source| {
                        persist_activation(
                            storage,
                            activation_pin,
                            install_id,
                            Arc::new(source),
                            activation_checkpoint,
                        )
                    })
                    .await
                    .map_err(|_| InstallError::Storage)?;
                match ack.acknowledge().await {
                    ManagedInstallAcknowledgementOutcome::Acknowledged => {
                        self.mark_ready(pin, id, &checkpoint)
                    }
                    ManagedInstallAcknowledgementOutcome::Indeterminate(recovery) => {
                        Err(WorkFailure::Unsettled(RetainedInstall::Acknowledgement {
                            recovery,
                            continuation: None,
                        }))
                    }
                }
            }
            state => Err(unsettled_receipt(state)),
        }
    }

    async fn settle_loader(
        &self,
        id: &str,
        pin: &GenerationPin,
        operation: axial_minecraft::managed_path::ManagedLibraryOperation,
        version_id: &str,
        outcome: LoaderInstallPublicationOutcome,
    ) -> Result<(), WorkFailure> {
        match outcome {
            LoaderInstallPublicationOutcome::ChildCommitted(receipt) => {
                self.settle_receipt(id, pin, operation, receipt).await
            }
            LoaderInstallPublicationOutcome::BaseCommitted(commit) => {
                let base_version = commit.base_version_id().to_owned();
                let (verified, evidence) =
                    match artifacts::inspect_loader_base_commit(operation.clone(), commit).await {
                        LoaderBaseState::AwaitingActivation { verified, evidence } => {
                            (verified, evidence)
                        }
                        state => return Err(unsettled_base(state)),
                    };
                let checkpoint = PublicationCheckpoint {
                    kind: CheckpointKind::Base,
                    version_id: base_version,
                    evidence_id: evidence,
                    contract_id: Some(verified.activation_contract_id().as_str().to_owned()),
                };
                let storage = self.inner.storage.clone();
                let install_id = id.to_owned();
                let activation_pin = pin.clone();
                let activation_checkpoint = checkpoint.clone();
                let (continuation, ack) = verified
                    .activate_with(move |source| {
                        persist_activation(
                            storage,
                            activation_pin,
                            install_id,
                            Arc::new(source),
                            activation_checkpoint,
                        )
                    })
                    .await
                    .map_err(base_activation_error)?;
                match ack.acknowledge().await {
                    ManagedInstallAcknowledgementOutcome::Acknowledged => {
                        self.mark_ready(pin, id, &checkpoint)?;
                    }
                    ManagedInstallAcknowledgementOutcome::Indeterminate(recovery) => {
                        return Err(WorkFailure::Unsettled(RetainedInstall::Acknowledgement {
                            recovery,
                            continuation: Some(continuation),
                        }));
                    }
                }
                let queue = self.clone();
                let progress_id = id.to_owned();
                match super::processors::continue_loader_install(
                    &operation,
                    continuation,
                    move |event| queue.progress(&progress_id, event),
                )
                .await
                {
                    Ok(receipt) => self.settle_receipt(id, pin, operation, receipt).await,
                    Err(LoaderInstallError::PublicationIndeterminate(recovery)) => Err(
                        WorkFailure::Unsettled(RetainedInstall::LoaderPublication(recovery)),
                    ),
                    Err(error) => {
                        log_loader_failure(&error);
                        self.classify_failure(id, &operation, version_id, error)
                            .await
                    }
                }
            }
        }
    }

    fn mark_ready(
        &self,
        pin: &GenerationPin,
        id: &str,
        checkpoint: &PublicationCheckpoint,
    ) -> Result<(), WorkFailure> {
        self.inner.storage.transaction(|db| {
            let count = db.execute("UPDATE installed_versions SET state='ready' WHERE library_id=?1 AND version_id=?2 AND install_id=?3 AND contract_id=?4", params![pin.library_id().to_string(), checkpoint.version_id, id, checkpoint.contract_id])?;
            if count != 1 { return Err(InstallError::Storage); }
            Ok::<_, InstallError>(())
        }).map_err(WorkFailure::from)
    }

    fn progress(&self, id: &str, mut event: DownloadProgress) {
        if event.done {
            return;
        }
        // Provider paths, filenames and errors are not public status text.
        event.file = None;
        event.error = None;
        let mut state = self.inner.state.lock().expect("install queue lock");
        let Some(entry) = state.entries.get_mut(id) else {
            return;
        };
        if entry.status.done || entry.status.view_model.phase_id == "settlement_required" {
            return;
        }
        entry.status.allowed_actions.clear();
        entry.status.view_model = progress_view(&event);
        if let Some(previous) = entry
            .status
            .progress
            .iter_mut()
            .find(|previous| previous.phase == event.phase)
        {
            *previous = event;
        } else if entry.status.progress.len() < 32 {
            entry.status.progress.push(event);
        }
        entry.status.revision = entry.status.revision.saturating_add(1);
        publish(&self.inner, &mut state);
    }

    fn complete(&self, id: &str, outcome: InstallOutcome, error: Option<InstallError>) -> bool {
        let mut state = self.inner.state.lock().expect("install queue lock");
        let Some(entry) = state.entries.get_mut(id) else {
            return false;
        };
        if entry.status.done {
            return true;
        }
        let mut status = entry.status.clone();
        finish_status(&mut status, outcome, error);
        if persist_status(&self.inner.storage, &status, "terminal").is_err() {
            entry.status.view_model = settlement_progress();
            publish(&self.inner, &mut state);
            return false;
        }
        entry.status = status;
        entry.pin = None;
        entry.recovery_lease = None;
        entry.retained = None;
        entry.setup = None;
        if let Some(failure) = &entry.status.failure_view_model {
            state.latest_failure = Some(InstallQueueFailureViewModel {
                failed_at_ms: now_ms(),
                queue_id: id.into(),
                install_id: id.into(),
                operation_id: entry.status.operation_id.clone(),
                label: item_label(&entry.item),
                install_item: entry.item.clone(),
                failure_view_model: failure.clone(),
            });
        }
        state.active = None;
        state.registry_revision = state.registry_revision.saturating_add(1);
        publish(&self.inner, &mut state);
        drop(state);
        if outcome == InstallOutcome::Failed {
            if let Some(telemetry) = &self.telemetry {
                telemetry.emit(TelemetryEvent::ErrorCaptured {
                    kind: TelemetryErrorKind::InstallFailed,
                });
            }
        }
        true
    }

    fn retain_work(&self, id: &str, lease: ExclusionLease, retained: RetainedInstall) {
        if let Some(entry) = self
            .inner
            .state
            .lock()
            .expect("install queue lock")
            .entries
            .get_mut(id)
        {
            entry.recovery_lease = Some(lease);
        }
        self.retain_unsettled(id, retained);
    }

    fn retain_unsettled(&self, id: &str, retained: RetainedInstall) {
        let retained_kind = match &retained {
            RetainedInstall::Retry => "retry",
            RetainedInstall::Content(_) => "content",
            RetainedInstall::Publication(_) => "publication",
            RetainedInstall::LoaderPublication(_) => "loader_publication",
            RetainedInstall::Acknowledgement { .. } => "acknowledgement",
            RetainedInstall::Outcome(outcome) => match outcome {
                ManagedInstallDurableOutcome::NoEffect => "durable_no_effect",
                ManagedInstallDurableOutcome::Mismatch => "durable_mismatch",
                ManagedInstallDurableOutcome::Committed(_) => "durable_committed",
                ManagedInstallDurableOutcome::RolledBack { .. } => "durable_rolled_back",
                ManagedInstallDurableOutcome::Indeterminate(_) => "durable_indeterminate",
            },
        };
        tracing::warn!(
            retained_kind,
            "Installation remains retained for settlement."
        );
        let mut state = self.inner.state.lock().expect("install queue lock");
        let mut setup_changed = false;
        if let Some(entry) = state.entries.get_mut(id) {
            setup_changed =
                entry.setup.is_some() && entry.status.view_model.phase_id != "settlement_required";
            entry.status.view_model = settlement_progress();
            entry.status.allowed_actions.clear();
            entry.status.revision = entry.status.revision.saturating_add(1);
            entry.retained = Some(retained);
            let _ = persist_status(&self.inner.storage, &entry.status, "settlement_required");
        }
        if setup_changed {
            state.registry_revision = state.registry_revision.saturating_add(1);
        }
        publish(&self.inner, &mut state);
    }
}

fn preserved_content_instance(state: &State) -> Result<Option<InstanceId>, InstallError> {
    let Some(active) = state.active.as_ref() else {
        return Ok(None);
    };
    let entry = state
        .entries
        .get(active)
        .ok_or(InstallError::SettlementRequired)?;
    if entry.status.done
        || entry.status.outcome.is_some()
        || entry.status.view_model.phase_id != "settlement_required"
        || !matches!(entry.retained, None | Some(RetainedInstall::Content(None)))
        || state.entries.iter().any(|(id, entry)| {
            entry.recovery_running
                || (id != active && (entry.retained.is_some() || entry.recovery_lease.is_some()))
        })
    {
        return Err(InstallError::SettlementRequired);
    }
    match &entry.request {
        InstallQueueRequest::Content { instance_id, .. } => instance_id
            .parse()
            .map(Some)
            .map_err(|_| InstallError::InvalidRequest),
        _ => Err(InstallError::SettlementRequired),
    }
}

enum WorkFailure {
    Failed(InstallError),
    Unsettled(RetainedInstall),
}

#[derive(Debug)]
struct DownloadFailureDiagnostic {
    category: &'static str,
    file_failure_class: Option<axial_minecraft::download::DownloadFileFailureClass>,
    io_kind: Option<std::io::ErrorKind>,
    raw_os_error: Option<i32>,
    http_status: Option<u16>,
    is_timeout: Option<bool>,
    is_connect: Option<bool>,
    is_decode: Option<bool>,
    runtime_source_kind: Option<&'static str>,
}

fn observer_finished(result: Result<(), tokio::task::JoinError>) {
    if let Err(error) = result {
        tracing::warn!(
            panicked = error.is_panic(),
            cancelled = error.is_cancelled(),
            "Install queue observer did not finish normally."
        );
    }
}

fn log_loader_failure(error: &LoaderInstallError) {
    match error {
        LoaderInstallError::Active(failure) => {
            let source = failure.source();
            let (io_kind, raw_os_error) = match source {
                axial_minecraft::loaders::LoaderError::Io(error) => {
                    (Some(error.kind()), error.raw_os_error())
                }
                _ => (None, None),
            };
            tracing::warn!(category = failure.kind().as_str(),
                provider_kind = ?source.provider_failure_kind(),
                http_status = source.provider_status(), ?io_kind, raw_os_error,
                "Loader installer failed; classifying publication state.");
        }
        LoaderInstallError::BaseInstallFailed(failure) => {
            let diagnostic = download_failure_diagnostic(failure.error());
            tracing::warn!(
                ?diagnostic,
                "Loader base installation failed; classifying publication state."
            );
        }
        LoaderInstallError::ArtifactDownloadFailed(failure) => {
            let recent_fact_kinds = failure
                .facts()
                .iter()
                .rev()
                .take(8)
                .map(|fact| fact.kind)
                .collect::<Vec<_>>();
            tracing::warn!(
                category = "artifact_download",
                ?recent_fact_kinds,
                "Loader installer failed; classifying publication state."
            );
        }
        LoaderInstallError::PublicationIndeterminate(_) => {
            tracing::warn!(
                category = "publication_indeterminate",
                "Loader publication remains retained."
            );
        }
    }
}

fn download_failure_diagnostic(error: &DownloadError) -> DownloadFailureDiagnostic {
    let mut diagnostic = DownloadFailureDiagnostic {
        category: match error {
            DownloadError::FileOperation(_) => "file_operation",
            DownloadError::ResolveManifest(_) => "resolve_manifest",
            DownloadError::Request(_) => "request",
            DownloadError::ParseVersion(_) => "parse_version",
            DownloadError::PrepareRuntime(_) => "prepare_runtime",
            DownloadError::RuntimeSource(_) => "runtime_source",
            DownloadError::RuntimeUnavailableForPlatform { .. } => "runtime_unavailable",
            DownloadError::RuntimeRosettaRequired { .. } => "runtime_rosetta_required",
            DownloadError::Integrity(_) => "integrity",
            DownloadError::PublicationIndeterminate(_) => "publication_indeterminate",
            DownloadError::LibraryPlan(_) => "library_plan",
        },
        file_failure_class: error.file_failure_class(),
        io_kind: None,
        raw_os_error: None,
        http_status: None,
        is_timeout: None,
        is_connect: None,
        is_decode: None,
        runtime_source_kind: None,
    };
    match error {
        DownloadError::FileOperation(error) => {
            diagnostic.io_kind = Some(error.kind());
            diagnostic.raw_os_error = error.raw_os_error();
        }
        DownloadError::Request(error) => {
            diagnostic.http_status = error.status().map(|status| status.as_u16());
            diagnostic.is_timeout = Some(error.is_timeout());
            diagnostic.is_connect = Some(error.is_connect());
            diagnostic.is_decode = Some(error.is_decode());
        }
        DownloadError::RuntimeSource(error) => {
            diagnostic.runtime_source_kind = Some(error.kind().as_str());
        }
        _ => {}
    }
    diagnostic
}

impl From<InstallError> for WorkFailure {
    fn from(error: InstallError) -> Self {
        tracing::warn!(?error, "Installation settlement requires retry.");
        Self::Unsettled(RetainedInstall::Retry)
    }
}

fn base_activation_error(
    error: axial_minecraft::loaders::LoaderInstallBaseActivationError,
) -> InstallError {
    let stage = match error {
        axial_minecraft::loaders::LoaderInstallBaseActivationError::Authority(_) => "authority",
        axial_minecraft::loaders::LoaderInstallBaseActivationError::Activation(_) => "activation",
    };
    tracing::warn!(stage, "Loader base activation rejected.");
    InstallError::Storage
}

enum RecoveryAction {
    Complete,
    Requeue,
    Continue(LoaderInstallBaseContinuation),
}

impl PublicationCheckpoint {
    fn verify(
        &self,
        operation: &axial_minecraft::managed_path::ManagedLibraryOperation,
        item: &InstallQueueInstallItemViewModel,
    ) -> Result<(), InstallError> {
        let evidence =
            axial_minecraft::ManagedInstallPublicationEvidenceId::parse(&self.evidence_id)
                .map_err(|_| InstallError::SettlementRequired)?;
        let is_target = self.version_id == item.version_id;
        let is_base = item
            .loader
            .as_ref()
            .is_some_and(|loader| loader.minecraft_version == self.version_id);
        if !evidence.matches_version_id(&self.version_id)
            || !axial_minecraft::verify_managed_install_publication_evidence_root(
                operation, &evidence,
            )
            || !match self.kind {
                CheckpointKind::Final => is_target,
                CheckpointKind::Base => is_base,
                CheckpointKind::RolledBack => is_target || is_base,
            }
            || match self.kind {
                CheckpointKind::RolledBack => self.contract_id.is_some(),
                _ => self.contract_id.as_deref().is_none_or(|contract| {
                    axial_minecraft::ManagedInstallActivationContractId::parse(contract).is_err()
                }),
            }
        {
            return Err(InstallError::SettlementRequired);
        }
        Ok(())
    }
}

fn unsettled_receipt(state: InstallReceiptState) -> WorkFailure {
    let outcome = match state {
        InstallReceiptState::NoEffect(_) => ManagedInstallDurableOutcome::NoEffect,
        InstallReceiptState::Mismatch(_) => ManagedInstallDurableOutcome::Mismatch,
        InstallReceiptState::ReceiptMismatch(failure) => {
            let (evidence, _) = failure.into_parts();
            ManagedInstallDurableOutcome::Committed(evidence)
        }
        InstallReceiptState::RolledBack {
            evidence, effect, ..
        } => ManagedInstallDurableOutcome::RolledBack { evidence, effect },
        InstallReceiptState::Indeterminate { recovery, .. } => {
            ManagedInstallDurableOutcome::Indeterminate(recovery)
        }
        InstallReceiptState::AwaitingActivation { .. } => {
            unreachable!("verified receipt is activated by the caller")
        }
    };
    WorkFailure::Unsettled(RetainedInstall::Outcome(outcome))
}

fn unsettled_base(state: LoaderBaseState) -> WorkFailure {
    let outcome = match state {
        LoaderBaseState::NoEffect(_) => ManagedInstallDurableOutcome::NoEffect,
        LoaderBaseState::Mismatch(_) => ManagedInstallDurableOutcome::Mismatch,
        LoaderBaseState::ReceiptMismatch(failure) => {
            let (evidence, _) = failure.into_parts();
            ManagedInstallDurableOutcome::Committed(evidence)
        }
        LoaderBaseState::RolledBack {
            evidence, effect, ..
        } => ManagedInstallDurableOutcome::RolledBack { evidence, effect },
        LoaderBaseState::Indeterminate { recovery, .. } => {
            ManagedInstallDurableOutcome::Indeterminate(recovery)
        }
        LoaderBaseState::AwaitingActivation { .. } => {
            unreachable!("verified loader base is activated by the caller")
        }
    };
    WorkFailure::Unsettled(RetainedInstall::Outcome(outcome))
}

async fn persist_activation(
    storage: Arc<MetadataStore>,
    pin: GenerationPin,
    install_id: String,
    source: Arc<KnownGoodActivationSource>,
    checkpoint: PublicationCheckpoint,
) -> Result<(), KnownGoodActivationRejected> {
    let activated = ActivatedVersion::from_source(&source);
    tokio::task::spawn_blocking(move || {
        activated.clone().verify(pin.clone()).map_err(|error| {
            tracing::warn!(stage = "inventory", ?error, "Installation activation rejected.");
            error
        })?;
        if activated.version_id != checkpoint.version_id || Some(&activated.contract_id) != checkpoint.contract_id.as_ref() {
            tracing::warn!(stage = "contract", "Installation activation rejected.");
            return Err(InstallError::SettlementRequired);
        }
        let library_id = pin.library_id().to_string();
        let encoded = serde_json::to_string(&activated).map_err(|_| {
            tracing::warn!(stage = "inventory_encoding", "Installation activation rejected.");
            InstallError::Storage
        })?;
        let checkpoint = serde_json::to_string(&checkpoint).map_err(|_| {
            tracing::warn!(stage = "checkpoint_encoding", "Installation activation rejected.");
            InstallError::Storage
        })?;
        storage.transaction(|db| {
            let changed = db.execute("INSERT INTO installed_versions(library_id,version_id,contract_id,inventory_json,install_id,state) VALUES(?1,?2,?3,?4,?5,'activating') ON CONFLICT(library_id,version_id) DO UPDATE SET contract_id=excluded.contract_id,inventory_json=excluded.inventory_json,install_id=excluded.install_id,state='activating'",
                params![library_id, activated.version_id, activated.contract_id, encoded, install_id]).map_err(|error| {
                    tracing::warn!(stage = "activation_write", sqlite_code = ?error.sqlite_error_code(), "Installation activation rejected.");
                    InstallError::Storage
                })?;
            if changed != 1 {
                tracing::warn!(stage = "activation_write", changed, "Installation activation was not persisted.");
                return Err(InstallError::Storage);
            }
            let changed = db.execute("UPDATE install_queue SET checkpoint_json=?1 WHERE id=?2", params![checkpoint, install_id]).map_err(|error| {
                tracing::warn!(stage = "checkpoint_write", sqlite_code = ?error.sqlite_error_code(), "Installation activation rejected.");
                InstallError::Storage
            })?;
            if changed != 1 {
                tracing::warn!(stage = "checkpoint_write", changed, "Installation activation was not persisted.");
                return Err(InstallError::Storage);
            }
            Ok::<_, InstallError>(())
        }).map_err(|error| {
            tracing::warn!(stage = "metadata_transaction", ?error, "Installation activation rejected.");
            error
        })
    }).await.map_err(|_| {
        tracing::warn!(stage = "activation_task", "Installation activation interrupted.");
        KnownGoodActivationRejected
    })?.map_err(|_| KnownGoodActivationRejected)
}

async fn resolve_loader(
    component: LoaderComponentId,
    build: &str,
) -> Result<LoaderBuildRecord, InstallError> {
    axial_minecraft::loaders::resolve_build_record_for_install(component, build)
        .await
        .map_err(|_| InstallError::LoaderUnavailable)
}
fn item_for_loader(record: &LoaderBuildRecord) -> InstallQueueInstallItemViewModel {
    InstallQueueInstallItemViewModel {
        version_id: record.version_id.clone(),
        content: None,
        loader: Some(InstallQueueLoaderItemViewModel {
            component_id: serde_json::to_value(record.component_id)
                .ok()
                .and_then(|value| value.as_str().map(str::to_owned))
                .unwrap_or_default(),
            build_id: record.build_id.clone(),
            minecraft_version: record.minecraft_version.clone(),
            loader_version: record.loader_version.clone(),
        }),
    }
}
pub fn library_artifact(library_id: &str) -> ArtifactKey {
    ArtifactKey::new(library_id, "managed-game-artifacts")
}
fn install_artifacts(request: &InstallQueueRequest, library_id: &str) -> Vec<ArtifactKey> {
    match request {
        InstallQueueRequest::Content { .. } => Vec::new(),
        _ => vec![library_artifact(library_id)],
    }
}
fn validate_content_request(
    label: &str,
    action: &InstallQueueContentActionRequest,
) -> Result<(), InstallError> {
    fn text(value: &str, maximum: usize) -> bool {
        !value.trim().is_empty() && value.len() <= maximum && !value.chars().any(char::is_control)
    }
    fn canonical(value: &str) -> bool {
        value.strip_prefix("modrinth:").is_some_and(|id| {
            text(id, 256)
                && id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        })
    }
    if !text(label, 256) {
        return Err(InstallError::InvalidRequest);
    }
    let valid = match action {
        InstallQueueContentActionRequest::Install { selections, .. } => {
            !selections.is_empty()
                && selections.len() <= 256
                && selections.iter().all(|selection| {
                    selection.kind != ContentKind::Modpack
                        && canonical(&selection.canonical_id)
                        && selection.version_id.as_ref().is_none_or(|id| text(id, 256))
                })
                && selections
                    .iter()
                    .map(|selection| &selection.canonical_id)
                    .collect::<std::collections::BTreeSet<_>>()
                    .len()
                    == selections.len()
        }
        InstallQueueContentActionRequest::Uninstall { canonical_ids } => {
            !canonical_ids.is_empty()
                && canonical_ids.len() <= 256
                && canonical_ids.iter().all(|id| canonical(id))
                && canonical_ids
                    .iter()
                    .collect::<std::collections::BTreeSet<_>>()
                    .len()
                    == canonical_ids.len()
        }
        InstallQueueContentActionRequest::Modpack {
            canonical_id,
            version_id,
            selected_file_ids,
            include_overrides,
        } => {
            canonical(canonical_id)
                && text(version_id, 256)
                && selected_file_ids.len() <= 500
                && selected_file_ids.iter().all(|id| text(id, 2048))
                && selected_file_ids
                    .iter()
                    .collect::<std::collections::BTreeSet<_>>()
                    .len()
                    == selected_file_ids.len()
                && (selected_file_ids.is_empty() || !include_overrides)
        }
    };
    if valid {
        Ok(())
    } else {
        Err(InstallError::InvalidRequest)
    }
}
fn item_label(item: &InstallQueueInstallItemViewModel) -> String {
    if let Some(content) = &item.content {
        return content.label.clone();
    }
    if let Some(loader) = &item.loader {
        let Some(component) = LoaderComponentId::parse(&loader.component_id) else {
            return "Unknown loader".into();
        };
        let name = component.display_name();
        let version = loader.loader_version.trim();
        let label = if version.is_empty() {
            format!("{name} loader")
        } else {
            format!("{name} {version}")
        };
        let minecraft = loader.minecraft_version.trim();
        return if minecraft.is_empty() {
            label
        } else {
            format!("{label} for Minecraft {minecraft}")
        };
    }
    let version = item.version_id.trim();
    if version.is_empty() {
        "Minecraft".into()
    } else {
        format!("Minecraft {version}")
    }
}
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}
fn persist_status(
    storage: &MetadataStore,
    status: &InstallStatusResponse,
    phase: &str,
) -> Result<(), InstallError> {
    let encoded = serde_json::to_string(status).map_err(|_| InstallError::Storage)?;
    storage.transaction(|db| {
        db.execute(
            "UPDATE install_queue SET status_json=?1, phase=?2 WHERE id=?3",
            params![encoded, phase, status.install_id],
        )?;
        Ok::<_, InstallError>(())
    })
}
fn start_response(status: &InstallStatusResponse) -> InstallStartResponse {
    InstallStartResponse {
        install_id: status.install_id.clone(),
        operation_id: status.operation_id.clone(),
        view_model: status.view_model.clone(),
    }
}
fn progress_view(event: &DownloadProgress) -> InstallProgressViewModel {
    let label = match event.phase.as_str() {
        "version_json" => "Preparing Minecraft",
        "client" => "Downloading Minecraft",
        "libraries" => "Downloading libraries",
        "assets" => "Downloading assets",
        "runtime" | "java" => "Preparing Java",
        "processors" => "Preparing loader",
        "content_planning" => "Planning content changes",
        "content_download" => "Downloading content",
        "content_commit" => "Applying content changes",
        _ => "Installing game files",
    };
    let percent = if event.total > 0 {
        ((i64::from(event.current.max(0)) * 100) / i64::from(event.total)).clamp(0, 99) as u8
    } else {
        0
    };
    InstallProgressViewModel {
        phase_id: event.phase.clone(),
        label: label.into(),
        progress_pct: percent,
        terminal: false,
        failed: false,
        active_step: Some(InstallProgressStepViewModel {
            phase_id: event.phase.clone(),
            label: label.into(),
            progress_pct: percent,
            current: event.current,
            total: event.total,
        }),
    }
}
fn settlement_progress() -> InstallProgressViewModel {
    InstallProgressViewModel {
        phase_id: "settlement_required".into(),
        label: "Installation requires settlement".into(),
        progress_pct: 0,
        terminal: false,
        failed: false,
        active_step: None,
    }
}
fn finish_status(
    status: &mut InstallStatusResponse,
    outcome: InstallOutcome,
    error: Option<InstallError>,
) {
    status.done = true;
    status.outcome = Some(outcome);
    status.allowed_actions.clear();
    status.revision = status.revision.saturating_add(1);
    let (phase, label) = match outcome {
        InstallOutcome::Succeeded => ("done", "Installation complete"),
        InstallOutcome::Failed => ("error", "Installation failed"),
        InstallOutcome::Cancelled => ("cancelled", "Installation cancelled"),
        InstallOutcome::Removed => ("removed", "Removed from queue"),
    };
    status.view_model = InstallProgressViewModel {
        phase_id: phase.into(),
        label: label.into(),
        progress_pct: if outcome == InstallOutcome::Succeeded {
            100
        } else {
            0
        },
        terminal: true,
        failed: outcome == InstallOutcome::Failed,
        active_step: None,
    };
    if let Some(error) = error {
        status.failure_view_model = Some(InstallFailureViewModel {
            state_id: "failed".into(),
            title: "Installation failed".into(),
            tone: "danger".into(),
            summary: error.to_string(),
            detail: None,
            details: Vec::new(),
            retry_action: InstallActionViewModel::enabled("retry", "Retry"),
            dismiss_action: InstallActionViewModel::enabled("dismiss", "Dismiss"),
        });
    }
}
fn publish(inner: &Inner, state: &mut State) {
    state.revision = state.revision.saturating_add(1);
    inner.latest.send_replace(project(state));
}
fn project_with_started(
    state: &State,
    started_install: Option<InstallStartResponse>,
) -> InstallQueueStateResponse {
    let mut snapshot = project(state);
    snapshot.started_install = started_install;
    snapshot
}
fn project(state: &State) -> InstallQueueStateResponse {
    let active = state.active.as_ref().and_then(|id| {
        state
            .entries
            .get(id)
            .map(|entry| InstallQueueActiveViewModel {
                queue_id: id.clone(),
                install_id: Some(id.clone()),
                operation_id: Some(entry.status.operation_id.clone()),
                install_started_at_ms: entry.started_at,
                kind: kind(&entry.request).into(),
                title: item_label(&entry.item),
                label: item_label(&entry.item),
                summary: entry.status.view_model.label.clone(),
                install_item: entry.item.clone(),
                progress: entry.status.view_model.clone(),
                retry_action: (!state.closed && retained_retry_eligible(entry))
                    .then(|| InstallActionViewModel::enabled("retry", "Retry settlement")),
            })
    });
    let total = state.queued.len();
    let items = state
        .queued
        .iter()
        .enumerate()
        .filter_map(|(index, id)| {
            state
                .entries
                .get(id)
                .map(|entry| InstallQueuedItemViewModel {
                    queue_id: id.clone(),
                    state_id: "queued".into(),
                    kind: kind(&entry.request).into(),
                    title: item_label(&entry.item),
                    label: item_label(&entry.item),
                    summary: "Waiting to install".into(),
                    detail: String::new(),
                    position: index + 1,
                    total,
                    install_item: entry.item.clone(),
                    remove_action: InstallActionViewModel::enabled(
                        "remove_from_queue",
                        "Remove from queue",
                    ),
                })
        })
        .collect();
    InstallQueueStateResponse {
        queue_epoch: state.epoch.clone(),
        revision: state.revision,
        registry_revision: state.registry_revision,
        latest_failure: state.latest_failure.clone(),
        active,
        items,
        view_model: InstallQueueViewModel {
            state_id: if state.active.is_some() {
                "active"
            } else if total > 0 {
                "queued"
            } else {
                "empty"
            }
            .into(),
            status_label: if state.active.is_some() {
                "Installing"
            } else {
                "Install queue"
            }
            .into(),
            title: "Install queue".into(),
            summary: String::new(),
            queued_count: total,
            queued_count_label: format!("{total} queued"),
            queued_item_label: "Queued installs".into(),
            next_label: None,
            active_queued_count_label: None,
            section_title: "Up next".into(),
            empty_title: "No queued installs".into(),
            empty_summary: "Installations will appear here.".into(),
        },
        notice: None,
        started_install: None,
        removed_instance_id: None,
    }
}
fn kind(request: &InstallQueueRequest) -> &'static str {
    match request {
        InstallQueueRequest::Vanilla { .. } => "vanilla",
        InstallQueueRequest::Loader { .. } => "loader",
        InstallQueueRequest::Content { .. } => "content",
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::library::LibraryOpenOutcome;
    use serde_json::Value;

    #[test]
    fn download_failure_diagnostics_exclude_native_text() {
        use axial_minecraft::download::DownloadFileFailureClass;
        use axial_minecraft::runtime::{RuntimeId, RuntimeSourceFailure, RuntimeSourceFailureKind};

        let private = "https://private.invalid/path?token=secret /private/local/file";
        for (error, category) in [
            (
                DownloadError::ResolveManifest(private.into()),
                "resolve_manifest",
            ),
            (
                DownloadError::PrepareRuntime(private.into()),
                "prepare_runtime",
            ),
            (DownloadError::Integrity(private.into()), "integrity"),
            (
                DownloadError::FileOperation(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    private,
                )),
                "file_operation",
            ),
        ] {
            let diagnostic = download_failure_diagnostic(&error);
            assert_eq!(diagnostic.category, category);
            assert!(!format!("{diagnostic:?}").contains("private"));
            assert!(diagnostic.http_status.is_none());
        }
        let io = std::io::Error::from_raw_os_error(13);
        let kind = io.kind();
        let diagnostic = download_failure_diagnostic(&DownloadError::FileOperation(io));
        assert_eq!(diagnostic.raw_os_error, Some(13));
        assert_eq!(diagnostic.io_kind, Some(kind));
        let diagnostic = download_failure_diagnostic(&DownloadError::FileOperation(
            std::io::Error::from(std::io::ErrorKind::PermissionDenied),
        ));
        assert_eq!(
            diagnostic.file_failure_class,
            Some(DownloadFileFailureClass::PermissionDenied)
        );
        let diagnostic =
            download_failure_diagnostic(&DownloadError::RuntimeSource(RuntimeSourceFailure::new(
                RuntimeId("private-component".into()),
                RuntimeSourceFailureKind::PolicyRejected,
                private,
            )));
        assert_eq!(diagnostic.category, "runtime_source");
        assert_eq!(diagnostic.runtime_source_kind, Some("policy_rejected"));
        assert!(!format!("{diagnostic:?}").contains("private"));
    }

    #[tokio::test]
    async fn download_failure_diagnostics_keep_status_without_request_url() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            socket.read(&mut [0_u8; 2_048]).await.unwrap();
            socket.write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
        });
        let error = reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .get(format!("http://{address}/private-path?token=secret"))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap_err();
        server.await.unwrap();
        let diagnostic = download_failure_diagnostic(&DownloadError::Request(error));
        assert_eq!(diagnostic.category, "request");
        assert_eq!(diagnostic.http_status, Some(503));
        assert_eq!(diagnostic.is_timeout, Some(false));
        assert_eq!(diagnostic.is_connect, Some(false));
        assert_eq!(diagnostic.is_decode, Some(false));
        assert!(diagnostic.io_kind.is_none());
        let encoded = format!("{diagnostic:?}");
        for private in ["private-path", "secret", "http://", &address.to_string()] {
            assert!(!encoded.contains(private));
        }
    }

    fn fixture() -> (
        tempfile::TempDir,
        Arc<MetadataStore>,
        LibraryLifecycle,
        Exclusions,
        TaskOwner,
        InstallQueue,
    ) {
        let root =
            tempfile::tempdir_in(std::fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
        let library = match LibraryLifecycle::open(root.path()) {
            LibraryOpenOutcome::Ready(library) => library,
            _ => panic!("isolated library admission"),
        };
        let storage = Arc::new(MetadataStore::in_memory().unwrap());
        storage.migrate(&[MIGRATION]).unwrap();
        let exclusions = Exclusions::new();
        let owner = TaskOwner::new(4).unwrap();
        let runtime = ManagedRuntimeCache::isolated_for_test().unwrap();
        let queue = InstallQueue::new(
            storage.clone(),
            library.clone(),
            exclusions.clone(),
            owner.clone(),
            runtime,
        )
        .unwrap();
        (root, storage, library, exclusions, owner, queue)
    }

    #[tokio::test]
    async fn active_count_excludes_queued_and_terminal_and_counts_all_restored_content() {
        use crate::{
            instances::{
                create::{InstanceService, tests::create},
                directory::{InstanceDirectories, Registry},
            },
            network::{ClientConfig, ProviderClient},
        };

        let (_root, storage, library, exclusions, owner, queue) = fixture();
        storage
            .migrate(&[
                crate::instances::directory::MIGRATION,
                crate::instances::create::MIGRATION,
                crate::instances::delete::MIGRATION,
                crate::content::install::MIGRATION,
                crate::performance::mutation::MIGRATION,
            ])
            .unwrap();
        let directories = InstanceDirectories::new(
            Registry::new(storage.clone()),
            library.clone(),
            exclusions.clone(),
        );
        let instances = InstanceService::new(directories.clone(), owner.clone());
        let first = create(&instances, "First census target").await;
        let second = create(&instances, "Second census target").await;
        let client = ProviderClient::new(ClientConfig::default()).unwrap();
        let queue = queue.with_content(
            Arc::new(ContentService::new(client.clone()).unwrap()),
            Arc::new(ContentMutations::new(directories, client, owner.clone())),
        );
        let mut ids = Vec::new();
        for (index, instance) in [&first, &first, &first, &second].into_iter().enumerate() {
            let request = InstallQueueRequest::Content {
                instance_id: instance.id.to_string(),
                label: format!("Remove census fixture {index}"),
                action: InstallQueueContentActionRequest::Uninstall {
                    canonical_ids: vec![format!("modrinth:fixture{index}")],
                },
            };
            let item = queue.resolve_target(&request).await.unwrap();
            ids.push(
                queue
                    .admit_locked(
                        &mut queue.inner.state.lock().unwrap(),
                        request,
                        item,
                        library.admit().unwrap(),
                        None,
                        false,
                    )
                    .unwrap(),
            );
        }
        assert_eq!(queue.snapshot().items.len(), 4);
        assert_eq!(queue.active_count(), 0);
        queue.remove(&ids[0]).await.unwrap();
        assert_eq!(
            queue.status(&ids[0]).unwrap().outcome,
            Some(InstallOutcome::Removed)
        );
        assert_eq!(queue.snapshot().items.len(), 3);
        assert_eq!(queue.active_count(), 0);

        for id in &ids[2..] {
            persist_status(&storage, &queue.status(id).unwrap(), "running").unwrap();
        }
        let runtime = queue.inner.runtime.clone();
        queue.close_admission();
        queue.shutdown_queued().unwrap();
        drop(queue);
        let restored =
            InstallQueue::new(storage, library.clone(), exclusions, owner.clone(), runtime)
                .unwrap();
        let snapshot = restored.snapshot();
        assert_eq!(snapshot.items.len(), 1);
        assert_eq!(snapshot.items[0].queue_id, ids[1]);
        assert!(ids[2..].contains(&snapshot.active.unwrap().queue_id));
        for id in &ids[2..] {
            let status = restored.status(id).unwrap();
            assert!(!status.done);
            assert_eq!(status.view_model.phase_id, "settlement_required");
        }
        assert!(restored.status(&ids[0]).unwrap().done);
        assert_eq!(restored.active_count(), 2);
        restored.close_admission();
        owner
            .shutdown(std::time::Duration::from_secs(1))
            .await
            .unwrap();
        restored.join_observers().await.unwrap();
        drop(restored);
        library.try_preserve().unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn queue_shutdown_releases_unpolled_observer_before_profile_reopen() {
        for schedule_observer in [false, true] {
            let (root, storage, library, exclusions, owner, isolated_queue) = fixture();
            drop(isolated_queue);
            let runtime = library.runtime_cache().unwrap();
            let queue = InstallQueue::new(
                storage,
                library.clone(),
                exclusions.clone(),
                owner.clone(),
                runtime.clone(),
            )
            .unwrap();
            assert_eq!(queue.join_observers().await, Err(InstallError::Busy));
            let pin = library.admit().unwrap();
            let blocker = exclusions
                .try_acquire(
                    std::iter::empty::<String>(),
                    [library_artifact(&pin.library_id().to_string())],
                )
                .unwrap();
            if schedule_observer {
                queue
                    .enqueue(InstallQueueRequest::Vanilla {
                        version_id: "1.21.4".into(),
                    })
                    .await
                    .unwrap();
                assert!(queue.inner.scheduling.load(Ordering::Acquire));
            }
            queue.close_admission();
            library.close_admission();
            assert_eq!(queue.join_observers().await, Err(InstallError::Busy));
            owner
                .shutdown(std::time::Duration::from_secs(1))
                .await
                .unwrap();
            let receipt = owner.shutdown_receipt().unwrap();
            let foreign = TaskOwner::new(1).unwrap();
            foreign.try_close_idle().unwrap();
            assert_eq!(
                queue.preserve_shutdown(&foreign.shutdown_receipt().unwrap()),
                Err(InstallError::Busy)
            );
            assert_eq!(queue.preserve_shutdown(&receipt), Err(InstallError::Busy));
            assert!(owner.status().is_idle());
            assert_eq!(
                queue.inner.scheduling.load(Ordering::Acquire),
                schedule_observer
            );
            if schedule_observer {
                let mut first_join = Box::pin(queue.join_observers());
                assert!(futures_util::poll!(&mut first_join).is_pending());
                let mut second_join = Box::pin(queue.join_observers());
                assert!(futures_util::poll!(&mut second_join).is_pending());
                drop(first_join);
                second_join.await.unwrap();
            } else {
                queue.join_observers().await.unwrap();
            }
            assert!(matches!(queue.observers(), Err(InstallError::Closed)));
            assert!(queue.inner.observers.lock().unwrap().tasks.is_empty());
            queue.preserve_shutdown(&receipt).unwrap();
            queue.preserve_shutdown(&receipt).unwrap();
            drop(blocker);
            drop(pin);
            queue.shutdown_queued().unwrap();
            runtime.settle().unwrap();
            library.try_preserve().unwrap();
            drop(queue);
            drop(runtime);
            drop(library);
            let reopened = LibraryLifecycle::open(root.path());
            assert!(
                matches!(reopened, LibraryOpenOutcome::Ready(_)),
                "queue observer={schedule_observer}: {reopened:?}"
            );
        }
    }

    #[tokio::test]
    async fn loader_queue_labels_preserve_identity_across_queued_active_and_failed_views() {
        use axial_minecraft::loaders::{build_id_for, installed_version_id_for};

        let (_root, _storage, library, _exclusions, _owner, queue) = fixture();
        for (component, name) in [
            (LoaderComponentId::Fabric, "Fabric"),
            (LoaderComponentId::Quilt, "Quilt"),
            (LoaderComponentId::Forge, "Forge"),
            (LoaderComponentId::NeoForge, "NeoForge"),
        ] {
            let item = InstallQueueInstallItemViewModel {
                version_id: installed_version_id_for(component, "1.20.1", "0.19.5").unwrap(),
                loader: Some(InstallQueueLoaderItemViewModel {
                    component_id: component.as_str().into(),
                    build_id: build_id_for(component, "1.20.1", "0.19.5"),
                    minecraft_version: "1.20.1".into(),
                    loader_version: "0.19.5".into(),
                }),
                content: None,
            };
            assert!(item.version_id.starts_with("loader-v2-"));
            let expected = format!("{name} 0.19.5 for Minecraft 1.20.1");
            let id = {
                let mut state = queue.inner.state.lock().unwrap();
                let id = queue
                    .admit_locked(
                        &mut state,
                        InstallQueueRequest::Loader {
                            component_id: component,
                            build_id: item.loader.as_ref().unwrap().build_id.clone(),
                        },
                        item.clone(),
                        library.admit().unwrap(),
                        None,
                        false,
                    )
                    .unwrap();
                let queued = project(&state).items.pop().unwrap();
                assert_eq!(queued.label, expected);
                assert_eq!(queued.title, expected);
                assert_eq!(queued.install_item, item);
                assert_eq!(state.queued.pop_front().as_deref(), Some(id.as_str()));
                state.active = Some(id.clone());
                let active = project(&state).active.unwrap();
                assert_eq!(active.label, expected);
                assert_eq!(active.title, expected);
                assert_eq!(active.install_item, item);
                id
            };
            assert!(queue.complete(&id, InstallOutcome::Failed, Some(InstallError::Failed)));
            let failure = queue.snapshot().latest_failure.unwrap();
            assert_eq!(failure.label, expected);
            assert_eq!(failure.install_item, item);
        }
    }

    #[test]
    fn install_labels_preserve_vanilla_and_content_wording() {
        let mut item = InstallQueueInstallItemViewModel {
            version_id: " 1.20.1 ".into(),
            loader: None,
            content: None,
        };
        assert_eq!(item_label(&item), "Minecraft 1.20.1");
        item.version_id = " ".into();
        assert_eq!(item_label(&item), "Minecraft");
        item.loader = Some(InstallQueueLoaderItemViewModel {
            component_id: LoaderComponentId::Fabric.as_str().into(),
            build_id: "opaque-build".into(),
            minecraft_version: " ".into(),
            loader_version: " ".into(),
        });
        assert_eq!(item_label(&item), "Fabric loader");
        item.content = Some(InstallQueueContentItemViewModel {
            instance_id: "instance".into(),
            label: "Install selected resource packs".into(),
            action: InstallQueueContentActionRequest::Uninstall {
                canonical_ids: vec!["pack".into()],
            },
        });
        assert_eq!(item_label(&item), "Install selected resource packs");
    }

    pub(crate) async fn install_ready_fixture(queue: &InstallQueue, version: &str) {
        install_ready_fixture_using(queue, version, |operation| async move {
            axial_minecraft::download::publish_managed_install_fixture_for_test(operation, version)
                .await
        })
        .await;
    }

    pub(crate) async fn install_ready_fixture_with_client(
        queue: &InstallQueue,
        version: &str,
        client: Vec<u8>,
    ) {
        install_ready_fixture_using(queue, version, |operation| async move {
            axial_minecraft::download::publish_managed_install_fixture_with_client_for_test(
                operation, version, client,
            )
            .await
        })
        .await;
    }

    async fn install_ready_fixture_using<Publish, Publication>(
        queue: &InstallQueue,
        version: &str,
        publish: Publish,
    ) where
        Publish: FnOnce(axial_minecraft::managed_path::ManagedLibraryOperation) -> Publication,
        Publication: std::future::Future<
                Output = Result<axial_minecraft::KnownGoodInstallReceipt, DownloadError>,
            >,
    {
        assert!(queue.snapshot().active.is_none());
        assert!(queue.snapshot().items.is_empty());
        let pin = queue.inner.library.admit().unwrap();
        let _writer = queue
            .inner
            .exclusions
            .try_acquire(
                std::iter::empty::<String>(),
                [library_artifact(&pin.library_id().to_string())],
            )
            .unwrap();
        let accepted = queue
            .enqueue(InstallQueueRequest::Vanilla {
                version_id: version.into(),
            })
            .await
            .unwrap();
        let id = accepted.started_install.unwrap().install_id;
        {
            let mut state = queue.inner.state.lock().unwrap();
            assert_eq!(state.queued.pop_front().as_deref(), Some(id.as_str()));
            state.active = Some(id.clone());
        }
        let operation = pin.managed_library().unwrap();
        let receipt = publish(operation.clone()).await.unwrap();
        assert!(
            queue
                .settle_receipt(&id, &pin, operation, receipt)
                .await
                .is_ok()
        );
        assert!(queue.complete(&id, InstallOutcome::Succeeded, None));
        let ready = queue.ready_version(&pin, version).await.unwrap();
        assert_eq!(ready.version().id, version);
        ready.revalidate().unwrap();
    }

    pub(crate) async fn interrupted_setup_fixture(
        queue: &InstallQueue,
        work: &SetupWork,
    ) -> (InstallQueue, String) {
        let request = work.request();
        let item = queue.resolve_target(&request).await.unwrap();
        let id = {
            let mut state = queue.inner.state.lock().unwrap();
            let id = queue
                .admit_locked(
                    &mut state,
                    request,
                    item,
                    work.instance().generation().clone(),
                    None,
                    false,
                )
                .unwrap();
            persist_status(&queue.inner.storage, &state.entries[&id].status, "running").unwrap();
            id
        };
        queue.close_admission();
        queue.shutdown_queued().unwrap();
        let restored = InstallQueue::new(
            queue.inner.storage.clone(),
            queue.inner.library.clone(),
            queue.inner.exclusions.clone(),
            queue.inner.owner.clone(),
            queue.inner.runtime.clone(),
        )
        .unwrap();
        (restored, id)
    }

    #[tokio::test]
    async fn terminal_worker_release_publishes_new_registry_revision() {
        let (_root, _storage, library, exclusions, owner, queue) = fixture();
        let pin = library.admit().unwrap();
        let artifact = library_artifact(&pin.library_id().to_string());
        let lease = exclusions
            .try_acquire(["setup-target"], [artifact.clone()])
            .unwrap();
        let request = InstallQueueRequest::Vanilla {
            version_id: "1.21.4".into(),
        };
        let item = queue.resolve_target(&request).await.unwrap();
        let id = {
            let mut state = queue.inner.state.lock().unwrap();
            let id = queue
                .admit_locked(&mut state, request, item, pin.clone(), None, false)
                .unwrap();
            assert_eq!(state.queued.pop_front().as_deref(), Some(id.as_str()));
            state.active = Some(id.clone());
            id
        };
        let worker_queue = queue.clone();
        let worker_id = id.clone();
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let worker_release = release.clone();
        let (completed, completion) = tokio::sync::oneshot::channel();
        let task = owner
            .try_spawn((pin, lease), move |_| async move {
                assert!(worker_queue.complete(&worker_id, InstallOutcome::Cancelled, None));
                completed.send(()).unwrap();
                worker_release.acquire_owned().await.unwrap().forget();
            })
            .unwrap();
        let joined_queue = queue.clone();
        let joined_id = id.clone();
        let waiter = tokio::spawn(async move { joined_queue.join_worker(&joined_id, task).await });
        completion.await.unwrap();
        let (terminal, mut changes) = queue.subscribe();
        assert!(queue.status(&id).unwrap().done);
        assert!(terminal.active.is_none());
        assert!(
            exclusions
                .try_acquire(["setup-target"], [artifact.clone()])
                .is_err()
        );
        release.add_permits(1);
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while changes.borrow_and_update().registry_revision == terminal.registry_revision {
                changes.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
        assert!(exclusions.try_acquire(["setup-target"], [artifact]).is_ok());
        waiter.await.unwrap();
        assert!(owner.status().is_idle());
        queue.close_admission();
        queue.shutdown_queued().unwrap();
    }

    #[tokio::test]
    async fn ready_version_ids_are_display_only_and_library_scoped() {
        let (root, storage, library, exclusions, owner, queue) = fixture();
        let loader_id = axial_minecraft::loaders::installed_version_id_for(
            LoaderComponentId::Fabric,
            "1.21.4",
            "0.16.9",
        )
        .unwrap();
        install_ready_fixture(&queue, "1.21.4").await;
        install_ready_fixture(&queue, &loader_id).await;
        let pin = library.admit().unwrap();
        let scanned = crate::catalog::installed_versions(&pin.managed_library().unwrap(), None)
            .await
            .unwrap();
        let loader = scanned
            .versions
            .iter()
            .find(|version| version.id == loader_id)
            .unwrap();
        assert!(loader.launchable);
        assert_eq!(
            loader.loader.as_ref().unwrap().component_id,
            LoaderComponentId::Fabric
        );
        storage.transaction(|db| {
            db.execute("UPDATE installed_versions SET state='activating' WHERE library_id=?1 AND version_id='1.21.4'", [pin.library_id().to_string()])?;
            Ok::<_, StorageError>(())
        }).unwrap();

        let other_root =
            tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
        let other_library = match LibraryLifecycle::open(other_root.path()) {
            LibraryOpenOutcome::Ready(library) => library,
            other => panic!("isolated library fixture: {other:?}"),
        };
        let other_queue = InstallQueue::new(
            storage,
            other_library.clone(),
            exclusions,
            owner.clone(),
            queue.runtime_cache().clone(),
        )
        .unwrap();
        install_ready_fixture(&other_queue, "other-library-version").await;
        let other_pin = other_library.admit().unwrap();
        assert_eq!(
            queue.ready_version_ids(&pin).unwrap(),
            BTreeSet::from([loader_id.clone()])
        );
        assert_eq!(
            queue.ready_version_ids(&other_pin).unwrap(),
            BTreeSet::from(["other-library-version".into()])
        );

        std::fs::write(
            root.path()
                .join(format!("versions/{loader_id}/{loader_id}.jar")),
            b"changed after activation",
        )
        .unwrap();
        assert_eq!(
            queue.ready_version_ids(&pin).unwrap(),
            BTreeSet::from([loader_id.clone()])
        );
        assert!(matches!(
            queue.ready_version(&pin, &loader_id).await,
            Err(InstallError::ClientJarCorrupt)
        ));
        queue.shutdown_queued().unwrap();
        other_queue.shutdown_queued().unwrap();
        owner
            .shutdown(std::time::Duration::from_secs(1))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn retained_retry_reuses_exact_native_intent_without_provider_admission() {
        for loader in [false, true] {
            for exact in [false, true] {
                let (_root, storage, _library, _exclusions, owner, queue) = fixture();
                let (request, item) = retained_request_fixture(loader);
                let id = retain_fixture(&queue, request.clone(), item);
                let original = queue.status(&id).unwrap();
                let active = serde_json::to_value(queue.snapshot()).unwrap();
                assert_eq!(active["active"]["retry_action"]["action"], "retry");
                assert_eq!(active["active"]["retry_action"]["enabled"], true);
                queue.inner.state.lock().unwrap().entries[&id]
                    .cancel
                    .cancel();
                let response = tokio::time::timeout(std::time::Duration::from_secs(3), async {
                    if exact {
                        queue.retry_retained(&id, &request).await
                    } else {
                        queue.retry(request.clone()).await
                    }
                })
                .await
                .unwrap()
                .unwrap();
                let started = response.started_install.unwrap();
                assert_eq!(started.install_id, id);
                assert_eq!(started.operation_id, original.operation_id);
                let mut changes = queue.inner.latest.subscribe();
                tokio::time::timeout(std::time::Duration::from_secs(3), async {
                    while !queue.status(&id).unwrap().done {
                        changes.changed().await.unwrap();
                    }
                })
                .await
                .unwrap();
                assert_eq!(
                    queue.status(&id).unwrap().outcome,
                    Some(InstallOutcome::Cancelled)
                );
                let count: usize = storage
                    .read(|db| {
                        db.query_row("SELECT COUNT(*) FROM install_queue", [], |row| row.get(0))
                            .map_err(InstallError::from)
                    })
                    .unwrap();
                assert_eq!(count, 1);
                assert!(queue.snapshot().active.is_none());
                owner
                    .shutdown(std::time::Duration::from_secs(1))
                    .await
                    .unwrap();
            }
        }
    }

    fn retained_request_fixture(
        loader: bool,
    ) -> (InstallQueueRequest, InstallQueueInstallItemViewModel) {
        if loader {
            let component = LoaderComponentId::Quilt;
            let build_id = axial_minecraft::loaders::build_id_for(component, "1.20.1", "0.30.1");
            (
                InstallQueueRequest::Loader {
                    component_id: component,
                    build_id: build_id.clone(),
                },
                InstallQueueInstallItemViewModel {
                    version_id: axial_minecraft::loaders::installed_version_id_for(
                        component, "1.20.1", "0.30.1",
                    )
                    .unwrap(),
                    loader: Some(InstallQueueLoaderItemViewModel {
                        component_id: component.as_str().into(),
                        build_id,
                        minecraft_version: "1.20.1".into(),
                        loader_version: "0.30.1".into(),
                    }),
                    content: None,
                },
            )
        } else {
            (
                InstallQueueRequest::Vanilla {
                    version_id: "retained-native-fixture".into(),
                },
                InstallQueueInstallItemViewModel {
                    version_id: "retained-native-fixture".into(),
                    loader: None,
                    content: None,
                },
            )
        }
    }

    fn retain_fixture(
        queue: &InstallQueue,
        request: InstallQueueRequest,
        item: InstallQueueInstallItemViewModel,
    ) -> String {
        let pin = queue.inner.library.admit().unwrap();
        let lease = queue
            .inner
            .exclusions
            .try_acquire(
                std::iter::empty::<String>(),
                [library_artifact(&pin.library_id().to_string())],
            )
            .unwrap();
        let mut state = queue.inner.state.lock().unwrap();
        assert!(state.active.is_none());
        let id = queue
            .admit_locked(&mut state, request, item, pin, None, false)
            .unwrap();
        assert_eq!(state.queued.pop_front().as_deref(), Some(id.as_str()));
        state.active = Some(id.clone());
        let entry = state.entries.get_mut(&id).unwrap();
        entry.status.view_model = settlement_progress();
        entry.status.allowed_actions.clear();
        entry.retained = Some(RetainedInstall::Retry);
        entry.recovery_lease = Some(lease);
        persist_status(&queue.inner.storage, &entry.status, "settlement_required").unwrap();
        publish(&queue.inner, &mut state);
        id
    }

    #[tokio::test]
    async fn retained_retry_refuses_stale_identity_and_preserves_unknown_effects() {
        let (_root, storage, library, exclusions, owner, queue) = fixture();
        let (request, item) = retained_request_fixture(true);
        let old = retain_fixture(&queue, request.clone(), item.clone());
        queue.inner.state.lock().unwrap().entries[&old]
            .cancel
            .cancel();
        queue.retry_retained(&old, &request).await.unwrap();
        let mut changes = queue.inner.latest.subscribe();
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while !queue.status(&old).unwrap().done {
                changes.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
        let current = retain_fixture(&queue, request.clone(), item);
        queue
            .inner
            .state
            .lock()
            .unwrap()
            .entries
            .get_mut(&current)
            .unwrap()
            .retained = Some(RetainedInstall::Outcome(
            ManagedInstallDurableOutcome::permanently_indeterminate_fixture_for_test(),
        ));
        let original = queue.status(&current).unwrap();
        assert_eq!(
            queue.retry_retained(&old, &request).await,
            Err(InstallError::Busy)
        );
        assert_eq!(
            queue.retry_retained("missing", &request).await,
            Err(InstallError::NotFound)
        );
        assert_eq!(
            queue
                .retry_retained(
                    &current,
                    &InstallQueueRequest::Vanilla {
                        version_id: "other".into()
                    }
                )
                .await,
            Err(InstallError::InvalidRequest)
        );
        assert!(
            queue
                .recover_interrupted_with(
                    &old,
                    |_, _, _| async { panic!("stale target must not reconstruct") },
                    |_, _, _| async { panic!("stale target must not continue") }
                )
                .await
                .is_ok()
        );
        assert_eq!(queue.status(&current).unwrap(), original);
        assert_eq!(
            queue.retry_retained(&current, &request).await,
            Err(InstallError::SettlementRequired)
        );
        assert_eq!(queue.snapshot().active.as_ref().unwrap().queue_id, current);
        assert_eq!(
            queue.status(&current).unwrap().operation_id,
            original.operation_id
        );
        assert_eq!(queue.status(&current).unwrap().outcome, None);
        assert!(queue.snapshot().active.unwrap().retry_action.is_some());
        let pin = library.admit().unwrap();
        assert!(
            exclusions
                .try_acquire(
                    std::iter::empty::<String>(),
                    [library_artifact(&pin.library_id().to_string())]
                )
                .is_err()
        );
        let count: usize = storage
            .read(|db| {
                db.query_row("SELECT COUNT(*) FROM install_queue", [], |row| row.get(0))
                    .map_err(InstallError::from)
            })
            .unwrap();
        assert_eq!(count, 2);
        // Remove only the synthetic obstruction; the real classifier must still
        // establish NoEffect before this untouched fixture can be requeued.
        queue
            .inner
            .state
            .lock()
            .unwrap()
            .entries
            .get_mut(&current)
            .unwrap()
            .retained = Some(RetainedInstall::Retry);
        queue.close_admission();
        queue.recover_interrupted().await.unwrap();
        queue.shutdown_queued().unwrap();
        owner
            .shutdown(std::time::Duration::from_secs(1))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn retained_retry_spawn_refusal_preserves_eligibility_and_authority() {
        let (_root, _storage, library, exclusions, owner, queue) = fixture();
        let (request, item) = retained_request_fixture(false);
        let id = retain_fixture(&queue, request.clone(), item);
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let mut tasks = Vec::new();
        for _ in 0..4 {
            let gate = gate.clone();
            tasks.push(
                owner
                    .try_spawn((), move |_| async move {
                        gate.acquire_owned().await.unwrap().forget();
                    })
                    .unwrap(),
            );
        }
        assert_eq!(
            queue.retry_retained(&id, &request).await,
            Err(InstallError::Busy)
        );
        assert!(!queue.inner.state.lock().unwrap().entries[&id].recovery_running);
        assert!(queue.snapshot().active.unwrap().retry_action.is_some());
        let pin = library.admit().unwrap();
        assert!(
            exclusions
                .try_acquire(
                    std::iter::empty::<String>(),
                    [library_artifact(&pin.library_id().to_string())]
                )
                .is_err()
        );
        gate.add_permits(4);
        for task in tasks {
            task.join().await.unwrap();
        }
        queue.close_admission();
        assert_eq!(
            queue.retry_retained(&id, &request).await,
            Err(InstallError::Closed)
        );
        assert!(queue.snapshot().active.unwrap().retry_action.is_none());
        queue.recover_interrupted().await.unwrap();
        queue.shutdown_queued().unwrap();
        owner
            .shutdown(std::time::Duration::from_secs(1))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn retry_moves_existing_and_new_intents_to_front_across_restart() {
        let (_root, storage, library, exclusions, owner, queue) = fixture();
        let pin = library.admit().unwrap();
        let blocker = exclusions
            .try_acquire(
                std::iter::empty::<String>(),
                [library_artifact(&pin.library_id().to_string())],
            )
            .unwrap();
        let mut original_ids = Vec::new();
        for version in ["first", "second", "third"] {
            let accepted = queue
                .enqueue(InstallQueueRequest::Vanilla {
                    version_id: version.into(),
                })
                .await
                .unwrap();
            original_ids.push(accepted.started_install.unwrap().install_id);
        }
        let retried = queue
            .retry(InstallQueueRequest::Vanilla {
                version_id: "second".into(),
            })
            .await
            .unwrap();
        assert_eq!(retried.started_install.unwrap().install_id, original_ids[1]);
        let new_retry = queue
            .retry(InstallQueueRequest::Vanilla {
                version_id: "new-retry".into(),
            })
            .await
            .unwrap();
        let expected = ["new-retry", "second", "first", "third"];
        assert_eq!(
            new_retry
                .items
                .iter()
                .map(|item| item.install_item.version_id.as_str())
                .collect::<Vec<_>>(),
            expected
        );
        queue.close_admission();
        queue.shutdown_queued().unwrap();
        let restarted = InstallQueue::new(
            storage,
            library,
            exclusions,
            owner.clone(),
            queue.inner.runtime.clone(),
        )
        .unwrap();
        assert_eq!(
            restarted
                .snapshot()
                .items
                .iter()
                .map(|item| item.install_item.version_id.as_str())
                .collect::<Vec<_>>(),
            expected
        );
        restarted.close_admission();
        restarted.shutdown_queued().unwrap();
        drop(blocker);
        drop(pin);
        owner
            .shutdown(std::time::Duration::from_secs(1))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn setup_retry_restores_prerequisite_order_and_removal_only_deletes_pristine_instances() {
        use crate::{
            instances::{
                create::InstanceService,
                directory::{InstanceDirectories, Registry},
            },
            network::{ClientConfig, ProviderClient},
        };
        for user_edit in [false, true] {
            let (_root, storage, library, exclusions, owner, queue) = fixture();
            storage
                .migrate(&[
                    crate::instances::directory::MIGRATION,
                    crate::instances::create::MIGRATION,
                    crate::instances::delete::MIGRATION,
                    crate::content::install::MIGRATION,
                    crate::performance::mutation::MIGRATION,
                ])
                .unwrap();
            let directories = InstanceDirectories::new(
                Registry::new(storage.clone()),
                library.clone(),
                exclusions.clone(),
            );
            let instances = Arc::new(InstanceService::new(directories.clone(), owner.clone()));
            let client = ProviderClient::new(ClientConfig::default()).unwrap();
            let content = Arc::new(ContentService::new(client.clone()).unwrap());
            let mutations = Arc::new(ContentMutations::new(directories, client, owner.clone()));
            let work = crate::instances::setup::tests::pending_content_work(
                instances.clone(),
                content.clone(),
                mutations.clone(),
            )
            .await;
            let instance_id = work.instance().record().instance.id.clone();
            let directory = work.instance().game_directory().read_projection().unwrap();
            if user_edit {
                std::fs::write(directory.join("notes.txt"), b"Keep these notes").unwrap();
            }
            let blocker = exclusions
                .try_acquire(
                    std::iter::empty::<String>(),
                    [library_artifact(
                        &work.instance().generation().library_id().to_string(),
                    )],
                )
                .unwrap();
            let queue = queue.with_content(content.clone(), mutations.clone());
            let accepted = queue
                .enqueue_setup_content(work.request(), work.prerequisite(), work.clone())
                .await
                .unwrap();
            let content_id = accepted.started_install.unwrap().install_id;
            let runtime_id = accepted
                .items
                .iter()
                .find(|item| item.kind == "vanilla")
                .unwrap()
                .queue_id
                .clone();
            queue.remove(&runtime_id).await.unwrap();
            queue.close_admission();
            queue.shutdown_queued().unwrap();
            let restarted = InstallQueue::new(
                storage.clone(),
                library,
                exclusions,
                owner.clone(),
                queue.inner.runtime.clone(),
            )
            .unwrap()
            .with_content(content, mutations);
            restarted
                .retry(InstallQueueRequest::Vanilla {
                    version_id: "unrelated".into(),
                })
                .await
                .unwrap();
            let resumed = restarted
                .retry_setup_content(work.request(), work.prerequisite(), work.clone())
                .await
                .unwrap();
            assert_eq!(resumed.started_install.unwrap().install_id, content_id);
            assert_eq!(
                resumed
                    .items
                    .iter()
                    .map(|item| item.kind.as_str())
                    .collect::<Vec<_>>(),
                ["vanilla", "content", "vanilla"]
            );
            assert_eq!(resumed.items[0].install_item.version_id, "1.21.4");
            assert_eq!(resumed.items[2].install_item.version_id, "unrelated");
            let revision = restarted.snapshot().registry_revision;
            let removed = restarted.remove(&content_id).await.unwrap();
            assert!(removed.registry_revision > revision);
            assert_eq!(
                restarted.status(&content_id).unwrap().outcome,
                Some(InstallOutcome::Removed)
            );
            if user_edit {
                assert!(removed.removed_instance_id.is_none());
                assert!(instances.registry().get_live(&instance_id).is_ok());
                assert!(crate::instances::setup::has_pending(&storage, &instance_id).unwrap());
                assert_eq!(
                    std::fs::read(directory.join("notes.txt")).unwrap(),
                    b"Keep these notes"
                );
            } else {
                assert_eq!(
                    removed.removed_instance_id.as_deref(),
                    Some(instance_id.as_str())
                );
                assert!(instances.registry().get_live(&instance_id).is_err());
                assert!(!crate::instances::setup::has_pending(&storage, &instance_id).unwrap());
                assert!(!directory.exists());
            }
            restarted.close_admission();
            restarted.shutdown_queued().unwrap();
            drop(work);
            drop(blocker);
            owner
                .shutdown(std::time::Duration::from_secs(1))
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn content_restart_without_receipt_never_invents_installed_success() {
        use crate::{
            instances::directory::{InstanceDirectories, Registry},
            network::{ClientConfig, ProviderClient},
        };
        let (_root, storage, library, exclusions, owner, queue) = fixture();
        storage
            .migrate(&[
                crate::instances::directory::MIGRATION,
                crate::instances::create::MIGRATION,
                crate::content::install::MIGRATION,
            ])
            .unwrap();
        let pin = library.admit().unwrap();
        let blocker = exclusions
            .try_acquire(
                std::iter::empty::<String>(),
                [library_artifact(&pin.library_id().to_string())],
            )
            .unwrap();
        let accepted = queue
            .enqueue(InstallQueueRequest::Vanilla {
                version_id: "content-target".into(),
            })
            .await
            .unwrap();
        let id = accepted.started_install.unwrap().install_id;
        let action = InstallQueueContentActionRequest::Uninstall {
            canonical_ids: vec!["modrinth:fixture".into()],
        };
        let instance_id = InstanceId::new().to_string();
        let request = InstallQueueRequest::Content {
            instance_id: instance_id.clone(),
            label: "Remove fixture".into(),
            action: action.clone(),
        };
        let item = InstallQueueInstallItemViewModel {
            version_id: "content-target".into(),
            loader: None,
            content: Some(InstallQueueContentItemViewModel {
                instance_id,
                label: "Remove fixture".into(),
                action,
            }),
        };
        storage.transaction(|db| {
            db.execute("UPDATE install_queue SET request_json=?1,target_json=?2,phase='running' WHERE id=?3", params![serde_json::to_string(&request).unwrap(), serde_json::to_string(&item).unwrap(), id])?;
            Ok::<_, StorageError>(())
        }).unwrap();
        queue.close_admission();
        queue.shutdown_queued().unwrap();
        let client = ProviderClient::new(ClientConfig::default()).unwrap();
        let content = Arc::new(ContentService::new(client.clone()).unwrap());
        let mutations = Arc::new(ContentMutations::new(
            InstanceDirectories::new(
                Registry::new(storage.clone()),
                library.clone(),
                exclusions.clone(),
            ),
            client,
            owner.clone(),
        ));
        let restarted = InstallQueue::new(
            storage,
            library,
            exclusions,
            owner.clone(),
            queue.inner.runtime.clone(),
        )
        .unwrap()
        .with_content(content, mutations);
        assert_eq!(
            restarted.status(&id).unwrap().view_model.phase_id,
            "settlement_required"
        );
        let mut occupied = Vec::new();
        for _ in 0..4 {
            let (release, wait) = tokio::sync::oneshot::channel();
            let task = owner
                .try_spawn((), move |_| async move {
                    let _ = wait.await;
                })
                .unwrap();
            occupied.push((release, task));
        }
        assert_eq!(
            restarted.recover_interrupted().await,
            Err(InstallError::Closed)
        );
        assert_eq!(
            restarted.status(&id).unwrap().view_model.phase_id,
            "settlement_required"
        );
        assert!(restarted.snapshot().active.is_some());
        for (release, task) in occupied {
            release.send(()).unwrap();
            task.join().await.unwrap();
        }
        restarted.recover_interrupted().await.unwrap();
        let status = restarted.status(&id).unwrap();
        assert_eq!(status.outcome, Some(InstallOutcome::Failed));
        assert_eq!(
            status.failure_view_model.unwrap().summary,
            InstallError::ContentInterrupted.to_string()
        );
        assert_eq!(restarted.snapshot().registry_revision, 2);
        assert!(restarted.snapshot().active.is_none());
        restarted.close_admission();
        restarted.shutdown_queued().unwrap();
        drop(blocker);
        drop(pin);
        owner
            .shutdown(std::time::Duration::from_secs(1))
            .await
            .unwrap();
    }

    fn reconstruct_fixture(
        version: String,
        _expected: ManagedInstallActivationContractId,
        _recorded: Option<axial_minecraft::RecordedVersionMetadata>,
    ) -> impl std::future::Future<
        Output = Result<axial_minecraft::KnownGoodReconstructionReceipt, InstallError>,
    > {
        async move {
            axial_minecraft::known_good::managed_install_reconstruction_receipt_fixture_for_test(
                &version,
            )
            .map_err(|_| InstallError::NotReady)
        }
    }

    async fn recover_fixture(queue: &InstallQueue) -> Result<(), InstallError> {
        let Some(id) = queue.inner.state.lock().unwrap().active.clone() else {
            return Ok(());
        };
        queue
            .recover_interrupted_with(&id, reconstruct_fixture, |_, _, _| async {
                panic!("vanilla recovery must not continue a loader")
            })
            .await
    }

    #[derive(Clone, Copy, Debug)]
    enum CrashPoint {
        BeforeActivation,
        BeforeAcknowledgement,
        BeforeReady,
        BeforeTerminal,
    }

    async fn publish_before_crash(
        queue: &InstallQueue,
        pin: &GenerationPin,
        id: &str,
        version: &str,
        kind: CheckpointKind,
        point: CrashPoint,
    ) {
        let operation = pin.managed_library().unwrap();
        let receipt = axial_minecraft::download::publish_managed_install_fixture_for_test(
            operation.clone(),
            version,
        )
        .await
        .unwrap();
        if matches!(point, CrashPoint::BeforeActivation) {
            return;
        }
        let InstallReceiptState::AwaitingActivation { verified, evidence } =
            artifacts::inspect_install_receipt(operation, receipt).await
        else {
            panic!("fixture must produce verified publication");
        };
        let checkpoint = PublicationCheckpoint {
            kind,
            version_id: version.to_owned(),
            evidence_id: evidence,
            contract_id: Some(verified.activation_contract_id().as_str().to_owned()),
        };
        let activation_checkpoint = checkpoint.clone();
        let storage = queue.inner.storage.clone();
        let activation_pin = pin.clone();
        let install_id = id.to_owned();
        let acknowledgement = verified
            .activate_with(move |source| {
                persist_activation(
                    storage,
                    activation_pin,
                    install_id,
                    Arc::new(source),
                    activation_checkpoint,
                )
            })
            .await
            .unwrap();
        if matches!(point, CrashPoint::BeforeAcknowledgement) {
            return;
        }
        assert!(matches!(
            acknowledgement.acknowledge().await,
            ManagedInstallAcknowledgementOutcome::Acknowledged
        ));
        if matches!(point, CrashPoint::BeforeTerminal) {
            assert!(queue.mark_ready(pin, id, &checkpoint).is_ok());
        }
    }

    fn interrupt_intent(storage: &MetadataStore, id: &str) {
        storage
            .transaction(|db| {
                db.execute("UPDATE install_queue SET phase='running' WHERE id=?1", [id])?;
                Ok::<_, InstallError>(())
            })
            .unwrap();
    }

    #[tokio::test]
    async fn restart_settles_each_publication_boundary_and_releases_next_queued_work() {
        for point in [
            CrashPoint::BeforeActivation,
            CrashPoint::BeforeAcknowledgement,
            CrashPoint::BeforeReady,
            CrashPoint::BeforeTerminal,
        ] {
            let (_root, storage, library, exclusions, owner, queue) = fixture();
            let pin = library.admit().unwrap();
            let writer = exclusions
                .try_acquire(
                    std::iter::empty::<String>(),
                    [library_artifact(&pin.library_id().to_string())],
                )
                .unwrap();
            let version = "restart-boundary";
            let id = queue
                .enqueue(InstallQueueRequest::Vanilla {
                    version_id: version.into(),
                })
                .await
                .unwrap()
                .items[0]
                .queue_id
                .clone();
            let next = queue
                .enqueue(InstallQueueRequest::Vanilla {
                    version_id: "next-queued".into(),
                })
                .await
                .unwrap()
                .items[1]
                .queue_id
                .clone();
            interrupt_intent(&storage, &id);
            publish_before_crash(&queue, &pin, &id, version, CheckpointKind::Final, point).await;
            queue.shutdown_queued().unwrap();
            drop(writer);
            let restarted = InstallQueue::new(
                storage,
                library,
                exclusions.clone(),
                owner,
                queue.runtime_cache().clone(),
            )
            .unwrap();
            // Cancellation at the untouched worker boundary makes the next
            // queued operation observable without fetching an external provider.
            restarted.inner.state.lock().unwrap().entries[&next]
                .cancel
                .cancel();
            let mut events = restarted.inner.latest.subscribe();
            recover_fixture(&restarted).await.unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(3), async {
                while !restarted.status(&next).unwrap().done {
                    events.changed().await.unwrap();
                }
            })
            .await
            .unwrap();
            let mut owner_changes = restarted.inner.owner.subscribe();
            tokio::time::timeout(std::time::Duration::from_secs(3), async {
                while !restarted.inner.owner.status().is_idle() {
                    owner_changes.changed().await.unwrap();
                }
            })
            .await
            .unwrap();
            assert_eq!(
                restarted.status(&id).unwrap().outcome,
                Some(InstallOutcome::Succeeded),
                "{point:?}"
            );
            assert_eq!(
                restarted.status(&next).unwrap().outcome,
                Some(InstallOutcome::Cancelled)
            );
            assert!(restarted.ready_version(&pin, version).await.is_ok());
            assert!(!restarted.has_unsettled_effects());
            assert!(
                exclusions
                    .try_acquire(
                        std::iter::empty::<String>(),
                        [library_artifact(&pin.library_id().to_string())]
                    )
                    .is_ok()
            );
        }
    }

    #[tokio::test]
    async fn acknowledged_activation_supplies_recorded_metadata_before_ready() {
        let (_root, storage, library, exclusions, owner, queue) = fixture();
        let pin = library.admit().unwrap();
        let operation = pin.managed_library().unwrap();
        let writer = exclusions
            .try_acquire(
                std::iter::empty::<String>(),
                [library_artifact(&pin.library_id().to_string())],
            )
            .unwrap();
        let version = "recorded-before-ready";
        let id = queue
            .enqueue(InstallQueueRequest::Vanilla {
                version_id: version.into(),
            })
            .await
            .unwrap()
            .items[0]
            .queue_id
            .clone();
        interrupt_intent(&storage, &id);
        publish_before_crash(
            &queue,
            &pin,
            &id,
            version,
            CheckpointKind::Final,
            CrashPoint::BeforeReady,
        )
        .await;
        let original: (String, String, String) = storage
            .read(|db| {
                db.query_row(
                    "SELECT q.checkpoint_json,v.inventory_json,v.state FROM install_queue q JOIN installed_versions v ON v.install_id=q.id WHERE q.id=?1",
                    [&id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .map_err(InstallError::from)
            })
            .unwrap();
        queue.shutdown_queued().unwrap();
        drop(writer);
        let restarted = InstallQueue::new(
            storage.clone(),
            library,
            exclusions.clone(),
            owner.clone(),
            queue.runtime_cache().clone(),
        )
        .unwrap();
        let (supplied, mut received) = tokio::sync::oneshot::channel();
        let recovered = restarted
            .recover_interrupted_with(
                &id,
                move |version, expected, recorded| {
                    let _ = supplied.send(recorded.is_some());
                    reconstruct_fixture(version, expected, recorded)
                },
                |_, _, _| async { panic!("vanilla recovery must not continue a loader") },
            )
            .await;
        let restored: (String, String, String) = storage
            .read(|db| {
                db.query_row(
                    "SELECT q.checkpoint_json,v.inventory_json,v.state FROM install_queue q JOIN installed_versions v ON v.install_id=q.id WHERE q.id=?1",
                    [&id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .map_err(InstallError::from)
            })
            .unwrap();
        let ready = restarted.ready_version(&pin, version).await.is_ok();
        restarted.close_admission();
        owner
            .shutdown(std::time::Duration::from_secs(3))
            .await
            .unwrap();
        restarted.join_observers().await.unwrap();
        queue.join_observers().await.unwrap();
        restarted.shutdown_queued().unwrap();

        assert_eq!(original.2, "activating");
        assert_eq!(recovered, Ok(()));
        assert!(
            received.try_recv().unwrap(),
            "ACK-before-Ready must authenticate stored metadata"
        );
        assert_eq!(restored, (original.0, original.1, "ready".into()));
        assert!(ready && !restarted.has_unsettled_effects());
        assert_eq!(
            restarted.status(&id).unwrap().outcome,
            Some(InstallOutcome::Succeeded)
        );
        assert!(axial_minecraft::VersionBundleReadGuard::acquire(&operation).is_ok());
        assert!(
            exclusions
                .try_acquire(
                    std::iter::empty::<String>(),
                    [library_artifact(&pin.library_id().to_string())],
                )
                .is_ok()
        );
    }

    #[tokio::test]
    async fn missing_committed_metadata_refuses_reconstruction_without_releasing_evidence() {
        let (root, storage, library, exclusions, owner, queue) = fixture();
        let pin = library.admit().unwrap();
        let operation = pin.managed_library().unwrap();
        let writer = exclusions
            .try_acquire(
                std::iter::empty::<String>(),
                [library_artifact(&pin.library_id().to_string())],
            )
            .unwrap();
        let version = "missing-recorded-metadata";
        let id = queue
            .enqueue(InstallQueueRequest::Vanilla {
                version_id: version.into(),
            })
            .await
            .unwrap()
            .items[0]
            .queue_id
            .clone();
        interrupt_intent(&storage, &id);
        publish_before_crash(
            &queue,
            &pin,
            &id,
            version,
            CheckpointKind::Final,
            CrashPoint::BeforeAcknowledgement,
        )
        .await;
        let original: (String, String) = storage.read(|db| {
            db.query_row(
                "SELECT q.checkpoint_json,v.inventory_json FROM install_queue q JOIN installed_versions v ON v.install_id=q.id WHERE q.id=?1",
                [&id], |row| Ok((row.get(0)?, row.get(1)?)),
            ).map_err(InstallError::from)
        }).unwrap();
        let checkpoint: PublicationCheckpoint = serde_json::from_str(&original.0).unwrap();
        let metadata = root
            .path()
            .join(format!("versions/{version}/{version}.json"));
        let aside = root.path().join("preserved-version.json");
        let original_bytes = std::fs::read(&metadata).unwrap();
        queue.shutdown_queued().unwrap();
        drop(writer);
        let restarted = InstallQueue::new(
            storage.clone(),
            library,
            exclusions.clone(),
            owner.clone(),
            queue.runtime_cache().clone(),
        )
        .unwrap();
        let (supplied, mut received) = tokio::sync::oneshot::channel();
        let classified = restarted
            .recover_interrupted_with(
                &id,
                move |_, _, recorded| async move {
                    let _ = supplied.send(recorded.is_some());
                    Err(InstallError::NotReady)
                },
                |_, _, _| async { panic!("vanilla recovery must not continue a loader") },
            )
            .await;
        let retained = || {
            let state = restarted.inner.state.lock().unwrap();
            let entry = &state.entries[&id];
            let evidence = match &entry.retained {
                Some(RetainedInstall::Outcome(ManagedInstallDurableOutcome::Committed(value))) => {
                    Some(value.id().as_str().to_owned())
                }
                _ => None,
            };
            (evidence, entry.recovery_lease.is_some())
        };
        let classified_authority = retained();
        std::fs::rename(&metadata, &aside).unwrap();
        let called = Arc::new(AtomicBool::new(false));
        let callback = called.clone();
        let refused = restarted
            .recover_interrupted_with(
                &id,
                move |_, _, _| async move {
                    callback.store(true, Ordering::SeqCst);
                    Err(InstallError::NotReady)
                },
                |_, _, _| async { panic!("vanilla recovery must not continue a loader") },
            )
            .await;
        let refused_authority = retained();
        let retained_guard = axial_minecraft::VersionBundleReadGuard::acquire(&operation)
            .err()
            .map(|error| error.kind());
        let excluded = exclusions
            .try_acquire(
                std::iter::empty::<String>(),
                [library_artifact(&pin.library_id().to_string())],
            )
            .is_err();
        let pending = restarted.status(&id).unwrap().outcome;
        let unsettled = restarted.has_unsettled_effects();
        std::fs::rename(&aside, &metadata).unwrap();
        let recovered = recover_fixture(&restarted).await;
        let restored: (String, String) = storage.read(|db| {
            db.query_row(
                "SELECT q.checkpoint_json,v.inventory_json FROM install_queue q JOIN installed_versions v ON v.install_id=q.id WHERE q.id=?1",
                [&id], |row| Ok((row.get(0)?, row.get(1)?)),
            ).map_err(InstallError::from)
        }).unwrap();
        let ready = restarted.ready_version(&pin, version).await.is_ok();
        restarted.close_admission();
        owner
            .shutdown(std::time::Duration::from_secs(3))
            .await
            .unwrap();
        restarted.join_observers().await.unwrap();
        queue.join_observers().await.unwrap();
        restarted.shutdown_queued().unwrap();

        assert_eq!(classified, Err(InstallError::SettlementRequired));
        assert!(
            received.try_recv().unwrap(),
            "real committed metadata must reach reconstruction"
        );
        assert_eq!(classified_authority, (Some(checkpoint.evidence_id), true));
        assert_eq!(refused_authority, classified_authority);
        assert_eq!(refused, Err(InstallError::SettlementRequired));
        assert!(
            !called.load(Ordering::SeqCst),
            "missing exact metadata must refuse before reconstruction"
        );
        assert_eq!(retained_guard, Some(std::io::ErrorKind::WouldBlock));
        assert!(excluded && unsettled);
        assert_eq!(pending, None);
        assert_eq!(recovered, Ok(()));
        assert_eq!(restored, original);
        assert_eq!(std::fs::read(&metadata).unwrap(), original_bytes);
        assert!(ready && !restarted.has_unsettled_effects());
        assert_eq!(
            restarted.status(&id).unwrap().outcome,
            Some(InstallOutcome::Succeeded)
        );
        assert!(axial_minecraft::VersionBundleReadGuard::acquire(&operation).is_ok());
        assert!(
            exclusions
                .try_acquire(
                    std::iter::empty::<String>(),
                    [library_artifact(&pin.library_id().to_string())],
                )
                .is_ok()
        );
    }

    #[tokio::test]
    async fn mismatched_reconstruction_stays_fenced_and_can_retry_the_retained_publication() {
        let (_root, storage, library, exclusions, owner, queue) = fixture();
        let pin = library.admit().unwrap();
        let operation = pin.managed_library().unwrap();
        let writer = exclusions
            .try_acquire(
                std::iter::empty::<String>(),
                [library_artifact(&pin.library_id().to_string())],
            )
            .unwrap();
        let version = "retained-reconstruction";
        let id = queue
            .enqueue(InstallQueueRequest::Vanilla {
                version_id: version.into(),
            })
            .await
            .unwrap()
            .items[0]
            .queue_id
            .clone();
        interrupt_intent(&storage, &id);
        publish_before_crash(
            &queue,
            &pin,
            &id,
            version,
            CheckpointKind::Final,
            CrashPoint::BeforeActivation,
        )
        .await;
        queue.shutdown_queued().unwrap();
        drop(writer);
        let restarted = InstallQueue::new(
            storage,
            library,
            exclusions.clone(),
            owner.clone(),
            queue.runtime_cache().clone(),
        )
        .unwrap();
        let result = restarted
            .recover_interrupted_with(
                &id,
                |_, expected, recorded| {
                    reconstruct_fixture("other-version".into(), expected, recorded)
                },
                |_, _, _| async { panic!("no loader") },
            )
            .await;
        let retained_guard = axial_minecraft::VersionBundleReadGuard::acquire(&operation)
            .err()
            .map(|error| error.kind());
        let unsettled = restarted.has_unsettled_effects();
        let excluded = exclusions
            .try_acquire(
                std::iter::empty::<String>(),
                [library_artifact(&pin.library_id().to_string())],
            )
            .is_err();
        let not_ready = restarted.ready_version(&pin, version).await.is_err();
        recover_fixture(&restarted).await.unwrap();
        restarted.close_admission();
        owner
            .shutdown(std::time::Duration::from_secs(3))
            .await
            .unwrap();
        restarted.join_observers().await.unwrap();
        queue.join_observers().await.unwrap();
        restarted.shutdown_queued().unwrap();

        assert_eq!(result, Err(InstallError::SettlementRequired));
        assert_eq!(retained_guard, Some(std::io::ErrorKind::WouldBlock));
        assert!(unsettled && excluded && not_ready);
        assert_eq!(
            restarted.status(&id).unwrap().outcome,
            Some(InstallOutcome::Succeeded)
        );
        assert!(!restarted.has_unsettled_effects());
    }

    #[tokio::test]
    async fn invalid_checkpoint_contract_retains_classified_publication_until_recovery() {
        for invalid in [None, Some("invalid-contract")] {
            let (_root, storage, library, exclusions, owner, queue) = fixture();
            let pin = library.admit().unwrap();
            let operation = pin.managed_library().unwrap();
            let writer = exclusions
                .try_acquire(
                    std::iter::empty::<String>(),
                    [library_artifact(&pin.library_id().to_string())],
                )
                .unwrap();
            let version = "retained-checkpoint-contract";
            let id = queue
                .enqueue(InstallQueueRequest::Vanilla {
                    version_id: version.into(),
                })
                .await
                .unwrap()
                .started_install
                .unwrap()
                .install_id;
            interrupt_intent(&storage, &id);
            publish_before_crash(
                &queue,
                &pin,
                &id,
                version,
                CheckpointKind::Final,
                CrashPoint::BeforeAcknowledgement,
            )
            .await;
            let (original, inventory): (String, String) = storage
                .read(|db| {
                    db.query_row(
                        "SELECT q.checkpoint_json,v.inventory_json FROM install_queue q JOIN installed_versions v ON v.install_id=q.id WHERE q.id=?1",
                        [&id],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .map_err(InstallError::from)
                })
                .unwrap();
            queue.shutdown_queued().unwrap();
            drop(writer);
            let restarted = InstallQueue::new(
                storage.clone(),
                library,
                exclusions,
                owner.clone(),
                queue.runtime_cache().clone(),
            )
            .unwrap();
            let initially_readable =
                axial_minecraft::VersionBundleReadGuard::acquire(&operation).is_ok();
            let classified = restarted
                .recover_interrupted_with(
                    &id,
                    |_, _, _| async { Err(InstallError::NotReady) },
                    |_, _, _| async { panic!("vanilla recovery must not continue a loader") },
                )
                .await;
            let classified_guard = axial_minecraft::VersionBundleReadGuard::acquire(&operation)
                .err()
                .map(|error| error.kind());
            let original_checkpoint: PublicationCheckpoint =
                serde_json::from_str(&original).unwrap();
            let mut invalid_checkpoint = original_checkpoint.clone();
            invalid_checkpoint.contract_id = invalid.map(str::to_owned);
            restarted
                .persist_checkpoint(&id, &invalid_checkpoint)
                .unwrap();
            let refused = restarted
                .recover_interrupted_with(
                    &id,
                    |_, _, _| async { panic!("invalid recorded contract must not reconstruct") },
                    |_, _, _| async { panic!("invalid recorded contract must not continue") },
                )
                .await;
            let refused_guard = axial_minecraft::VersionBundleReadGuard::acquire(&operation)
                .err()
                .map(|error| error.kind());
            let pending = restarted.status(&id).unwrap().outcome;

            restarted
                .persist_checkpoint(&id, &original_checkpoint)
                .unwrap();
            recover_fixture(&restarted).await.unwrap();
            let recovered = restarted.status(&id).unwrap().outcome;
            let restored: (String, String) = storage
                .read(|db| {
                    db.query_row(
                        "SELECT q.checkpoint_json,v.inventory_json FROM install_queue q JOIN installed_versions v ON v.install_id=q.id WHERE q.id=?1",
                        [&id],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .map_err(InstallError::from)
                })
                .unwrap();
            let settled_readable =
                axial_minecraft::VersionBundleReadGuard::acquire(&operation).is_ok();
            restarted.close_admission();
            owner
                .shutdown(std::time::Duration::from_secs(3))
                .await
                .unwrap();
            restarted.join_observers().await.unwrap();
            queue.join_observers().await.unwrap();
            restarted.shutdown_queued().unwrap();

            assert!(initially_readable);
            assert_eq!(classified, Err(InstallError::SettlementRequired));
            assert_eq!(classified_guard, Some(std::io::ErrorKind::WouldBlock));
            assert_eq!(refused, Err(InstallError::SettlementRequired));
            assert_eq!(pending, None);
            assert_eq!(recovered, Some(InstallOutcome::Succeeded));
            assert_eq!(restored, (original, inventory));
            assert!(settled_readable);
            assert_eq!(
                refused_guard,
                Some(std::io::ErrorKind::WouldBlock),
                "recorded contract {invalid:?} released classified publication authority"
            );
        }
    }

    #[tokio::test]
    async fn acknowledged_checkpoint_from_another_root_cannot_adopt_identical_files() {
        let (_root, storage, library, exclusions, owner, queue) = fixture();
        let pin = library.admit().unwrap();
        let writer = exclusions
            .try_acquire(
                std::iter::empty::<String>(),
                [library_artifact(&pin.library_id().to_string())],
            )
            .unwrap();
        let version = "same-bytes-different-root";
        let id = queue
            .enqueue(InstallQueueRequest::Vanilla {
                version_id: version.into(),
            })
            .await
            .unwrap()
            .items[0]
            .queue_id
            .clone();
        interrupt_intent(&storage, &id);
        publish_before_crash(
            &queue,
            &pin,
            &id,
            version,
            CheckpointKind::Final,
            CrashPoint::BeforeReady,
        )
        .await;
        let original: String = storage
            .read(|db| {
                db.query_row(
                    "SELECT checkpoint_json FROM install_queue WHERE id=?1",
                    [&id],
                    |row| row.get(0),
                )
                .map_err(InstallError::from)
            })
            .unwrap();
        let mut forged: PublicationCheckpoint = serde_json::from_str(&original).unwrap();
        let (_foreign_root, _, foreign_library, _, _, _) = fixture();
        let foreign_operation = foreign_library.admit().unwrap().managed_library().unwrap();
        let foreign_receipt = axial_minecraft::download::publish_managed_install_fixture_for_test(
            foreign_operation.clone(),
            version,
        )
        .await
        .unwrap();
        let InstallReceiptState::AwaitingActivation { evidence, verified } =
            artifacts::inspect_install_receipt(foreign_operation, foreign_receipt).await
        else {
            panic!("foreign fixture publication");
        };
        assert_eq!(
            Some(verified.activation_contract_id().as_str()),
            forged.contract_id.as_deref()
        );
        forged.evidence_id = evidence;
        queue.persist_checkpoint(&id, &forged).unwrap();
        drop(verified);
        queue.shutdown_queued().unwrap();
        drop(writer);
        let restarted = InstallQueue::new(
            storage.clone(),
            library,
            exclusions.clone(),
            owner,
            queue.runtime_cache().clone(),
        )
        .unwrap();
        for _ in 0..2 {
            assert_eq!(
                recover_fixture(&restarted).await,
                Err(InstallError::SettlementRequired)
            );
            assert_eq!(restarted.status(&id).unwrap().outcome, None);
            assert!(restarted.ready_version(&pin, version).await.is_err());
            assert!(
                exclusions
                    .try_acquire(
                        std::iter::empty::<String>(),
                        [library_artifact(&pin.library_id().to_string())]
                    )
                    .is_err()
            );
        }
        storage
            .transaction(|db| {
                db.execute(
                    "UPDATE install_queue SET checkpoint_json=?1 WHERE id=?2",
                    params![original, id],
                )?;
                Ok::<_, InstallError>(())
            })
            .unwrap();
        recover_fixture(&restarted).await.unwrap();
        assert_eq!(
            restarted.status(&id).unwrap().outcome,
            Some(InstallOutcome::Succeeded)
        );
    }

    #[tokio::test]
    async fn dropping_recovery_waiter_preserves_retry_and_next_queue_dispatch() {
        for reconstruction_fails in [false, true] {
            let (_root, storage, library, exclusions, owner, queue) = fixture();
            let pin = library.admit().unwrap();
            let writer = exclusions
                .try_acquire(
                    std::iter::empty::<String>(),
                    [library_artifact(&pin.library_id().to_string())],
                )
                .unwrap();
            let version = "dropped-recovery-waiter";
            let id = queue
                .enqueue(InstallQueueRequest::Vanilla {
                    version_id: version.into(),
                })
                .await
                .unwrap()
                .items[0]
                .queue_id
                .clone();
            let next = queue
                .enqueue(InstallQueueRequest::Vanilla {
                    version_id: "after-recovery-waiter".into(),
                })
                .await
                .unwrap()
                .items[1]
                .queue_id
                .clone();
            interrupt_intent(&storage, &id);
            publish_before_crash(
                &queue,
                &pin,
                &id,
                version,
                CheckpointKind::Final,
                CrashPoint::BeforeActivation,
            )
            .await;
            queue.shutdown_queued().unwrap();
            drop(writer);
            let restarted = InstallQueue::new(
                storage,
                library,
                exclusions,
                owner,
                queue.runtime_cache().clone(),
            )
            .unwrap();
            restarted.inner.state.lock().unwrap().entries[&next]
                .cancel
                .cancel();
            let (entered, started) = tokio::sync::oneshot::channel();
            let (release, released) = tokio::sync::oneshot::channel();
            let recovering = restarted.clone();
            let recovery_id = id.clone();
            let caller = tokio::spawn(async move {
                recovering
                    .recover_interrupted_with(
                        &recovery_id,
                        move |version, expected, recorded| async move {
                            let _ = entered.send(());
                            released.await.unwrap();
                            if reconstruction_fails {
                                Err(InstallError::NotReady)
                            } else {
                                reconstruct_fixture(version, expected, recorded).await
                            }
                        },
                        |_, _, _| async { panic!("no loader") },
                    )
                    .await
            });
            started.await.unwrap();
            assert!(restarted.snapshot().active.unwrap().retry_action.is_none());
            let request = restarted.inner.state.lock().unwrap().entries[&id]
                .request
                .clone();
            assert_eq!(
                restarted.retry_retained(&id, &request).await,
                Err(InstallError::Busy)
            );
            assert_eq!(restarted.retry(request).await, Err(InstallError::Busy));
            caller.abort();
            assert!(caller.await.unwrap_err().is_cancelled());
            let mut changes = restarted.inner.latest.subscribe();
            release.send(()).unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(3), async {
                while restarted.inner.state.lock().unwrap().entries[&id].recovery_running {
                    changes.changed().await.unwrap();
                }
            })
            .await
            .unwrap();
            if reconstruction_fails {
                assert!(restarted.has_unsettled_effects());
                assert_eq!(restarted.status(&id).unwrap().outcome, None);
                recover_fixture(&restarted).await.unwrap();
            }
            tokio::time::timeout(std::time::Duration::from_secs(3), async {
                while !restarted.status(&next).unwrap().done {
                    changes.changed().await.unwrap();
                }
            })
            .await
            .unwrap();
            assert_eq!(
                restarted.status(&id).unwrap().outcome,
                Some(InstallOutcome::Succeeded)
            );
            assert_eq!(
                restarted.status(&next).unwrap().outcome,
                Some(InstallOutcome::Cancelled)
            );
        }
    }

    #[tokio::test]
    async fn loader_base_restart_continues_child_across_activation_and_acknowledgement() {
        for point in [
            CrashPoint::BeforeActivation,
            CrashPoint::BeforeAcknowledgement,
            CrashPoint::BeforeReady,
            CrashPoint::BeforeTerminal,
        ] {
            let (_root, storage, library, exclusions, owner, queue) = fixture();
            install_ready_fixture(&queue, "1.20.1").await;
            let pin = library.admit().unwrap();
            let writer = exclusions
                .try_acquire(
                    std::iter::empty::<String>(),
                    [library_artifact(&pin.library_id().to_string())],
                )
                .unwrap();
            let base = "1.20.1";
            let target = axial_minecraft::loaders::installed_version_id_for(
                LoaderComponentId::Fabric,
                base,
                "0.16.0",
            )
            .unwrap();
            let id = queue
                .enqueue(InstallQueueRequest::Vanilla {
                    version_id: target.clone(),
                })
                .await
                .unwrap()
                .items[0]
                .queue_id
                .clone();
            let item = InstallQueueInstallItemViewModel {
                version_id: target.clone(),
                content: None,
                loader: Some(InstallQueueLoaderItemViewModel {
                    component_id: "fabric".into(),
                    build_id: "fixture".into(),
                    minecraft_version: base.into(),
                    loader_version: "0.16.0".into(),
                }),
            };
            storage.transaction(|db| {
                db.execute("UPDATE install_queue SET phase='running',target_json=?1,request_json=?2 WHERE id=?3", params![serde_json::to_string(&item).unwrap(), serde_json::to_string(&InstallQueueRequest::Loader { component_id: LoaderComponentId::Fabric, build_id: "fixture".into() }).unwrap(), id])?;
                Ok::<_, InstallError>(())
            }).unwrap();
            publish_before_crash(&queue, &pin, &id, base, CheckpointKind::Base, point).await;
            queue.shutdown_queued().unwrap();
            drop(writer);
            let restarted = InstallQueue::new(
                storage,
                library,
                exclusions,
                owner,
                queue.runtime_cache().clone(),
            )
            .unwrap();
            let child = target.clone();
            let observed_queue = restarted.clone();
            let observed_pin = pin.clone();
            restarted
                .recover_interrupted_with(
                    &id,
                    reconstruct_fixture,
                    move |operation, continuation, id| async move {
                        assert_eq!(continuation.base_version_id(), base);
                        assert!(
                            observed_queue
                                .ready_version(&observed_pin, base)
                                .await
                                .is_ok()
                        );
                        assert_eq!(observed_queue.status(&id).unwrap().outcome, None);
                        assert!(matches!(
                            axial_minecraft::classify_managed_install_publication(
                                operation.clone(),
                                base
                            )
                            .await,
                            ManagedInstallDurableOutcome::NoEffect
                        ));
                        Ok(
                            axial_minecraft::download::publish_managed_install_fixture_for_test(
                                operation, &child,
                            )
                            .await
                            .unwrap(),
                        )
                    },
                )
                .await
                .unwrap();
            assert_eq!(
                restarted.status(&id).unwrap().outcome,
                Some(InstallOutcome::Succeeded),
                "{point:?}"
            );
            assert!(restarted.ready_version(&pin, &target).await.is_ok());
            assert!(!restarted.has_unsettled_effects());
        }
    }

    #[tokio::test]
    async fn settled_failure_telemetry_is_consent_gated_and_emitted_once() {
        for enabled in [false, true] {
            let (_root, _storage, library, exclusions, _owner, queue) = fixture();
            // Exercise only the terminal coordination boundary, without
            // running a provider or claiming a payload publication fixture.
            let pin = library.admit().unwrap();
            let _blocker = exclusions
                .try_acquire(
                    std::iter::empty::<String>(),
                    [library_artifact(&pin.library_id().to_string())],
                )
                .unwrap();
            let telemetry = Telemetry::configured_for_test();
            if enabled {
                telemetry
                    .consent_change()
                    .await
                    .publish(true, Some("4d8fa83c-5815-4ea2-aac1-ddcc336c405e"));
            }
            let queue = queue.with_telemetry(telemetry.clone());
            assert!(matches!(
                queue
                    .enqueue(InstallQueueRequest::Vanilla {
                        version_id: "../private-canary".into()
                    })
                    .await,
                Err(InstallError::InvalidRequest)
            ));
            assert!(telemetry.queued_events_for_test().is_empty());
            let accepted = queue
                .enqueue(InstallQueueRequest::Vanilla {
                    version_id: "settled-failure-fixture".into(),
                })
                .await
                .unwrap();
            let id = accepted.items[0].queue_id.clone();
            {
                let mut state = queue.inner.state.lock().unwrap();
                assert_eq!(state.queued.pop_front().as_deref(), Some(id.as_str()));
                state.active = Some(id.clone());
            }
            assert!(telemetry.queued_events_for_test().is_empty());
            assert!(queue.complete(&id, InstallOutcome::Failed, Some(InstallError::Failed)));
            let expected = if enabled {
                vec![TelemetryEvent::ErrorCaptured {
                    kind: TelemetryErrorKind::InstallFailed,
                }]
            } else {
                Vec::new()
            };
            assert_eq!(telemetry.queued_events_for_test(), expected);
            if !enabled {
                // Opting in later must not replay an opted-out terminal event.
                telemetry
                    .consent_change()
                    .await
                    .publish(true, Some("4d8fa83c-5815-4ea2-aac1-ddcc336c405e"));
            }

            assert!(queue.complete(&id, InstallOutcome::Failed, Some(InstallError::Storage)));
            for _ in 0..3 {
                let _ = queue.snapshot();
                assert_eq!(
                    queue.status(&id).unwrap().outcome,
                    Some(InstallOutcome::Failed)
                );
            }
            let cancelled = queue
                .enqueue(InstallQueueRequest::Vanilla {
                    version_id: "cancelled-fixture".into(),
                })
                .await
                .unwrap();
            let cancelled_id = cancelled.items[0].queue_id.clone();
            {
                let mut state = queue.inner.state.lock().unwrap();
                assert_eq!(
                    state.queued.pop_front().as_deref(),
                    Some(cancelled_id.as_str())
                );
                state.active = Some(cancelled_id.clone());
            }
            assert!(queue.complete(&cancelled_id, InstallOutcome::Cancelled, None));
            assert_eq!(telemetry.queued_events_for_test(), expected);
            queue.shutdown_queued().unwrap();
        }
    }

    #[tokio::test]
    async fn repeated_base_activation_rebinds_only_after_durable_checkpoint() {
        let (_root, storage, library, _exclusions, _owner, queue) = fixture();
        let pin = library.admit().unwrap();
        let base = "1.20.1";
        let mut prior: Option<(String, String)> = None;
        for _ in 0..2 {
            install_ready_fixture(&queue, base).await;
            let (id, checkpoint): (String, String) = storage.read(|db| {
                db.query_row(
                    "SELECT v.install_id,q.checkpoint_json FROM installed_versions v JOIN install_queue q ON q.id=v.install_id WHERE v.version_id=?1 AND v.state='ready'",
                    [base], |row| Ok((row.get(0)?, row.get(1)?)),
                ).map_err(InstallError::from)
            }).unwrap();
            let checkpoint: PublicationCheckpoint = serde_json::from_str(&checkpoint).unwrap();
            assert_eq!(checkpoint.version_id, base);
            assert_eq!(
                queue.status(&id).unwrap().outcome,
                Some(InstallOutcome::Succeeded)
            );
            if let Some((previous_id, previous_evidence)) = prior {
                assert_ne!(id, previous_id);
                assert_ne!(checkpoint.evidence_id, previous_evidence);
                assert_eq!(
                    queue.status(&previous_id).unwrap().outcome,
                    Some(InstallOutcome::Succeeded)
                );
            }
            prior = Some((id, checkpoint.evidence_id));
            assert!(matches!(
                axial_minecraft::classify_managed_install_publication(
                    pin.managed_library().unwrap(),
                    base,
                )
                .await,
                ManagedInstallDurableOutcome::NoEffect
            ));
        }
    }

    #[tokio::test]
    async fn activation_checkpoint_writes_are_atomic_before_acknowledgement() {
        for existing in [false, true] {
            for rejection in [
                "missing_intent",
                "ignore_activation",
                "abort_activation",
                "ignore_checkpoint",
                "abort_checkpoint",
            ] {
                let (_root, storage, library, exclusions, _owner, queue) = fixture();
                let version = "activation-write-fixture";
                if existing {
                    install_ready_fixture(&queue, version).await;
                }
                let prior: Option<(String, String, String)> = storage.read(|db| {
                    db.query_row("SELECT install_id,state,inventory_json FROM installed_versions WHERE version_id=?1",
                        [version], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))).optional().map_err(InstallError::from)
                }).unwrap();
                let pin = library.admit().unwrap();
                let _writer = exclusions
                    .try_acquire(
                        std::iter::empty::<String>(),
                        [library_artifact(&pin.library_id().to_string())],
                    )
                    .unwrap();
                let id = queue
                    .enqueue(InstallQueueRequest::Vanilla {
                        version_id: version.into(),
                    })
                    .await
                    .unwrap()
                    .started_install
                    .unwrap()
                    .install_id;
                let item = queue.inner.state.lock().unwrap().entries[&id].item.clone();
                let accepted: Vec<rusqlite::types::Value> = storage.read(|db| {
                    db.query_row("SELECT id,operation_id,library_id,request_json,target_json,status_json,phase,accepted_at,checkpoint_json FROM install_queue WHERE id=?1",
                        [&id], |row| (0..9).map(|column| row.get(column)).collect())
                        .map_err(InstallError::from)
                }).unwrap();
                storage.transaction(|db| {
                    if rejection == "missing_intent" {
                        db.execute("DELETE FROM install_queue WHERE id=?1", [&id])?;
                    } else {
                        let target = if rejection.ends_with("activation") {
                            if existing { "UPDATE ON installed_versions" } else { "INSERT ON installed_versions" }
                        } else {
                            "UPDATE OF checkpoint_json ON install_queue"
                        };
                        let action = if rejection.starts_with("ignore") { "IGNORE" } else { "ABORT,'fixture rejection'" };
                        db.execute_batch(&format!("CREATE TRIGGER reject_activation BEFORE {target} BEGIN SELECT RAISE({action}); END;"))?;
                    }
                    Ok::<_, InstallError>(())
                }).unwrap();
                let operation = pin.managed_library().unwrap();
                let receipt = axial_minecraft::download::publish_managed_install_fixture_for_test(
                    operation.clone(),
                    version,
                )
                .await
                .unwrap();
                assert!(
                    matches!(
                        queue
                            .settle_receipt(&id, &pin, operation.clone(), receipt)
                            .await,
                        Err(WorkFailure::Unsettled(RetainedInstall::Retry))
                    ),
                    "existing={existing}, rejection={rejection}"
                );
                let after: Option<(String, String, String)> = storage.read(|db| {
                    db.query_row("SELECT install_id,state,inventory_json FROM installed_versions WHERE version_id=?1",
                        [version], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))).optional().map_err(InstallError::from)
                }).unwrap();
                assert_eq!(after, prior, "existing={existing}, rejection={rejection}");
                let checkpoint: Option<Option<String>> = storage
                    .read(|db| {
                        db.query_row(
                            "SELECT checkpoint_json FROM install_queue WHERE id=?1",
                            [&id],
                            |row| row.get(0),
                        )
                        .optional()
                        .map_err(InstallError::from)
                    })
                    .unwrap();
                assert_eq!(
                    checkpoint,
                    if rejection == "missing_intent" {
                        None
                    } else {
                        Some(None)
                    }
                );
                assert_eq!(queue.status(&id).unwrap().outcome, None);
                assert!(matches!(
                    axial_minecraft::classify_managed_install_publication(
                        operation.clone(),
                        version
                    )
                    .await,
                    ManagedInstallDurableOutcome::Committed(_)
                ));
                storage
                    .transaction(|db| {
                        if rejection == "missing_intent" {
                            assert_eq!(db.execute("INSERT INTO install_queue(id,operation_id,library_id,request_json,target_json,status_json,phase,accepted_at,checkpoint_json) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                                rusqlite::params_from_iter(accepted.iter()))?, 1);
                        } else {
                            db.execute_batch("DROP TRIGGER reject_activation")?;
                        }
                        Ok::<_, InstallError>(())
                    })
                    .unwrap();
                assert!(matches!(
                    queue
                        .recover_publication(
                            &id,
                            &pin,
                            &operation,
                            &item,
                            Some(RetainedInstall::Retry),
                            reconstruct_fixture
                        )
                        .await,
                    Ok(RecoveryAction::Complete)
                ));
                assert!(queue.ready_version(&pin, version).await.is_ok());
                assert!(matches!(
                    axial_minecraft::classify_managed_install_publication(operation, version).await,
                    ManagedInstallDurableOutcome::NoEffect
                ));
            }
        }
    }

    #[tokio::test]
    async fn settlement_checkpoint_rejects_missing_or_suppressed_intent_write() {
        for rejection in ["missing", "ignored", "aborted"] {
            let (_root, storage, library, exclusions, _owner, queue) = fixture();
            let pin = library.admit().unwrap();
            let _writer = exclusions
                .try_acquire(
                    std::iter::empty::<String>(),
                    [library_artifact(&pin.library_id().to_string())],
                )
                .unwrap();
            let id = queue
                .enqueue(InstallQueueRequest::Vanilla {
                    version_id: "checkpoint-fixture".into(),
                })
                .await
                .unwrap()
                .started_install
                .unwrap()
                .install_id;
            storage.transaction(|db| {
                if rejection == "missing" {
                    db.execute("DELETE FROM install_queue WHERE id=?1", [&id])?;
                } else {
                    let action = if rejection == "ignored" { "IGNORE" } else { "ABORT,'fixture rejection'" };
                    db.execute_batch(&format!("CREATE TRIGGER reject_checkpoint BEFORE UPDATE OF checkpoint_json ON install_queue BEGIN SELECT RAISE({action}); END;"))?;
                }
                Ok::<_, InstallError>(())
            }).unwrap();
            let checkpoint = PublicationCheckpoint {
                kind: CheckpointKind::RolledBack,
                version_id: "checkpoint-fixture".into(),
                evidence_id: "fixture".into(),
                contract_id: None,
            };
            assert_eq!(
                queue.persist_checkpoint(&id, &checkpoint),
                Err(InstallError::Storage)
            );
            let saved: Option<Option<String>> = storage
                .read(|db| {
                    db.query_row(
                        "SELECT checkpoint_json FROM install_queue WHERE id=?1",
                        [&id],
                        |row| row.get(0),
                    )
                    .optional()
                    .map_err(InstallError::from)
                })
                .unwrap();
            assert_eq!(
                saved,
                if rejection == "missing" {
                    None
                } else {
                    Some(None)
                }
            );
        }
    }

    #[tokio::test]
    async fn malformed_recorded_client_digest_is_not_reported_as_corrupt_bytes() {
        let (root, storage, library, _exclusions, owner, queue) = fixture();
        let version = "recorded-digest-fixture";
        install_ready_fixture(&queue, version).await;
        let pin = library.admit().unwrap();
        assert!(queue.ready_version(&pin, version).await.is_ok());
        let relative = format!("versions/{version}/{version}.jar");
        let client = root.path().join(&relative);
        let before = std::fs::read(&client).unwrap();
        storage.transaction(|db| {
            let encoded: String = db.query_row(
                "SELECT inventory_json FROM installed_versions WHERE library_id=?1 AND version_id=?2 AND state='ready'",
                params![pin.library_id().to_string(), version], |row| row.get(0),
            )?;
            let mut inventory: serde_json::Value = serde_json::from_str(&encoded).unwrap();
            let entry = inventory["files"].as_array_mut().unwrap().iter_mut()
                .find(|entry| entry["path"] == relative).unwrap();
            entry["sha1"] = serde_json::Value::String("z".repeat(40));
            assert_eq!(db.execute(
                "UPDATE installed_versions SET inventory_json=?1 WHERE library_id=?2 AND version_id=?3 AND state='ready'",
                params![inventory.to_string(), pin.library_id().to_string(), version],
            )?, 1);
            Ok::<_, InstallError>(())
        }).unwrap();
        let observed = queue.ready_version(&pin, version).await;
        let after = std::fs::read(&client).unwrap();
        queue.shutdown_queued().unwrap();
        owner
            .shutdown(std::time::Duration::from_secs(1))
            .await
            .unwrap();

        assert_eq!(after, before);
        assert!(
            matches!(observed, Err(InstallError::NotReady)),
            "{observed:?}"
        );
    }

    #[tokio::test]
    async fn exact_publication_is_not_ready_before_activation_and_acknowledgement() {
        let (root, storage, library, exclusions, owner, queue) = fixture();
        let pin = library.admit().unwrap();
        let _writer = exclusions
            .try_acquire(
                std::iter::empty::<String>(),
                [library_artifact(&pin.library_id().to_string())],
            )
            .unwrap();
        let operation = pin.managed_library().unwrap();
        let version_id = "verified-install-fixture";
        let id = queue
            .enqueue(InstallQueueRequest::Vanilla {
                version_id: version_id.into(),
            })
            .await
            .unwrap()
            .started_install
            .unwrap()
            .install_id;
        let receipt = axial_minecraft::download::publish_managed_install_fixture_for_test(
            operation.clone(),
            version_id,
        )
        .await
        .unwrap();
        assert!(matches!(
            queue.ready_version(&pin, version_id).await,
            Err(InstallError::NotReady)
        ));
        assert!(
            queue
                .settle_receipt(&id, &pin, operation.clone(), receipt)
                .await
                .is_ok()
        );
        let ready = queue.ready_version(&pin, version_id).await.unwrap();
        assert_eq!(ready.version().id, version_id);
        assert!(ready.revalidate().is_ok());
        assert!(matches!(
            axial_minecraft::classify_managed_install_publication(operation, version_id).await,
            axial_minecraft::ManagedInstallDurableOutcome::NoEffect
        ));
        let restarted = InstallQueue::new(
            storage,
            library,
            exclusions,
            owner,
            queue.runtime_cache().clone(),
        )
        .unwrap();
        assert!(restarted.ready_version(&pin, version_id).await.is_ok());
        std::fs::write(
            root.path()
                .join(format!("versions/{version_id}/{version_id}.jar")),
            b"changed",
        )
        .unwrap();
        assert!(ready.revalidate().is_err());
        assert!(matches!(
            restarted.ready_version(&pin, version_id).await,
            Err(InstallError::ClientJarCorrupt)
        ));
    }

    #[tokio::test]
    async fn queued_removal_has_no_payload_effect_and_persists_terminal_status() {
        let (root, storage, library, exclusions, owner, queue) = fixture();
        let pin = library.admit().unwrap();
        let blocker = exclusions
            .try_acquire(
                std::iter::empty::<String>(),
                [library_artifact(&pin.library_id().to_string())],
            )
            .unwrap();
        let before = std::fs::read_dir(root.path()).unwrap().count();
        let queued = queue
            .enqueue(InstallQueueRequest::Vanilla {
                version_id: "queued-fixture".into(),
            })
            .await
            .unwrap();
        assert!(queued.active.is_none());
        assert_eq!(queued.items.len(), 1);
        let id = queued.items[0].queue_id.clone();
        assert_eq!(queued.items[0].remove_action.action, "remove_from_queue");
        assert!(queue.remove(&id).await.unwrap().items.is_empty());
        assert_eq!(
            queue.status(&id).unwrap().outcome,
            Some(InstallOutcome::Removed)
        );
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), before);
        drop(blocker);
        let restarted = InstallQueue::new(
            storage,
            library,
            exclusions,
            owner,
            queue.runtime_cache().clone(),
        )
        .unwrap();
        assert_eq!(
            restarted.status(&id).unwrap().outcome,
            Some(InstallOutcome::Removed)
        );
    }

    #[tokio::test]
    async fn interrupted_before_publication_is_requeued_without_inventing_completion() {
        let (_root, storage, library, exclusions, owner, queue) = fixture();
        let pin = library.admit().unwrap();
        let writer = exclusions
            .try_acquire(
                std::iter::empty::<String>(),
                [library_artifact(&pin.library_id().to_string())],
            )
            .unwrap();
        let accepted = queue
            .enqueue(InstallQueueRequest::Vanilla {
                version_id: "interrupted-before-effect".into(),
            })
            .await
            .unwrap();
        let id = accepted.items[0].queue_id.clone();
        storage
            .transaction(|db| {
                db.execute(
                    "UPDATE install_queue SET phase='running' WHERE id=?1",
                    [&id],
                )?;
                Ok::<_, InstallError>(())
            })
            .unwrap();
        queue.shutdown_queued().unwrap();
        drop(writer);
        let restarted = InstallQueue::new(
            storage,
            library,
            exclusions,
            owner,
            queue.runtime_cache().clone(),
        )
        .unwrap();
        assert!(restarted.has_unsettled_effects());
        // Keep recovered admission paused so this case inspects the durable
        // requeue boundary without starting an external download.
        restarted.close_admission();
        restarted.recover_interrupted().await.unwrap();
        let snapshot = restarted.snapshot();
        assert_ne!(snapshot.queue_epoch, accepted.queue_epoch);
        assert!(!restarted.has_unsettled_effects());
        assert_eq!(snapshot.items.len(), 1);
        assert_eq!(restarted.status(&id).unwrap().outcome, None);
        restarted.remove(&id).await.unwrap();
        restarted.shutdown_queued().unwrap();
    }

    #[tokio::test]
    async fn acknowledged_install_crash_before_terminal_status_recovers_exact_intent() {
        let (_root, storage, library, exclusions, owner, queue) = fixture();
        let pin = library.admit().unwrap();
        let writer = exclusions
            .try_acquire(
                std::iter::empty::<String>(),
                [library_artifact(&pin.library_id().to_string())],
            )
            .unwrap();
        let version = "interrupted-after-acknowledgement";
        let accepted = queue
            .enqueue(InstallQueueRequest::Vanilla {
                version_id: version.into(),
            })
            .await
            .unwrap();
        let id = accepted.items[0].queue_id.clone();
        let operation = pin.managed_library().unwrap();
        let receipt = axial_minecraft::download::publish_managed_install_fixture_for_test(
            operation.clone(),
            version,
        )
        .await
        .unwrap();
        assert!(
            queue
                .settle_receipt(&id, &pin, operation, receipt)
                .await
                .is_ok()
        );
        storage
            .transaction(|db| {
                db.execute(
                    "UPDATE install_queue SET phase='running' WHERE id=?1",
                    [&id],
                )?;
                Ok::<_, InstallError>(())
            })
            .unwrap();
        queue.shutdown_queued().unwrap();
        drop(writer);
        let restarted = InstallQueue::new(
            storage,
            library,
            exclusions,
            owner,
            queue.runtime_cache().clone(),
        )
        .unwrap();
        recover_fixture(&restarted).await.unwrap();
        assert_eq!(
            restarted.status(&id).unwrap().outcome,
            Some(InstallOutcome::Succeeded)
        );
        assert_eq!(restarted.snapshot().registry_revision, 2);
        assert!(!restarted.has_unsettled_effects());
        assert!(restarted.ready_version(&pin, version).await.is_ok());
    }

    #[cfg(unix)]
    const FD_FIXTURE_CHILD: &str = "AXIAL_INSTALL_FD_FIXTURE_CHILD";

    #[cfg(unix)]
    pub(crate) fn bounded_fd_child_command() -> std::process::Command {
        use std::os::unix::process::CommandExt;

        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "install::queue::tests::large_install_inventory_uses_bounded_file_descriptors",
                "--nocapture",
            ])
            .env(FD_FIXTURE_CHILD, "1");
        // Limit only the isolated child; parallel tests retain their own
        // process limits. No installed/user directory enters this fixture.
        unsafe {
            command.pre_exec(|| {
                let limit = libc::rlimit {
                    rlim_cur: 128,
                    rlim_max: 128,
                };
                if libc::setrlimit(libc::RLIMIT_NOFILE, &limit) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        command
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn large_install_inventory_uses_bounded_file_descriptors() {
        use sha1::Digest;
        if std::env::var_os(FD_FIXTURE_CHILD).is_none() {
            let result = bounded_fd_child_command().output().unwrap();
            assert!(
                result.status.success(),
                "bounded-FD fixture failed: {} {}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
            return;
        }
        let (root, _storage, library, exclusions, _owner, queue) = fixture();
        let pin = library.admit().unwrap();
        let _writer = exclusions
            .try_acquire(
                std::iter::empty::<String>(),
                [library_artifact(&pin.library_id().to_string())],
            )
            .unwrap();
        let operation = pin.managed_library().unwrap();
        let version = "bounded-file-descriptors";
        let id = queue
            .enqueue(InstallQueueRequest::Vanilla {
                version_id: version.into(),
            })
            .await
            .unwrap()
            .started_install
            .unwrap()
            .install_id;
        let receipt = axial_minecraft::download::publish_managed_install_fixture_for_test(
            operation.clone(),
            version,
        )
        .await
        .unwrap();
        assert!(
            queue
                .settle_receipt(&id, &pin, operation, receipt)
                .await
                .is_ok()
        );
        let source =
            axial_minecraft::known_good::managed_version_bundle_activation_source_fixture_for_test(
                version,
            )
            .unwrap();
        let mut projection = ActivatedVersion::from_source(&source);
        for index in 0..512 {
            let relative = format!("assets/objects/fixture-{:04}/object-{index:04}", index / 4);
            let target = root.path().join(&relative);
            std::fs::create_dir_all(target.parent().unwrap()).unwrap();
            std::fs::write(target, b"x").unwrap();
            projection
                .files
                .push(super::super::artifacts::ActivatedFile {
                    path: relative,
                    sha1: format!("{:x}", sha1::Sha1::digest(b"x")),
                    size: 1,
                });
        }
        let receipt = projection
            .verify(pin)
            .expect("512 observed assets below 128 descriptor limit");
        receipt.revalidate().unwrap();
    }
    #[test]
    fn terminal_status_is_separate_from_leaf_publication_progress() {
        for (phase, label, current, total, percent) in [
            ("libraries", "Downloading libraries", 10, 10, 99),
            ("game_publish", "Installing game files", 0, 1, 0),
            ("game_publish", "Installing game files", 1, 1, 99),
        ] {
            let event = DownloadProgress {
                phase: phase.into(),
                current,
                total,
                file: Some("private/path".into()),
                error: None,
                done: false,
                bytes_done: None,
                bytes_total: None,
            };
            assert_eq!(
                serde_json::to_value(progress_view(&event)).unwrap(),
                serde_json::json!({
                    "phase_id": phase, "label": label, "progress_pct": percent,
                    "terminal": false, "failed": false,
                    "active_step": {
                        "phase_id": phase, "label": label, "progress_pct": percent,
                        "current": current, "total": total
                    }
                })
            );
        }
        assert!(!settlement_progress().terminal);
    }
    #[test]
    fn activation_projection_never_confers_filesystem_authority() {
        let malformed: Value =
            serde_json::json!({"version_id":"1.0","contract_id":"invalid","files":[],"ready":true});
        assert!(serde_json::from_value::<ActivatedVersion>(malformed).is_err());
    }
}
