//! Create selection and setup projections. Provider records, installed scans,
//! registry publication and installation keep their separate authorities.

use super::{
    create::{CreateInstanceRequest, CreateTarget, InstanceService, SetupIntent},
    model::{
        EnrichedInstance, Instance, InstanceError, InstanceLaunchAction, InstanceResult,
        InstanceVersionDisplay,
    },
};
use super::{directory::RegisteredInstance, model::InstanceId};
use crate::content::{
    catalog::ContentService,
    install::{ContentMutations, MutationError, PlannedFile},
    resolve::{ContentResolution, ResolutionSelection, ResolutionTarget},
    view::{ResolutionPlan, TargetRef, into_plan, preview_draft},
};
use crate::storage::{MetadataStore, StorageError, rusqlite::OptionalExtension};
use crate::{
    catalog::{Catalog, installed_versions},
    install::{
        model::{
            InstallQueueContentActionRequest, InstallQueueInstallItemViewModel,
            InstallQueueLoaderItemViewModel, InstallQueueRequest, InstallQueueStateResponse,
        },
        queue::InstallQueue,
    },
    settings::SettingsStore,
    tasks::{CancellationToken, TaskHandle},
};
use axial_minecraft::{
    VersionEntry,
    loaders::{self, LoaderBuildRecord, LoaderComponentId, LoaderSelectionReason},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
};

pub struct SetupService {
    pub(super) instances: Arc<InstanceService>,
    catalog: Arc<Catalog>,
    pub(super) installs: Arc<InstallQueue>,
    settings: Arc<SettingsStore>,
    launch: Arc<crate::launch::coordinator::LaunchCoordinator>,
    pub(super) content: Option<(Arc<ContentService>, Arc<ContentMutations>)>,
    plans: Mutex<BTreeMap<String, StoredSetupPlan>>,
    pub(super) pending: Mutex<BTreeMap<InstanceId, RegisteredInstance>>,
    #[cfg(test)]
    loader_catalog_fixture: Option<readiness_tests::LoaderCatalogFixture>,
}

type QueuedSetup = (Instance, InstallQueueStateResponse, CreateResultView);

/// The queue schedules this accepted setup, while instances retains its exact
/// plan and completion authority. This value has no queue or scheduler owner.
#[derive(Clone)]
pub(crate) struct SetupWork {
    instances: Arc<InstanceService>,
    content: Arc<ContentService>,
    mutations: Arc<ContentMutations>,
    admitted: RegisteredInstance,
    stored: StoredSetupIntent,
    request_json: String,
    prepared_pack: Option<super::from_pack::PreparedPackCreation>,
}

impl SetupWork {
    pub(crate) fn instance(&self) -> &RegisteredInstance {
        &self.admitted
    }

    pub(crate) fn request(&self) -> InstallQueueRequest {
        self.stored.queue_request(&self.admitted.record().instance)
    }

    pub(crate) fn prerequisite(&self) -> InstallQueueRequest {
        self.stored.prerequisite().clone()
    }

    pub(crate) async fn remove(
        &self,
        operation_id: uuid::Uuid,
    ) -> InstanceResult<Option<InstanceId>> {
        use super::delete::DeletionStatus;
        let snapshot = self
            .instances
            .delete_pristine_setup_admitted(self.admitted.clone(), operation_id)
            .map_err(|_| InstanceError::SettlementRequired)?
            .join()
            .await
            .map_err(|_| InstanceError::SettlementRequired)?
            .map_err(|_| InstanceError::SettlementRequired)?;
        match snapshot {
            Some(snapshot) if snapshot.status == DeletionStatus::Removed => {
                Ok(Some(snapshot.instance_id))
            }
            Some(snapshot) if snapshot.status == DeletionStatus::Aborted => Ok(None),
            None => Ok(None),
            Some(_) => Err(InstanceError::SettlementRequired),
        }
    }

    /// The queue verifies the exact runtime prerequisite before entering this
    /// method. File publication and incomplete receipts remain content-owned.
    pub(crate) async fn execute(
        &self,
        cancel: &CancellationToken,
        progress: Arc<dyn Fn(axial_minecraft::DownloadProgress) + Send + Sync>,
    ) -> InstanceResult<()> {
        if cancel.is_cancelled() {
            return Err(InstanceError::Cancelled);
        }
        self.admitted.validate_current()?;
        let content = self.content.with_cancellation(cancel.clone());
        let mutations = self.mutations.as_ref().clone().with_progress(progress);
        match &self.stored {
            StoredSetupIntent::Content(stored) => {
                if self.admitted.record().instance.version_id != stored.version_id {
                    return Err(InstanceError::Conflict);
                }
                let installed = match &stored.artifacts {
                    Some(artifacts) => {
                        if artifact_fingerprint(artifacts)? != stored.fingerprint
                            || artifact_selections(artifacts) != stored.selections
                        {
                            return Err(InstanceError::Conflict);
                        }
                        mutations
                            .installed_content_admitted(&self.admitted, artifacts)
                            .map_err(|_| InstanceError::Conflict)?
                    }
                    None => false,
                };
                if !installed {
                    let plan = mutations
                        .plan_admitted(&content, &self.admitted, &stored.selections)
                        .await
                        .map_err(|_| InstanceError::SetupUnavailable)?;
                    if content_fingerprint(plan.resolution())? != stored.fingerprint {
                        return Err(InstanceError::Conflict);
                    }
                    mutations
                        .install_admitted(self.admitted.clone(), plan, false)
                        .map_err(|_| InstanceError::SetupUnavailable)?
                        .join()
                        .await
                        .map_err(|_| InstanceError::SettlementRequired)?
                        .map_err(|_| InstanceError::SettlementRequired)?;
                }
            }
            StoredSetupIntent::Modpack(stored) => {
                stored
                    .install(
                        &self.admitted,
                        &content,
                        &mutations,
                        self.prepared_pack.clone(),
                        cancel,
                    )
                    .await?;
            }
        }
        self.instances.registry().storage().transaction(|tx| -> InstanceResult<()> {
            let changed = tx.execute(
                "UPDATE instance_setups SET phase='complete' WHERE instance_id=?1 AND phase='pending' AND request_json=?2",
                (self.admitted.record().instance.id.as_str(), &self.request_json),
            )?;
            if changed == 0 {
                let complete: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM instance_setups WHERE instance_id=?1 AND phase='complete' AND request_json=?2)",
                    (self.admitted.record().instance.id.as_str(), &self.request_json), |row| row.get(0),
                )?;
                if !complete { return Err(InstanceError::Conflict); }
            }
            Ok(())
        })
    }
}

impl SetupService {
    pub fn new(
        instances: Arc<InstanceService>,
        catalog: Arc<Catalog>,
        installs: Arc<InstallQueue>,
        settings: Arc<SettingsStore>,
        launch: Arc<crate::launch::coordinator::LaunchCoordinator>,
    ) -> Self {
        Self {
            instances,
            catalog,
            installs,
            settings,
            launch,
            content: None,
            plans: Mutex::new(BTreeMap::new()),
            pending: Mutex::new(BTreeMap::new()),
            #[cfg(test)]
            loader_catalog_fixture: None,
        }
    }

    pub fn with_content(
        mut self,
        content: Arc<ContentService>,
        mutations: Arc<ContentMutations>,
    ) -> Self {
        self.content = Some((content, mutations));
        self
    }

    pub async fn plan_setup(
        &self,
        request: InstanceSetupPlanRequest,
    ) -> InstanceResult<InstanceSetupPlanResponse> {
        if request.selections.is_empty() || request.selections.len() > 40 {
            return Err(InstanceError::InvalidInput);
        }
        let (content, _) = self
            .content
            .as_ref()
            .ok_or(InstanceError::SetupUnavailable)?;
        let (target, install) = self.resolve(&request.selection_id).await?;
        let resolution_target = ResolutionTarget {
            loader: target.loader_key.clone(),
            game_version: target.minecraft_version.clone(),
            supports_mods: target.loader_key != "vanilla",
        };
        match request.target {
            TargetRef::Draft {
                loader,
                game_version,
            } if game_version == resolution_target.game_version
                && loader.as_deref().unwrap_or("vanilla") == resolution_target.loader => {}
            _ => return Err(InstanceError::Conflict),
        }
        let expires_at_ms = now_ms().saturating_add(5 * 60 * 1000);
        let resolution = preview_draft(content, &resolution_target, &request.selections)
            .await
            .map_err(|_| InstanceError::SetupUnavailable)?;
        let plan = into_plan(&resolution, None, &resolution_target);
        let plan_id = if resolution.conflicts.is_empty() {
            let mut plans = self.plans.lock().expect("setup plans lock poisoned");
            plans.retain(|_, plan| plan.expires_at_ms > now_ms());
            if plans.len() >= 128 {
                return Err(InstanceError::Busy);
            }
            let id = uuid::Uuid::new_v4().to_string();
            plans.insert(
                id.clone(),
                StoredSetupPlan {
                    selection_id: request.selection_id.clone(),
                    version_id: target.version_id.clone(),
                    target: resolution_target,
                    // Pin the entire approved closure, including unversioned
                    // dependencies. A later provider release cannot change an
                    // accepted setup into a request for different artifacts.
                    selections: frozen_selections(&resolution),
                    artifacts: Some(approved_artifacts(&resolution)?),
                    install,
                    fingerprint: content_fingerprint(&resolution)?,
                    expires_at_ms,
                    create: None,
                },
            );
            Some(id)
        } else {
            None
        };
        Ok(InstanceSetupPlanResponse {
            plan_id,
            expires_at_ms,
            selection_id: request.selection_id,
            plan,
        })
    }

