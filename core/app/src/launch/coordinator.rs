//! Public launch admission. Only this owner may turn a launch request into a
//! prepared session; transport input never contains executable paths or args.

mod error;
mod intents;
mod preflight;
#[cfg(test)]
mod tests;

use error::*;
pub use error::{LaunchError, LaunchErrorResponse};
pub(super) use error::{install_read_error, physical_work_error};
#[cfg(test)]
pub(super) use intents::accepted_for_test;
use intents::*;
pub(super) use intents::{AcceptedIntent, IntentBinding, acknowledge_terminal};
pub use intents::{
    INTENT_MIGRATION, LaunchIntentStatus, LaunchIntentView, LaunchIntents, LaunchRequest,
};
pub use preflight::*;

use crate::instances::model::InstanceId;
use crate::storage::{
    MetadataStore, Migration, StorageError,
    rusqlite::{OptionalExtension, params},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use tokio::sync::watch;
use ts_rs::TS;

use super::session::SessionSnapshot;
use super::{
    model::{LaunchAuthContext, LaunchOptions, LaunchPlanRequest},
    prepare::PreparedSession,
    session::{SessionError, SessionManager},
};
use crate::{
    accounts::{
        directory::AccountDirectory,
        model::{AccountKind, LaunchAuthMode},
        selection::{CapturedAccount, CapturedSelection},
        session::AuthService,
    },
    install::{
        artifacts::{Inspection, InventoryBudget, InventoryEvidence, VersionInspection},
        queue::{InstallError, InstallQueue},
    },
    instances::{
        directory::{InstanceDirectories, ReadInstance, RegisteredInstance},
        model::InstanceError,
    },
    library::{ApplicationRootPin, GenerationPin},
    composition::{CompositionError, CompositionService},
    runtime::{
        discovery::{RuntimeDiscovery, RuntimeObservation, RuntimeProjection},
        model::JavaDiscoveryError,
    },
    settings::{EffectiveLaunchSettings, OverrideOrigin, SettingsStore},
    tasks::{CancellationToken, ExclusionLease, TaskOwner},
};

#[derive(Clone)]
pub struct LaunchCoordinator {
    instances: InstanceDirectories,
    accounts: Arc<AccountDirectory>,
    settings: Arc<SettingsStore>,
    installs: InstallQueue,
    runtimes: RuntimeDiscovery,
    compositions: CompositionService,
    auth: Arc<AuthService>,
    skins: Arc<crate::skins::ProfileMedia>,
    sessions: SessionManager,
    tasks: TaskOwner,
    intents: LaunchIntents,
    telemetry: Option<Arc<crate::telemetry::Telemetry>>,
    #[cfg(test)]
    pub(crate) fresh_install_checks: Arc<std::sync::atomic::AtomicUsize>,
    #[cfg(test)]
    pub(crate) installed_scans: Arc<std::sync::atomic::AtomicUsize>,
}

impl LaunchCoordinator {
    pub fn new(
        instances: InstanceDirectories,
        accounts: Arc<AccountDirectory>,
        settings: Arc<SettingsStore>,
        installs: InstallQueue,
        runtimes: RuntimeDiscovery,
        compositions: CompositionService,
        auth: Arc<AuthService>,
        skins: Arc<crate::skins::ProfileMedia>,
        sessions: SessionManager,
        tasks: TaskOwner,
    ) -> Self {
        Self {
            instances,
            accounts,
            settings,
            installs,
            runtimes,
            compositions,
            auth,
            skins,
            sessions,
            tasks,
            intents: LaunchIntents::new(4096),
            telemetry: None,
            #[cfg(test)]
            fresh_install_checks: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            #[cfg(test)]
            installed_scans: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    pub fn with_telemetry(mut self, telemetry: Arc<crate::telemetry::Telemetry>) -> Self {
        self.telemetry = Some(telemetry);
        self
    }

    /// Composition restores this same owner before other feature recovery.
    pub fn with_intents(mut self, intents: LaunchIntents) -> Self {
        self.intents = intents;
        self
    }

    pub fn intent(&self, key: &str) -> Result<Option<LaunchIntentStatus>, LaunchError> {
        self.intents.snapshot(key)
    }

    pub fn intent_view(&self, key: &str) -> Result<Option<LaunchIntentView>, LaunchError> {
        Ok(self.intent(key)?.map(|status| match status {
            LaunchIntentStatus::Preparing => LaunchIntentView::Preparing,
            LaunchIntentStatus::Accepted { session } => LaunchIntentView::Accepted {
                session: self
                    .sessions
                    .snapshot_by_session_id(&session.session_id)
                    .unwrap_or(session),
            },
            LaunchIntentStatus::Rejected { error } => LaunchIntentView::Rejected {
                error: error.error,
                code: error.code,
            },
            LaunchIntentStatus::Interrupted { session_id } => LaunchIntentView::Interrupted {
                session_id,
                error: LaunchError::Interrupted.to_string(),
                code: LaunchError::Interrupted,
            },
        }))
    }

    fn admit(&self, id: &InstanceId) -> Result<RegisteredInstance, LaunchError> {
        let pin = self
            .instances
            .library()
            .admit()
            .map_err(|_| LaunchError::LibraryUnavailable)?;
        let artifacts = [crate::install::queue::library_artifact(
            &pin.library_id().to_string(),
        )];
        let lease = self
            .instances
            .exclusions()
            .try_acquire_read_artifacts([id.as_str()], artifacts)
            .map_err(|_| LaunchError::InstanceBusy)?;
        let registry = self.instances.registry();
        let record = registry.get_live(id).map_err(instance_error)?;
        InstanceDirectories::admit_record(registry.clone(), record, pin, lease)
            .map_err(instance_error)
    }

    pub async fn launch(&self, request: LaunchRequest) -> Result<SessionSnapshot, LaunchError> {
        let reservation = self.intents.reserve_context(&request, None)?;
        let mut receiver = match reservation {
            ReservedIntent::Existing(receiver) => receiver,
            ReservedIntent::New {
                key,
                session_id,
                receiver,
            } => {
                let admitted = match self.admit(&request.instance_id) {
                    Ok(admitted) => admitted,
                    Err(error) => {
                        self.intents.settle(&key, Err(error.clone()));
                        return Err(error);
                    }
                };
                // The task owner keeps the exact extraction even if a later
                // preparation step panics before handing it to the session.
                let native_retention = Arc::new(Mutex::new(None));
                let library_retention = Arc::new(super::libraries::Retention::new(
                    admitted.game_directory().clone(),
                ));
                let retained = (
                    admitted.clone(),
                    native_retention.clone(),
                    library_retention.clone(),
                );
                let coordinator = self.clone();
                let failure_key = key.clone();
                if let Err(error) = self.tasks.try_spawn(retained, move |cancel| async move {
                    let telemetry = super::session::LaunchAttemptTelemetry::started(
                        coordinator.telemetry.clone(),
                        &admitted.record().instance.loader_key,
                    );
                    let mut sentinel = PreparationSentinel {
                        intents: coordinator.intents.clone(),
                        key: key.clone(),
                        settled: false,
                        telemetry: telemetry.clone(),
                    };
                    let result = match coordinator
                        .prepare(
                            admitted,
                            &request,
                            &cancel,
                            &native_retention,
                            library_retention.clone(),
                            telemetry.clone(),
                        )
                        .await
                    {
                        Ok(prepared) => {
                            let natives = prepared.natives();
                            // Durable acceptance precedes the first possible
                            // process instruction. An interrupted acceptance
                            // can never be retried as a new spawn.
                            let result = coordinator
                                .instances
                                .library()
                                .admit_application_root()
                                .map_err(|_| LaunchError::LibraryUnavailable)
                                .and_then(|application| prepared.intent_binding(&application))
                                .and_then(|binding| coordinator.intents.accept(&key, binding))
                                .and_then(|acceptance| {
                                    coordinator
                                        .sessions
                                        .start_reserved(prepared, session_id, acceptance)
                                        .map_err(session_error)
                                });
                            if result.is_err() {
                                super::prepare::settle_natives(natives).await;
                            }
                            result
                        }
                        Err(error) => Err(error),
                    };
                    super::libraries::settle(library_retention).await;
                    if result.is_err() {
                        telemetry.failure(None);
                    }
                    coordinator.intents.settle(&key, result);
                    sentinel.settled = true;
                }) {
                    let error = if matches!(error, crate::tasks::SpawnError::Closed) {
                        LaunchError::Closed
                    } else {
                        LaunchError::AtCapacity
                    };
                    self.intents.settle(&failure_key, Err(error));
                }
                receiver
            }
        };
        loop {
            match receiver.borrow_and_update().clone() {
                LaunchIntentStatus::Preparing => {}
                LaunchIntentStatus::Accepted { session } => {
                    return Ok(self
                        .sessions
                        .snapshot_by_session_id(&session.session_id)
                        .unwrap_or(session));
                }
                LaunchIntentStatus::Rejected { error } => return Err(error.code),
                LaunchIntentStatus::Interrupted { .. } => return Err(LaunchError::Interrupted),
            }
            receiver.changed().await.map_err(|_| LaunchError::Closed)?;
        }
    }

    /// Captures the account selection and effective settings.
    fn capture(
        &self,
        instance: &crate::instances::model::Instance,
        username: Option<&str>,
    ) -> Result<(CapturedSelection, EffectiveLaunchSettings, String), LaunchError> {
        let selection = CapturedSelection::capture(&self.accounts)
            .map_err(|_| LaunchError::AccountUnavailable)?;
        if let Some(account) = selection.account() {
            if account.kind().launch_mode() != account.launch_auth_mode() {
                return Err(LaunchError::OnlineAccountUnavailable);
            }
            if account.kind() == AccountKind::Offline
                && username
                    .filter(|value| !value.trim().is_empty())
                    .is_some_and(|name| name.trim() != account.display_name())
            {
                return Err(LaunchError::AccountChanged);
            }
        }
        let global = self
            .settings
            .current()
            .map_err(|_| LaunchError::SettingsChanged)?;
        let settings = instance
            .settings
            .effective(&global)
            .map_err(|_| LaunchError::PlanRejected)?;
        super::plan::split_extra_args(&settings.extra_jvm_args)
            .map_err(|_| LaunchError::PlanRejected)?;
        let player_name = selection
            .account()
            .map_or_else(
                || {
                    username
                        .map(str::trim)
                        .filter(|value| !value.is_empty())
                        .unwrap_or_else(|| global.offline_username())
                },
                CapturedAccount::display_name,
            )
            .to_owned();
        Ok((selection, settings, player_name))
    }

    /// Prepares one accepted launch. Account work runs beside the installation
    /// chain; within it the runtime and natives run beside the ordered
    /// game-library and asset effects once the version names its runtime.
    /// Every branch is joined before any result is used, so a failure never
    /// abandons accepted sibling work, and a published native directory is
    /// settled on every refusal.
    #[allow(clippy::too_many_arguments)]
    async fn prepare(
        &self,
        admitted: RegisteredInstance,
        request: &LaunchRequest,
        cancellation: &CancellationToken,
        native_retention: &Mutex<Option<Arc<crate::install::vanilla::PreparedNatives>>>,
        library_retention: Arc<super::libraries::Retention>,
        telemetry: Arc<super::session::LaunchAttemptTelemetry>,
    ) -> Result<PreparedSession, LaunchError> {
        let result = self
            .prepare_admitted(
                admitted,
                request,
                cancellation,
                native_retention,
                library_retention,
                telemetry,
            )
            .await;
        if result.is_err() {
            let natives = native_retention
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone();
            super::prepare::settle_natives(natives).await;
        }
        result
    }

    #[allow(clippy::too_many_arguments)]
    async fn prepare_admitted(
        &self,
        admitted: RegisteredInstance,
        request: &LaunchRequest,
        cancellation: &CancellationToken,
        native_retention: &Mutex<Option<Arc<crate::install::vanilla::PreparedNatives>>>,
        library_retention: Arc<super::libraries::Retention>,
        telemetry: Arc<super::session::LaunchAttemptTelemetry>,
    ) -> Result<PreparedSession, LaunchError> {
        let stages = super::reports::StageTimings::default();
        let host = super::resources::capture_host();
        let (selection, mut effective, player_name) =
            self.capture(&admitted.record().instance, request.username.as_deref())?;
        if let Some(value) = request.max_memory_mb.filter(|value| *value > 0) {
            effective.heap = crate::settings::HeapChoice::Explicit(value);
        }
        if let Some(value) = request.min_memory_mb.filter(|value| *value > 0) {
            effective.min_memory_mb = value;
        }
        let instance = admitted.record().instance.clone();
        if request
            .version_id
            .as_ref()
            .is_some_and(|version| version != &instance.version_id)
        {
            return Err(LaunchError::InstanceChanged);
        }
        // The heap is decided against every other running game's reservation
        // and reserved for this session's life in the same step.
        let (activity, heap) = super::resources::reserve_heap(
            &axial_resource::process_physical_work(),
            effective.heap,
            &host,
        )?;
        let (max_memory_mb, min_memory_mb, _) = effective.heap_bounds(heap.max_mb);
        let mut options = launch_options(&effective, max_memory_mb, min_memory_mb)?;
        let library_operation = admitted
            .generation()
            .managed_library()
            .map_err(|_| LaunchError::LibraryUnavailable)?;
        let library_dir = admitted
            .generation()
            .read_projection()
            .map_err(|_| LaunchError::LibraryUnavailable)?;
        let game_dir = admitted
            .game_directory()
            .read_projection()
            .map_err(|_| LaunchError::InstanceChanged)?;
        let version_guard = axial_minecraft::VersionBundleReadGuard::acquire(&library_operation)
            .map_err(|_| LaunchError::InstallUnavailable)?;

        let account = stages.time("account", "Account", async {
            match selection.account() {
                Some(account) => {
                    let (account, auth, secrets, expires) =
                        self.prepare_auth(account.clone()).await?;
                    Ok::<_, LaunchError>((
                        CapturedSelection::from_account(account),
                        auth,
                        secrets,
                        expires,
                    ))
                }
                None => {
                    selection
                        .validate(&self.accounts)
                        .map_err(|_| LaunchError::AccountChanged)?;
                    Ok((
                        selection.clone(),
                        LaunchAuthContext::offline(player_name.clone()),
                        Vec::new(),
                        None,
                    ))
                }
            }
        });
        let installation = async {
            let (installed, installed_versions) = stages
                .time("installation", "Installed game files", async {
                    let (installed, installed_versions) = tokio::join!(
                        self.installs
                            .ready_version(admitted.generation(), &instance.version_id),
                        crate::catalog::installed_snapshot(admitted.generation()),
                    );
                    let installed = installed.map_err(install_read_error)?;
                    let installed_versions =
                        installed_versions.map_err(|_| LaunchError::LibraryUnavailable)?;
                    if !installed_versions.admits_target(&instance.version_id) {
                        return Err(LaunchError::InstalledVersionsDegraded);
                    }
                    Ok((installed, installed_versions))
                })
                .await?;
            let (installed, game_libraries) = installed
                .prepare_game_libraries(None)
                .await
                .map_err(game_libraries_error)?;
            let version = installed.version();
            let required = axial_minecraft::effective_java_version_for(
                &instance.minecraft_version,
                &version.kind,
                &version.java_version,
            );
            let mut environment = axial_minecraft::default_environment();
            environment
                .features
                .insert("has_custom_resolution".into(), options.resolution.is_some());
            let (composition, runtime, files, natives) = tokio::join!(
                stages.time("composition", "Managed files", async {
                    self.compositions
                        .settled_for_launch(&admitted)
                        .await
                        .map_err(composition_error)
                }),
                stages.time("runtime", "Java runtime", async {
                    let runtime = self
                        .runtimes
                        .prepare_for_launch(&required, &effective.java_path, cancellation)
                        .await
                        .map_err(LaunchError::RuntimeFailure)?;
                    let launcher_args = super::jvm::proven_launcher_args(
                        options.performance_mode,
                        &options.jvm_preset,
                        &runtime.probe,
                        cancellation.cancelled(),
                    )
                    .await;
                    if cancellation.is_cancelled() {
                        return Err(LaunchError::Cancelled);
                    }
                    let launcher_args = launcher_args.map_err(|error| {
                        tracing::warn!(?error, stage = "launcher_args", "Launch plan rejected.");
                        LaunchError::PlanRejected
                    })?;
                    Ok::<_, LaunchError>((runtime, launcher_args))
                }),
                stages.time("game_files", "Game libraries and assets", async {
                    let game_libraries = match game_libraries {
                        Some(inputs) => Some(
                            super::libraries::prepare(
                                inputs,
                                &library_operation,
                                library_retention.clone(),
                                cancellation,
                            )
                            .await?,
                        ),
                        None => None,
                    };
                    let game_assets = super::assets::prepare(
                        &installed,
                        &library_operation,
                        &library_dir,
                        &game_dir,
                        library_retention.clone(),
                        cancellation,
                    )
                    .await?;
                    Ok::<_, LaunchError>((game_libraries, game_assets))
                }),
                stages.time("natives", "Native libraries", async {
                    let natives = installed
                        .prepare_natives(
                            &library_operation,
                            &library_dir,
                            &environment,
                            Some(self.installs.natives()),
                        )
                        .await
                        .map_err(|_| LaunchError::InstallUnavailable)?
                        .map(Arc::new);
                    // Retained before any sibling result is read: every
                    // later refusal settles this exact directory.
                    *native_retention
                        .lock()
                        .unwrap_or_else(|error| error.into_inner()) = natives.clone();
                    Ok::<_, LaunchError>(natives)
                }),
            );
            let composition = composition?;
            let runtime = runtime?;
            let (game_libraries, game_assets) = files?;
            Ok::<_, LaunchError>((
                composition,
                installed,
                installed_versions,
                runtime,
                game_libraries,
                game_assets,
                natives?,
            ))
        };
        let (account, installation) = tokio::join!(account, installation);
let (
            composition,
            installed,
            installed_versions,
            runtime,
            game_libraries,
            game_assets,
            prepared_natives,
        ) = installation?;
        let (selection, auth, secrets, credential_expires_at) = account?;
        let (runtime, launcher_args) = runtime;
        options.launcher_args = launcher_args;
        if cancellation.is_cancelled() {
            return Err(LaunchError::Cancelled);
        }
        let resource_budget = super::resources::capture(
            &host,
            self.sessions.resource_use(),
            self.installs.active_count(),
            max_memory_mb,
            [&library_dir, &game_dir],
        );
        let mut command = stages
            .time("plan", "Launch command", async {
                super::plan::build(LaunchPlanRequest {
                    library_operation,
                    library_dir,
                    target_version_id: instance.minecraft_version.clone(),
                    game_dir,
                    auth,
                    runtime: runtime.probe,
                    managed_launch: runtime.managed,
                    installed,
                    version_guard,
                    prepared_natives,
                    game_assets,
                    settings: options,
                })
                .map_err(|error| {
                    tracing::warn!(?error, stage = "build", "Launch plan rejected.");
                    LaunchError::PlanRejected
                })
            })
            .await?;
        command.game_libraries = game_libraries;
        if cancellation.is_cancelled() {
            return Err(LaunchError::Cancelled);
        }
        let mut scenario = super::reports::LaunchProofScenario::default();
        scenario.performance_mode = effective.performance_mode.as_str().into();
        scenario.version_id = Some(instance.version_id.clone());
        scenario.requested_memory_mb = Some(max_memory_mb);
        PreparedSession::new(
            instance.id,
            instance.version_id,
            command,
            installed_versions,
            self.accounts.clone(),
            selection,
            self.settings.clone(),
            effective.global_config_revision,
            secrets,
            admitted,
            composition,
            scenario,
            resource_budget,
            credential_expires_at,
            telemetry,
            stages.take(),
            activity,
        )
    }

    async fn prepare_auth(
        &self,
        mut capture: CapturedAccount,
    ) -> Result<(CapturedAccount, LaunchAuthContext, Vec<String>, Option<u64>), LaunchError> {
        self.accounts
            .validate_capture(&capture)
            .map_err(|_| LaunchError::AccountChanged)?;
        if capture.launch_auth_mode() == LaunchAuthMode::Offline {
            let auth = LaunchAuthContext::offline(capture.display_name());
            if auth.uuid != capture.minecraft_uuid() {
                return Err(LaunchError::AccountUnavailable);
            }
            return Ok((capture, auth, Vec::new(), None));
        }
        if !capture.owns_minecraft_java() {
            return Err(LaunchError::OnlineAccountUnavailable);
        }
        // One blob read per attempt: a refresh writes and reads back its own
        // credentials; after the profile flush only the secure head is rechecked.
        let credentials = match self.auth.launch_credentials(&capture).await {
            Ok(credentials) => credentials,
            Err(_) => {
                capture = self
                    .auth
                    .refresh(capture)
                    .await
                    .map_err(|_| LaunchError::OnlineAccountUnavailable)?;
                self.auth
                    .launch_credentials(&capture)
                    .await
                    .map_err(|_| LaunchError::OnlineAccountUnavailable)?
            }
        };
        let account_id = capture.account_id().to_owned();
        self.skins
            .flush_account(&account_id)
            .await
            .map_err(|_| LaunchError::ProfileUnsettled)?;
        let capture = capture_after_profile(&self.accounts, &capture)?;
        self.auth
            .revalidate_launch_credentials(&capture, &credentials)
            .await
            .map_err(|_| LaunchError::OnlineAccountUnavailable)?;
        let token = credentials.minecraft_access_token().to_owned();
        let auth = LaunchAuthContext {
            player_name: capture.display_name().into(),
            uuid: capture.minecraft_uuid().into(),
            access_token: token.clone(),
            client_id: String::new(),
            xuid: String::new(),
            user_type: "msa".into(),
        };
        Ok((
            capture,
            auth,
            vec![token],
            Some(credentials.minecraft_expires_at()),
        ))
    }
}

fn capture_after_profile(
    accounts: &AccountDirectory,
    before: &CapturedAccount,
) -> Result<CapturedAccount, LaunchError> {
    // Profile publication advances the directory-wide revision itself.
    // Preserve the actual selected login and credential incarnation across
    // that expected change, then validate the fresh complete capture.
    let current = accounts
        .capture_selected()
        .map_err(|_| LaunchError::AccountChanged)?;
    if current.account_id() != before.account_id()
        || current.launch_auth_mode() != LaunchAuthMode::Online
        || current.login_id() != before.login_id()
        || current.credential_revision() != before.credential_revision()
    {
        return Err(LaunchError::AccountChanged);
    }
    accounts
        .validate_capture(&current)
        .map_err(|_| LaunchError::AccountChanged)?;
    Ok(current)
}

fn launch_options(
    effective: &EffectiveLaunchSettings,
    max_memory_mb: i32,
    min_memory_mb: i32,
) -> Result<LaunchOptions, LaunchError> {
    let resolution = match (effective.window_width, effective.window_height) {
        (0, 0) => None,
        (width, height) if width > 0 && height > 0 => Some((width as u32, height as u32)),
        _ => return Err(LaunchError::PlanRejected),
    };
    Ok(LaunchOptions {
        min_memory_mb: Some(min_memory_mb),
        max_memory_mb: Some(max_memory_mb),
        resolution,
        extra_jvm_args: super::plan::split_extra_args(&effective.extra_jvm_args)
            .map_err(|_| LaunchError::PlanRejected)?,
        jvm_preset: effective.jvm_preset.as_str().into(),
        performance_mode: effective.performance_mode,
        ..Default::default()
    })
}

fn composition_error(error: CompositionError) -> LaunchError {
    tracing::warn!(
        stage = "composition",
        cause = error.diagnostic_code(),
        "Launch preparation rejected."
    );
    match error {
        CompositionError::InstanceUnavailable => LaunchError::InstanceBusy,
        _ => LaunchError::PerformanceUnsettled,
    }
}
