//! Public launch admission. Only this owner may turn a launch request into a
//! prepared session; transport input never contains executable paths or args.

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

pub const INTENT_MIGRATION: Migration = Migration {
    id: "launch_intents.v1",
    sql: "CREATE TABLE launch_intents (
        intent_key TEXT PRIMARY KEY NOT NULL CHECK(length(intent_key)=36),
        payload BLOB NOT NULL CHECK(length(payload) BETWEEN 1 AND 16384),
        state TEXT NOT NULL CHECK(state IN ('pending','accepted','rejected','interrupted')),
        error TEXT
    ) STRICT;",
};

pub const INTENT_TERMINAL_MIGRATION: Migration = Migration {
    id: "launch_intents.v2",
    sql: "ALTER TABLE launch_intents ADD COLUMN terminal_ack INTEGER NOT NULL DEFAULT 0 CHECK(terminal_ack IN (0,1));
    CREATE INDEX launch_intents_unresolved ON launch_intents(intent_key)
        WHERE state IN ('accepted','interrupted') AND terminal_ack=0;
    CREATE INDEX launch_intents_session ON launch_intents(
        json_extract(CASE WHEN json_valid(CAST(payload AS TEXT)) THEN CAST(payload AS TEXT) ELSE '{}' END, '$.session_id'))
        WHERE state IN ('accepted','interrupted');",
};

pub const INTENT_SETTLEMENT_MIGRATION: Migration = Migration {
    id: "launch_intents.v3",
    sql: "ALTER TABLE launch_intents ADD COLUMN settlement BLOB
        CHECK(settlement IS NULL OR length(settlement) BETWEEN 1 AND 4096);
    DROP INDEX launch_intents_unresolved;
    CREATE INDEX launch_intents_unresolved ON launch_intents(intent_key)
        WHERE state IN ('accepted','interrupted') AND terminal_ack=0 AND
        CASE WHEN json_valid(CAST(settlement AS TEXT))
             THEN json_type(CAST(settlement AS TEXT), '$.version') IS NOT 'integer'
               OR json_extract(CAST(settlement AS TEXT), '$.version') IS NOT 1 ELSE 1 END;",
};

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
        selection::CapturedAccount,
        session::AuthService,
    },
    install::queue::{InstallError, InstallQueue},
    instances::{
        directory::{InstanceDirectories, RegisteredInstance},
        model::InstanceError,
    },
    library::ApplicationRootPin,
    performance::{PerformanceMutationError, PerformanceService},
    runtime::discovery::RuntimeDiscovery,
    settings::{EffectiveLaunchSettings, SettingsStore},
    tasks::{CancellationToken, TaskOwner},
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
}

#[derive(Clone, Debug, Serialize)]
pub struct LaunchPreflight {
    pub instance_id: InstanceId,
    pub launchable: bool,
    pub error: Option<LaunchErrorResponse>,
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
                        self.intents.settle(&key, Err(error));
                        return Err(error);
                    }
                };
                // The task owner keeps the exact extraction even if a later
                // preparation step panics before handing it to the session.
                let native_retention = Arc::new(Mutex::new(None));
                let retained = (admitted.clone(), native_retention.clone());
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

    /// Read-only readiness with an owned Java diagnostic probe, never game
    /// launch or native extraction. It does not reserve the instance while probing.
    pub async fn preflight(&self, id: InstanceId) -> LaunchPreflight {
        let result = self.check_preflight(&id).await;
        LaunchPreflight {
            instance_id: id,
            launchable: result.is_ok(),
            error: result.err().map(Into::into),
        }
    }

    async fn check_preflight(&self, id: &InstanceId) -> Result<(), LaunchError> {
        let admitted = Arc::new(self.instances.admit_read(id).map_err(instance_error)?);
        let pin = admitted.game_directory().pin().clone();
        let artifacts = self
            .instances
            .exclusions()
            .try_acquire_read_artifacts(
                std::iter::empty::<String>(),
                [crate::install::queue::library_artifact(
                    &pin.library_id().to_string(),
                )],
            )
            .map_err(|_| LaunchError::InstanceBusy)?;
        let retained = (admitted.clone(), artifacts);
        let coordinator = self.clone();
        let task = self
            .tasks
            .try_spawn(retained, move |cancel| async move {
                let mut bundle_guard = None;
                let mut planned_performance = None;
                let result = async {
                    // Installation can be repaired without selecting an account.
                    // Only the guarded install proof establishes that need;
                    // catalogue display flags and transient admission failures do not.
                    let operation = pin
                        .managed_library()
                        .map_err(|_| LaunchError::LibraryUnavailable)?;
                    bundle_guard = Some(
                        axial_minecraft::VersionBundleReadGuard::acquire(&operation).map_err(
                            |error| {
                                if error.kind() == std::io::ErrorKind::WouldBlock {
                                    LaunchError::InstanceBusy
                                } else {
                                    LaunchError::LibraryUnavailable
                                }
                            },
                        )?,
                    );
                    let installed = coordinator
                        .installs
                        .ready_version(&pin, &admitted.record().instance.version_id)
                        .await
                        .map_err(|error| match error {
                            InstallError::NotReady => LaunchError::InstallUnavailable,
                            InstallError::Busy => LaunchError::InstanceBusy,
                            InstallError::AtCapacity => LaunchError::AtCapacity,
                            InstallError::Closed => LaunchError::Closed,
                            _ => LaunchError::LibraryUnavailable,
                        })?;
                    let version = installed.version();
                    let (account, settings) =
                        coordinator.capture(&admitted.record().instance, None)?;
                    let result = async {
                        if account.kind() == AccountKind::Microsoft {
                            coordinator
                                .auth
                                .credentials(&account)
                                .await
                                .map_err(|_| LaunchError::OnlineAccountUnavailable)?;
                        }
                        let target = &admitted.record().instance;
                        let performance_request = coordinator.performance.resolution_request(
                            target.minecraft_version.clone(),
                            target.loader_key.clone(),
                            crate::performance::plan::configured_mode(settings.performance_mode),
                        );
                        let performance = planned_performance.insert(
                            coordinator
                                .performance
                                .rules()
                                .plan(performance_request)
                                .await
                                .map_err(|_| LaunchError::PerformanceUnavailable)?,
                        );
                        let required = axial_minecraft::effective_java_version_for(
                            &admitted.record().instance.minecraft_version,
                            &version.kind,
                            &version.java_version,
                        );
                        let runtime = coordinator
                            .runtimes
                            .select(&required, &settings.java_path, &cancel)
                            .await
                            .map_err(|_| LaunchError::RuntimeUnavailable)?;
                        let contribution =
                            axial_performance::effective_performance_plan(performance.plan());
                        let options = launch_options(&settings, target, &contribution)?;
                        super::plan::validate_options(
                            &options,
                            &target.minecraft_version,
                            runtime.probe.info(),
                        )
                        .map_err(|error| {
                            tracing::warn!(
                                ?error,
                                stage = "validate_options",
                                "Launch plan rejected."
                            );
                            LaunchError::PlanRejected
                        })?;
                        Ok::<(), LaunchError>(())
                    }
                    .await;
                    coordinator
                        .accounts
                        .validate_capture(&account)
                        .map_err(|_| LaunchError::AccountChanged)?;
                    coordinator
                        .settings
                        .validate_revision(settings.global_config_revision)
                        .map_err(|_| LaunchError::SettingsChanged)?;
                    result
                }
                .await;
                // Nonblocking and synchronous: preserve the busy projection
                // and fence both successful and failed readiness observations.
                let _target = coordinator
                    .instances
                    .exclusions()
                    .try_acquire([admitted.record().instance.id.as_str()], [])
                    .map_err(|_| LaunchError::InstanceBusy)?;
                admitted.validate_current().map_err(instance_error)?;
                let current = coordinator
                    .instances
                    .registry()
                    .get_live(&admitted.record().instance.id)
                    .map_err(instance_error)?;
                if current.instance.settings != admitted.record().instance.settings {
                    return Err(LaunchError::InstanceChanged);
                }
                drop((planned_performance, bundle_guard));
                result
            })
            .map_err(|_| LaunchError::AtCapacity)?;
        task.join()
            .await
            .map_err(|_| LaunchError::PreparationFailed)?
    }

    fn capture(
        &self,
        instance: &crate::instances::model::Instance,
        username: Option<&str>,
    ) -> Result<(CapturedAccount, EffectiveLaunchSettings), LaunchError> {
        let account = self
            .accounts
            .capture_selected()
            .map_err(|_| LaunchError::AccountUnavailable)?;
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
        Ok((account, settings))
    }

    async fn prepare(
        &self,
        admitted: RegisteredInstance,
        request: &LaunchRequest,
        cancellation: &CancellationToken,
        context: Option<super::reports::LaunchProofScenario>,
        native_retention: &Mutex<Option<Arc<crate::install::vanilla::PreparedNatives>>>,
        telemetry: Arc<super::session::LaunchAttemptTelemetry>,
    ) -> Result<PreparedSession, LaunchError> {
        let (account, mut effective) =
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
        let mode = crate::performance::plan::configured_mode(effective.performance_mode);
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
            .map_err(|_| LaunchError::InstallUnavailable)?;
        let version = installed.version();
        let required = axial_minecraft::effective_java_version_for(
            &instance.minecraft_version,
            &version.kind,
            &version.java_version,
        );
        let runtime = self
            .runtimes
            .select(&required, &effective.java_path, cancellation)
            .await
            .map_err(|_| LaunchError::RuntimeUnavailable)?;
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
        let (account, auth, secrets, credential_expires_at) = self.prepare_auth(account).await?;
        let game_dir = admitted
            .game_directory()
            .read_projection()
            .map_err(|_| LaunchError::InstanceChanged)?;
        let options = launch_options(&effective, &instance, &contribution)?;
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
            let command = super::plan::build(LaunchPlanRequest {
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
                settings: options,
            })
            .map_err(|error| {
                tracing::warn!(?error, stage = "build", "Launch plan rejected.");
                LaunchError::PlanRejected
            })?;
            if cancellation.is_cancelled() {
                return Err(LaunchError::Cancelled);
            }
            let mut scenario = context.unwrap_or_default();
            scenario.performance_mode = match mode {
                axial_performance::PerformanceMode::Managed => "managed",
                axial_performance::PerformanceMode::Vanilla => "vanilla",
                axial_performance::PerformanceMode::Custom => "custom",
            }
            .into();
            scenario.version_id = Some(instance.version_id.clone());
            scenario.requested_memory_mb = Some(effective.max_memory_mb);
            PreparedSession::new(
                instance.id,
                instance.version_id,
                command,
                self.accounts.clone(),
                account,
                self.settings.clone(),
                effective.global_config_revision,
                secrets,
                admitted,
                performance,
                scenario,
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
    contribution: &axial_performance::EffectivePerformancePlan,
) -> Result<LaunchOptions, LaunchError> {
    let resolution = match (effective.window_width, effective.window_height) {
        (0, 0) => None,
        (width, height) if width > 0 && height > 0 => Some((width as u32, height as u32)),
        _ => return Err(LaunchError::PlanRejected),
    };
    let mut host = sysinfo::System::new();
    host.refresh_memory();
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
            != axial_performance::EffectiveLaunchSmoothingPolicy::UserControlled,
        logical_cores: std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(4),
        total_memory_mb: Some(host.total_memory() / (1024 * 1024)),
        loader: instance.loader_key.clone(),
        is_modded: !matches!(instance.loader_key.as_str(), "" | "vanilla"),
        ..Default::default()
    })
}

fn instance_error(error: InstanceError) -> LaunchError {
    match error {
        InstanceError::NotFound => LaunchError::InstanceNotFound,
        InstanceError::Busy => LaunchError::InstanceBusy,
        InstanceError::LibraryUnavailable => LaunchError::LibraryUnavailable,
        InstanceError::Closed => LaunchError::Closed,
        _ => LaunchError::InstanceChanged,
    }
}