    pub async fn execute_setup(
        self: &Arc<Self>,
        request: InstanceSetupExecuteRequest,
    ) -> InstanceResult<CreateInstanceResponse> {
        if uuid::Uuid::parse_str(&request.plan_id).is_err() {
            return Err(InstanceError::InvalidInput);
        }
        let persisted = self
            .instances
            .registry()
            .storage()
            .read(|db| -> InstanceResult<_> {
                Ok(db
                    .query_row(
                        "SELECT instance_id,request_json FROM instance_setups WHERE plan_id=?1",
                        [&request.plan_id],
                        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                    )
                    .optional()?)
            })?;
        if let Some((id, json)) = persisted {
            let stored: StoredSetupPlan =
                serde_json::from_str(&json).map_err(|_| InstanceError::SettlementRequired)?;
            if stored.create.as_ref() != Some(&request.create) {
                return Err(InstanceError::Conflict);
            }
            return self.resume_setup(&id.parse()?).await;
        }
        let mut stored = self
            .plans
            .lock()
            .expect("setup plans lock poisoned")
            .get(&request.plan_id)
            .cloned()
            .ok_or(InstanceError::Conflict)?;
        if stored.expires_at_ms <= now_ms() || stored.selection_id != request.create.selection_id {
            return Err(InstanceError::Conflict);
        }
        let (content, _) = self
            .content
            .as_ref()
            .ok_or(InstanceError::SetupUnavailable)?;
        let (target, install) = self.resolve(&stored.selection_id).await?;
        if target.version_id != stored.version_id || install != stored.install {
            return Err(InstanceError::Conflict);
        }
        let current = preview_draft(content, &stored.target, &stored.selections)
            .await
            .map_err(|_| InstanceError::SetupUnavailable)?;
        if content_fingerprint(&current)? != stored.fingerprint {
            return Err(InstanceError::Conflict);
        }
        if self
            .plans
            .lock()
            .expect("setup plans lock poisoned")
            .remove(&request.plan_id)
            .is_none()
        {
            return Err(InstanceError::Busy);
        }
        stored.create = Some(request.create.clone());
        let setup = SetupIntent {
            plan_id: request.plan_id,
            request_json: serde_json::to_string(&stored)
                .map_err(|_| InstanceError::InvalidInput)?,
        };
        let service = Arc::clone(self);
        let work = self
            .instances
            .tasks
            .try_spawn((), move |_cancel| async move {
                let admitted = service
                    .instances
                    .create_admitted(request.create, target, setup)?
                    .join()
                    .await
                    .map_err(|_| InstanceError::SettlementRequired)??;
                service.queue_setup(admitted, None, false).await
            })
            .map_err(|_| InstanceError::Closed)?;
        self.finish_setup(work).await
    }

    pub async fn resume_setup(
        self: &Arc<Self>,
        id: &InstanceId,
    ) -> InstanceResult<CreateInstanceResponse> {
        self.resume_setup_with_priority(id, false, None).await
    }

    /// Adapt a retained Downloads retry only when it is exactly the accepted
    /// setup content request. Ordinary content remains queue-owned.
    pub async fn retry_queued_setup(
        self: &Arc<Self>,
        request: &InstallQueueRequest,
    ) -> InstanceResult<Option<InstallQueueStateResponse>> {
        let InstallQueueRequest::Content { instance_id, .. } = request else {
            return Ok(None);
        };
        let id: InstanceId = instance_id.parse()?;
        if !has_pending(self.instances.registry().storage(), &id)? {
            return Ok(None);
        }
        let response = self
            .resume_setup_with_priority(&id, true, Some(request))
            .await?;
        response
            .install_queue
            .map(Some)
            .ok_or(InstanceError::SetupUnavailable)
    }

    async fn resume_setup_with_priority(
        self: &Arc<Self>,
        id: &InstanceId,
        retry: bool,
        expected: Option<&InstallQueueRequest>,
    ) -> InstanceResult<CreateInstanceResponse> {
        let (stored, phase) =
            self.instances
                .registry()
                .storage()
                .read(|db| -> InstanceResult<_> {
                    let (json, phase): (String, String) = db
                        .query_row(
                            "SELECT request_json,phase FROM instance_setups WHERE instance_id=?1",
                            [id.as_str()],
                            |row| Ok((row.get(0)?, row.get(1)?)),
                        )
                        .optional()?
                        .ok_or(InstanceError::NotFound)?;
                    if json.len() > 4 * 1024 * 1024 {
                        return Err(InstanceError::SettlementRequired);
                    }
                    let stored: StoredSetupIntent = serde_json::from_str(&json)
                        .map_err(|_| InstanceError::SettlementRequired)?;
                    Ok((stored, phase))
                })?;
        if let Some(expected) = expected {
            let instance = self.instances.registry().get_live(id)?.instance;
            if &stored.queue_request(&instance) != expected {
                return Err(InstanceError::Conflict);
            }
        }
        let attached = if retry {
            self.installs.retry_attached_setup(id).await
        } else {
            self.installs.resume_attached_setup(id).await
        }
        .map_err(|_| InstanceError::SettlementRequired)?;
        if let Some(snapshot) = attached {
            let instance = self.instances.registry().get_live(id)?.instance;
            let versions = self.installed().await.unwrap_or_default();
            return Ok(CreateInstanceResponse {
                instance: self.enrich(instance, &versions).await,
                install_queue: Some(snapshot),
                view_model: setup_queued_result(),
            });
        }
        if phase == "complete" {
            let instance = self.instances.registry().get_live(id)?.instance;
            let snapshot = self.installs.snapshot();
            let versions = self.installed().await.unwrap_or_default();
            return Ok(CreateInstanceResponse {
                instance: self.enrich(instance, &versions).await,
                install_queue: Some(snapshot),
                view_model: match stored {
                    StoredSetupIntent::Content(_) => setup_result(true),
                    StoredSetupIntent::Modpack(_) => super::from_pack::pack_setup_result(true),
                },
            });
        }
        let (_, mutations) = self
            .content
            .as_ref()
            .ok_or(InstanceError::SetupUnavailable)?;
        if crate::content::install::has_pending(self.instances.registry().storage(), id)? {
            match mutations
                .resume(id)
                .map_err(|_| InstanceError::SettlementRequired)?
                .join()
                .await
                .map_err(|_| InstanceError::SettlementRequired)?
            {
                Ok(_) => {}
                Err(MutationError::Cancelled)
                    if !crate::content::install::has_pending(
                        self.instances.registry().storage(),
                        id,
                    )
                    .map_err(|_| InstanceError::SettlementRequired)? => {}
                Err(_) => return Err(InstanceError::SettlementRequired),
            }
        }
        let retained = self
            .pending
            .lock()
            .expect("setup pending lock poisoned")
            .remove(id);
        let admitted = match retained {
            Some(admitted) => admitted,
            None => self
                .instances
                .directories()
                .admit_for_setup_settlement(id)?,
        };
        let service = Arc::clone(self);
        let rejected = admitted.clone();
        let work = self
            .instances
            .tasks
            .try_spawn(admitted.clone(), move |_cancel| async move {
                service.queue_setup(admitted, None, retry).await
            })
            .map_err(|_| {
                self.pending
                    .lock()
                    .expect("setup pending lock poisoned")
                    .insert(id.clone(), rejected);
                InstanceError::Closed
            })?;
        self.finish_setup(work).await
    }

    pub(super) async fn finish_setup(
        &self,
        work: TaskHandle<InstanceResult<QueuedSetup>>,
    ) -> InstanceResult<CreateInstanceResponse> {
        let installs = self.installs.clone();
        let (sender, receiver) = tokio::sync::oneshot::channel();
        // Accepted work owns every effect and retained guard. This waiter only
        // invalidates read projections after release, even if the caller leaves.
        tokio::spawn(async move {
            let result = match work.join().await {
                Ok(result) => {
                    installs.invalidate_registry();
                    result.map(|(instance, accepted, view)| {
                        let mut snapshot = installs.snapshot();
                        snapshot.started_install = accepted.started_install;
                        snapshot.notice = accepted.notice;
                        snapshot.removed_instance_id = accepted.removed_instance_id;
                        (instance, snapshot, view)
                    })
                }
                Err(_) => Err(InstanceError::SettlementRequired),
            };
            drop(installs);
            let _ = sender.send(result);
        });
        let (instance, snapshot, view_model) = receiver
            .await
            .map_err(|_| InstanceError::SettlementRequired)??;
        let versions = self.installed().await.unwrap_or_default();
        Ok(CreateInstanceResponse {
            instance: self.enrich(instance, &versions).await,
            install_queue: Some(snapshot),
            view_model,
        })
    }

    pub(super) async fn queue_setup(
        &self,
        admitted: RegisteredInstance,
        prepared_pack: Option<super::from_pack::PreparedPackCreation>,
        retry: bool,
    ) -> InstanceResult<QueuedSetup> {
        let (content, mutations) = self
            .content
            .as_ref()
            .ok_or(InstanceError::SetupUnavailable)?;
        let (stored, request_json) = self.instances.registry().storage().read(
            |db| -> InstanceResult<(StoredSetupIntent, String)> {
                let json: String = db.query_row(
                    "SELECT request_json FROM instance_setups WHERE instance_id=?1 AND phase='pending'",
                    [admitted.record().instance.id.as_str()],
                    |row| row.get(0),
                )?;
                if json.len() > 4 * 1024 * 1024 {
                    return Err(InstanceError::SettlementRequired);
                }
                let stored: StoredSetupIntent =
                    serde_json::from_str(&json).map_err(|_| InstanceError::SettlementRequired)?;
                Ok((stored, json))
            },
        )?;
        let work = SetupWork {
            instances: self.instances.clone(),
            content: content.clone(),
            mutations: mutations.clone(),
            admitted: admitted.clone(),
            stored,
            request_json,
            prepared_pack,
        };
        let result = if retry {
            self.installs
                .retry_setup_content(work.request(), work.prerequisite(), work)
                .await
        } else {
            self.installs
                .enqueue_setup_content(work.request(), work.prerequisite(), work)
                .await
        };
        let instance = admitted.record().instance.clone();
        let success = result.is_ok();
        if success {
            drop(admitted);
        } else {
            self.pending
                .lock()
                .expect("setup pending lock poisoned")
                .insert(instance.id.clone(), admitted);
        }
        Ok((
            instance,
            result.unwrap_or_else(|_| self.installs.snapshot()),
            if success {
                setup_queued_result()
            } else {
                setup_result(false)
            },
        ))
    }

