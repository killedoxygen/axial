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
    performance::{PerformanceMutationError, PerformanceService},
    runtime::{
        discovery::{RuntimeDiscovery, RuntimeObservation},
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
    performance: PerformanceService,
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
        performance: PerformanceService,
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
            performance,
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

    pub fn with_storage(
        mut self,
        storage: Arc<MetadataStore>,
        reports: super::reports::LaunchReportStore,
    ) -> Result<Self, LaunchError> {
        self.intents = LaunchIntents::restore(storage, reports, &self.instances)?;
        Ok(self)
    }

    /// Composition restores this same owner before other feature recovery.
    pub fn with_intents(mut self, intents: LaunchIntents) -> Self {
        self.intents = intents;
        self
    }

    /// Reserve the exact session identity before the benchmark commits its run
    /// mapping. This grants no process authority; launch still rechecks fences.
    pub(crate) fn reserve_benchmark(
        &self,
        request: &LaunchRequest,
        scenario: &super::reports::LaunchProofScenario,
    ) -> Result<String, LaunchError> {
        let context = serde_json::to_string(scenario).map_err(|_| LaunchError::InvalidRequest)?;
        self.intents.reserve_identity(request, Some(context))
    }

    pub(crate) fn benchmark_request(
        &self,
        key: &str,
        session_id: &str,
        instance_id: &InstanceId,
        scenario: &super::reports::LaunchProofScenario,
    ) -> Result<LaunchRequest, LaunchError> {
        let context = serde_json::to_string(scenario).map_err(|_| LaunchError::InvalidRequest)?;
        let record = self
            .intents
            .record(key)?
            .ok_or(LaunchError::IntentUnavailable)?;
        if record.session_id != session_id
            || &record.request.instance_id != instance_id
            || record.context.as_deref() != Some(context.as_str())
        {
            return Err(LaunchError::IntentConflict);
        }
        Ok(record.request)
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

    #[cfg(test)]
    pub(crate) async fn observe_unstarted_benchmark_for_test(
        &self,
        request: LaunchRequest,
        scenario: super::reports::LaunchProofScenario,
    ) -> SessionSnapshot {
        let context = serde_json::to_string(&scenario).unwrap();
        let ReservedIntent::New {
            key, session_id, ..
        } = self
            .intents
            .reserve_context(&request, Some(context))
            .unwrap()
        else {
            panic!("fixture launch already accepted")
        };
        let instance = self.admit(&request.instance_id).unwrap();
        let application = self.instances.library().admit_application_root().unwrap();
        let acceptance = self
            .intents
            .accept(
                &key,
                IntentBinding::capture(&instance, &application).unwrap(),
            )
            .unwrap();
        let version_id = request
            .version_id
            .clone()
            .unwrap_or_else(|| instance.record().instance.version_id.clone());
        super::session::finish_unstarted_for_test(
            acceptance,
            session_id,
            request.instance_id,
            version_id,
            self.intents.reports.clone().unwrap(),
            false,
        )
        .await;
        let Some(LaunchIntentStatus::Accepted { session }) = self.intents.snapshot(&key).unwrap()
        else {
            panic!("fixture did not settle")
        };
        session
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
        self.launch_with_context(request, None).await
    }

    pub async fn launch_benchmark_configured(
        &self,
        request: LaunchRequest,
        scenario: super::reports::LaunchProofScenario,
    ) -> Result<SessionSnapshot, LaunchError> {
        self.launch_with_context(request, Some(scenario)).await
    }

    async fn launch_with_context(
        &self,
        request: LaunchRequest,
        context: Option<super::reports::LaunchProofScenario>,
    ) -> Result<SessionSnapshot, LaunchError> {
        let fingerprint = context
            .as_ref()
            .map(|value| serde_json::to_string(value).map_err(|_| LaunchError::InvalidRequest))
            .transpose()?;
        let reservation = self.intents.reserve_context(&request, fingerprint)?;
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
                            context,
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

    async fn prepare(
        &self,
        admitted: RegisteredInstance,
        request: &LaunchRequest,
        cancellation: &CancellationToken,
        context: Option<super::reports::LaunchProofScenario>,
        native_retention: &Mutex<Option<Arc<crate::install::vanilla::PreparedNatives>>>,
        library_retention: Arc<super::libraries::Retention>,
        telemetry: Arc<super::session::LaunchAttemptTelemetry>,
    ) -> Result<PreparedSession, LaunchError> {
        let (selection, mut effective, player_name) =
            self.capture(&admitted.record().instance, request.username.as_deref())?;
        if let Some(value) = request.max_memory_mb.filter(|value| *value > 0) {
            effective.max_memory_mb = value;
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
        let library_operation = admitted
            .generation()
            .managed_library()
            .map_err(|_| LaunchError::LibraryUnavailable)?;
        let library_dir = admitted
            .generation()
            .read_projection()
            .map_err(|_| LaunchError::LibraryUnavailable)?;
        let version_guard = axial_minecraft::VersionBundleReadGuard::acquire(&library_operation)
            .map_err(|_| LaunchError::InstallUnavailable)?;
        let installed = self
            .installs
            .ready_version(admitted.generation(), &instance.version_id)
            .await
            .map_err(install_read_error)?;
        let installed_versions = crate::catalog::installed_snapshot(admitted.generation())
            .await
            .map_err(|_| LaunchError::LibraryUnavailable)?;
        if installed_versions.is_degraded() {
            return Err(LaunchError::InstalledVersionsDegraded);
        }
        let (installed, game_libraries) = installed
            .prepare_game_libraries(None)
            .await
            .map_err(game_libraries_error)?;
        let mode = crate::performance::plan::configured_mode(effective.performance_mode);
        let version = installed.version();
        let required = axial_minecraft::effective_java_version_for(
            &instance.minecraft_version,
            &version.kind,
            &version.java_version,
        );
        let runtime = self
            .runtimes
            .prepare_for_launch(&required, &effective.java_path, cancellation)
            .await
            .map_err(LaunchError::RuntimeFailure)?;
        if cancellation.is_cancelled() {
            return Err(LaunchError::Cancelled);
        }
        let performance = self
            .performance
            .prepare_for_launch(
                &admitted,
                self.performance.resolution_request(
                    instance.minecraft_version.clone(),
                    instance.loader_key.clone(),
                    mode,
                ),
            )
            .await
            .map_err(|error| {
                tracing::warn!(
                    stage = "performance_prepare",
                    cause = error.diagnostic_code(),
                    "Launch preparation rejected."
                );
                match error {
                    PerformanceMutationError::Unsettled => LaunchError::PerformanceUnsettled,
                    PerformanceMutationError::Cancelled => LaunchError::Cancelled,
                    PerformanceMutationError::InstanceUnavailable => LaunchError::InstanceBusy,
                    _ => LaunchError::PerformanceUnavailable,
                }
            })?;
        let contribution = performance.effective();
        let mut environment = axial_minecraft::default_environment();
        let resolution = match (effective.window_width, effective.window_height) {
            (0, 0) => None,
            (width, height) if width > 0 && height > 0 => Some((width as u32, height as u32)),
            _ => return Err(LaunchError::PlanRejected),
        };
        environment
            .features
            .insert("has_custom_resolution".into(), resolution.is_some());
        // Complete fallible account/settings work before publishing a fresh
        // native directory. Once published, every rejection must settle it.
        let (selection, auth, secrets, credential_expires_at) =
            if let Some(account) = selection.account() {
                let (account, auth, secrets, expires) = self.prepare_auth(account.clone()).await?;
                (
                    CapturedSelection::from_account(account),
                    auth,
                    secrets,
                    expires,
                )
            } else {
                selection
                    .validate(&self.accounts)
                    .map_err(|_| LaunchError::AccountChanged)?;
                (
                    selection,
                    LaunchAuthContext::offline(player_name),
                    Vec::new(),
                    None,
                )
            };
        let game_dir = admitted
            .game_directory()
            .read_projection()
            .map_err(|_| LaunchError::InstanceChanged)?;
        let host = super::resources::capture_host();
        let options = launch_options(&effective, &instance, &contribution, &host)?;
        let resource_budget = super::resources::capture(
            &host,
            self.sessions.resource_use(),
            self.installs.active_count(),
            effective.max_memory_mb,
            [&library_dir, &game_dir],
        );
        let game_libraries = match game_libraries {
            Some(inputs) => Some(
                super::libraries::prepare(inputs, library_retention.clone(), cancellation).await?,
            ),
            None => None,
        };
        let game_assets = super::assets::prepare(
            &installed,
            &library_operation,
            &library_dir,
            &game_dir,
            library_retention,
            cancellation,
        )
        .await?;
        let prepared_natives = installed
            .prepare_natives(&library_operation, &library_dir, &environment)
            .await
            .map_err(|_| LaunchError::InstallUnavailable)?
            .map(Arc::new);
        let retained_natives = prepared_natives.clone();
        *native_retention
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = retained_natives.clone();
        let result = (|| {
            if cancellation.is_cancelled() {
                return Err(LaunchError::Cancelled);
            }
            let mut command = super::plan::build(LaunchPlanRequest {
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
            })?;
            command.game_libraries = game_libraries;
            if cancellation.is_cancelled() {
                return Err(LaunchError::Cancelled);
            }
            let mut scenario = context.unwrap_or_default();
            scenario.performance_mode = match mode {
                crate::performance::model::PerformanceMode::Managed => "managed",
                crate::performance::model::PerformanceMode::Vanilla => "vanilla",
                crate::performance::model::PerformanceMode::Custom => "custom",
            }
            .into();
            scenario.version_id = Some(instance.version_id.clone());
            scenario.requested_memory_mb = Some(effective.max_memory_mb);
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
                performance,
                scenario,
                resource_budget,
                credential_expires_at,
                telemetry,
            )
        })();
        if result.is_err() {
            super::prepare::settle_natives(retained_natives).await;
        }
        result
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
        if self.auth.launch_credentials(&capture).await.is_err() {
            capture = self
                .auth
                .refresh(capture)
                .await
                .map_err(|_| LaunchError::OnlineAccountUnavailable)?;
        }
        let account_id = capture.account_id().to_owned();
        self.skins
            .flush_account(&account_id)
            .await
            .map_err(|_| LaunchError::ProfileUnsettled)?;
        let capture = capture_after_profile(&self.accounts, &capture)?;
        let credentials = self
            .auth
            .launch_credentials(&capture)
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
    instance: &crate::instances::model::Instance,
    contribution: &crate::performance::effective::EffectivePerformancePlan,
    host: &super::resources::HostResources,
) -> Result<LaunchOptions, LaunchError> {
    let resolution = match (effective.window_width, effective.window_height) {
        (0, 0) => None,
        (width, height) if width > 0 && height > 0 => Some((width as u32, height as u32)),
        _ => return Err(LaunchError::PlanRejected),
    };
    Ok(LaunchOptions {
        min_memory_mb: Some(effective.min_memory_mb),
        max_memory_mb: Some(effective.max_memory_mb),
        resolution,
        extra_jvm_args: super::plan::split_extra_args(&effective.extra_jvm_args)
            .map_err(|_| LaunchError::PlanRejected)?,
        jvm_preset: if effective.jvm_preset.as_str().is_empty() {
            contribution
                .jvm_contribution
                .preset
                .clone()
                .unwrap_or_default()
        } else {
            effective.jvm_preset.as_str().into()
        },
        low_impact_startup: contribution.launch_smoothing.policy
            != crate::performance::effective::EffectiveLaunchSmoothingPolicy::UserControlled,
        logical_cores: host.cpu_threads.unwrap_or(4),
        total_memory_mb: Some(host.total_memory_mb),
        loader: instance.loader_key.clone(),
        is_modded: !matches!(instance.loader_key.as_str(), "" | "vanilla"),
        ..Default::default()
    })
}