fn session_error(error: SessionError) -> LaunchError {
    match error {
        SessionError::Busy => LaunchError::InstanceBusy,
        SessionError::Closing => LaunchError::Closed,
        SessionError::AtCapacity => LaunchError::AtCapacity,
        SessionError::NotFound => LaunchError::PreparationFailed,
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LaunchRequest {
    pub instance_id: InstanceId,
    #[serde(default)]
    pub version_id: Option<String>,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub max_memory_mb: Option<i32>,
    #[serde(default)]
    pub min_memory_mb: Option<i32>,
    #[serde(default)]
    pub client_started_at_ms: Option<i64>,
    #[serde(default)]
    pub intent_key: Option<String>,
}

impl LaunchRequest {
    pub fn validate(&self) -> Result<(), LaunchError> {
        if let Some(key) = &self.intent_key {
            validate_intent_key(key)?;
        }
        if let Some(name) = &self.username {
            if !name.trim().is_empty() {
                crate::settings::validate_username(name.trim())
                    .map_err(|_| LaunchError::InvalidUsername)?;
            }
        }
        for memory in [self.max_memory_mb, self.min_memory_mb]
            .into_iter()
            .flatten()
        {
            if !(0..=1024 * 1024).contains(&memory) {
                return Err(LaunchError::InvalidMemory);
            }
        }
        if self.client_started_at_ms.is_some_and(|value| value < 0) {
            return Err(LaunchError::InvalidRequest);
        }
        Ok(())
    }

    fn same_effect(&self, other: &Self) -> bool {
        self.instance_id == other.instance_id
            && self.version_id == other.version_id
            && self.username == other.username
            && self.max_memory_mb == other.max_memory_mb
            && self.min_memory_mb == other.min_memory_mb
    }
}

fn validate_intent_key(key: &str) -> Result<(), LaunchError> {
    let id = uuid::Uuid::parse_str(key).map_err(|_| LaunchError::InvalidIntent)?;
    if id.is_nil() || id.to_string() != key {
        return Err(LaunchError::InvalidIntent);
    }
    Ok(())
}

/// Only bounded domain text can cross the transport boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaunchError {
    #[error("The launch request is invalid.")]
    InvalidRequest,
    #[error("The launch intent identity is invalid.")]
    InvalidIntent,
    #[error("This launch intent already belongs to a different request.")]
    IntentConflict,
    #[error(
        "This launch was interrupted. Its process outcome is not known; it will not be repeated."
    )]
    Interrupted,
    #[error(
        "The durable launch status is unavailable. Inspect the existing attempt before retrying."
    )]
    IntentUnavailable,
    #[error(
        "Launch history reconciliation is incomplete. Restart to continue. Existing files are preserved."
    )]
    RecoveryIncomplete,
    #[error("The launch request capacity is full. Wait for accepted work to finish.")]
    AtCapacity,
    #[error("Enter a username between 3 and 16 letters, numbers or underscores.")]
    InvalidUsername,
    #[error("The requested memory bounds are invalid.")]
    InvalidMemory,
    #[error("The instance was not found.")]
    InstanceNotFound,
    #[error("The instance is busy. Wait for its current operation to finish.")]
    InstanceBusy,
    #[error("The instance changed while launch was being prepared. Try again.")]
    InstanceChanged,
    #[error("The selected account changed while launch was being prepared. Try again.")]
    AccountChanged,
    #[error("Select an account before launching.")]
    AccountUnavailable,
    #[error("Online launch requires a verified Minecraft Java account. Sign in again.")]
    OnlineAccountUnavailable,
    #[error("The saved profile change could not be settled. Open Skins before retrying.")]
    ProfileUnsettled,
    #[error("The selected library is unavailable. Reconnect it before launching.")]
    LibraryUnavailable,
    #[error("The installed version requires a completed installation before launch.")]
    InstallUnavailable,
    #[error("The selected Java runtime could not be verified. Check Java settings.")]
    RuntimeUnavailable,
    #[error("The launch settings changed while launch was being prepared. Try again.")]
    SettingsChanged,
    #[error("Performance changes require settlement before this instance can launch.")]
    PerformanceUnsettled,
    #[error("The selected Performance mode is not available for this launch.")]
    PerformanceUnavailable,
    #[error(
        "The launch command could not be validated. Check the instance settings and installation."
    )]
    PlanRejected,
    #[error("Launch was cancelled before a game process was started.")]
    Cancelled,
    #[error("Launches are unavailable while the application is shutting down.")]
    Closed,
    #[error("The launch could not be prepared. Its status is retained for inspection.")]
    PreparationFailed,
}

#[derive(Clone, Debug, Serialize)]
pub struct LaunchErrorResponse {
    pub error: String,
    pub code: LaunchError,
}