    pub fn instances(&self) -> &Arc<InstanceService> {
        &self.instances
    }

    /// Once this exact task owner is joined, a pure setup intent can safely
    /// remain durable across exit. Content/Performance keep ownership of their
    /// own unsettled receipts; setup never clears those or its pending row.
    pub fn release_shutdown_admissions(
        &self,
        receipt: &crate::tasks::ShutdownReceipt,
    ) -> InstanceResult<()> {
        release_setup_admissions(&self.instances, &self.pending, receipt)
    }

    pub async fn initialize(self: &Arc<Self>) -> InstanceResult<SetupStatusResponse> {
        let pin = self
            .instances
            .directories()
            .library()
            .admit()
            .map_err(|_| InstanceError::LibraryUnavailable)?;
        let service = Arc::clone(self);
        let task = self
            .instances
            .tasks
            .try_spawn(pin.clone(), move |cancel| async move {
                tokio::task::spawn_blocking(move || {
                    if cancel.is_cancelled() {
                        return Err(InstanceError::Cancelled);
                    }
                    service.instances.ensure_parent(&pin)?;
                    pin.managed_library()
                        .map_err(|_| InstanceError::LibraryUnavailable)?;
                    Ok(SetupStatusResponse { status: "ok" })
                })
                .await
                .unwrap_or_else(|error| {
                    if error.is_panic() {
                        std::panic::resume_unwind(error.into_panic());
                    }
                    Err(InstanceError::Cancelled)
                })
            })
            .map_err(|_| InstanceError::Closed)?;
        task.join()
            .await
            .map_err(|_| InstanceError::SettlementRequired)?
    }

    pub async fn resolve(
        &self,
        selection_id: &str,
    ) -> InstanceResult<(CreateTarget, InstallQueueRequest)> {
        let selection_id = selection_id.trim();
        if selection_id.len() > 2048 {
            return Err(InstanceError::InvalidInput);
        }
        let parts: Vec<_> = selection_id.split('|').collect();
        let pin = self
            .instances
            .directories()
            .library()
            .admit()
            .map_err(|_| InstanceError::LibraryUnavailable)?;
        let library = pin
            .managed_library()
            .map_err(|_| InstanceError::LibraryUnavailable)?;
        let mut verified_target = None;
        let request = match parts.as_slice() {
            ["vanilla", version] if !version.is_empty() => {
                // Existing verified installations remain selectable offline.
                let installed = installed_versions(&library, None)
                    .await
                    .map_err(|_| InstanceError::VersionUnavailable)?;
                let present = installed.versions.iter().any(|entry| {
                    entry.id == *version
                        && entry.installed
                        && entry.launchable
                        && entry.loader.is_none()
                }) && !loaders::is_canonical_installed_loader_id(version)
                    && self.installs.ready_version(&pin, version).await.is_ok();
                if present {
                    verified_target = Some(InstallQueueInstallItemViewModel {
                        version_id: (*version).to_owned(),
                        loader: None,
                        content: None,
                    });
                } else {
                    self.catalog
                        .resolve_install(&library, version, &CancellationToken::new())
                        .await
                        .map_err(|_| InstanceError::VersionUnavailable)?;
                }
                InstallQueueRequest::Vanilla {
                    version_id: (*version).to_owned(),
                }
            }
            ["loader_build", component, build_id] => {
                let component_id =
                    LoaderComponentId::parse(component).ok_or(InstanceError::InvalidInput)?;
                let target = loader_install_target(component_id, build_id)?;
                if self
                    .installs
                    .ready_version(&pin, &target.version_id)
                    .await
                    .is_ok()
                {
                    verified_target = Some(target);
                }
                InstallQueueRequest::Loader {
                    component_id,
                    build_id: (*build_id).to_owned(),
                }
            }
            ["loader_auto", component, minecraft] => {
                let component_id =
                    LoaderComponentId::parse(component).ok_or(InstanceError::InvalidInput)?;
                let (builds, state) = self
                    .loader_build_catalog(&library, component_id, minecraft)
                    .await
                    .map_err(|_| InstanceError::VersionUnavailable)?;
                let build = preferred_build(builds).ok_or(InstanceError::VersionUnavailable)?;
                let target = loader_install_target(component_id, &build.build_id)?;
                if target.version_id != build.version_id {
                    return Err(InstanceError::Conflict);
                }
                if self
                    .installs
                    .ready_version(&pin, &target.version_id)
                    .await
                    .is_ok()
                {
                    verified_target = Some(target);
                } else if !state.availability.fresh || state.availability.stale {
                    return Err(InstanceError::VersionUnavailable);
                }
                InstallQueueRequest::Loader {
                    component_id,
                    build_id: build.build_id,
                }
            }
            _ => return Err(InstanceError::InvalidInput),
        };
        let resolved = match verified_target {
            Some(target) => target,
            None => self
                .installs
                .resolve_target(&request)
                .await
                .map_err(|_| InstanceError::VersionUnavailable)?,
        };
        let (minecraft_version, loader_key) = match resolved.loader {
            Some(loader) => (
                loader.minecraft_version,
                LoaderComponentId::parse(&loader.component_id)
                    .ok_or(InstanceError::VersionUnavailable)?
                    .short_key()
                    .to_owned(),
            ),
            None => (resolved.version_id.clone(), "vanilla".to_owned()),
        };
        Ok((
            CreateTarget {
                selection_id: selection_id.to_owned(),
                version_id: resolved.version_id,
                minecraft_version,
                loader_key,
            },
            request,
        ))
    }

    /// The accepted create and its queue handoff outlive the HTTP waiter.
    pub async fn create(
        self: &Arc<Self>,
        request: CreateInstanceRequest,
    ) -> InstanceResult<CreateInstanceResponse> {
        let (target, install) = self.resolve(&request.selection_id).await?;
        let service = Arc::clone(self);
        let pin = self
            .instances
            .directories()
            .library()
            .admit()
            .map_err(|_| InstanceError::LibraryUnavailable)?;
        let work = self
            .instances
            .tasks
            .try_spawn(pin.clone(), move |_cancel| async move {
                let instance = service
                    .instances
                    .create(request, target)?
                    .join()
                    .await
                    .map_err(|_| InstanceError::SettlementRequired)??;
                let ready = service
                    .installs
                    .ready_version(&pin, &instance.version_id)
                    .await
                    .is_ok();
                let (install_queue, view_model) = if ready {
                    (
                        None,
                        CreateResultView {
                            state_id: "created",
                            tone: "success",
                            title: "Instance created",
                            summary: "Instance created.",
                            detail: None,
                        },
                    )
                } else {
                    match service.installs.enqueue(install).await {
                        Ok(queue) => (
                            Some(queue),
                            CreateResultView {
                                state_id: "created",
                                tone: "success",
                                title: "Instance created",
                                summary: "Instance created. Installation is queued.",
                                detail: None,
                            },
                        ),
                        Err(_) => (
                            None,
                            CreateResultView {
                                state_id: "created_install_unavailable",
                                tone: "warn",
                                title: "Instance created",
                                summary: "Instance created. Installation could not be queued.",
                                detail: Some("Use Install on this instance to try again."),
                            },
                        ),
                    }
                };
                let versions = service.installed().await.unwrap_or_default();
                let instance = service.enrich(instance, &versions).await;
                Ok(CreateInstanceResponse {
                    instance,
                    install_queue,
                    view_model,
                })
            })
            .map_err(|_| InstanceError::Closed)?;
        work.join()
            .await
            .map_err(|_| InstanceError::SettlementRequired)?
    }

    pub async fn installed(&self) -> InstanceResult<Vec<VersionEntry>> {
        let pin = self
            .instances
            .directories()
            .library()
            .admit()
            .map_err(|_| InstanceError::LibraryUnavailable)?;
        let library = pin
            .managed_library()
            .map_err(|_| InstanceError::LibraryUnavailable)?;
        installed_versions(&library, None)
            .await
            .map(|result| result.versions)
            .map_err(|_| InstanceError::VersionUnavailable)
    }

    /// Display status combines settled install metadata with the current scan.
    /// It is not launch or create admission; those verify the selected payload.
    fn display_installed_ids(
        &self,
        pin: &crate::library::GenerationPin,
        versions: &[VersionEntry],
    ) -> InstanceResult<BTreeSet<String>> {
        let settled = self
            .installs
            .ready_version_ids(pin)
            .map_err(|_| InstanceError::VersionUnavailable)?;
        Ok(versions
            .iter()
            .filter(|entry| entry.installed && entry.launchable && settled.contains(&entry.id))
            .map(|entry| entry.id.clone())
            .collect())
    }

    async fn loader_build_catalog(
        &self,
        library: &axial_minecraft::managed_path::ManagedLibraryOperation,
        component: LoaderComponentId,
        minecraft: &str,
    ) -> Result<(Vec<LoaderBuildRecord>, loaders::LoaderCatalogState), loaders::LoaderError> {
        #[cfg(test)]
        if let Some(fixture) = &self.loader_catalog_fixture {
            return Ok((fixture.builds.clone(), fixture.state.clone()));
        }
        loaders::fetch_builds(library, component, minecraft).await
    }

    async fn loader_version_catalog(
        &self,
        library: &axial_minecraft::managed_path::ManagedLibraryOperation,
        component: LoaderComponentId,
    ) -> Result<
        (
            Vec<axial_minecraft::LoaderGameVersion>,
            loaders::LoaderCatalogState,
        ),
        loaders::LoaderError,
    > {
        #[cfg(test)]
        if let Some(fixture) = &self.loader_catalog_fixture {
            return Ok((fixture.versions.clone(), fixture.state.clone()));
        }
        loaders::fetch_supported_versions(library, component).await
    }

    pub async fn enrich(&self, instance: Instance, versions: &[VersionEntry]) -> EnrichedInstance {
        let preflight = self.launch.preflight(instance.id.clone()).await;
        self.enrich_preflight(instance, versions, preflight)
    }

    pub async fn enrich_all(
        &self,
        instances: Vec<Instance>,
        versions: &[VersionEntry],
    ) -> Vec<EnrichedInstance> {
        let mut pending = instances.into_iter().enumerate().collect::<Vec<_>>();
        // Registry version strings only schedule adjacent work. Launch admission
        // checks the actual generation, library and version before sharing proof.
        pending.sort_by(|(_, left), (_, right)| left.version_id.cmp(&right.version_id));
        let mut projection = self.launch.preflight_projection();
        let mut enriched = Vec::with_capacity(pending.len());
        for (index, instance) in pending {
            let preflight = self
                .launch
                .preflight_with_projection(instance.id.clone(), &mut projection)
                .await;
            enriched.push((index, instance, preflight));
        }
        let scan = self.launch.finish_preflight_projection(&projection).await;
        enriched.sort_unstable_by_key(|(index, _, _)| *index);
        enriched.into_iter().map(|(_, instance, preflight)| {
            let current = scan.clone().and_then(|_| self.launch
                .validate_projection_target(&projection, &instance));
            let preflight = match &current {
                Ok(()) => preflight,
                Err(error) if preflight.launchable || preflight.diagnostics.is_some()
                    || preflight.error.as_ref().is_some_and(|error|
                        error.code == crate::launch::coordinator::LaunchError::InstallUnavailable) =>
                    crate::launch::coordinator::LaunchPreflight::refused(
                    instance.id.clone(), error.clone(),
                ),
                Err(_) => preflight,
            };
            self.enrich_preflight(instance, versions, preflight)
        }).collect()
    }

    fn enrich_preflight(
        &self,
        instance: Instance,
        versions: &[VersionEntry],
        preflight: crate::launch::coordinator::LaunchPreflight,
    ) -> EnrichedInstance {
        let version = versions
            .iter()
            .find(|entry| entry.id == instance.version_id);
        let mut install_target = install_target_for_instance(&instance, version);
        let repair_install = preflight.error.as_ref().is_some_and(|error| {
            error.code == crate::launch::coordinator::LaunchError::InstallUnavailable
        });
        let (setup_pending, setup_available) =
            match has_pending(self.instances.registry().storage(), &instance.id) {
                Ok(pending) => (pending, true),
                Err(_) => (false, false),
            };
        let status_detail = if setup_available {
            preflight.error.map(|error| error.error).unwrap_or_default()
        } else {
            "Instance setup status could not be read. Refresh and try again.".to_owned()
        };
        let launchable = preflight.launchable && setup_available;
        let queued_setup_target = (setup_pending && self.installs.has_setup_work(&instance.id))
            .then(|| setup_queue_target(&self.installs.snapshot(), &instance))
            .flatten();
        let setup_attached = queued_setup_target.is_some();
        if setup_pending {
            // A restored queue row without its accepted setup admission needs
            // an explicit resume. Do not project it as a running installation
            // that would disable the only resume action in the existing UI.
            install_target = queued_setup_target;
        }
        let action = if !setup_available {
            "blocked"
        } else if launchable {
            "launch"
        } else if setup_pending || (repair_install && install_target.is_some()) {
            "install"
        } else {
            "blocked"
        };
        let loader_label = match instance.loader_key.as_str() {
            "fabric" => "Fabric",
            "quilt" => "Quilt",
            "forge" => "Forge",
            "neoforge" => "NeoForge",
            _ => "Vanilla",
        }
        .to_owned();
        let loader_version_label = version
            .and_then(|entry| entry.loader.as_ref())
            .map(|loader| loader.loader_version.clone())
            .unwrap_or_default();
        let minecraft_label = instance.minecraft_version.clone();
        let supports_mods = instance.loader_key != "vanilla";
        EnrichedInstance {
            version_display: InstanceVersionDisplay {
                loader_key: instance.loader_key.clone(),
                loader_label: loader_label.clone(),
                minecraft_label: minecraft_label.clone(),
                loader_version_label: loader_version_label.clone(),
                loader_detail_label: if supports_mods {
                    format!("{loader_label} {loader_version_label}")
                        .trim()
                        .to_owned()
                } else {
                    String::new()
                },
                summary_label: if supports_mods {
                    format!("{minecraft_label}, {loader_label}")
                } else {
                    minecraft_label
                },
                supports_mods,
            },
            launchable,
            launch_action: InstanceLaunchAction {
                state_id: if setup_pending {
                    if setup_attached {
                        "setup_queued"
                    } else {
                        "setup_pending"
                    }
                } else if launchable {
                    "ready"
                } else {
                    action
                }
                .into(),
                label: if launchable {
                    "Launch"
                } else if setup_pending && !setup_attached {
                    "Resume setup"
                } else if action == "blocked" {
                    "Unavailable"
                } else {
                    "Install"
                }
                .into(),
                tone: if launchable { "ok" } else { "warn" }.into(),
                launchable,
                primary_action: action.into(),
                disabled_reason: (action == "blocked").then(|| status_detail.clone()),
            },
            needs_install: if action == "install" {
                instance.version_id.clone()
            } else {
                String::new()
            },
            install_target,
            java_major: version.map(|entry| entry.java_major).unwrap_or_default(),
            status_detail,
            instance: super::create::public_instance(instance),
            // Detailed resource counts belong to the resource endpoint, not
            // list/get/create enrichment or a launch-held mutation admission.
            saves_count: 0,
            mods_count: 0,
            resource_count: 0,
            shader_count: 0,
            counts_available: false,
        }
    }

    pub async fn create_view(&self, source: Option<&str>) -> InstanceResult<CreateView> {
        let source = source
            .filter(|value| !value.is_empty())
            .unwrap_or("vanilla");
        let component = if source == "vanilla" {
            None
        } else {
            Some(LoaderComponentId::parse(source).ok_or(InstanceError::InvalidInput)?)
        };
        let pin = self
            .instances
            .directories()
            .library()
            .admit()
            .map_err(|_| InstanceError::LibraryUnavailable)?;
        let library = pin
            .managed_library()
            .map_err(|_| InstanceError::LibraryUnavailable)?;
        let cancel = CancellationToken::new();
        let installed = installed_versions(&library, None)
            .await
            .map_err(|_| InstanceError::VersionUnavailable)?;
        let displayed_installed = self.display_installed_ids(&pin, &installed.versions)?;
        let mut notices = Vec::new();
        let versions = if let Some(component) = component {
            let (mut versions, state) = self
                .loader_version_catalog(&library, component)
                .await
                .map_err(|_| InstanceError::VersionUnavailable)?;
            self.catalog
                .enrich_loader_versions(&library, &mut versions, &cancel)
                .await;
            let fresh = state.availability.fresh && !state.availability.stale;
            if !fresh {
                notices.push(CreateNotice::catalog_unavailable());
            }
            versions
                .into_iter()
                .map(|entry| {
                    let full = installed.versions.iter().any(|installed| {
                        displayed_installed.contains(&installed.id)
                            && installed_loader_matches(installed, component, &entry.id)
                    });
                    CreateVersionRow {
                        source_id: source.to_owned(),
                        selection_id: format!("loader_auto|{source}|{}", entry.id),
                        minecraft_version_id: entry.id.clone(),
                        display_name: display_name(&entry.id, &entry.minecraft_meta.display_name),
                        hint: nonempty(entry.minecraft_meta.display_hint),
                        channel: entry.lifecycle.channel,
                        download_state: download_state(
                            displayed_installed.contains(&entry.id),
                            full,
                        )
                        .into(),
                        create_enabled: fresh || full,
                        disabled_reason: (!(fresh || full)).then(|| {
                            "Refresh the loader catalog before creating this instance.".into()
                        }),
                    }
                })
                .collect()
        } else {
            let snapshot = self.catalog.snapshot(&library, &cancel).await;
            let fresh = snapshot.catalog_state.fresh;
            if !fresh {
                notices.push(CreateNotice::catalog_unavailable());
            }
            let mut entries = snapshot.versions;
            for entry in &installed.versions {
                if entry.loader.is_none()
                    && !entries.iter().any(|candidate| candidate.id == entry.id)
                {
                    entries.push(entry.clone());
                }
            }
            entries
                .into_iter()
                .map(|entry| {
                    let present = displayed_installed.contains(&entry.id);
                    CreateVersionRow {
                        source_id: "vanilla".into(),
                        selection_id: format!("vanilla|{}", entry.id),
                        minecraft_version_id: entry.id.clone(),
                        display_name: display_name(&entry.id, &entry.minecraft_meta.display_name),
                        hint: nonempty(entry.minecraft_meta.display_hint),
                        channel: entry.lifecycle.channel,
                        download_state: download_state(present, present).into(),
                        create_enabled: fresh || present,
                        disabled_reason: (!(fresh || present)).then(|| {
                            "Refresh the version catalog before creating this instance.".into()
                        }),
                    }
                })
                .collect()
        };
        let config = self
            .settings
            .current()
            .map_err(|_| InstanceError::InvalidSettings)?;
        let mut sources = vec![CreateOption {
            id: "vanilla".into(),
            label: "Vanilla".into(),
            enabled: true,
        }];
        sources.extend(
            loaders::fetch_components()
                .into_iter()
                .map(|component| CreateOption {
                    id: component.id.as_str().into(),
                    label: component.name,
                    enabled: true,
                }),
        );
        Ok(CreateView {
            sources,
            channels: [
                ("stable", "Stable"),
                ("preview", "Preview"),
                ("experimental", "Experimental"),
                ("legacy", "Legacy"),
                ("unknown", "Other"),
            ]
            .into_iter()
            .map(|(id, label)| CreateOption {
                id: id.into(),
                label: label.into(),
                enabled: true,
            })
            .collect(),
            versions,
            preset_options: preset_options(),
            optimize_option: OptimizeOption {
                id: "auto_optimize",
                label: "Optimize automatically",
                detail: "Apply the selected Performance settings when the instance is prepared.",
                default_enabled: true,
            },
            notices,
            defaults: CreateDefaults {
                source_id: source.into(),
                channel_id: "stable",
                jvm_preset_id: config.jvm_preset.as_str().into(),
                max_memory_mb: config.max_memory_mb,
                window_width: config.window_width,
                window_height: config.window_height,
            },
        })
    }