impl From<LaunchError> for LaunchErrorResponse {
    fn from(code: LaunchError) -> Self {
        Self {
            error: code.to_string(),
            code,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum LaunchIntentStatus {
    Preparing,
    Accepted { session: SessionSnapshot },
    Rejected { error: LaunchErrorResponse },
    Interrupted { session_id: String },
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct IntentRecord {
    request: LaunchRequest,
    context: Option<String>,
    session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    binding: Option<IntentBinding>,
}

/// Exact accepted-row authority, handed only to the owning session before spawn.
#[derive(Clone)]
pub(super) struct AcceptedIntent {
    storage: Option<Arc<MetadataStore>>,
    key: String,
    payload: Vec<u8>,
    record: IntentRecord,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum ObservationError {
    Invalid,
    Conflict,
    Storage,
}

impl From<StorageError> for ObservationError {
    fn from(_: StorageError) -> Self {
        Self::Storage
    }
}

impl From<crate::storage::rusqlite::Error> for ObservationError {
    fn from(_: crate::storage::rusqlite::Error) -> Self {
        Self::Storage
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SettlementRecord {
    version: u8,
    accepted_payload: [u8; 32],
    observation: super::session::ObservedSettlement,
}

impl AcceptedIntent {
    pub(super) fn matches_session(&self, session_id: &str, instance_id: &InstanceId) -> bool {
        self.record.session_id == session_id && self.record.request.instance_id == *instance_id
    }

    pub(super) fn observe(
        &self,
        observation: &super::session::ObservedSettlement,
    ) -> Result<bool, ObservationError> {
        let settlement = serde_json::to_vec(&SettlementRecord {
            version: 1,
            accepted_payload: Sha256::digest(&self.payload).into(),
            observation: observation.clone(),
        })
        .map_err(|_| ObservationError::Invalid)?;
        decode_settlement(&self.record, &self.payload, &settlement)
            .map_err(|_| ObservationError::Invalid)?;
        let Some(storage) = &self.storage else {
            return Ok(false);
        };
        storage.transaction(|tx| -> Result<(), ObservationError> {
            let read = || -> Result<_, ObservationError> {
                Ok(tx.query_row(
                    "SELECT payload,state,settlement FROM launch_intents WHERE intent_key=?1",
                    [&self.key],
                    |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, String>(1)?, row.get::<_, Option<Vec<u8>>>(2)?)),
                ).optional()?)
            };
            let Some((payload, state, prior)) = read()? else { return Err(ObservationError::Conflict); };
            if payload != self.payload || state != "accepted" {
                return Err(ObservationError::Conflict);
            }
            if let Some(prior) = prior {
                return if prior == settlement { Ok(()) } else { Err(ObservationError::Conflict) };
            }
            if tx.execute(
                "UPDATE launch_intents SET settlement=?3 WHERE intent_key=?1 AND payload=?2 AND state='accepted' AND settlement IS NULL",
                params![self.key, self.payload, settlement],
            )? != 1 || read()? != Some((self.payload.clone(), "accepted".into(), Some(settlement.clone()))) {
                return Err(ObservationError::Conflict);
            }
            Ok(())
        }).map(|()| true)
    }
}

fn decode_settlement(
    record: &IntentRecord,
    accepted_payload: &[u8],
    bytes: &[u8],
) -> Result<super::session::ObservedSettlement, LaunchError> {
    if bytes.len() > 4096
        || record
            .binding
            .as_ref()
            .is_none_or(|binding| binding.version != 1)
    {
        return Err(LaunchError::IntentUnavailable);
    }
    let settlement: SettlementRecord =
        serde_json::from_slice(bytes).map_err(|_| LaunchError::IntentUnavailable)?;
    if settlement.version != 1
        || settlement.accepted_payload != <[u8; 32]>::from(Sha256::digest(accepted_payload))
        || !settlement.observation.validate()
        || !settlement
            .observation
            .matches_requested_version(record.request.version_id.as_deref())
        || serde_json::to_vec(&settlement).map_err(|_| LaunchError::IntentUnavailable)? != bytes
    {
        return Err(LaunchError::IntentUnavailable);
    }
    Ok(settlement.observation)
}

/// Equality evidence from admitted capabilities, never authority to open a path.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct IntentBinding {
    version: u8,
    library_id: String,
    library_root: [u8; 32],
    application_root: [u8; 32],
    directory_name: String,
    directory_receipt: String,
}

impl IntentBinding {
    pub(super) fn capture(
        instance: &RegisteredInstance,
        application: &ApplicationRootPin,
    ) -> Result<Self, LaunchError> {
        instance.validate_current().map_err(instance_error)?;
        let binding = Self {
            version: 1,
            library_id: instance.generation().library_id().to_string(),
            library_root: instance
                .generation()
                .directory()
                .and_then(|directory| directory.identity())
                .map_err(|_| LaunchError::LibraryUnavailable)?
                .filesystem_witness(),
            application_root: application
                .directory()
                .and_then(|directory| directory.identity())
                .map_err(|_| LaunchError::LibraryUnavailable)?
                .filesystem_witness(),
            directory_name: instance.record().directory_name.clone(),
            directory_receipt: instance
                .directory()
                .receipt()
                .map_err(|_| LaunchError::InstanceChanged)?,
        };
        instance.validate_current().map_err(instance_error)?;
        application
            .revalidate()
            .map_err(|_| LaunchError::LibraryUnavailable)?;
        Ok(binding)
    }
}

struct IntentEntry {
    record: IntentRecord,
    claimed: bool,
    accepted: bool,
    status: watch::Sender<LaunchIntentStatus>,
}

/// The bounded live cache retains waiters. Durable identities survive process
/// restart and can never authorize a second process.
#[derive(Clone)]
pub struct LaunchIntents {
    entries: Arc<Mutex<BTreeMap<String, IntentEntry>>>,
    capacity: usize,
    storage: Option<Arc<MetadataStore>>,
    reports: Option<super::reports::LaunchReportStore>,
    restored: Arc<Vec<(RegisteredInstance, ApplicationRootPin)>>,
}

const MAX_RECOVERY_INTENTS: usize = 4096;
const MAX_RECOVERY_BYTES: usize = 32 * 1024 * 1024;

struct PreparationSentinel {
    intents: LaunchIntents,
    key: String,
    settled: bool,
    telemetry: Arc<super::session::LaunchAttemptTelemetry>,
}
impl Drop for PreparationSentinel {
    fn drop(&mut self) {
        if !self.settled {
            self.telemetry.failure(None);
            self.intents.abandon(&self.key);
        }
    }
}

enum ReservedIntent {
    Existing(watch::Receiver<LaunchIntentStatus>),
    New {
        key: String,
        session_id: String,
        receiver: watch::Receiver<LaunchIntentStatus>,
    },
}

impl LaunchIntents {
    fn new(capacity: usize) -> Self {
        Self {
            entries: Arc::new(Mutex::new(BTreeMap::new())),
            capacity,
            storage: None,
            reports: None,
            restored: Arc::new(Vec::new()),
        }
    }

    fn with_storage(
        storage: Arc<MetadataStore>,
        reports: super::reports::LaunchReportStore,
        capacity: usize,
    ) -> Result<Self, LaunchError> {
        Ok(Self {
            storage: Some(storage),
            reports: Some(reports),
            ..Self::new(capacity)
        })
    }

    #[cfg(test)]
    fn needs_recovery(
        storage: Arc<MetadataStore>,
        reports: super::reports::LaunchReportStore,
    ) -> Result<bool, LaunchError> {
        Ok(!Self::with_storage(storage, reports, MAX_RECOVERY_INTENTS)?
            .interrupted_records()?
            .is_empty())
    }

    /// Restore before constructing effect-producing services. No process is
    /// adopted, signalled or declared settled by restoring these reservations.
    pub fn restore(
        storage: Arc<MetadataStore>,
        reports: super::reports::LaunchReportStore,
        directories: &InstanceDirectories,
    ) -> Result<Self, LaunchError> {
        let mut intents = Self::with_storage(storage, reports, MAX_RECOVERY_INTENTS)?;
        let records = match intents.interrupted_records() {
            Ok(records) => records,
            Err(error) => {
                directories.library().fence_interrupted_launch();
                return Err(error);
            }
        };
        if records.is_empty() {
            return Ok(intents);
        }
        // The lifecycle fence deliberately outlives this owner or a failed
        // constructor. Only a future exact process-settlement owner may clear it.
        directories.library().fence_interrupted_launch();
        let application = directories
            .library()
            .admit_application_root()
            .map_err(|_| LaunchError::LibraryUnavailable)?;
        let mut restored = Vec::with_capacity(records.len());
        for record in records {
            let binding = record
                .binding
                .as_ref()
                .ok_or(LaunchError::IntentUnavailable)?;
            let pin = directories
                .library()
                .admit()
                .map_err(|_| LaunchError::LibraryUnavailable)?;
            let library_root = pin
                .directory()
                .and_then(|directory| directory.identity())
                .map_err(|_| LaunchError::LibraryUnavailable)?
                .filesystem_witness();
            let application_root = application
                .directory()
                .and_then(|directory| directory.identity())
                .map_err(|_| LaunchError::LibraryUnavailable)?
                .filesystem_witness();
            if binding.version != 1
                || binding.library_id != pin.library_id().to_string()
                || binding.library_root != library_root
                || binding.application_root != application_root
            {
                return Err(LaunchError::IntentUnavailable);
            }
            let captured = directories
                .registry()
                .get_live(&record.request.instance_id)
                .map_err(|_| LaunchError::IntentUnavailable)?;
            if captured.library_id != binding.library_id
                || captured.directory_name != binding.directory_name
                || captured.directory_receipt.as_deref() != Some(binding.directory_receipt.as_str())
            {
                return Err(LaunchError::IntentUnavailable);
            }
            let lease = directories
                .exclusions()
                .try_acquire_read_artifacts(
                    [record.request.instance_id.as_str()],
                    [crate::install::queue::library_artifact(&binding.library_id)],
                )
                .map_err(|_| LaunchError::InstanceBusy)?;
            let instance = InstanceDirectories::admit_record(
                directories.registry().clone(),
                captured,
                pin,
                lease,
            )
            .map_err(|_| LaunchError::IntentUnavailable)?;
            if IntentBinding::capture(&instance, &application)? != *binding {
                return Err(LaunchError::IntentUnavailable);
            }
            restored.push((instance, application.clone()));
        }
        intents.restored = Arc::new(restored);
        Ok(intents)
    }

    #[cfg(test)]
    fn has_interrupted_launches(&self) -> bool {
        !self.restored.is_empty()
    }

    fn interrupted_records(&self) -> Result<Vec<IntentRecord>, LaunchError> {
        self.interrupted_records_with_limits(MAX_RECOVERY_INTENTS, MAX_RECOVERY_BYTES)
    }

    fn interrupted_records_with_limits(
        &self,
        record_limit: usize,
        byte_limit: usize,
    ) -> Result<Vec<IntentRecord>, LaunchError> {
        let storage = self
            .storage
            .as_ref()
            .ok_or(LaunchError::IntentUnavailable)?;
        let reports = self
            .reports
            .as_ref()
            .ok_or(LaunchError::IntentUnavailable)?;
        let mut interrupted = Vec::new();
        let mut after = String::new();
        let mut bytes = 0usize;
        let mut examined = 0usize;
        let mut acknowledged = false;
        loop {
            // The partial index excludes lifetime history. One row at a time
            // also lets old profiles make durable, bounded migration progress.
            let row = storage
                .read(|db| -> Result<_, StorageError> {
                    Ok(db
                        .query_row(
                            "SELECT CASE WHEN length(intent_key)=36 THEN intent_key END,
                     CASE WHEN length(payload)<=16384 THEN payload END,settlement
                     FROM launch_intents WHERE state IN ('accepted','interrupted')
                     AND terminal_ack=0 AND CASE WHEN json_valid(CAST(settlement AS TEXT))
                     THEN json_type(CAST(settlement AS TEXT), '$.version') IS NOT 'integer'
                       OR json_extract(CAST(settlement AS TEXT), '$.version') IS NOT 1 ELSE 1 END
                     AND intent_key>?1 ORDER BY intent_key LIMIT 1",
                            [&after],
                            |row| {
                                Ok((
                                    row.get::<_, String>(0)?,
                                    row.get::<_, Vec<u8>>(1)?,
                                    row.get::<_, Option<Vec<u8>>>(2)?,
                                ))
                            },
                        )
                        .optional()?)
                })
                .map_err(|_| LaunchError::IntentUnavailable)?;
            let Some((key, payload, settlement)) = row else {
                return Ok(interrupted);
            };
            let row_bytes = payload
                .len()
                .saturating_add(settlement.as_ref().map_or(0, Vec::len));
            if examined == record_limit || bytes.saturating_add(row_bytes) > byte_limit {
                return Err(if acknowledged {
                    LaunchError::RecoveryIncomplete
                } else {
                    LaunchError::AtCapacity
                });
            }
            examined += 1;
            bytes += row_bytes;
            let record = decode_intent(&key, &payload)?;
            if let Some(bytes) = settlement {
                // Unsupported envelopes remain in the bounded recovery index.
                // Only the exact owner publication can waive restoration.
                decode_settlement(&record, &payload, &bytes)?;
                return Err(LaunchError::IntentUnavailable);
            }
            if let Some(context) = &record.context {
                serde_json::from_str::<super::reports::LaunchProofScenario>(context)
                    .map_err(|_| LaunchError::IntentUnavailable)?;
            }
            // Account for terminal proof bytes before asking its existing owner
            // to decode them; the whole restart scan has one work budget.
            let report_bytes = storage
                .read(|db| -> Result<Option<usize>, StorageError> {
                    Ok(db
                        .query_row(
                            "SELECT length(payload) FROM launch_reports WHERE session_id=?1",
                            [&record.session_id],
                            |row| row.get(0),
                        )
                        .optional()?)
                })
                .map_err(|_| LaunchError::IntentUnavailable)?
                .unwrap_or(0);
            if bytes.saturating_add(report_bytes) > byte_limit {
                return Err(if acknowledged {
                    LaunchError::RecoveryIncomplete
                } else {
                    LaunchError::AtCapacity
                });
            }
            bytes += report_bytes;
            if let Some(report) = reports
                .get(&record.session_id)
                .map_err(|_| LaunchError::IntentUnavailable)?
            {
                terminal_snapshot(&record, &report)?;
                // Revalidate the immutable proof and acknowledge atomically.
                reports
                    .acknowledge_intent(&record.session_id)
                    .map_err(|_| LaunchError::IntentUnavailable)?;
                acknowledged = true;
            } else {
                interrupted.push(record);
            }
            after = key;
        }
    }

    fn reserve(&self, request: &LaunchRequest) -> Result<ReservedIntent, LaunchError> {
        self.reserve_context(request, None)
    }

    fn reserve_context(
        &self,
        request: &LaunchRequest,
        context: Option<String>,
    ) -> Result<ReservedIntent, LaunchError> {
        let mut request = request.clone();
        request
            .intent_key
            .get_or_insert_with(|| uuid::Uuid::new_v4().to_string());
        self.reserve_identity(&request, context)?;
        let key = request.intent_key.expect("reserved launch intent");
        let mut entries = self.entries.lock().map_err(|_| LaunchError::Closed)?;
        let entry = entries
            .get_mut(&key)
            .ok_or(LaunchError::IntentUnavailable)?;
        if entry.claimed || !matches!(*entry.status.borrow(), LaunchIntentStatus::Preparing) {
            return Ok(ReservedIntent::Existing(entry.status.subscribe()));
        }
        entry.claimed = true;
        Ok(ReservedIntent::New {
            key,
            session_id: entry.record.session_id.clone(),
            receiver: entry.status.subscribe(),
        })
    }

    fn reserve_identity(
        &self,
        request: &LaunchRequest,
        context: Option<String>,
    ) -> Result<String, LaunchError> {
        request.validate()?;
        let key = request
            .intent_key
            .as_ref()
            .ok_or(LaunchError::InvalidIntent)?;
        let mut entries = self.entries.lock().map_err(|_| LaunchError::Closed)?;
        if !entries.contains_key(key) {
            if entries.len() >= self.capacity {
                return Err(LaunchError::AtCapacity);
            }
            if let Some(entry) = self.load(key)? {
                entries.insert(key.clone(), entry);
            }
        }
        if let Some(entry) = entries.get(key) {
            if !request.same_effect(&entry.record.request) || context != entry.record.context {
                return Err(LaunchError::IntentConflict);
            }
            return Ok(entry.record.session_id.clone());
        }
        let record = IntentRecord {
            request: request.clone(),
            context,
            session_id: uuid::Uuid::new_v4().to_string(),
            binding: None,
        };
        if let Some(storage) = &self.storage {
            let payload =
                serde_json::to_vec(&record).map_err(|_| LaunchError::IntentUnavailable)?;
            storage.transaction(|tx| -> Result<(), StorageError> {
                tx.execute("INSERT INTO launch_intents(intent_key,payload,state) VALUES(?1,?2,'pending')", params![key, payload])?;
                Ok(())
            }).map_err(|_| LaunchError::IntentUnavailable)?;
        }
        let session_id = record.session_id.clone();
        entries.insert(
            key.clone(),
            IntentEntry {
                record,
                claimed: false,
                accepted: false,
                status: watch::channel(LaunchIntentStatus::Preparing).0,
            },
        );
        Ok(session_id)
    }

    fn load(&self, key: &str) -> Result<Option<IntentEntry>, LaunchError> {
        let Some(storage) = &self.storage else {
            return Ok(None);
        };
        let row = storage
            .read(|connection| -> Result<_, StorageError> {
                Ok(connection
                    .query_row(
                        "SELECT payload,state,error,settlement FROM launch_intents WHERE intent_key=?1",
                        [key],
                        |row| {
                            Ok((
                                row.get::<_, Vec<u8>>(0)?,
                                row.get::<_, String>(1)?,
                                row.get::<_, Option<String>>(2)?,
                                row.get::<_, Option<Vec<u8>>>(3)?,
                            ))
                        },
                    )
                    .optional()?)
            })
            .map_err(|_| LaunchError::IntentUnavailable)?;
        row.map(|(payload, state, error, settlement)| {
            let record = decode_intent(key, &payload)?;
            if settlement.is_some() && !matches!(state.as_str(), "accepted" | "interrupted") {
                return Err(LaunchError::IntentUnavailable);
            }
            let status = match state.as_str() {
                "pending" if record.context.is_some() => LaunchIntentStatus::Preparing,
                "pending" => {
                    // No process was authorized. Ordinary launch recovery only
                    // reads status; do not leave it waiting for a lost worker.
                    self.reject(key, LaunchError::PreparationFailed)?;
                    LaunchIntentStatus::Rejected {
                        error: LaunchError::PreparationFailed.into(),
                    }
                }
                "accepted" | "interrupted" => {
                    match self.terminal_session(&record, &payload, settlement.as_deref())? {
                        Some(session) => LaunchIntentStatus::Accepted { session },
                        None => LaunchIntentStatus::Interrupted {
                            session_id: record.session_id.clone(),
                        },
                    }
                }
                "rejected" => LaunchIntentStatus::Rejected {
                    error: serde_json::from_str::<LaunchError>(
                        &error.ok_or(LaunchError::IntentUnavailable)?,
                    )
                    .map_err(|_| LaunchError::IntentUnavailable)?
                    .into(),
                },
                _ => return Err(LaunchError::IntentUnavailable),
            };
            Ok(IntentEntry {
                record,
                claimed: false,
                accepted: false,
                status: watch::channel(status).0,
            })
        })
        .transpose()
    }

    fn terminal_session(
        &self,
        record: &IntentRecord,
        payload: &[u8],
        settlement: Option<&[u8]>,
    ) -> Result<Option<SessionSnapshot>, LaunchError> {
        let observation = settlement
            .map(|bytes| decode_settlement(record, payload, bytes))
            .transpose()?;
        if let Some(report) = self
            .reports
            .as_ref()
            .map(|reports| reports.get(&record.session_id))
            .transpose()
            .map_err(|_| LaunchError::IntentUnavailable)?
            .flatten()
        {
            if observation
                .as_ref()
                .is_some_and(|value| !value.matches_report(&report))
            {
                return Err(LaunchError::IntentUnavailable);
            }
            return terminal_snapshot(record, &report).map(Some);
        }
        Ok(observation.map(|value| {
            value.snapshot(
                record.session_id.clone(),
                record.request.instance_id.clone(),
            )
        }))
    }

    fn accept(&self, key: &str, binding: IntentBinding) -> Result<AcceptedIntent, LaunchError> {
        let mut entries = self.entries.lock().map_err(|_| LaunchError::Closed)?;
        let entry = entries.get_mut(key).ok_or(LaunchError::IntentUnavailable)?;
        if !entry.claimed
            || entry.accepted
            || !matches!(*entry.status.borrow(), LaunchIntentStatus::Preparing)
            || binding.version != 1
            || binding.directory_name != entry.record.request.instance_id.as_str()
        {
            return Err(LaunchError::IntentConflict);
        }
        let mut record = entry.record.clone();
        record.binding = Some(binding);
        let payload = serde_json::to_vec(&record).map_err(|_| LaunchError::IntentUnavailable)?;
        if let Some(storage) = &self.storage {
            storage.transaction(|tx| -> Result<(), StorageError> {
                let prior: Vec<u8> = tx.query_row(
                    "SELECT CASE WHEN length(payload)<=16384 THEN payload END FROM launch_intents WHERE intent_key=?1 AND state='pending' AND settlement IS NULL",
                    [key], |row| row.get(0),
                )?;
                if decode_intent(key, &prior).map_err(|_| StorageError::Corrupt)? != entry.record {
                    return Err(StorageError::Corrupt);
                }
                if tx.execute("UPDATE launch_intents SET payload=?2,state='accepted' WHERE intent_key=?1 AND state='pending' AND payload=?3 AND settlement IS NULL", params![key, payload, prior])? != 1 {
                    return Err(StorageError::Corrupt);
                }
                let saved: (Vec<u8>, String, Option<Vec<u8>>) = tx.query_row(
                    "SELECT payload,state,settlement FROM launch_intents WHERE intent_key=?1", [key],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )?;
                if saved != (payload.clone(), "accepted".into(), None) { return Err(StorageError::Corrupt); }
                Ok(())
            }).map_err(|_| LaunchError::IntentUnavailable)?;
        }
        entry.record = record.clone();
        entry.accepted = true;
        Ok(AcceptedIntent {
            storage: self.storage.clone(),
            key: key.into(),
            payload,
            record,
        })
    }

    fn abandon(&self, key: &str) {
        let Ok(entries) = self.entries.lock() else {
            return;
        };
        let Some(entry) = entries.get(key) else {
            return;
        };
        if entry.accepted {
            entry.status.send_replace(LaunchIntentStatus::Interrupted {
                session_id: entry.record.session_id.clone(),
            });
            // Keep durable acceptance: startup converts it to interruption.
        } else {
            drop(entries);
            self.settle(key, Err(LaunchError::PreparationFailed));
        }
    }

    fn settle(&self, key: &str, result: Result<SessionSnapshot, LaunchError>) {
        let Ok(entries) = self.entries.lock() else {
            return;
        };
        let Some(entry) = entries.get(key) else {
            return;
        };
        if !matches!(*entry.status.borrow(), LaunchIntentStatus::Preparing) {
            return;
        }
        let result = match result {
            Err(error) => self.reject(key, error).and(Err(error)),
            success => success,
        };
        entry.status.send_replace(match result {
            Ok(session) => LaunchIntentStatus::Accepted { session },
            Err(error) => LaunchIntentStatus::Rejected {
                error: error.into(),
            },
        });
    }

    fn reject(&self, key: &str, error: LaunchError) -> Result<(), LaunchError> {
        if let Some(storage) = &self.storage {
            let error =
                serde_json::to_string(&error).map_err(|_| LaunchError::IntentUnavailable)?;
            storage
                .transaction(|tx| -> Result<(), StorageError> {
                    tx.execute(
                        "UPDATE launch_intents SET state='rejected',error=?2 WHERE intent_key=?1",
                        params![key, error],
                    )?;
                    Ok(())
                })
                .map_err(|_| LaunchError::IntentUnavailable)?;
        }
        Ok(())
    }

    fn snapshot(&self, key: &str) -> Result<Option<LaunchIntentStatus>, LaunchError> {
        validate_intent_key(key)?;
        let entries = self.entries.lock().map_err(|_| LaunchError::Closed)?;
        if let Some(entry) = entries.get(key) {
            if entry.accepted {
                if let Some(storage) = &self.storage {
                    let settled = storage.read(|db| -> Result<bool, StorageError> {
                        Ok(db.query_row("SELECT settlement IS NOT NULL FROM launch_intents WHERE intent_key=?1", [key], |row| row.get(0))?)
                    }).map_err(|_| LaunchError::IntentUnavailable)?;
                    if settled {
                        return Ok(self.load(key)?.map(|loaded| loaded.status.borrow().clone()));
                    }
                }
            }
            return Ok(Some(entry.status.borrow().clone()));
        }
        Ok(self.load(key)?.map(|entry| entry.status.borrow().clone()))
    }

    fn record(&self, key: &str) -> Result<Option<IntentRecord>, LaunchError> {
        validate_intent_key(key)?;
        let entries = self.entries.lock().map_err(|_| LaunchError::Closed)?;
        if let Some(entry) = entries.get(key) {
            return Ok(Some(entry.record.clone()));
        }
        Ok(self.load(key)?.map(|entry| entry.record))
    }
}

fn terminal_snapshot(
    record: &IntentRecord,
    report: &super::reports::LaunchProofRecord,
) -> Result<SessionSnapshot, LaunchError> {
    let scenario = record
        .context
        .as_deref()
        .map(serde_json::from_str::<super::reports::LaunchProofScenario>)
        .transpose()
        .map_err(|_| LaunchError::IntentUnavailable)?
        .unwrap_or_default();
    // scenario_id is derived after preparation. The captured benchmark
    // dimensions, requested version and memory are the durable request identity.
    if report.session_id != record.session_id
        || report.instance_id != record.request.instance_id.as_str()
        || report.scenario.benchmark_id != scenario.benchmark_id
        || report.scenario.benchmark_profile != scenario.benchmark_profile
        || report.scenario.benchmark_run_type != scenario.benchmark_run_type
        || report.scenario.benchmark_mode != scenario.benchmark_mode
        || report.scenario.version_id.as_deref() != Some(report.version_id.as_str())
        || record
            .request
            .version_id
            .as_ref()
            .is_some_and(|version| version != &report.version_id)
        || record
            .request
            .max_memory_mb
            .filter(|value| *value > 0)
            .is_some_and(|memory| report.scenario.requested_memory_mb != Some(memory))
    {
        return Err(LaunchError::IntentUnavailable);
    }
    Ok(SessionSnapshot::from_report(
        report,
        record.request.instance_id.clone(),
    ))
}

/// Called only inside the report owner's transaction with canonical, persisted
/// terminal proof. This does not create an intent or authorize a process.
pub(super) fn acknowledge_terminal(
    tx: &crate::storage::rusqlite::Transaction<'_>,
    report: &super::reports::LaunchProofRecord,
) -> Result<(), super::reports::ReportError> {
    use super::reports::ReportError;
    let exists: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='launch_intents')",
        [],
        |row| row.get(0),
    )?;
    if !exists {
        return Ok(());
    }
    let mut query = tx.prepare(
        "SELECT CASE WHEN length(intent_key)=36 THEN intent_key END,
         CASE WHEN length(payload)<=16384 THEN payload END,terminal_ack,settlement FROM launch_intents
         WHERE state IN ('accepted','interrupted') AND
         json_extract(CASE WHEN json_valid(CAST(payload AS TEXT)) THEN CAST(payload AS TEXT) ELSE '{}' END, '$.session_id')=?1 LIMIT 2",
    )?;
    let rows = query
        .query_map([&report.session_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, bool>(2)?,
                row.get::<_, Option<Vec<u8>>>(3)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    if rows.len() > 1 {
        return Err(ReportError::Invalid);
    }
    if let Some((key, payload, acknowledged, settlement)) = rows.into_iter().next() {
        let record = decode_intent(&key, &payload).map_err(|_| ReportError::Invalid)?;
        terminal_snapshot(&record, report).map_err(|_| ReportError::Invalid)?;
        if let Some(bytes) = &settlement {
            let observation =
                decode_settlement(&record, &payload, bytes).map_err(|_| ReportError::Invalid)?;
            if !observation.matches_report(report) {
                return Err(ReportError::Invalid);
            }
        }
        if !acknowledged
            && tx.execute(
                "UPDATE launch_intents SET terminal_ack=1 WHERE intent_key=?1 AND payload=?2
             AND state IN ('accepted','interrupted') AND terminal_ack=0",
                params![key, payload],
            )? != 1
        {
            return Err(ReportError::Invalid);
        }
        let saved: (Vec<u8>, bool, Option<Vec<u8>>) = tx.query_row(
            "SELECT payload,terminal_ack,settlement FROM launch_intents WHERE intent_key=?1 AND state IN ('accepted','interrupted')",
            [&key], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        if saved != (payload, true, settlement) {
            return Err(ReportError::Invalid);
        }
    }
    Ok(())
}

fn decode_intent(key: &str, payload: &[u8]) -> Result<IntentRecord, LaunchError> {
    if payload.len() > 16_384 {
        return Err(LaunchError::IntentUnavailable);
    }
    let record: IntentRecord =
        serde_json::from_slice(payload).map_err(|_| LaunchError::IntentUnavailable)?;
    record
        .request
        .validate()
        .map_err(|_| LaunchError::IntentUnavailable)?;
    if record.request.intent_key.as_deref() != Some(key)
        || validate_intent_key(&record.session_id).is_err()
    {
        return Err(LaunchError::IntentUnavailable);
    }
    Ok(record)
}

#[cfg(test)]
pub(super) fn accepted_for_test(
    storage: Arc<MetadataStore>,
) -> (AcceptedIntent, String, InstanceId) {
    let intents = tests::durable_intents(storage);
    let request = tests::request();
    let ReservedIntent::New {
        key, session_id, ..
    } = intents.reserve(&request).unwrap()
    else {
        panic!()
    };
    let acceptance = intents.accept(&key, tests::binding(&request)).unwrap();
    (acceptance, session_id, request.instance_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    async fn preflight_fixture() -> (tempfile::TempDir, LaunchCoordinator, InstanceId) {
        use crate::{
            accounts::credential_store::CredentialStore,
            network::{ClientConfig, ProviderClient},
            skins::{ProfileMedia, library::SavedSkinLibrary, store::SavedSkinStore},
        };
        use std::os::unix::fs::PermissionsExt;

        let (root, directories, storage, request) = recovery_fixture().await;
        storage
            .migrate(&[
                crate::install::queue::MIGRATION,
                crate::install::queue::MIGRATION_V2,
                crate::performance::rules::MIGRATION,
                crate::skins::store::MIGRATION,
            ])
            .unwrap();
        let tasks = TaskOwner::new(16).unwrap();
        let cache = axial_minecraft::ManagedRuntimeCache::isolated_for_test().unwrap();
        let installs = InstallQueue::new(
            storage.clone(),
            directories.library().clone(),
            directories.exclusions().clone(),
            tasks.clone(),
            cache.clone(),
        )
        .unwrap();
        crate::install::queue::tests::install_ready_fixture(&installs, "1.20.1").await;
        let pin = directories.library().admit().unwrap();
        let installed = installs.ready_version(&pin, "1.20.1").await.unwrap();
        let major = installed.version().java_version.major_version;
        let version = if major == 8 {
            "1.8.0_312".into()
        } else {
            format!("{major}.0.3")
        };
        let java = root.path().join("probe-java");
        std::fs::write(&java, format!(
            "#!/bin/sh\nprobe_dir=${{0%/*}}\nprintf '%s\\n' \"$$\" > \"$probe_dir/probe-started\"\nwhile [ ! -f \"$probe_dir/probe-release\" ]; do sleep 0.01; done\nif [ -f \"$probe_dir/probe-fail\" ]; then exit 1; fi\nprintf 'java.version = {version}\\nos.arch = {}\\njava.vendor = Eclipse Adoptium\\n' >&2\n",
            std::env::consts::ARCH,
        )).unwrap();
        std::fs::set_permissions(&java, std::fs::Permissions::from_mode(0o755)).unwrap();
        let settings = Arc::new(SettingsStore::new(storage.clone()).unwrap());
        settings
            .update(
                serde_json::from_value(serde_json::json!({
                    "expected_revision":0, "performance_mode":"vanilla",
                    "java_path_override":java.to_str().unwrap(),
                }))
                .unwrap(),
            )
            .unwrap();
        let accounts = Arc::new(AccountDirectory::new(storage.clone()).unwrap());
        accounts.create_offline_account("Preflight").unwrap();
        let auth = Arc::new(AuthService::new(
            accounts.clone(),
            Arc::new(CredentialStore::isolated_for_tests()),
            tasks.clone(),
        ));
        let content = Arc::new(
            crate::content::catalog::ContentService::new(
                ProviderClient::new(ClientConfig::default()).unwrap(),
            )
            .unwrap(),
        );
        let performance = PerformanceService::new(
            storage.clone(),
            directories.clone(),
            tasks.clone(),
            content,
            crate::performance::public_transfer_resolver(),
        )
        .unwrap();
        let root_pin = directories.library().admit_application_root().unwrap();
        let skins = ProfileMedia::new(
            Arc::new(SavedSkinLibrary::new(
                SavedSkinStore::new(storage),
                root_pin.clone(),
            )),
            accounts.clone(),
            auth.clone(),
            tasks.clone(),
            root_pin,
        )
        .unwrap();
        let coordinator = LaunchCoordinator::new(
            directories,
            accounts,
            settings,
            installs,
            RuntimeDiscovery::new(cache, tasks.clone()),
            performance,
            auth,
            skins,
            SessionManager::new(tasks.clone()),
            tasks,
        );
        (root, coordinator, request.instance_id)
    }

    #[cfg(unix)]
    async fn wait_for_preflight_probe(
        root: &std::path::Path,
    ) -> Result<i32, tokio::time::error::Elapsed> {
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                // Redirection creates the file before printf writes its PID.
                if let Some(pid) = std::fs::read_to_string(root.join("probe-started"))
                    .ok()
                    .and_then(|value| value.strip_suffix('\n')?.parse::<i32>().ok())
                    .filter(|pid| *pid > 1)
                {
                    return pid;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn preflight_probe_does_not_reserve_the_foreground_launch_target() {
        let (root, coordinator, id) = preflight_fixture().await;
        assert!(
            !coordinator
                .performance
                .rules()
                .status()
                .status
                .remote_refresh
        );
        let waiter = tokio::spawn({
            let coordinator = coordinator.clone();
            let id = id.clone();
            async move { coordinator.preflight(id).await }
        });
        let started = wait_for_preflight_probe(root.path()).await;
        let foreground = coordinator.admit(&id);
        let accepted = foreground.is_ok();
        let refusal = foreground.err();
        let pin = coordinator.instances.library().admit().unwrap();
        let operation = pin.managed_library().unwrap();
        let artifact = crate::install::queue::library_artifact(&pin.library_id().to_string());
        let artifact_blocked = coordinator
            .instances
            .exclusions()
            .try_acquire(std::iter::empty::<String>(), [artifact.clone()])
            .is_err();
        let publication_blocked =
            axial_minecraft::VersionBundlePublicationGuardForTest::acquire(&operation).is_err();
        let rules_blocked = tokio::time::timeout(
            std::time::Duration::from_millis(10),
            coordinator.performance.rules().refresh(),
        )
        .await
        .is_err();
        std::fs::write(root.path().join("probe-release"), b"release").unwrap();
        let result = waiter.await.unwrap();
        coordinator
            .tasks
            .shutdown(std::time::Duration::from_secs(3))
            .await
            .unwrap();
        assert!(
            started.is_ok(),
            "preflight did not reach its owned Java probe: {result:?}"
        );
        assert!(
            accepted,
            "foreground launch admission was blocked by preflight: {refusal:?}"
        );
        assert!(result.launchable, "{result:?}");
        assert!(artifact_blocked && publication_blocked && rules_blocked);
        assert!(
            coordinator
                .instances
                .exclusions()
                .try_acquire(std::iter::empty::<String>(), [artifact])
                .is_ok()
        );
        assert!(axial_minecraft::VersionBundlePublicationGuardForTest::acquire(&operation).is_ok());
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_secs(1),
                coordinator.performance.rules().refresh(),
            )
            .await
            .is_ok()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn preflight_rechecks_captured_inputs_even_when_the_probe_fails() {
        use crate::instances::model::InstancePatch;

        for (change, fail_probe, expected) in [
            ("instance", false, Some(LaunchError::InstanceChanged)),
            ("instance", true, Some(LaunchError::InstanceChanged)),
            ("global", true, Some(LaunchError::SettingsChanged)),
            ("account", true, Some(LaunchError::AccountChanged)),
            ("foreground", false, Some(LaunchError::InstanceBusy)),
            ("foreground", true, Some(LaunchError::InstanceBusy)),
            ("display", false, None),
            ("none", true, Some(LaunchError::RuntimeUnavailable)),
        ] {
            let (root, coordinator, id) = preflight_fixture().await;
            if fail_probe {
                std::fs::write(root.path().join("probe-fail"), b"fail").unwrap();
            }
            let waiter = tokio::spawn({
                let coordinator = coordinator.clone();
                let id = id.clone();
                async move { coordinator.preflight(id).await }
            });
            let started = wait_for_preflight_probe(root.path()).await;
            let mut foreground = None;
            let changed = (|| -> Result<(), LaunchError> {
                match change {
                    "instance" | "display" => {
                        let admitted = coordinator.admit(&id)?;
                        if change == "display" {
                            admitted
                                .record_successful_launch("2026-09-27T10:00:00.000Z")
                                .map_err(instance_error)?;
                        }
                        let record = coordinator
                            .instances
                            .registry()
                            .get_live(&id)
                            .map_err(instance_error)?;
                        let patch = if change == "instance" {
                            InstancePatch {
                                max_memory_mb: Some(8192),
                                ..Default::default()
                            }
                        } else {
                            InstancePatch {
                                name: Some("Renamed during probe".into()),
                                ..Default::default()
                            }
                        };
                        coordinator
                            .instances
                            .registry()
                            .update(&id, record.revision, patch)
                            .map_err(instance_error)?;
                    }
                    "global" => {
                        let revision = coordinator
                            .settings
                            .current()
                            .map_err(|_| LaunchError::SettingsChanged)?
                            .revision;
                        coordinator
                            .settings
                            .update(
                                serde_json::from_value(serde_json::json!({
                                    "expected_revision":revision, "max_memory_mb":8192,
                                }))
                                .unwrap(),
                            )
                            .map_err(|_| LaunchError::SettingsChanged)?;
                    }
                    "account" => {
                        coordinator
                            .accounts
                            .create_offline_account("Elsewhere")
                            .map_err(|_| LaunchError::AccountChanged)?;
                    }
                    "foreground" => foreground = Some(coordinator.admit(&id)?),
                    "none" => {}
                    _ => unreachable!(),
                }
                Ok(())
            })();
            let released = std::fs::write(root.path().join("probe-release"), b"release");
            let result = waiter.await;
            drop(foreground);
            let stopped = coordinator
                .tasks
                .shutdown(std::time::Duration::from_secs(3))
                .await;
            assert!(started.is_ok(), "{change}: probe did not start");
            assert!(changed.is_ok(), "{change}: {changed:?}");
            released.unwrap();
            stopped.unwrap();
            let result = result.unwrap();
            assert_eq!(result.error.map(|error| error.code), expected, "{change}");
            assert_eq!(result.launchable, expected.is_none(), "{change}");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn preflight_waiter_loss_retains_artifacts_until_the_probe_is_reaped() {
        let (root, coordinator, id) = preflight_fixture().await;
        let waiter = tokio::spawn({
            let coordinator = coordinator.clone();
            let id = id.clone();
            async move { coordinator.preflight(id).await }
        });
        let started = wait_for_preflight_probe(root.path()).await;
        let pid = started.as_ref().ok().copied();
        waiter.abort();
        let cancelled = waiter.await.is_err_and(|error| error.is_cancelled());
        let retained = !coordinator.tasks.status().is_idle();
        let foreground_available = coordinator.admit(&id).is_ok();
        let pin = coordinator.instances.library().admit().unwrap();
        let artifact = crate::install::queue::library_artifact(&pin.library_id().to_string());
        let artifact_blocked = coordinator
            .instances
            .exclusions()
            .try_acquire(std::iter::empty::<String>(), [artifact.clone()])
            .is_err();
        let stopped = coordinator
            .tasks
            .shutdown(std::time::Duration::from_secs(3))
            .await;
        // Release even on a shutdown regression before making assertions.
        let released = std::fs::write(root.path().join("probe-release"), b"release");
        let drained = coordinator
            .tasks
            .shutdown(std::time::Duration::from_secs(3))
            .await;
        let reap_observation = pid.map(|pid| {
            // The PID is emitted by this test's retained diagnostic child.
            let result = unsafe { libc::kill(pid, 0) };
            (result, std::io::Error::last_os_error().raw_os_error())
        });
        assert!(started.is_ok() && cancelled && retained);
        assert!(foreground_available && artifact_blocked);
        released.unwrap();
        stopped.unwrap();
        drained.unwrap();
        assert_eq!(
            reap_observation,
            Some((-1, Some(libc::ESRCH))),
            "PID {pid:?}"
        );
        assert!(coordinator.tasks.status().is_idle());
        assert!(
            coordinator
                .instances
                .exclusions()
                .try_acquire(std::iter::empty::<String>(), [artifact])
                .is_ok()
        );
    }

    #[test]
    fn profile_commit_can_advance_revision_but_cannot_replace_the_selected_login() {
        use crate::accounts::{microsoft::MinecraftProfile, model::MicrosoftIdentity};
        let accounts = AccountDirectory::new(Arc::new(
            crate::storage::MetadataStore::in_memory().unwrap(),
        ))
        .unwrap();
        let profile_id = uuid::Uuid::new_v4().simple().to_string();
        let mut identity = MicrosoftIdentity {
            login_id: uuid::Uuid::new_v4().to_string(),
            profile_id: profile_id.clone(),
            display_name: "PlayerOne".into(),
            credential_revision: 1,
            profile: MinecraftProfile {
                id: profile_id,
                name: "PlayerOne".into(),
                skins: vec![],
                capes: vec![],
            },
        };
        let before = accounts
            .commit_microsoft(accounts.selection_revision().unwrap(), identity.clone())
            .unwrap();
        identity.profile.name = "PlayerTwo".into();
        identity.display_name = "PlayerTwo".into();
        accounts
            .refresh_account_microsoft(&before, identity.clone())
            .unwrap();
        let after = capture_after_profile(&accounts, &before).unwrap();
        assert!(after.selection_revision() > before.selection_revision());
        assert_eq!(after.display_name(), "PlayerTwo");
        accounts.create_offline_account("Elsewhere").unwrap();
        assert!(matches!(
            capture_after_profile(&accounts, &after),
            Err(LaunchError::AccountChanged)
        ));
        accounts.select(after.account_id()).unwrap();
        identity.login_id = uuid::Uuid::new_v4().to_string();
        identity.credential_revision = 2;
        accounts
            .commit_microsoft(accounts.selection_revision().unwrap(), identity)
            .unwrap();
        assert!(matches!(
            capture_after_profile(&accounts, &after),
            Err(LaunchError::AccountChanged)
        ));
    }

    pub(super) fn request() -> LaunchRequest {
        LaunchRequest {
            instance_id: InstanceId::new(),
            version_id: None,
            username: Some("Player".into()),
            max_memory_mb: None,
            min_memory_mb: None,
            client_started_at_ms: None,
            intent_key: Some(uuid::Uuid::new_v4().to_string()),
        }
    }

    pub(super) fn binding(request: &LaunchRequest) -> IntentBinding {
        IntentBinding {
            version: 1,
            library_id: uuid::Uuid::new_v4().to_string(),
            library_root: [1; 32],
            application_root: [2; 32],
            directory_name: request.instance_id.to_string(),
            directory_receipt: format!("axial-dir-v1:{}:{}", "01".repeat(32), "02".repeat(32)),
        }
    }

    #[test]
    fn intent_retry_retains_rejection_and_cannot_change_target() {
        let intents = LaunchIntents::new(2);
        let request = request();
        let ReservedIntent::New { key, .. } = intents.reserve(&request).unwrap() else {
            panic!()
        };
        intents.settle(&key, Err(LaunchError::AccountUnavailable));
        let ReservedIntent::Existing(receiver) = intents.reserve(&request).unwrap() else {
            panic!()
        };
        assert!(matches!(
            *receiver.borrow(),
            LaunchIntentStatus::Rejected { .. }
        ));
        let mut different = request.clone();
        different.instance_id = InstanceId::new();
        assert!(matches!(
            intents.reserve(&different),
            Err(LaunchError::IntentConflict)
        ));
    }

    pub(super) fn durable_intents(storage: Arc<MetadataStore>) -> LaunchIntents {
        storage
            .migrate(&[
                INTENT_MIGRATION,
                INTENT_TERMINAL_MIGRATION,
                INTENT_SETTLEMENT_MIGRATION,
            ])
            .unwrap();
        let reports = super::super::reports::LaunchReportStore::new(storage.clone()).unwrap();
        LaunchIntents::with_storage(storage, reports, 8).unwrap()
    }

    fn refuse_reports(storage: &MetadataStore) {
        storage.transaction(|tx| -> Result<(), StorageError> {
            tx.execute_batch("CREATE TRIGGER refuse_report BEFORE INSERT ON launch_reports BEGIN SELECT RAISE(ABORT,'unavailable'); END;")?;
            Ok(())
        }).unwrap();
    }

    async fn observed_fixture(
        storage: Arc<MetadataStore>,
    ) -> (
        LaunchIntents,
        LaunchRequest,
        AcceptedIntent,
        SessionSnapshot,
        super::super::reports::LaunchProofRecord,
    ) {
        let intents = durable_intents(storage.clone());
        let request = request();
        let ReservedIntent::New {
            key, session_id, ..
        } = intents.reserve(&request).unwrap()
        else {
            panic!()
        };
        let acceptance = intents.accept(&key, binding(&request)).unwrap();
        refuse_reports(&storage);
        let (session, report) = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            super::super::session::finish_unstarted_for_test(
                acceptance.clone(),
                session_id,
                request.instance_id.clone(),
                "1.21.1".into(),
                intents.reports.clone().unwrap(),
                true,
            ),
        )
        .await
        .expect("report failure must not retain settled owner");
        (intents, request, acceptance, session, report.unwrap())
    }

    #[tokio::test]
    async fn observed_settlement_survives_report_failure_and_disk_reopen() {
        let root = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
        let path = root.path().join("metadata.sqlite");
        let storage = Arc::new(MetadataStore::open(&path).unwrap());
        let (intents, request, acceptance, session, _) = observed_fixture(storage.clone()).await;
        let key = request.intent_key.as_deref().unwrap();
        assert_eq!(session.phase, super::super::session::SessionPhase::Exited);
        assert!(
            session.tree_settled
                && session.output_drained
                && !session.process_alive
                && !session.stop_allowed
        );
        assert_eq!(session.notice.as_ref().unwrap().tone, "warned");
        assert!(
            session
                .notice
                .as_ref()
                .unwrap()
                .message
                .contains("report is unavailable")
        );
        assert!(
            intents
                .reports
                .as_ref()
                .unwrap()
                .get(&session.session_id)
                .unwrap()
                .is_none()
        );
        assert!(!terminal_ack(&storage, key));
        assert!(
            intents
                .interrupted_records_with_limits(0, 0)
                .unwrap()
                .is_empty()
        );
        assert!(
            matches!(intents.snapshot(key).unwrap(), Some(LaunchIntentStatus::Accepted { session }) if session.view_model.terminal)
        );
        drop((intents, acceptance, storage));
        let reopened = durable_intents(Arc::new(MetadataStore::open(&path).unwrap()));
        let Some(LaunchIntentStatus::Accepted { session: restored }) =
            reopened.snapshot(key).unwrap()
        else {
            panic!()
        };
        assert_eq!(restored.session_id, session.session_id);
        assert_eq!(restored.launched_at, session.launched_at);
        assert_eq!(restored.outcome, session.outcome);
        assert_eq!(restored.notice, session.notice);
        assert!(matches!(
            reopened.reserve(&request).unwrap(),
            ReservedIntent::Existing(_)
        ));
        // Another accepted attempt still requires recovery; terminal evidence is
        // never a library-wide waiver of unrelated obligations.
        let other = self::request();
        reopened.reserve(&other).unwrap();
        reopened
            .accept(other.intent_key.as_deref().unwrap(), binding(&other))
            .unwrap();
        let unresolved = reopened.interrupted_records_with_limits(1, 16384).unwrap();
        assert_eq!(unresolved.len(), 1);
        assert_eq!(unresolved[0].request.instance_id, other.instance_id);
    }

    #[tokio::test]
    async fn observed_settlement_is_immutable_and_report_concordance_is_required() {
        let storage = Arc::new(MetadataStore::in_memory().unwrap());
        let (intents, request, acceptance, session, report) =
            observed_fixture(storage.clone()).await;
        let key = request.intent_key.as_deref().unwrap();
        let bytes: Vec<u8> = storage
            .read(|db| -> Result<_, StorageError> {
                Ok(db.query_row(
                    "SELECT settlement FROM launch_intents WHERE intent_key=?1",
                    [key],
                    |row| row.get(0),
                )?)
            })
            .unwrap();
        let observation =
            decode_settlement(&acceptance.record, &acceptance.payload, &bytes).unwrap();
        assert_eq!(acceptance.observe(&observation), Ok(true));
        let mut changed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        changed["observation"]["ended_at"] = "2026-01-01T00:00:00.000Z".into();
        let changed: SettlementRecord = serde_json::from_value(changed).unwrap();
        assert_eq!(
            acceptance.observe(&changed.observation),
            Err(ObservationError::Conflict)
        );
        storage
            .transaction(|tx| -> Result<(), StorageError> {
                tx.execute_batch("DROP TRIGGER refuse_report")?;
                Ok(())
            })
            .unwrap();
        let reports = intents.reports.as_ref().unwrap();
        for field in ["version", "launched", "ended", "exit", "boot", "outcome"] {
            let mut contradictory = report.clone();
            match field {
                "version" => {
                    contradictory.version_id = "1.20.1".into();
                    contradictory.scenario.version_id = Some("1.20.1".into());
                }
                "launched" => contradictory.launched_at = "2026-01-01T00:00:00.000Z".into(),
                "ended" => contradictory.recorded_at = "2026-01-01T00:00:01.000Z".into(),
                "exit" => contradictory.exit_code = Some(7),
                "boot" => contradictory.boot_duration_ms = Some(1),
                "outcome" => {
                    contradictory.session_outcome.reason =
                        super::super::outcome::SessionExitReason::StartupFailed;
                    contradictory.session_outcome.summary =
                        contradictory.session_outcome.summary().into();
                }
                _ => unreachable!(),
            }
            assert!(
                reports
                    .record(contradictory, &super::super::logs::Redactor::new(vec![]))
                    .is_err(),
                "{field}"
            );
            assert!(reports.get(&session.session_id).unwrap().is_none());
            assert!(!terminal_ack(&storage, key));
        }
        reports
            .record(report, &super::super::logs::Redactor::new(vec![]))
            .unwrap();
        assert!(terminal_ack(&storage, key));
        let Some(LaunchIntentStatus::Accepted { session }) = intents.snapshot(key).unwrap() else {
            panic!()
        };
        assert_ne!(session.notice.unwrap().tone, "warned");
    }

    #[tokio::test]
    async fn observed_settlement_rejects_unsupported_and_noncanonical_evidence() {
        for change in [
            "unsupported",
            "boolean",
            "malformed",
            "binding",
            "noncanonical",
        ] {
            let storage = Arc::new(MetadataStore::in_memory().unwrap());
            let (intents, request, acceptance, _, _) = observed_fixture(storage.clone()).await;
            let key = request.intent_key.as_deref().unwrap();
            let original: Vec<u8> = storage
                .read(|db| -> Result<_, StorageError> {
                    Ok(db.query_row(
                        "SELECT settlement FROM launch_intents WHERE intent_key=?1",
                        [key],
                        |row| row.get(0),
                    )?)
                })
                .unwrap();
            let observation =
                decode_settlement(&acceptance.record, &acceptance.payload, &original).unwrap();
            storage
                .transaction(|tx| -> Result<(), StorageError> {
                    let bytes: Vec<u8> = tx.query_row(
                        "SELECT settlement FROM launch_intents WHERE intent_key=?1",
                        [key],
                        |row| row.get(0),
                    )?;
                    let mut value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                    let altered = match change {
                        "unsupported" => {
                            value["version"] = 2.into();
                            serde_json::to_vec(&value).unwrap()
                        }
                        "boolean" => {
                            value["version"] = true.into();
                            serde_json::to_vec(&value).unwrap()
                        }
                        "malformed" => b"{".to_vec(),
                        "binding" => {
                            value["accepted_payload"][0] =
                                (value["accepted_payload"][0].as_u64().unwrap() ^ 1).into();
                            serde_json::to_vec(&value).unwrap()
                        }
                        "noncanonical" => {
                            let mut bytes = bytes;
                            bytes.push(b' ');
                            bytes
                        }
                        _ => unreachable!(),
                    };
                    tx.execute(
                        "UPDATE launch_intents SET settlement=?2 WHERE intent_key=?1",
                        params![key, altered],
                    )?;
                    Ok(())
                })
                .unwrap();
            assert!(
                matches!(intents.snapshot(key), Err(LaunchError::IntentUnavailable)),
                "{change}"
            );
            if matches!(change, "unsupported" | "boolean" | "malformed") {
                assert!(
                    matches!(
                        intents.interrupted_records(),
                        Err(LaunchError::IntentUnavailable)
                    ),
                    "{change}"
                );
            }
            assert_eq!(
                acceptance.observe(&observation),
                Err(ObservationError::Conflict)
            );
        }
    }

    #[tokio::test]
    async fn observed_settlement_write_faults_retain_owner_until_verified_retry() {
        for trigger in [
            "CREATE TRIGGER refuse_settlement BEFORE UPDATE OF settlement ON launch_intents BEGIN SELECT RAISE(ABORT,'unavailable'); END;",
            "CREATE TRIGGER refuse_settlement BEFORE UPDATE OF settlement ON launch_intents BEGIN SELECT RAISE(IGNORE); END;",
            "CREATE TRIGGER refuse_settlement AFTER UPDATE OF settlement ON launch_intents BEGIN UPDATE launch_intents SET settlement=NULL WHERE intent_key=NEW.intent_key; END;",
        ] {
            let storage = Arc::new(MetadataStore::in_memory().unwrap());
            let intents = durable_intents(storage.clone());
            let request = request();
            let ReservedIntent::New {
                key, session_id, ..
            } = intents.reserve(&request).unwrap()
            else {
                panic!()
            };
            let acceptance = intents.accept(&key, binding(&request)).unwrap();
            storage
                .transaction(|tx| -> Result<(), StorageError> {
                    tx.execute_batch(trigger)?;
                    Ok(())
                })
                .unwrap();
            let mut finish = Box::pin(super::super::session::finish_unstarted_for_test(
                acceptance,
                session_id.clone(),
                request.instance_id,
                "1.21.1".into(),
                intents.reports.clone().unwrap(),
                true,
            ));
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(200), &mut finish)
                    .await
                    .is_err()
            );
            assert!(
                intents
                    .reports
                    .as_ref()
                    .unwrap()
                    .get(&session_id)
                    .unwrap()
                    .is_none()
            );
            assert!(!terminal_ack(&storage, &key));
            assert_eq!(intents.interrupted_records().unwrap().len(), 1);
            storage
                .transaction(|tx| -> Result<(), StorageError> {
                    let settlement: Option<Vec<u8>> = tx.query_row(
                        "SELECT settlement FROM launch_intents WHERE intent_key=?1",
                        [&key],
                        |row| row.get(0),
                    )?;
                    assert!(settlement.is_none(), "failed publication must roll back");
                    tx.execute_batch("DROP TRIGGER refuse_settlement")?;
                    Ok(())
                })
                .unwrap();
            let (session, _) = tokio::time::timeout(std::time::Duration::from_secs(2), finish)
                .await
                .unwrap();
            assert_eq!(session.phase, super::super::session::SessionPhase::Exited);
            assert!(terminal_ack(&storage, &key));
            assert!(intents.interrupted_records().unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn observed_settlement_without_durable_owner_does_not_release_failed_report() {
        let storage = Arc::new(MetadataStore::in_memory().unwrap());
        let reports = super::super::reports::LaunchReportStore::new(storage.clone()).unwrap();
        let intents = LaunchIntents::new(8);
        let request = request();
        let ReservedIntent::New {
            key, session_id, ..
        } = intents.reserve(&request).unwrap()
        else {
            panic!()
        };
        let acceptance = intents.accept(&key, binding(&request)).unwrap();
        refuse_reports(&storage);
        let mut finish = Box::pin(super::super::session::finish_unstarted_for_test(
            acceptance,
            session_id.clone(),
            request.instance_id,
            "1.21.1".into(),
            reports.clone(),
            true,
        ));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(200), &mut finish)
                .await
                .is_err()
        );
        assert!(reports.get(&session_id).unwrap().is_none());
        storage
            .transaction(|tx| -> Result<(), StorageError> {
                tx.execute_batch("DROP TRIGGER refuse_report")?;
                Ok(())
            })
            .unwrap();
        let (session, _) = tokio::time::timeout(std::time::Duration::from_secs(2), finish)
            .await
            .unwrap();
        assert_eq!(session.phase, super::super::session::SessionPhase::Exited);
        assert!(reports.get(&session_id).unwrap().is_some());
    }

    fn terminal_report(
        request: &LaunchRequest,
        session_id: String,
    ) -> super::super::reports::LaunchProofRecord {
        use crate::launch::{
            outcome::SessionOutcome,
            reports::{LaunchProofRecord, SessionReportInput},
        };
        let mut report = LaunchProofRecord::from_session(SessionReportInput {
            session_id,
            instance_id: request.instance_id.to_string(),
            version_id: "fixture".into(),
            launched_at: "2026-09-27T10:00:00.000Z".into(),
            ended_at: "2026-09-27T10:00:01.000Z".into(),
            outcome: SessionOutcome::spawn_failed(),
            entries: vec![],
            exit_code: None,
            boot_duration_ms: None,
            logs_dropped: 0,
        });
        report.scenario.version_id = Some("fixture".into());
        report
    }

    fn insert_historical_report(
        storage: &MetadataStore,
        report: &super::super::reports::LaunchProofRecord,
    ) {
        let bytes = super::super::reports::encode_report(report).unwrap();
        storage.transaction(|tx| -> Result<(), StorageError> {
            tx.execute("INSERT INTO launch_reports(session_id,instance_id,recorded_at,payload) VALUES(?1,?2,?3,?4)",
                params![report.session_id, report.instance_id, report.recorded_at, bytes])?;
            Ok(())
        }).unwrap();
    }

    fn terminal_ack(storage: &MetadataStore, key: &str) -> bool {
        storage
            .read(|db| -> Result<_, StorageError> {
                Ok(db.query_row(
                    "SELECT terminal_ack FROM launch_intents WHERE intent_key=?1",
                    [key],
                    |row| row.get(0),
                )?)
            })
            .unwrap()
    }

    #[test]
    fn canonical_loader_report_preserves_exact_terminal_acknowledgement() {
        use crate::launch::logs::Redactor;
        use axial_minecraft::loaders::{LoaderComponentId, installed_version_id_for};
        let version =
            installed_version_id_for(LoaderComponentId::Fabric, "1.20.1", "0.19.5").unwrap();
        for case in [
            "exact",
            "raw_secret",
            "coordinate_secret",
            "different_loader",
            "lossy_old_report",
        ] {
            let storage = Arc::new(MetadataStore::in_memory().unwrap());
            let intents = durable_intents(storage.clone());
            let mut request = request();
            request.version_id = Some(version.clone());
            let key = request.intent_key.as_deref().unwrap();
            let ReservedIntent::New { session_id, .. } = intents.reserve(&request).unwrap() else {
                panic!()
            };
            intents.accept(key, binding(&request)).unwrap();
            let mut report = terminal_report(&request, session_id.clone());
            report.version_id = version.clone();
            report.scenario.version_id = Some(version.clone());
            let secrets = match case {
                "raw_secret" => vec![version[10..25].to_owned()],
                "coordinate_secret" => vec!["0.19.5".to_owned()],
                "different_loader" => {
                    report.version_id =
                        installed_version_id_for(LoaderComponentId::Fabric, "1.20.1", "0.19.4")
                            .unwrap();
                    report.scenario.version_id = Some(report.version_id.clone());
                    vec![]
                }
                "lossy_old_report" => {
                    report.version_id = "unknown".into();
                    report.scenario.version_id = None;
                    vec![]
                }
                _ => vec![],
            };
            let reports = intents.reports.as_ref().unwrap().clone();
            let result = reports.record(report.clone(), &Redactor::new(secrets));
            if case != "exact" {
                assert!(result.is_err(), "{case}");
                assert!(!terminal_ack(&storage, key));
                assert!(reports.get(&session_id).unwrap().is_none());
                continue;
            }
            result.unwrap();
            let saved = reports.get(&session_id).unwrap().unwrap();
            assert_eq!(saved.version_id, version);
            assert_eq!(saved.scenario.version_id.as_deref(), Some(version.as_str()));
            assert!(terminal_ack(&storage, key));
            reports.record(report, &Redactor::new(vec![])).unwrap();
            drop(intents);
            let reopened = durable_intents(storage);
            assert!(
                reopened
                    .interrupted_records_with_limits(0, 0)
                    .unwrap()
                    .is_empty()
            );
            assert!(
                matches!(reopened.snapshot(key).unwrap(), Some(LaunchIntentStatus::Accepted { session }) if session.session_id == session_id && session.view_model.terminal)
            );
            assert!(matches!(
                reopened.reserve(&request).unwrap(),
                ReservedIntent::Existing(_)
            ));
        }
    }

    #[test]
    fn terminal_report_and_acknowledgement_commit_or_fail_together() {
        use crate::launch::logs::Redactor;
        for table in ["launch_reports", "launch_intents"] {
            for failure in ["IGNORE", "ABORT,'unavailable'"] {
                let storage = Arc::new(MetadataStore::in_memory().unwrap());
                let intents = durable_intents(storage.clone());
                let request = request();
                let key = request.intent_key.as_deref().unwrap();
                let ReservedIntent::New { session_id, .. } = intents.reserve(&request).unwrap()
                else {
                    panic!()
                };
                intents.accept(key, binding(&request)).unwrap();
                let action = if table == "launch_reports" {
                    "INSERT"
                } else {
                    "UPDATE"
                };
                storage.transaction(|tx| -> Result<(), StorageError> {
                    tx.execute_batch(&format!("CREATE TRIGGER refuse_terminal BEFORE {action} ON {table} BEGIN SELECT RAISE({failure}); END;"))?;
                    Ok(())
                }).unwrap();
                let reports = intents.reports.as_ref().unwrap();
                let report = terminal_report(&request, session_id.clone());
                assert!(
                    reports
                        .record(report.clone(), &Redactor::new(vec![]))
                        .is_err()
                );
                assert!(reports.get(&session_id).unwrap().is_none());
                assert!(!terminal_ack(&storage, key));
                storage
                    .transaction(|tx| -> Result<(), StorageError> {
                        tx.execute_batch("DROP TRIGGER refuse_terminal")?;
                        Ok(())
                    })
                    .unwrap();
                reports
                    .record(report.clone(), &Redactor::new(vec![]))
                    .unwrap();
                assert!(terminal_ack(&storage, key));
                reports.record(report, &Redactor::new(vec![])).unwrap();
                assert!(
                    intents
                        .interrupted_records_with_limits(0, 0)
                        .unwrap()
                        .is_empty()
                );
                let reopened = durable_intents(storage);
                assert!(matches!(
                    reopened.reserve(&request).unwrap(),
                    ReservedIntent::Existing(_)
                ));
            }
        }
    }

    #[test]
    fn old_terminal_history_makes_bounded_durable_progress() {
        let storage = Arc::new(MetadataStore::in_memory().unwrap());
        let intents = durable_intents(storage.clone());
        let mut saved = Vec::new();
        for _ in 0..5 {
            let request = request();
            let ReservedIntent::New { session_id, .. } = intents.reserve(&request).unwrap() else {
                panic!()
            };
            intents
                .accept(request.intent_key.as_deref().unwrap(), binding(&request))
                .unwrap();
            insert_historical_report(&storage, &terminal_report(&request, session_id));
            saved.push(request);
        }
        assert!(matches!(
            intents.interrupted_records_with_limits(2, MAX_RECOVERY_BYTES),
            Err(LaunchError::RecoveryIncomplete)
        ));
        assert_eq!(
            saved
                .iter()
                .filter(|request| terminal_ack(&storage, request.intent_key.as_deref().unwrap()))
                .count(),
            2
        );
        let reopened = durable_intents(storage.clone());
        assert!(matches!(
            reopened.interrupted_records_with_limits(2, MAX_RECOVERY_BYTES),
            Err(LaunchError::RecoveryIncomplete)
        ));
        assert!(
            reopened
                .interrupted_records_with_limits(2, MAX_RECOVERY_BYTES)
                .unwrap()
                .is_empty()
        );
        assert!(
            reopened
                .interrupted_records_with_limits(0, 0)
                .unwrap()
                .is_empty()
        );
        for request in saved {
            assert!(matches!(
                reopened.reserve(&request).unwrap(),
                ReservedIntent::Existing(_)
            ));
        }
    }

    #[test]
    fn noncanonical_or_misindexed_terminal_proof_never_acknowledges() {
        for corruption in ["instance", "recorded_at", "payload", "pending"] {
            let storage = Arc::new(MetadataStore::in_memory().unwrap());
            let intents = durable_intents(storage.clone());
            let request = request();
            let key = request.intent_key.as_deref().unwrap();
            let ReservedIntent::New { session_id, .. } = intents.reserve(&request).unwrap() else {
                panic!()
            };
            if corruption != "pending" {
                intents.accept(key, binding(&request)).unwrap();
            }
            let report = terminal_report(&request, session_id.clone());
            insert_historical_report(&storage, &report);
            storage
                .transaction(|tx| -> Result<(), StorageError> {
                    match corruption {
                        "instance" => {
                            tx.execute("UPDATE launch_reports SET instance_id='different'", [])?;
                        }
                        "recorded_at" => {
                            tx.execute(
                                "UPDATE launch_reports SET recorded_at='2026-09-28T10:00:01.000Z'",
                                [],
                            )?;
                        }
                        "payload" => {
                            let mut changed = report.clone();
                            changed.outcome = "not-the-recorded-outcome".into();
                            tx.execute(
                                "UPDATE launch_reports SET payload=?1",
                                [serde_json::to_vec(&changed).unwrap()],
                            )?;
                        }
                        _ => {}
                    }
                    Ok(())
                })
                .unwrap();
            let result = intents
                .reports
                .as_ref()
                .unwrap()
                .acknowledge_intent(&session_id);
            if corruption != "pending" {
                assert!(result.is_err(), "{corruption}");
            }
            assert!(!terminal_ack(&storage, key));
        }
    }

    async fn recovery_fixture() -> (
        tempfile::TempDir,
        InstanceDirectories,
        Arc<MetadataStore>,
        LaunchRequest,
    ) {
        use crate::{
            instances::{
                create::{CreateInstanceRequest, CreateTarget, InstanceService},
                directory::Registry,
            },
            library::{LibraryLifecycle, LibraryOpenOutcome},
            tasks::Exclusions,
        };
        let root = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
        let library = match LibraryLifecycle::open(root.path()) {
            LibraryOpenOutcome::Ready(library) => library,
            other => panic!("fixture root: {other:?}"),
        };
        let storage = Arc::new(MetadataStore::open(root.path().join("metadata.sqlite")).unwrap());
        storage
            .migrate(&[
                crate::instances::directory::MIGRATION,
                crate::instances::create::MIGRATION,
                crate::instances::create::DUPLICATE_WITNESS_MIGRATION,
                crate::content::install::MIGRATION,
                crate::performance::mutation::MIGRATION,
                INTENT_MIGRATION,
                INTENT_TERMINAL_MIGRATION,
                INTENT_SETTLEMENT_MIGRATION,
            ])
            .unwrap();
        let directories =
            InstanceDirectories::new(Registry::new(storage.clone()), library, Exclusions::new());
        let tasks = TaskOwner::new(4).unwrap();
        let instance = InstanceService::new(directories.clone(), tasks)
            .create(
                CreateInstanceRequest {
                    name: "Restart binding".into(),
                    selection_id: "1.20.1".into(),
                    ..Default::default()
                },
                CreateTarget {
                    selection_id: "1.20.1".into(),
                    version_id: "1.20.1".into(),
                    minecraft_version: "1.20.1".into(),
                    loader_key: "vanilla".into(),
                },
            )
            .unwrap()
            .join()
            .await
            .unwrap()
            .unwrap();
        let mut request = request();
        request.instance_id = instance.id;
        (root, directories, storage, request)
    }

    fn accept_admitted(
        intents: &LaunchIntents,
        directories: &InstanceDirectories,
        request: &LaunchRequest,
    ) -> IntentBinding {
        let instance = directories.admit(&request.instance_id).unwrap();
        let application = directories.library().admit_application_root().unwrap();
        let binding = IntentBinding::capture(&instance, &application).unwrap();
        intents.reserve(request).unwrap();
        intents
            .accept(request.intent_key.as_deref().unwrap(), binding.clone())
            .unwrap();
        binding
    }

    #[tokio::test]
    async fn accepted_binding_restores_target_artifact_and_root_admission() {
        let (_root, directories, storage, request) = recovery_fixture().await;
        let intents = durable_intents(storage.clone());
        let binding = accept_admitted(&intents, &directories, &request);
        let key = request.intent_key.as_deref().unwrap();
        let (payload, state): (Vec<u8>, String) = storage
            .read(|db| -> Result<_, StorageError> {
                Ok(db.query_row(
                    "SELECT payload,state FROM launch_intents WHERE intent_key=?1",
                    [key],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?)
            })
            .unwrap();
        assert_eq!(state, "accepted");
        assert_eq!(
            decode_intent(key, &payload).unwrap().binding,
            Some(binding.clone())
        );
        drop(intents);
        let reports = super::super::reports::LaunchReportStore::new(storage.clone()).unwrap();
        assert!(LaunchIntents::needs_recovery(storage.clone(), reports.clone()).unwrap());
        let restored = LaunchIntents::restore(storage, reports, &directories).unwrap();
        assert!(restored.has_interrupted_launches());
        assert!(matches!(
            restored.snapshot(key).unwrap(),
            Some(LaunchIntentStatus::Interrupted { .. })
        ));
        assert!(directories.admit(&request.instance_id).is_err());
        assert!(
            directories
                .exclusions()
                .try_acquire(
                    std::iter::empty::<String>(),
                    [crate::install::queue::library_artifact(&binding.library_id)]
                )
                .is_err()
        );
        assert!(
            directories
                .exclusions()
                .try_acquire_read_artifacts(
                    ["unrelated"],
                    [crate::install::queue::library_artifact(&binding.library_id)]
                )
                .is_ok()
        );
        assert!(directories.library().begin_switch().is_err());
        let retained = restored.clone();
        drop(restored);
        assert!(directories.admit(&request.instance_id).is_err());
        drop(retained);
        // Lifecycle protection is sticky, not the lifetime of this status owner.
        assert!(directories.library().begin_switch().is_err());
        directories.library().close_admission();
        assert!(directories.library().take_reset_session().is_err());
    }

    #[tokio::test]
    async fn old_or_changed_binding_refuses_startup_and_leaves_lifecycle_fenced() {
        for change in [
            "old",
            "version",
            "library",
            "application",
            "directory",
            "invalid",
        ] {
            let (_root, directories, storage, request) = recovery_fixture().await;
            let intents = durable_intents(storage.clone());
            accept_admitted(&intents, &directories, &request);
            let key = request.intent_key.as_deref().unwrap();
            let mut record = intents.record(key).unwrap().unwrap();
            match change {
                "old" => record.binding = None,
                "version" => record.binding.as_mut().unwrap().version = 2,
                "library" => record.binding.as_mut().unwrap().library_root[0] ^= 1,
                "application" => record.binding.as_mut().unwrap().application_root[0] ^= 1,
                "directory" => record.binding.as_mut().unwrap().directory_receipt.push('0'),
                _ => {}
            }
            let payload = if change == "invalid" {
                b"invalid json".to_vec()
            } else {
                serde_json::to_vec(&record).unwrap()
            };
            storage
                .transaction(|tx| -> Result<(), StorageError> {
                    tx.execute(
                        "UPDATE launch_intents SET payload=?2 WHERE intent_key=?1",
                        params![key, payload],
                    )?;
                    Ok(())
                })
                .unwrap();
            drop(intents);
            let reports = super::super::reports::LaunchReportStore::new(storage.clone()).unwrap();
            assert!(
                LaunchIntents::restore(storage, reports, &directories).is_err(),
                "{change}"
            );
            assert!(directories.library().begin_switch().is_err(), "{change}");
        }
    }

    #[tokio::test]
    async fn replaced_directory_is_not_rebound_from_matching_registry_metadata() {
        let (root, directories, storage, request) = recovery_fixture().await;
        let intents = durable_intents(storage.clone());
        let binding = accept_admitted(&intents, &directories, &request);
        drop(intents);
        let original = root.path().join("instances").join(&binding.directory_name);
        let displaced = root.path().join("displaced-instance");
        std::fs::rename(&original, &displaced).unwrap();
        std::fs::create_dir(&original).unwrap();
        std::fs::write(original.join("canary"), b"unrelated replacement").unwrap();
        let reports = super::super::reports::LaunchReportStore::new(storage.clone()).unwrap();
        assert!(LaunchIntents::restore(storage, reports, &directories).is_err());
        assert_eq!(
            std::fs::read(original.join("canary")).unwrap(),
            b"unrelated replacement"
        );
        assert!(displaced.join("mods").is_dir());
        assert!(directories.library().begin_switch().is_err());
    }

    #[tokio::test]
    async fn ignored_or_failed_acceptance_does_not_persist_a_binding() {
        for failure in ["IGNORE", "ABORT,'unavailable'"] {
            let (_root, directories, storage, request) = recovery_fixture().await;
            let intents = durable_intents(storage.clone());
            let instance = directories.admit(&request.instance_id).unwrap();
            let application = directories.library().admit_application_root().unwrap();
            let binding = IntentBinding::capture(&instance, &application).unwrap();
            intents.reserve(&request).unwrap();
            storage.transaction(|tx| -> Result<(), StorageError> {
                tx.execute_batch(&format!("CREATE TRIGGER refuse_accept BEFORE UPDATE ON launch_intents WHEN NEW.state='accepted' BEGIN SELECT RAISE({failure}); END;"))?;
                Ok(())
            }).unwrap();
            let key = request.intent_key.as_deref().unwrap();
            assert!(intents.accept(key, binding).is_err());
            let (payload, state): (Vec<u8>, String) = storage
                .read(|db| -> Result<_, StorageError> {
                    Ok(db.query_row(
                        "SELECT payload,state FROM launch_intents WHERE intent_key=?1",
                        [key],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )?)
                })
                .unwrap();
            assert_eq!(state, "pending");
            assert!(decode_intent(key, &payload).unwrap().binding.is_none());
            assert!(intents.record(key).unwrap().unwrap().binding.is_none());
        }
    }

    #[test]
    fn recovery_refuses_missing_metadata_and_excessive_total_work() {
        let storage = Arc::new(MetadataStore::in_memory().unwrap());
        let reports = super::super::reports::LaunchReportStore::new(storage.clone()).unwrap();
        assert!(LaunchIntents::needs_recovery(storage.clone(), reports.clone()).is_err());
        storage
            .migrate(&[
                INTENT_MIGRATION,
                INTENT_TERMINAL_MIGRATION,
                INTENT_SETTLEMENT_MIGRATION,
            ])
            .unwrap();
        storage.transaction(|tx| -> Result<(), StorageError> {
            for _ in 0..=MAX_RECOVERY_INTENTS {
                let request = request();
                let record = IntentRecord { request, context: None, session_id: uuid::Uuid::new_v4().to_string(), binding: None };
                tx.execute("INSERT INTO launch_intents(intent_key,payload,state) VALUES(?1,?2,'accepted')",
                    params![record.request.intent_key, serde_json::to_vec(&record).unwrap()])?;
            }
            Ok(())
        }).unwrap();
        assert_eq!(
            LaunchIntents::needs_recovery(storage, reports),
            Err(LaunchError::AtCapacity)
        );
    }

    #[test]
    fn recovery_bounds_aggregate_payload_bytes() {
        let storage = Arc::new(MetadataStore::in_memory().unwrap());
        let intents = durable_intents(storage.clone());
        storage.transaction(|tx| -> Result<(), StorageError> {
            for _ in 0..2 {
                let record = IntentRecord { request: request(), context: None, session_id: uuid::Uuid::new_v4().to_string(), binding: None };
                tx.execute("INSERT INTO launch_intents(intent_key,payload,state) VALUES(?1,?2,'accepted')",
                    params![record.request.intent_key, serde_json::to_vec(&record).unwrap()])?;
            }
            Ok(())
        }).unwrap();
        let single_payload_bytes = storage
            .read(|db| -> Result<usize, StorageError> {
                Ok(db.query_row(
                    "SELECT length(payload) FROM launch_intents LIMIT 1",
                    [],
                    |row| row.get(0),
                )?)
            })
            .unwrap();
        assert!(matches!(
            intents.interrupted_records_with_limits(4096, single_payload_bytes),
            Err(LaunchError::AtCapacity)
        ));
    }

    #[tokio::test]
    async fn physical_binding_restores_in_a_fresh_process() {
        let (root, directories, storage, request) = recovery_fixture().await;
        let intents = durable_intents(storage.clone());
        let binding = accept_admitted(&intents, &directories, &request);
        drop(intents);
        directories.library().try_preserve().unwrap();
        drop(directories);
        drop(storage);
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "launch::coordinator::tests::physical_binding_restart_helper",
                "--ignored",
            ])
            .env("AXIAL_LAUNCH_BINDING_TEST_ROOT", root.path())
            .env("AXIAL_LAUNCH_BINDING_TEST_LIBRARY", binding.library_id)
            .env(
                "AXIAL_LAUNCH_BINDING_TEST_INSTANCE",
                request.instance_id.to_string(),
            )
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[test]
    #[ignore = "fresh-process binding fixture invoked by physical_binding_restores_in_a_fresh_process"]
    fn physical_binding_restart_helper() {
        use crate::{
            instances::directory::Registry,
            library::{LibraryId, LibraryLifecycle, LibraryOpenOutcome},
            tasks::Exclusions,
        };
        let root =
            std::path::PathBuf::from(std::env::var_os("AXIAL_LAUNCH_BINDING_TEST_ROOT").unwrap());
        let id =
            LibraryId::parse(&std::env::var("AXIAL_LAUNCH_BINDING_TEST_LIBRARY").unwrap()).unwrap();
        let instance: InstanceId = std::env::var("AXIAL_LAUNCH_BINDING_TEST_INSTANCE")
            .unwrap()
            .parse()
            .unwrap();
        let library = match LibraryLifecycle::open_with_id(&root, id) {
            LibraryOpenOutcome::Ready(library) => library,
            other => panic!("reopen fixture: {other:?}"),
        };
        let storage = Arc::new(MetadataStore::open(root.join("metadata.sqlite")).unwrap());
        let reports = super::super::reports::LaunchReportStore::new(storage.clone()).unwrap();
        let directories =
            InstanceDirectories::new(Registry::new(storage.clone()), library, Exclusions::new());
        let restored = LaunchIntents::restore(storage, reports, &directories).unwrap();
        assert!(restored.has_interrupted_launches());
        assert!(directories.admit(&instance).is_err());
    }

    #[test]
    fn restart_before_acceptance_reuses_the_reserved_session_and_captured_request() {
        let root =
            tempfile::tempdir_in(std::fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
        let path = root.path().join("metadata.sqlite");
        let request = request();
        let key = request.intent_key.as_deref().unwrap();
        let context = Some("benchmark scenario".to_owned());
        let session_id = {
            let intents = durable_intents(Arc::new(MetadataStore::open(&path).unwrap()));
            let reserved = intents.reserve_identity(&request, context.clone()).unwrap();
            let ReservedIntent::New { session_id, .. } =
                intents.reserve_context(&request, context.clone()).unwrap()
            else {
                panic!()
            };
            assert_eq!(reserved, session_id);
            assert!(matches!(
                intents.reserve_context(&request, context.clone()).unwrap(),
                ReservedIntent::Existing(_)
            ));
            session_id
        };
        let recovered = durable_intents(Arc::new(MetadataStore::open(&path).unwrap()));
        assert_eq!(recovered.record(key).unwrap().unwrap().request, request);
        let ReservedIntent::New {
            session_id: retry_id,
            ..
        } = recovered.reserve_context(&request, context).unwrap()
        else {
            panic!()
        };
        assert_eq!(retry_id, session_id);
    }

    #[test]
    fn restart_after_acceptance_retains_interruption_without_authorizing_a_spawn() {
        let root =
            tempfile::tempdir_in(std::fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
        let path = root.path().join("metadata.sqlite");
        let request = request();
        let key = request.intent_key.as_deref().unwrap();
        let session_id = {
            let intents = durable_intents(Arc::new(MetadataStore::open(&path).unwrap()));
            let ReservedIntent::New { session_id, .. } = intents.reserve(&request).unwrap() else {
                panic!()
            };
            intents.accept(key, binding(&request)).unwrap();
            session_id
        };
        let recovered = durable_intents(Arc::new(MetadataStore::open(&path).unwrap()));
        let ReservedIntent::Existing(receiver) = recovered.reserve(&request).unwrap() else {
            panic!("accepted intent was retried")
        };
        assert!(
            matches!(&*receiver.borrow(), LaunchIntentStatus::Interrupted { session_id: saved } if saved == &session_id)
        );
        assert_eq!(
            recovered.accept(key, binding(&request)).map(|_| ()),
            Err(LaunchError::IntentConflict)
        );
        let mut different = request.clone();
        different.instance_id = InstanceId::new();
        assert!(matches!(
            recovered.reserve(&different),
            Err(LaunchError::IntentConflict)
        ));
    }

    #[test]
    fn ordinary_preparation_without_acceptance_is_rejected_after_disk_reopen() {
        let root =
            tempfile::tempdir_in(std::fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
        let path = root.path().join("metadata.sqlite");
        let request = request();
        let key = request.intent_key.as_deref().unwrap();
        {
            let intents = durable_intents(Arc::new(MetadataStore::open(&path).unwrap()));
            assert!(matches!(
                intents.reserve(&request).unwrap(),
                ReservedIntent::New { .. }
            ));
        }
        for _ in 0..2 {
            let recovered = durable_intents(Arc::new(MetadataStore::open(&path).unwrap()));
            assert!(
                matches!(recovered.snapshot(key).unwrap(), Some(LaunchIntentStatus::Rejected { error }) if error.code == LaunchError::PreparationFailed)
            );
            assert!(matches!(
                recovered.reserve(&request).unwrap(),
                ReservedIntent::Existing(_)
            ));
        }
    }

    #[test]
    fn accepted_terminal_proof_remains_historical_acceptance_after_disk_reopen() {
        use crate::launch::{
            logs::Redactor,
            outcome::SessionOutcome,
            reports::{LaunchProofRecord, LaunchProofScenario, SessionReportInput},
            session::SessionPhase,
        };
        for mismatch in [None, Some("instance"), Some("scenario")] {
            let root =
                tempfile::tempdir_in(std::fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
            let path = root.path().join("metadata.sqlite");
            let request = request();
            let key = request.intent_key.as_deref().unwrap();
            let launched_at = "2026-09-27T10:00:00.000Z";
            let session_id = {
                let intents = durable_intents(Arc::new(MetadataStore::open(&path).unwrap()));
                let ReservedIntent::New { session_id, .. } = intents.reserve(&request).unwrap()
                else {
                    panic!()
                };
                intents.accept(key, binding(&request)).unwrap();
                let mut report = LaunchProofRecord::from_session(SessionReportInput {
                    session_id: session_id.clone(),
                    instance_id: if mismatch == Some("instance") {
                        InstanceId::new().to_string()
                    } else {
                        request.instance_id.to_string()
                    },
                    version_id: "fixture".into(),
                    launched_at: launched_at.into(),
                    ended_at: "2026-09-27T10:00:01.000Z".into(),
                    outcome: SessionOutcome::spawn_failed(),
                    entries: vec![],
                    exit_code: None,
                    boot_duration_ms: None,
                    logs_dropped: 0,
                });
                report.scenario = LaunchProofScenario {
                    performance_mode: "vanilla".into(),
                    version_id: Some("fixture".into()),
                    benchmark_id: (mismatch == Some("scenario")).then(|| "unrelated".into()),
                    ..LaunchProofScenario::default()
                };
                if mismatch.is_some() {
                    assert!(
                        intents
                            .reports
                            .as_ref()
                            .unwrap()
                            .record(report.clone(), &Redactor::new(vec![]))
                            .is_err()
                    );
                }
                // Simulate a pre-acknowledgement profile, including malformed
                // historical linkage that the new report writer rejects.
                insert_historical_report(intents.storage.as_ref().unwrap(), &report);
                // Historical rows did not capture physical bindings. Exact
                // session-owned terminal proof is still their settlement evidence.
                let mut old = intents.record(key).unwrap().unwrap();
                old.binding = None;
                let payload = serde_json::to_vec(&old).unwrap();
                intents
                    .storage
                    .as_ref()
                    .unwrap()
                    .transaction(|tx| -> Result<(), StorageError> {
                        tx.execute(
                            "UPDATE launch_intents SET payload=?2 WHERE intent_key=?1",
                            params![key, payload],
                        )?;
                        Ok(())
                    })
                    .unwrap();
                session_id
            };
            let recovered = durable_intents(Arc::new(MetadataStore::open(&path).unwrap()));
            if mismatch.is_some() {
                assert!(recovered.interrupted_records().is_err());
                assert!(matches!(
                    recovered.snapshot(key),
                    Err(LaunchError::IntentUnavailable)
                ));
                assert!(matches!(
                    recovered.reserve(&request),
                    Err(LaunchError::IntentUnavailable)
                ));
                continue;
            }
            assert!(recovered.interrupted_records().unwrap().is_empty());
            let Some(LaunchIntentStatus::Accepted { session }) = recovered.snapshot(key).unwrap()
            else {
                panic!("terminal proof lost")
            };
            assert_eq!(session.session_id, session_id);
            assert_eq!(session.instance_id, request.instance_id);
            assert_eq!(session.launched_at, launched_at);
            assert_eq!(session.phase, SessionPhase::Exited);
            assert_eq!(session.outcome, Some(SessionOutcome::spawn_failed()));
            assert!(session.view_model.terminal && session.tree_settled && session.output_drained);
            assert!(!session.process_alive && !session.stop_allowed);
            assert_eq!(session.pid, None);
            assert_eq!(session.started_at_ms, None);
            assert!(matches!(
                recovered.reserve(&request).unwrap(),
                ReservedIntent::Existing(_)
            ));
        }
    }

    #[test]
    fn acceptance_storage_failure_keeps_preparation_safely_retryable() {
        let storage = Arc::new(MetadataStore::in_memory().unwrap());
        let intents = durable_intents(storage.clone());
        let request = request();
        let key = request.intent_key.as_deref().unwrap();
        let context = Some("benchmark scenario".to_owned());
        let ReservedIntent::New { session_id, .. } =
            intents.reserve_context(&request, context.clone()).unwrap()
        else {
            panic!()
        };
        storage.transaction(|tx| -> Result<(), StorageError> {
            tx.execute_batch("CREATE TRIGGER refuse_accept BEFORE UPDATE ON launch_intents WHEN NEW.state='accepted' BEGIN SELECT RAISE(ABORT,'unavailable'); END;")?;
            Ok(())
        }).unwrap();
        assert_eq!(
            intents.accept(key, binding(&request)).map(|_| ()),
            Err(LaunchError::IntentUnavailable)
        );
        let recovered = durable_intents(storage);
        let ReservedIntent::New {
            session_id: retry_id,
            ..
        } = recovered.reserve_context(&request, context).unwrap()
        else {
            panic!()
        };
        assert_eq!(retry_id, session_id);
    }

    #[test]
    fn restart_retains_rejection_and_refuses_changed_benchmark_context() {
        let storage = Arc::new(MetadataStore::in_memory().unwrap());
        let intents = durable_intents(storage.clone());
        let request = request();
        let key = request.intent_key.as_deref().unwrap();
        intents
            .reserve_context(&request, Some("baseline".into()))
            .unwrap();
        intents.settle(key, Err(LaunchError::AccountUnavailable));
        let recovered = durable_intents(storage);
        let ReservedIntent::Existing(receiver) = recovered
            .reserve_context(&request, Some("baseline".into()))
            .unwrap()
        else {
            panic!()
        };
        assert!(
            matches!(&*receiver.borrow(), LaunchIntentStatus::Rejected { error } if error.code == LaunchError::AccountUnavailable)
        );
        assert!(matches!(
            recovered.reserve_context(&request, Some("managed".into())),
            Err(LaunchError::IntentConflict)
        ));
    }

    #[test]
    fn panic_after_acceptance_preserves_uncertain_process_ownership() {
        let storage = Arc::new(MetadataStore::in_memory().unwrap());
        let intents = durable_intents(storage.clone());
        let request = request();
        let key = request.intent_key.as_deref().unwrap();
        let session_id = intents.reserve_identity(&request, None).unwrap();
        assert_eq!(
            intents.accept(key, binding(&request)).map(|_| ()),
            Err(LaunchError::IntentConflict)
        );
        intents.reserve(&request).unwrap();
        intents.accept(key, binding(&request)).unwrap();
        intents.abandon(key);
        assert!(
            matches!(intents.snapshot(key).unwrap(), Some(LaunchIntentStatus::Interrupted { session_id: id }) if id == session_id)
        );
        let recovered = durable_intents(storage);
        assert!(
            matches!(recovered.snapshot(key).unwrap(), Some(LaunchIntentStatus::Interrupted { session_id: id }) if id == session_id)
        );
    }

    #[test]
    fn capacity_refuses_new_work_without_discarding_old_intents() {
        let intents = LaunchIntents::new(1);
        let first = request();
        assert!(matches!(
            intents.reserve(&first),
            Ok(ReservedIntent::New { .. })
        ));
        assert!(matches!(
            intents.reserve(&request()),
            Err(LaunchError::AtCapacity)
        ));
        assert!(matches!(
            intents.reserve(&first),
            Ok(ReservedIntent::Existing(_))
        ));
    }

    #[test]
    fn transport_cannot_supply_command_authority() {
        let id = InstanceId::new();
        let value =
            serde_json::json!({"instance_id": id, "program": "/bin/sh", "args": ["-c", "true"]});
        assert!(serde_json::from_value::<LaunchRequest>(value).is_err());
        let mut invalid = request();
        invalid.intent_key = Some("../intent".into());
        assert_eq!(invalid.validate(), Err(LaunchError::InvalidIntent));
        invalid.intent_key = None;
        invalid.max_memory_mb = Some(-1);
        assert_eq!(invalid.validate(), Err(LaunchError::InvalidMemory));
    }
}