    pub async fn loader_builds(
        &self,
        source: &str,
        minecraft: &str,
    ) -> InstanceResult<CreateLoaderBuildsView> {
        let component = LoaderComponentId::parse(source).ok_or(InstanceError::InvalidInput)?;
        let pin = self
            .instances
            .directories()
            .library()
            .admit()
            .map_err(|_| InstanceError::LibraryUnavailable)?;
        let library = pin
            .managed_library()
            .map_err(|_| InstanceError::LibraryUnavailable)?;
        let (builds, catalog) = self
            .loader_build_catalog(&library, component, minecraft)
            .await
            .map_err(|_| InstanceError::VersionUnavailable)?;
        let installed = installed_versions(&library, None)
            .await
            .map_err(|_| InstanceError::VersionUnavailable)?;
        let displayed_installed = self.display_installed_ids(&pin, &installed.versions)?;
        let fresh = catalog.availability.fresh && !catalog.availability.stale;
        let preferred = preferred_build(builds.clone());
        let auto_enabled = preferred
            .as_ref()
            .is_some_and(|build| fresh || displayed_installed.contains(&build.version_id));
        Ok(CreateLoaderBuildsView {
            source_id: source.into(),
            minecraft_version_id: minecraft.into(),
            auto: AutoBuildOption {
                selection_id: format!("loader_auto|{source}|{minecraft}"),
                label: "Automatic".into(),
                detail: "Use the preferred compatible loader build.".into(),
                enabled: auto_enabled,
                disabled_reason: (!auto_enabled).then(|| {
                    if preferred.is_none() {
                        "No compatible loader build is available for this Minecraft version."
                    } else {
                        "Refresh the loader catalog before installing the automatic build."
                    }
                    .into()
                }),
            },
            builds: builds
                .into_iter()
                .map(|build| {
                    let present = displayed_installed.contains(&build.version_id);
                    let unstable = unstable_build(&build);
                    let disabled_reason = if incompatible_quilt_build(
                        build.component_id,
                        &build.minecraft_version,
                        &build.loader_version,
                    ) {
                        Some(format!(
                            "This {} build is known to be incompatible with Minecraft {}.",
                            build.component_id.display_name(),
                            build.minecraft_version
                        ))
                    } else if !fresh && !present {
                        Some("Refresh the loader catalog before installing this build.".into())
                    } else {
                        None
                    };
                    BuildOption {
                        selection_id: format!("loader_build|{source}|{}", build.build_id),
                        recommended: preferred
                            .as_ref()
                            .is_some_and(|preferred| preferred.build_id == build.build_id),
                        build_id: build.build_id,
                        label: build.loader_version,
                        channel_id: if unstable { "beta" } else { "stable" },
                        channel_label: if unstable { "Beta" } else { "Stable" },
                        installed: present,
                        enabled: disabled_reason.is_none(),
                        disabled_reason,
                    }
                })
                .collect(),
        })
    }
}

fn nonempty(value: String) -> Option<String> {
    (!value.is_empty()).then_some(value)
}
fn display_name(id: &str, label: &str) -> String {
    if label.is_empty() { id } else { label }.to_owned()
}

fn download_state(base_installed: bool, full_installed: bool) -> &'static str {
    if full_installed {
        "full"
    } else if base_installed {
        "base"
    } else {
        "none"
    }
}

fn installed_loader_matches(
    entry: &VersionEntry,
    component: LoaderComponentId,
    minecraft: &str,
) -> bool {
    if let Ok(identity) = loaders::api::decode_installed_version_id(&entry.id) {
        return identity.component_id() == component && identity.minecraft_version() == minecraft;
    }
    entry.loader.as_ref().is_some_and(|loader| {
        loader.component_id == component
            && (entry.inherits_from == minecraft
                || loaders::parse_build_id(&loader.build_id)
                    .is_some_and(|(found, version, _)| found == component && version == minecraft))
    })
}

fn loader_install_target(
    component: LoaderComponentId,
    build_id: &str,
) -> InstanceResult<InstallQueueInstallItemViewModel> {
    let (found, minecraft_version, loader_version) = loaders::parse_build_id(build_id)
        .filter(|(found, _, _)| *found == component)
        .ok_or(InstanceError::InvalidInput)?;
    if incompatible_quilt_build(found, &minecraft_version, &loader_version) {
        return Err(InstanceError::VersionUnavailable);
    }
    let version_id = loaders::installed_version_id_for(found, &minecraft_version, &loader_version)
        .map_err(|_| InstanceError::InvalidInput)?;
    Ok(InstallQueueInstallItemViewModel {
        version_id,
        content: None,
        loader: Some(InstallQueueLoaderItemViewModel {
            component_id: component.as_str().into(),
            build_id: build_id.into(),
            minecraft_version,
            loader_version,
        }),
    })
}

/// The registered selection survives a failed or removed install even when no
/// installed profile or queue entry remains. This only projects retry input;
/// the install queue still resolves the build against provider authority.
fn install_target_for_instance(
    instance: &Instance,
    version: Option<&VersionEntry>,
) -> Option<InstallQueueInstallItemViewModel> {
    let loader =
        if let Ok(identity) = loaders::api::decode_installed_version_id(&instance.version_id) {
            let component = identity.component_id();
            if component.short_key() != instance.loader_key
                || identity.minecraft_version() != instance.minecraft_version
            {
                return None;
            }
            Some(InstallQueueLoaderItemViewModel {
                component_id: component.as_str().to_owned(),
                build_id: loaders::build_id_for(
                    component,
                    identity.minecraft_version(),
                    identity.loader_version(),
                ),
                minecraft_version: identity.minecraft_version().to_owned(),
                loader_version: identity.loader_version().to_owned(),
            })
        } else if let Some(loader) = version.and_then(|entry| entry.loader.as_ref()) {
            // Preserve retry support for installed profiles recognized by the
            // version owner, including profiles with an older identity format.
            Some(InstallQueueLoaderItemViewModel {
                component_id: loader.component_id.as_str().to_owned(),
                build_id: loader.build_id.clone(),
                minecraft_version: instance.minecraft_version.clone(),
                loader_version: loader.loader_version.clone(),
            })
        } else if instance.loader_key == "vanilla" {
            None
        } else {
            return None;
        };
    let version_id = if loader.is_none() {
        version
            .map(|entry| entry.needs_install.as_str())
            .filter(|target| !target.is_empty())
            .unwrap_or(&instance.version_id)
            .to_owned()
    } else {
        instance.version_id.clone()
    };
    Some(InstallQueueInstallItemViewModel {
        version_id,
        loader,
        content: None,
    })
}

fn stable_build(build: &LoaderBuildRecord) -> bool {
    matches!(
        build.build_meta.selection.reason,
        LoaderSelectionReason::Recommended
            | LoaderSelectionReason::LatestStable
            | LoaderSelectionReason::Stable
            | LoaderSelectionReason::Unlabeled
    )
}

fn unstable_build(build: &LoaderBuildRecord) -> bool {
    matches!(
        build.build_meta.selection.reason,
        LoaderSelectionReason::Latest
            | LoaderSelectionReason::LatestUnstable
            | LoaderSelectionReason::Unstable
    )
}

fn incompatible_quilt_build(
    component: LoaderComponentId,
    minecraft_version: &str,
    loader_version: &str,
) -> bool {
    let minecraft = minecraft_version.trim();
    let loader = loader_version.trim();
    component == LoaderComponentId::Quilt
        && (minecraft == "26" || minecraft.starts_with("26."))
        && axial_minecraft::compare_version_like(loader, "0.30.0").is_lt()
        && !loader.starts_with("0.30.")
}

fn preferred_build(builds: Vec<LoaderBuildRecord>) -> Option<LoaderBuildRecord> {
    let mut compatible: Vec<_> = builds
        .into_iter()
        .filter(|build| {
            !incompatible_quilt_build(
                build.component_id,
                &build.minecraft_version,
                &build.loader_version,
            )
        })
        .collect();
    let index = compatible
        .iter()
        .position(stable_build)
        .or_else(|| compatible.iter().position(unstable_build))?;
    Some(compatible.remove(index))
}

#[derive(Serialize)]
pub struct SetupStatusResponse {
    pub status: &'static str,
}
#[derive(Serialize)]
pub struct CreateInstanceResponse {
    #[serde(flatten)]
    pub instance: EnrichedInstance,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub install_queue: Option<InstallQueueStateResponse>,
    pub view_model: CreateResultView,
}
#[derive(Serialize)]
pub struct CreateResultView {
    pub state_id: &'static str,
    pub tone: &'static str,
    pub title: &'static str,
    pub summary: &'static str,
    pub detail: Option<&'static str>,
}
#[derive(Serialize)]
pub struct CreateOption {
    pub id: String,
    pub label: String,
    pub enabled: bool,
}
#[derive(Serialize)]
pub struct CreateNotice {
    pub state_id: &'static str,
    pub tone: &'static str,
    pub message: &'static str,
}
impl CreateNotice {
    fn catalog_unavailable() -> Self {
        Self {
            state_id: "catalog_unavailable",
            tone: "warn",
            message: "The catalog could not be refreshed. Previously installed versions remain available.",
        }
    }
}
#[derive(Serialize)]
pub struct CreateVersionRow {
    pub source_id: String,
    pub selection_id: String,
    pub minecraft_version_id: String,
    pub display_name: String,
    pub hint: Option<String>,
    pub channel: axial_minecraft::LifecycleChannel,
    pub download_state: String,
    pub create_enabled: bool,
    pub disabled_reason: Option<String>,
}
#[derive(Serialize)]
pub struct CreateDefaults {
    pub source_id: String,
    pub channel_id: &'static str,
    pub jvm_preset_id: String,
    pub max_memory_mb: i32,
    pub window_width: i32,
    pub window_height: i32,
}
#[derive(Serialize)]
pub struct OptimizeOption {
    pub id: &'static str,
    pub label: &'static str,
    pub detail: &'static str,
    pub default_enabled: bool,
}
#[derive(Serialize)]
pub struct PresetOption {
    pub id: &'static str,
    pub label: &'static str,
    pub detail: &'static str,
    pub default: bool,
}
#[derive(Serialize)]
pub struct CreateView {
    pub sources: Vec<CreateOption>,
    pub channels: Vec<CreateOption>,
    pub versions: Vec<CreateVersionRow>,
    pub preset_options: Vec<PresetOption>,
    pub optimize_option: OptimizeOption,
    pub notices: Vec<CreateNotice>,
    pub defaults: CreateDefaults,
}
#[derive(Serialize, ts_rs::TS)]
pub struct AutoBuildOption {
    pub selection_id: String,
    pub label: String,
    pub detail: String,
    pub enabled: bool,
    pub disabled_reason: Option<String>,
}
#[derive(Serialize, ts_rs::TS)]
pub struct BuildOption {
    pub selection_id: String,
    pub build_id: String,
    pub label: String,
    pub channel_id: &'static str,
    pub channel_label: &'static str,
    pub recommended: bool,
    pub installed: bool,
    pub enabled: bool,
    pub disabled_reason: Option<String>,
}
#[derive(Serialize, ts_rs::TS)]
pub struct CreateLoaderBuildsView {
    pub source_id: String,
    pub minecraft_version_id: String,
    pub auto: AutoBuildOption,
    pub builds: Vec<BuildOption>,
}

fn preset_options() -> Vec<PresetOption> {
    [
        (
            "",
            "Automatic",
            "Choose a preset for this Minecraft and Java version.",
        ),
        ("smooth", "Smooth", "Favor consistent frame pacing."),
        ("performance", "Performance", "Favor throughput."),
        (
            "ultra_low_latency",
            "Ultra low latency",
            "Favor low pause times on supported Java runtimes.",
        ),
        ("graalvm", "GraalVM", "Use a compatible GraalVM runtime."),
        ("legacy", "Legacy", "Settings for older Minecraft versions."),
        (
            "legacy_pvp",
            "Legacy PvP",
            "Settings for legacy PvP versions.",
        ),
        (
            "legacy_heavy",
            "Legacy heavy",
            "Settings for larger legacy modpacks.",
        ),
    ]
    .into_iter()
    .map(|(id, label, detail)| PresetOption {
        id,
        label,
        detail,
        default: id.is_empty(),
    })
    .collect()
}

pub fn has_pending(storage: &MetadataStore, id: &InstanceId) -> Result<bool, StorageError> {
    storage.read(|db| {
        Ok(db.query_row(
            "SELECT EXISTS(SELECT 1 FROM instance_setups WHERE instance_id=?1 AND phase='pending')",
            [id.as_str()],
            |row| row.get(0),
        )?)
    })
}

pub(super) fn release_setup_admissions(
    instances: &InstanceService,
    pending: &Mutex<BTreeMap<InstanceId, RegisteredInstance>>,
    receipt: &crate::tasks::ShutdownReceipt,
) -> InstanceResult<()> {
    if !receipt.belongs_to(&instances.tasks) {
        return Err(InstanceError::Busy);
    }
    let mut pending = pending.lock().expect("setup pending lock poisoned");
    let mut unsafe_pending = false;
    pending.retain(|id, _| {
        let unsettled = crate::content::install::has_pending(instances.registry().storage(), id)
            .and_then(|content| {
                crate::performance::mutation::has_pending(instances.registry().storage(), id)
                    .map(|performance| content || performance)
            })
            .unwrap_or(true);
        unsafe_pending |= unsettled;
        unsettled
    });
    if unsafe_pending {
        Err(InstanceError::SettlementRequired)
    } else {
        Ok(())
    }
}

/// Content records predate the tagged modpack branch and retain their original
/// JSON shape. Unknown fields must not let an unsupported tagged intent fall
/// through to the older content workflow.
#[derive(Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub(super) enum StoredSetupIntent {
    Modpack(super::from_pack::StoredPackSetup),
    Content(StoredSetupPlan),
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct StoredSetupPlan {
    selection_id: String,
    version_id: String,
    target: ResolutionTarget,
    selections: Vec<ResolutionSelection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    artifacts: Option<Vec<PlannedFile>>,
    install: InstallQueueRequest,
    fingerprint: String,
    expires_at_ms: u64,
    create: Option<CreateInstanceRequest>,
}

impl StoredSetupIntent {
    fn prerequisite(&self) -> &InstallQueueRequest {
        match self {
            Self::Content(stored) => &stored.install,
            Self::Modpack(stored) => stored.prerequisite(),
        }
    }

    fn queue_request(&self, instance: &Instance) -> InstallQueueRequest {
        match self {
            Self::Content(stored) => InstallQueueRequest::Content {
                instance_id: instance.id.to_string(),
                label: format!("Setting up {}", instance.name),
                action: InstallQueueContentActionRequest::Install {
                    selections: stored.selections.clone(),
                    allow_incompatible: false,
                },
            },
            Self::Modpack(stored) => stored.queue_request(instance),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstanceSetupPlanRequest {
    pub selection_id: String,
    pub target: TargetRef,
    pub selections: Vec<ResolutionSelection>,
}

#[derive(Serialize)]
pub struct InstanceSetupPlanResponse {
    pub plan_id: Option<String>,
    pub expires_at_ms: u64,
    pub selection_id: String,
    pub plan: ResolutionPlan,
}

pub struct InstanceSetupExecuteRequest {
    pub plan_id: String,
    pub create: CreateInstanceRequest,
}

impl<'de> Deserialize<'de> for InstanceSetupExecuteRequest {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct FlatRequest;
        impl<'de> serde::de::Visitor<'de> for FlatRequest {
            type Value = InstanceSetupExecuteRequest;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a setup plan id and flat instance creation fields")
            }

            fn visit_map<M: serde::de::MapAccess<'de>>(
                self,
                mut map: M,
            ) -> Result<Self::Value, M::Error> {
                use serde::de::Error;
                let mut fields = serde_json::Map::new();
                while let Some((key, value)) = map.next_entry::<String, serde_json::Value>()? {
                    if fields.insert(key, value).is_some() {
                        return Err(M::Error::custom("duplicate setup field"));
                    }
                }
                let plan_id = serde_json::from_value(
                    fields
                        .remove("plan_id")
                        .ok_or_else(|| M::Error::missing_field("plan_id"))?,
                )
                .map_err(M::Error::custom)?;
                // Deserialize the remaining flat fields through their single
                // owner. Serde flatten does not enforce its deny_unknown_fields.
                let create = serde_json::from_value(serde_json::Value::Object(fields))
                    .map_err(M::Error::custom)?;
                Ok(InstanceSetupExecuteRequest { plan_id, create })
            }
        }
        deserializer.deserialize_map(FlatRequest)
    }
}

fn now_ms() -> u64 {
    chrono::Utc::now().timestamp_millis().max(0) as u64
}

fn content_fingerprint(resolution: &ContentResolution) -> InstanceResult<String> {
    if !resolution.conflicts.is_empty() {
        return Err(InstanceError::Conflict);
    }
    artifact_fingerprint(&approved_artifacts(resolution)?)
}

fn approved_artifacts(resolution: &ContentResolution) -> InstanceResult<Vec<PlannedFile>> {
    resolution
        .items
        .iter()
        .map(|item| {
            item.to_planned()
                .map_err(|_| InstanceError::SetupUnavailable)
        })
        .collect()
}

fn artifact_fingerprint(files: &[PlannedFile]) -> InstanceResult<String> {
    let mut artifacts = files
        .iter()
        .map(|item| {
            (
                item.canonical_id.as_str(),
                item.provider.as_str(),
                item.project_id.as_str(),
                item.kind.as_str(),
                item.version_id.as_str(),
                item.file.filename.as_str(),
                item.file.size,
                item.file
                    .sha512
                    .as_ref()
                    .map(|hash| hash.to_ascii_lowercase()),
                item.file
                    .sha1
                    .as_ref()
                    .map(|hash| hash.to_ascii_lowercase()),
            )
        })
        .collect::<Vec<_>>();
    artifacts.sort_unstable();
    Ok(hex::encode(Sha256::digest(
        serde_json::to_vec(&artifacts).map_err(|_| InstanceError::InvalidInput)?,
    )))
}

fn artifact_selections(files: &[PlannedFile]) -> Vec<ResolutionSelection> {
    files
        .iter()
        .map(|file| ResolutionSelection {
            canonical_id: file.canonical_id.as_str().to_owned(),
            kind: file.kind,
            version_id: Some(file.version_id.clone()),
        })
        .collect()
}

fn frozen_selections(resolution: &ContentResolution) -> Vec<ResolutionSelection> {
    resolution
        .items
        .iter()
        .map(|item| ResolutionSelection {
            canonical_id: item.canonical_id.as_str().to_owned(),
            kind: item.kind,
            version_id: Some(item.version_id.clone()),
        })
        .collect()
}

fn setup_result(success: bool) -> CreateResultView {
    if success {
        CreateResultView {
            state_id: "setup_complete",
            tone: "success",
            title: "Instance created",
            summary: "Instance created and selected content installed.",
            detail: None,
        }
    } else {
        CreateResultView {
            state_id: "setup_pending",
            tone: "warn",
            title: "Instance setup incomplete",
            summary: "The instance was created, but its content or installation setup needs to be resumed.",
            detail: Some("This instance stays unavailable until its accepted setup is completed."),
        }
    }
}

fn setup_queued_result() -> CreateResultView {
    CreateResultView {
        state_id: "setup_queued",
        tone: "success",
        title: "Instance created",
        summary: "Instance setup is queued. Required game files install before its content.",
        detail: None,
    }
}

fn setup_queue_target(
    queue: &InstallQueueStateResponse,
    instance: &Instance,
) -> Option<InstallQueueInstallItemViewModel> {
    if let Some(active) = &queue.active {
        let target = &active.install_item;
        let same_content = target
            .content
            .as_ref()
            .is_some_and(|content| content.instance_id == instance.id.as_str());
        if same_content && active.progress.phase_id == "settlement_required" {
            return None;
        }
        if same_content || (target.content.is_none() && target.version_id == instance.version_id) {
            return Some(target.clone());
        }
    }
    queue.items.iter().find_map(|entry| {
        entry
            .install_item
            .content
            .as_ref()
            .filter(|content| content.instance_id == instance.id.as_str())
            .map(|_| entry.install_item.clone())
    })
}

#[cfg(test)]
#[path = "setup_readiness_tests.rs"]
mod readiness_tests;

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::content::{
        model::{CanonicalId, ContentKind, FileRef, ProviderId},
        resolve::{ResolutionReason, ResolvedContentItem},
    };

    fn loader_instance(component: LoaderComponentId) -> Instance {
        Instance {
            id: InstanceId::new(),
            name: component.display_name().into(),
            version_id: loaders::installed_version_id_for(component, "1.21.4", "exact-build.7")
                .unwrap(),
            created_at: "2026-09-27T09:00:00Z".into(),
            last_played_at: String::new(),
            art_seed: 42,
            settings: crate::settings::InstanceSettings::default(),
            icon: String::new(),
            accent: String::new(),
            loader_key: component.short_key().into(),
            minecraft_version: "1.21.4".into(),
            revision: 0,
        }
    }

    #[test]
    fn registered_loader_retry_target_survives_restart_without_an_installed_version() {
        use crate::instances::directory::{MIGRATION, Registry};
        let migrations = [MIGRATION, crate::instances::create::MIGRATION];
        let temp = tempfile::Builder::new()
            .tempdir_in(std::env::temp_dir().canonicalize().unwrap())
            .unwrap();
        let path = temp.path().join("metadata.sqlite3");
        {
            let storage = Arc::new(MetadataStore::open(&path).unwrap());
            storage.migrate(&migrations).unwrap();
            let registry = Registry::new(storage);
            for component in [
                LoaderComponentId::Fabric,
                LoaderComponentId::Quilt,
                LoaderComponentId::Forge,
                LoaderComponentId::NeoForge,
            ] {
                registry
                    .storage()
                    .transaction(|tx| {
                        let reserved =
                            registry.reserve(tx, loader_instance(component), "test-library")?;
                        registry.commit_reserved(tx, &reserved, "owner-verified-test-receipt")
                    })
                    .unwrap();
            }
        }
        let storage = Arc::new(MetadataStore::open(&path).unwrap());
        storage.migrate(&migrations).unwrap();
        let registry = Registry::new(storage);
        let records = registry.list().unwrap();
        assert_eq!(records.len(), 4);
        for record in records {
            // Ordinary creation has no durable content-setup intent. A retry
            // must work from the registry after the old queue/profile is gone.
            assert!(!has_pending(registry.storage(), &record.instance.id).unwrap());
            let target = install_target_for_instance(&record.instance, None).unwrap();
            assert_eq!(target.version_id, record.instance.version_id);
            assert!(target.content.is_none());
            let loader = target.loader.unwrap();
            let component = LoaderComponentId::parse(&loader.component_id).unwrap();
            assert_eq!(component.short_key(), record.instance.loader_key);
            assert_eq!(loader.minecraft_version, "1.21.4");
            assert_eq!(loader.loader_version, "exact-build.7");
            assert_eq!(
                loaders::parse_build_id(&loader.build_id),
                Some((component, "1.21.4".into(), "exact-build.7".into()))
            );
        }
    }

    #[test]
    fn malformed_or_mismatched_loader_selection_has_no_vanilla_retry_target() {
        let mut instance = loader_instance(LoaderComponentId::Fabric);
        instance.loader_key = "quilt".into();
        assert!(install_target_for_instance(&instance, None).is_none());
        instance.loader_key = "fabric".into();
        instance.minecraft_version = "1.20.1".into();
        assert!(install_target_for_instance(&instance, None).is_none());
        instance.version_id = "loader-v2-invalid".into();
        assert_eq!(
            serde_json::to_value(install_target_for_instance(&instance, None)).unwrap(),
            serde_json::Value::Null
        );
    }

    #[test]
    fn recognized_installed_loader_attachment_keeps_its_opaque_retry_build() {
        let mut instance = loader_instance(LoaderComponentId::Fabric);
        instance.version_id = "fabric-loader-0.16.14-1.21.4".into();
        let version: VersionEntry = serde_json::from_value(serde_json::json!({
            "id": instance.version_id, "launchable": false, "installed": true,
            "status": "incomplete", "needs_install": "1.21.4",
            "loader": {"component_id": "net.fabricmc.fabric-loader", "component_name": "Fabric",
                "build_id": "retained-opaque-build", "loader_version": "0.16.14"}
        }))
        .unwrap();
        let target = install_target_for_instance(&instance, Some(&version)).unwrap();
        assert_eq!(target.version_id, instance.version_id);
        let loader = target.loader.unwrap();
        assert_eq!(loader.build_id, "retained-opaque-build");
        assert_eq!(loader.minecraft_version, "1.21.4");
        assert_eq!(loader.loader_version, "0.16.14");
    }

    #[test]
    fn vanilla_retry_keeps_the_version_owners_missing_base_target() {
        let mut instance = loader_instance(LoaderComponentId::Fabric);
        instance.version_id = "custom-profile".into();
        instance.loader_key = "vanilla".into();
        let version: VersionEntry = serde_json::from_value(serde_json::json!({
            "id": instance.version_id, "launchable": false, "installed": true,
            "status": "incomplete", "needs_install": "1.21.4"
        }))
        .unwrap();
        let target = install_target_for_instance(&instance, Some(&version)).unwrap();
        assert_eq!(target.version_id, "1.21.4");
        assert!(target.loader.is_none());
        assert!(target.content.is_none());
    }

    fn approved_content() -> ContentResolution {
        let item = |id: &str, reason| ResolvedContentItem {
            canonical_id: CanonicalId::for_project(ProviderId::Modrinth, id),
            provider: ProviderId::Modrinth,
            project_id: id.into(),
            kind: ContentKind::Mod,
            version_id: format!("{id}-v1"),
            version_number: "1.0".into(),
            title: id.into(),
            file: FileRef {
                url: format!("https://cdn.modrinth.com/data/{id}/test.jar"),
                filename: format!("{id}.jar"),
                sha1: None,
                sha512: Some("a".repeat(128)),
                size: Some(17),
                primary: true,
            },
            dependencies: Vec::new(),
            reason,
            already_installed: false,
            update: false,
        };
        ContentResolution {
            items: vec![
                item("selected", ResolutionReason::Selected),
                item("dependency", ResolutionReason::Dependency),
            ],
            conflicts: Vec::new(),
        }
    }

    pub(crate) async fn pending_content_work(
        instances: Arc<InstanceService>,
        content: Arc<ContentService>,
        mutations: Arc<ContentMutations>,
    ) -> SetupWork {
        let mut resolution = approved_content();
        for item in &mut resolution.items {
            item.kind = ContentKind::ResourcePack;
            item.file.filename = format!("{}.zip", item.project_id);
            item.file.size = Some(7);
            item.file.sha512 = Some(hex::encode(sha2::Sha512::digest(b"fixture")));
        }
        let stored = StoredSetupIntent::Content(StoredSetupPlan {
            selection_id: "vanilla|1.21.4".into(),
            version_id: "1.21.4".into(),
            target: ResolutionTarget {
                loader: "vanilla".into(),
                game_version: "1.21.4".into(),
                supports_mods: false,
            },
            selections: frozen_selections(&resolution),
            artifacts: Some(approved_artifacts(&resolution).unwrap()),
            install: InstallQueueRequest::Vanilla {
                version_id: "1.21.4".into(),
            },
            fingerprint: content_fingerprint(&resolution).unwrap(),
            expires_at_ms: 123,
            create: None,
        });
        let request_json = serde_json::to_string(&stored).unwrap();
        let admitted = instances
            .create_admitted(
                super::super::create::tests::request("Queued content fixture"),
                super::super::create::tests::target(),
                SetupIntent {
                    plan_id: uuid::Uuid::new_v4().to_string(),
                    request_json: request_json.clone(),
                },
            )
            .unwrap()
            .join()
            .await
            .unwrap()
            .unwrap();
        SetupWork {
            instances,
            content,
            mutations,
            admitted,
            stored,
            request_json,
            prepared_pack: None,
        }
    }

    #[tokio::test]
    async fn settled_content_completes_setup_offline_but_never_accepts_changed_evidence() {
        use crate::{
            content::provenance::{ContentManifest, MANIFEST_FILE, ManifestEntry},
            network::{ClientConfig, OriginPolicy, ProviderClient},
        };
        for drift in ["none", "file", "missing", "manifest", "intent", "witness"] {
            let (root, instances) = super::super::create::tests::fixture();
            let instances = Arc::new(instances);
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let origin = format!("http://{}", listener.local_addr().unwrap());
            let client = ProviderClient::new(ClientConfig::default()).unwrap();
            let content = Arc::new(
                ContentService::with_base_url(
                    client.clone(),
                    &origin,
                    OriginPolicy::loopback_for_tests([&origin], 0).unwrap(),
                )
                .unwrap(),
            );
            let mutations = Arc::new(ContentMutations::new(
                instances.directories().clone(),
                client,
                instances.tasks.clone(),
            ));
            let mut work = pending_content_work(instances.clone(), content, mutations).await;
            let id = work.instance().record().instance.id.clone();
            let directory = root.path().join("instances").join(id.as_str());
            let StoredSetupIntent::Content(stored) = &mut work.stored else {
                unreachable!()
            };
            let files = stored.artifacts.as_mut().unwrap();
            let mut manifest = ContentManifest::default();
            for file in files.iter() {
                let entry = ManifestEntry::managed(
                    file.canonical_id.clone(),
                    file.provider,
                    file.project_id.clone(),
                    file.version_id.clone(),
                    file.kind,
                    &file.file,
                    file.dependencies.clone(),
                    file.title.clone(),
                )
                .unwrap();
                manifest.try_upsert(entry).unwrap();
                std::fs::write(
                    directory.join("resourcepacks").join(&file.file.filename),
                    b"fixture",
                )
                .unwrap();
            }
            let payload = directory
                .join("resourcepacks")
                .join(&files[0].file.filename);
            let mut encoded: serde_json::Value =
                serde_json::from_slice(&manifest.encode_managed().unwrap()).unwrap();
            match drift {
                "file" => std::fs::write(&payload, b"changed").unwrap(),
                "missing" => std::fs::remove_file(&payload).unwrap(),
                "manifest" => {
                    encoded["entries"][0]["version_id"] = serde_json::json!("other-version")
                }
                "intent" => instances
                    .registry()
                    .storage()
                    .transaction(|tx| {
                        tx.execute(
                            "UPDATE instance_setups SET request_json='{}' WHERE instance_id=?1",
                            [id.as_str()],
                        )?;
                        Ok::<_, StorageError>(())
                    })
                    .unwrap(),
                "witness" => files[0].file.sha512 = Some("b".repeat(128)),
                _ => {}
            }
            std::fs::write(
                directory.join(MANIFEST_FILE),
                serde_json::to_vec(&encoded).unwrap(),
            )
            .unwrap();
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(1),
                work.execute(&CancellationToken::new(), Arc::new(|_| {})),
            )
            .await
            .expect("local completion proof must not wait for provider data");
            assert_eq!(result.is_ok(), drift == "none", "drift={drift}: {result:?}");
            assert_eq!(
                has_pending(instances.registry().storage(), &id).unwrap(),
                drift != "none"
            );
            assert!(
                matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock),
                "no provider connection is needed for settled evidence or an explicit mismatch"
            );
            assert_eq!(instances.registry().list().unwrap().len(), 1);
        }
    }

    #[test]
    fn setup_projection_tracks_prerequisite_content_and_actionable_settlement() {
        let instance = loader_instance(LoaderComponentId::Fabric);
        let runtime = install_target_for_instance(&instance, None).unwrap();
        let content = serde_json::json!({
            "version_id": instance.version_id,
            "content": {"instance_id": instance.id, "label": "Setting up Fixture", "action": {
                "kind":"install", "selections":[{"canonical_id":"modrinth:fixture", "kind":"mod", "version_id":"v1"}]
            }}
        });
        let mut queue: InstallQueueStateResponse = serde_json::from_value(serde_json::json!({
            "queue_epoch":"fixture", "revision":1, "registry_revision":0,
            "active": {"queue_id":"runtime", "kind":"loader", "title":"Runtime", "label":"Runtime", "summary":"Installing",
                "install_item":runtime, "progress":{"phase_id":"libraries", "label":"Libraries", "progress_pct":30, "terminal":false, "failed":false}},
            "items":[{"queue_id":"content", "state_id":"queued", "kind":"content", "title":"Content", "label":"Content",
                "summary":"Waiting", "detail":"", "position":1, "total":1, "install_item":content,
                "remove_action":{"action":"remove_from_queue", "label":"Remove", "enabled":true}}],
            "view_model":{"state_id":"active", "status_label":"Installing", "title":"Downloads", "summary":"", "queued_count":1,
                "queued_count_label":"1 queued", "queued_item_label":"Content", "section_title":"Queue", "empty_title":"Empty", "empty_summary":""}
        })).unwrap();
        assert_eq!(setup_queue_target(&queue, &instance), Some(runtime));
        queue.active.as_mut().unwrap().install_item = queue.items.remove(0).install_item;
        assert!(
            setup_queue_target(&queue, &instance)
                .unwrap()
                .content
                .is_some()
        );
        queue.active.as_mut().unwrap().progress.phase_id = "settlement_required".into();
        assert!(setup_queue_target(&queue, &instance).is_none());
        queue.active.as_mut().unwrap().progress.phase_id = "content_commit".into();
        queue
            .active
            .as_mut()
            .unwrap()
            .install_item
            .content
            .as_mut()
            .unwrap()
            .instance_id = InstanceId::new().to_string();
        assert!(
            setup_queue_target(&queue, &instance).is_none(),
            "unrelated content must not disable Resume setup"
        );
    }

    #[test]
    fn retained_flat_setup_request_deserializes_without_accepting_paths() {
        let request: InstanceSetupExecuteRequest = serde_json::from_value(serde_json::json!({
            "plan_id": "847aa4f8-e4e1-456e-9b8f-2532bd3287bb", "name": "With content",
            "selection_id": "vanilla|1.21.4", "icon": "", "accent": "", "max_memory_mb": 4096
        }))
        .unwrap();
        assert_eq!(request.create.name, "With content");
        assert_eq!(request.create.max_memory_mb, Some(4096));
        assert!(
            serde_json::from_value::<InstanceSetupExecuteRequest>(serde_json::json!({
                "plan_id": "847aa4f8-e4e1-456e-9b8f-2532bd3287bb", "name": "With content",
                "selection_id": "vanilla|1.21.4", "path": "/unrelated"
            }))
            .is_err()
        );
        assert!(serde_json::from_str::<InstanceSetupExecuteRequest>(
            r#"{"plan_id":"first","plan_id":"second","name":"Name","selection_id":"vanilla|1.21.4"}"#,
        ).is_err());
        assert!(serde_json::from_str::<InstanceSetupExecuteRequest>(
            r#"{"plan_id":"first","name":"Name","name":"Other","selection_id":"vanilla|1.21.4"}"#,
        ).is_err());
    }

    #[test]
    fn setup_resume_reads_original_content_records_and_tagged_modpack_records() {
        let original = r#"{
            "selection_id":"vanilla|1.21.4","version_id":"1.21.4",
            "target":{"loader":"vanilla","game_version":"1.21.4","supports_mods":false},
            "selections":[],"install":{"kind":"vanilla","version_id":"1.21.4"},
            "fingerprint":"original-content-proof","expires_at_ms":123
        }"#;
        let StoredSetupIntent::Content(content) = serde_json::from_str(original).unwrap() else {
            panic!("original untagged record must retain the content workflow");
        };
        assert_eq!(content.version_id, "1.21.4");
        assert!(content.create.is_none());
        assert!(content.artifacts.is_none());
        let pack = r#"{
            "kind":"modpack","canonical_id":"modrinth:fixture","version_id":"provider-v1",
            "selection_id":"vanilla|1.21.4","installed_version_id":"1.21.4",
            "minecraft_version":"1.21.4","loader_key":"vanilla",
            "archive_fingerprint":"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "install":{"kind":"vanilla","version_id":"1.21.4"}
        }"#;
        assert!(matches!(
            serde_json::from_str::<StoredSetupIntent>(pack).unwrap(),
            StoredSetupIntent::Modpack(_)
        ));
        let mut unknown: serde_json::Value = serde_json::from_str(original).unwrap();
        unknown["kind"] = serde_json::json!("unsupported_setup");
        assert!(serde_json::from_value::<StoredSetupIntent>(unknown).is_err());
        assert!(
            serde_json::from_str::<StoredSetupIntent>(
                &pack.replace("\"modpack\"", "\"unsupported_setup\"")
            )
            .is_err()
        );
    }

    #[test]
    fn accepted_setup_persists_the_exact_dependency_closure_and_base_install() {
        let resolution = approved_content();
        let stored = StoredSetupPlan {
            selection_id: "loader_auto|fabric|1.21.4".into(),
            version_id: "fabric-loader-0.16.14-1.21.4".into(),
            target: ResolutionTarget {
                loader: "fabric".into(),
                game_version: "1.21.4".into(),
                supports_mods: true,
            },
            selections: frozen_selections(&resolution),
            artifacts: Some(approved_artifacts(&resolution).unwrap()),
            install: InstallQueueRequest::Loader {
                component_id: LoaderComponentId::Fabric,
                build_id: "1.21.4|0.16.14".into(),
            },
            fingerprint: content_fingerprint(&resolution).unwrap(),
            expires_at_ms: 123,
            create: None,
        };
        let resumed: StoredSetupPlan =
            serde_json::from_str(&serde_json::to_string(&stored).unwrap()).unwrap();
        assert_eq!(resumed.install, stored.install);
        assert_eq!(resumed.selections.len(), 2);
        assert_eq!(
            resumed.selections[0].version_id.as_deref(),
            Some("selected-v1")
        );
        assert_eq!(
            resumed.selections[1].version_id.as_deref(),
            Some("dependency-v1")
        );
        assert_eq!(resumed.fingerprint, stored.fingerprint);
        let artifacts = resumed.artifacts.as_ref().unwrap();
        assert_eq!(
            artifact_fingerprint(artifacts).unwrap(),
            resumed.fingerprint
        );
        assert_eq!(artifact_selections(artifacts), resumed.selections);
        let instance = loader_instance(LoaderComponentId::Fabric);
        let intent = StoredSetupIntent::Content(resumed);
        assert_eq!(intent.prerequisite(), &stored.install);
        let InstallQueueRequest::Content {
            instance_id,
            action,
            ..
        } = intent.queue_request(&instance)
        else {
            panic!("setup content must have a separate queue request");
        };
        assert_eq!(instance_id, instance.id.as_str());
        assert_eq!(
            action,
            InstallQueueContentActionRequest::Install {
                selections: stored.selections,
                allow_incompatible: false,
            }
        );
    }

    #[test]
    fn setup_proof_ignores_provider_presentation_and_tracks_exact_artifacts() {
        let original = approved_content();
        let proof = content_fingerprint(&original).unwrap();
        let mut changed = original.clone();
        changed.items.reverse();
        for item in &mut changed.items {
            item.title = "Changed display title".into();
            item.version_number = "Updated display label".into();
            item.file.url = "https://cdn.modrinth.com/moved/test.jar".into();
            item.already_installed = true;
            item.update = true;
            item.reason = ResolutionReason::Selected;
        }
        assert_eq!(content_fingerprint(&changed).unwrap(), proof);
        changed.items[0].version_id.push_str("-new-release");
        assert_ne!(content_fingerprint(&changed).unwrap(), proof);
        let mut changed = original;
        changed.items[0].file.sha512 = Some("b".repeat(128));
        assert_ne!(content_fingerprint(&changed).unwrap(), proof);
    }
}
