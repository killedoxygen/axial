mod accounts;
mod auth_logins;
mod auth_persistence;
pub mod benchmark_suite_drivers;
pub mod benchmark_suites;
mod config;
pub mod contracts;
pub mod failure_memory;
mod installs;
mod instance_lifecycle;
mod instance_registry;
mod journals;
pub(crate) mod launch_reports;
mod lifecycle;
pub mod ownership;
mod performance_managed;
pub mod performance_operations;
mod performance_rules;
pub mod presence;
mod remote_flags;
mod sessions;
mod shutdown;
pub mod skins;

use axial_config::{
    AppConfig, ConfigStore as StartupConfigStore, ConfigStoreError,
    InstanceStore as StartupInstanceStore, InstanceStoreError, find_flag, is_canonical_instance_id,
};
pub use axial_launcher::{LaunchEvent, LaunchLogEvent, LaunchSessionRecord, LaunchStatusEvent};
pub use axial_minecraft::download::DownloadProgress;
use axial_performance::PerformanceManager;
use std::path::{Path, PathBuf};
use std::sync::Arc;
#[cfg(test)]
use std::sync::RwLock;
use tokio::sync::broadcast;

use crate::observability::telemetry::TelemetryHub;

const STARTUP_WARNING_LIMIT: usize = 8;
const STARTUP_WARNING_MAX_CHARS: usize = 240;

pub use accounts::{
    LauncherAccountKind, LauncherAccountRecord, LauncherAccountStore, microsoft_account_id,
    offline_account_id,
};
pub use auth_logins::{
    ActiveMinecraftAccountState, ActiveMsaTokenState, AuthLoginAccountState,
    AuthLoginMinecraftAccount, AuthLoginMinecraftCape, AuthLoginMinecraftProfile,
    AuthLoginMinecraftSkin, AuthLoginMsaToken, AuthLoginStore, NewAuthLoginMinecraftAccount,
    NewAuthLoginMsaToken,
};
pub use config::AppConfigStore;
pub use failure_memory::GuardianFailureMemoryStore;
pub(crate) use installs::InstallInitializationStatus;
pub use installs::{
    ActiveQueuedInstallEntry, InstallProgressRecord, InstallQueueEnqueueOutcome,
    InstallQueuePlacement, InstallQueueSnapshot, InstallQueueSpec, InstallSnapshot, InstallStore,
    QueuedInstallEntry,
};
pub use instance_registry::AppInstanceStore;
pub(crate) use instance_registry::new_instance;
pub(crate) use instance_registry::{ensure_instance_layout, instance_not_found_error};
pub(crate) use journals::{
    OperationJournalReconciliation, operation_journal_completed_step_is_visible,
    operation_journal_plan_is_visible, operation_journal_terminal_is_visible,
};
pub use journals::{OperationJournalStore, OperationJournalStoreError};
pub(crate) use lifecycle::{
    AppLifecycle, LifecycleAdmissionError, ProducerLease, RequestLease, RequestProducerHandoff,
};
#[cfg(test)]
pub(crate) use lifecycle::{AppLifecyclePhase, LifecycleQuiesceError};
pub(crate) use performance_managed::{
    AppManagedCompositionAdmission, ManagedCompositionCloseError, ManagedInspectionError,
    ManagedInstanceAdmissionError,
};
pub use performance_rules::AppPerformanceStore;
pub(crate) use remote_flags::{
    RemoteFlagRefreshOutcome, RemoteFlagStore, ResolvedFlagSource, resolve_flag,
};
pub(crate) use sessions::{LaunchFailureTermination, LaunchFailureTerminationErrorClass};
pub use sessions::{SessionAdmissionError, SessionStore, StartupOutcome};
use shutdown::AppShutdownCoordinator;
pub use shutdown::{AppShutdownError, AppShutdownStep};

#[derive(Clone)]
pub struct AppState {
    app_name: String,
    version: String,
    config: Arc<AppConfigStore>,
    instances: Arc<AppInstanceStore>,
    accounts: Arc<LauncherAccountStore>,
    auth_logins: Arc<AuthLoginStore>,
    installs: Arc<InstallStore>,
    failure_memory: Arc<GuardianFailureMemoryStore>,
    journals: Arc<OperationJournalStore>,
    sessions: Arc<SessionStore>,
    skins: Arc<skins::SavedSkinStore>,
    benchmark_suites: Arc<benchmark_suites::BenchmarkSuiteStore>,
    benchmark_suite_drivers: Arc<benchmark_suite_drivers::BenchmarkSuiteDriverStore>,
    performance_operations: Arc<performance_operations::PerformanceOperationStore>,
    performance: Arc<AppPerformanceStore>,
    telemetry: Arc<TelemetryHub>,
    remote_flags: Arc<RemoteFlagStore>,
    launch_reports: Arc<launch_reports::LaunchReportStore>,
    instance_lifecycle_gates: instance_lifecycle::InstanceLifecycleGates,
    lifecycle: AppLifecycle,
    shutdown_coordinator: AppShutdownCoordinator,
    startup_warnings: Arc<Vec<String>>,
    config_changes: Arc<broadcast::Sender<()>>,
    #[cfg(test)]
    auth_chain_client_override: Arc<RwLock<Option<crate::auth_chain::AuthChainClient>>>,
    frontend_dir: Arc<PathBuf>,
}

pub struct AppStateInit {
    pub app_name: String,
    pub version: String,
    pub config: Arc<StartupConfigStore>,
    pub instances: Arc<StartupInstanceStore>,
    pub installs: Arc<InstallStore>,
    pub sessions: Arc<SessionStore>,
    pub performance: Arc<PerformanceManager>,
    pub startup_warnings: Vec<String>,
    pub frontend_dir: PathBuf,
}

impl AppState {
    #[cfg(test)]
    pub fn new(init: AppStateInit) -> Self {
        let config =
            Arc::new(AppConfigStore::claim(&init.config).unwrap_or_else(|error| {
                panic!("failed to initialize config persistence: {error}")
            }));
        let telemetry = Arc::new(TelemetryHub::from_env(config.clone()));
        assert!(
            !config.current().telemetry_enabled
                || !telemetry.export_configured()
                || !config.current().telemetry_install_id.is_empty(),
            "synchronous test state requires a committed telemetry install id"
        );
        Self::new_with_telemetry_inner(
            init,
            config,
            telemetry,
            Arc::new(AuthLoginStore::new()),
            Arc::new(RemoteFlagStore::default()),
        )
    }

    pub async fn load(mut init: AppStateInit) -> Self {
        let config =
            Arc::new(AppConfigStore::claim(&init.config).unwrap_or_else(|error| {
                panic!("failed to initialize config persistence: {error}")
            }));
        let telemetry = Arc::new(TelemetryHub::from_env(config.clone()));
        let telemetry_identity_required = config.current().telemetry_enabled
            && telemetry.export_configured()
            && config.current().telemetry_install_id.is_empty();
        if telemetry_identity_required
            && config
                .mutate(|_| Ok(()), true, Arc::new(|_, _| {}))
                .await
                .is_err()
        {
            init.startup_warnings.push(
                "Axial could not persist telemetry identity; telemetry remains disabled until settings persistence recovers."
                    .to_string(),
            );
        }
        let remote_flags_config_dir = init.config.paths().config_dir.clone();
        let (auth_logins, remote_flags) = tokio::join!(
            AuthLoginStore::load_from_secure_store(),
            RemoteFlagStore::load_from_config_dir(remote_flags_config_dir),
        );
        tokio::task::spawn_blocking(move || {
            Self::new_with_telemetry_inner(
                init,
                config,
                telemetry,
                Arc::new(auth_logins),
                Arc::new(remote_flags),
            )
        })
        .await
        .unwrap_or_else(|_| panic!("persisted state startup task stopped"))
    }

    #[cfg(test)]
    pub(crate) fn new_with_telemetry(init: AppStateInit, telemetry: Arc<TelemetryHub>) -> Self {
        let config =
            Arc::new(AppConfigStore::claim(&init.config).unwrap_or_else(|error| {
                panic!("failed to initialize config persistence: {error}")
            }));
        telemetry.replace_config_source(config.clone());
        Self::new_with_telemetry_inner(
            init,
            config,
            telemetry,
            Arc::new(AuthLoginStore::new()),
            Arc::new(RemoteFlagStore::default()),
        )
    }

    #[cfg(test)]
    pub(crate) fn with_operation_stores(
        mut self,
        journals: Arc<OperationJournalStore>,
        performance_operations: Arc<performance_operations::PerformanceOperationStore>,
    ) -> Self {
        self.journals = journals;
        self.performance_operations = performance_operations;
        self
    }

    #[cfg(test)]
    pub(crate) fn with_accounts(mut self, accounts: Arc<LauncherAccountStore>) -> Self {
        self.accounts = accounts;
        self
    }

    #[cfg(test)]
    pub(crate) fn with_benchmark_suites(
        mut self,
        benchmark_suites: Arc<benchmark_suites::BenchmarkSuiteStore>,
    ) -> Self {
        self.launch_reports
            .bind_proof_retention(benchmark_suites.proof_retention_handle());
        self.benchmark_suites = benchmark_suites;
        self
    }

    fn new_with_telemetry_inner(
        init: AppStateInit,
        config: Arc<AppConfigStore>,
        telemetry: Arc<TelemetryHub>,
        auth_logins: Arc<AuthLoginStore>,
        remote_flags: Arc<RemoteFlagStore>,
    ) -> Self {
        let instances = Arc::new(AppInstanceStore::claim(&init.instances).unwrap_or_else(
            |error| panic!("failed to initialize instance registry persistence: {error}"),
        ));
        let instance_lifecycle_gates = instance_lifecycle::InstanceLifecycleGates::default();
        let performance = Arc::new(
            AppPerformanceStore::claim(
                init.performance,
                &config.paths().config_dir,
                &instances.paths().instances_dir,
                instance_lifecycle_gates.clone(),
            )
            .unwrap_or_else(|error| {
                panic!("failed to initialize performance rules persistence: {error}")
            }),
        );
        let benchmark_suite_retention_claims =
            benchmark_suites::BenchmarkSuiteRetentionClaims::default();
        let benchmark_suite_drivers =
            benchmark_suite_drivers::BenchmarkSuiteDriverStore::prepare_load_from_paths(
                config.paths(),
                benchmark_suite_retention_claims.clone(),
            );
        let benchmark_suites = Arc::new(benchmark_suites::BenchmarkSuiteStore::load_from_paths(
            config.paths(),
            benchmark_suite_retention_claims,
        ));
        let launch_reports = Arc::new(launch_reports::LaunchReportStore::load_from_paths(
            config.paths(),
            benchmark_suites.proof_retention_handle(),
        ));
        let benchmark_suite_drivers =
            Arc::new(benchmark_suite_drivers.bind(benchmark_suites.retention_handle()));
        let performance_operations = Arc::new(
            performance_operations::PerformanceOperationStore::load_from_paths(config.paths()),
        );
        let skins = Arc::new(skins::SavedSkinStore::load_from_paths(config.paths()));
        let accounts = Arc::new(LauncherAccountStore::load_from_paths(config.paths()));
        let failure_memory = Arc::new(GuardianFailureMemoryStore::load_from_paths(config.paths()));
        let journals = Arc::new(OperationJournalStore::load_from_paths(config.paths()));
        let (config_changes, _) = broadcast::channel(32);

        Self {
            app_name: init.app_name,
            version: init.version,
            config,
            instances,
            accounts,
            auth_logins,
            installs: init.installs,
            failure_memory,
            journals,
            sessions: init.sessions,
            skins,
            benchmark_suites,
            benchmark_suite_drivers,
            performance_operations,
            performance,
            telemetry,
            remote_flags,
            launch_reports,
            instance_lifecycle_gates,
            lifecycle: AppLifecycle::new(),
            shutdown_coordinator: AppShutdownCoordinator::new(),
            startup_warnings: Arc::new(bound_startup_warnings(init.startup_warnings)),
            config_changes: Arc::new(config_changes),
            #[cfg(test)]
            auth_chain_client_override: Arc::new(RwLock::new(None)),
            frontend_dir: Arc::new(init.frontend_dir),
        }
    }

    pub fn app_name(&self) -> &str {
        &self.app_name
    }

    pub fn version(&self) -> &str {
        &self.version
    }

    pub fn config(&self) -> &Arc<AppConfigStore> {
        &self.config
    }

    pub fn instances(&self) -> &Arc<AppInstanceStore> {
        &self.instances
    }

    pub fn accounts(&self) -> &Arc<LauncherAccountStore> {
        &self.accounts
    }

    pub fn sessions(&self) -> &Arc<SessionStore> {
        &self.sessions
    }

    pub fn skins(&self) -> &Arc<skins::SavedSkinStore> {
        &self.skins
    }

    pub fn auth_logins(&self) -> &Arc<AuthLoginStore> {
        &self.auth_logins
    }

    pub fn benchmark_suite_drivers(
        &self,
    ) -> &Arc<benchmark_suite_drivers::BenchmarkSuiteDriverStore> {
        &self.benchmark_suite_drivers
    }

    pub fn benchmark_suites(&self) -> &Arc<benchmark_suites::BenchmarkSuiteStore> {
        &self.benchmark_suites
    }

    pub fn performance_operations(
        &self,
    ) -> &Arc<performance_operations::PerformanceOperationStore> {
        &self.performance_operations
    }

    pub fn installs(&self) -> &Arc<InstallStore> {
        &self.installs
    }

    pub fn failure_memory(&self) -> &Arc<GuardianFailureMemoryStore> {
        &self.failure_memory
    }

    pub fn journals(&self) -> &Arc<OperationJournalStore> {
        &self.journals
    }

    pub fn performance(&self) -> &Arc<AppPerformanceStore> {
        &self.performance
    }

    pub(crate) async fn refresh_performance_rules(
        &self,
    ) -> Result<axial_performance::PerformanceRulesStatus, axial_performance::RulesRefreshError>
    {
        let performance = self.performance.clone();
        let gate = performance.acquire_refresh().await?;
        let (completed_tx, completed_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let result = performance.refresh_with_gate(gate).await;
            let _ = completed_tx.send(result);
        });
        completed_rx.await.map_err(|_| {
            axial_performance::RulesRefreshError::Cache(std::io::Error::other(
                "performance rules refresh owner stopped before reporting completion",
            ))
        })?
    }

    pub(crate) async fn close_performance_rules(
        &self,
    ) -> Result<(), axial_performance::RulesRefreshError> {
        self.performance.close().await
    }

    pub fn telemetry(&self) -> &Arc<TelemetryHub> {
        &self.telemetry
    }

    pub(crate) fn remote_flags(&self) -> &Arc<RemoteFlagStore> {
        &self.remote_flags
    }

    pub(crate) fn launch_reports(&self) -> &Arc<launch_reports::LaunchReportStore> {
        &self.launch_reports
    }

    pub(crate) fn try_admit_request(&self) -> Result<RequestLease, LifecycleAdmissionError> {
        self.lifecycle.try_admit_request()
    }

    pub(crate) fn try_claim_producer(&self) -> Result<ProducerLease, LifecycleAdmissionError> {
        self.lifecycle.try_claim_producer()
    }

    pub(crate) fn subscribe_shutdown(&self) -> tokio::sync::watch::Receiver<bool> {
        self.lifecycle.subscribe_shutdown()
    }

    #[cfg(test)]
    pub(crate) async fn quiesce(&self) -> Result<(), LifecycleQuiesceError> {
        self.lifecycle.quiesce().await
    }

    pub async fn shutdown(&self) -> Result<(), AppShutdownError> {
        self.lifecycle.begin_quiesce();
        self.shutdown_coordinator.start(self.clone()).wait().await
    }

    #[cfg(test)]
    pub(crate) fn lifecycle_phase(&self) -> AppLifecyclePhase {
        self.lifecycle.phase()
    }

    pub(crate) fn remote_flag_identity_for(&self, config: &AppConfig) -> Option<String> {
        self.telemetry.export_identity_for_config(config)
    }

    pub fn startup_warnings(&self) -> Vec<String> {
        self.startup_warnings.as_ref().clone()
    }

    pub fn library_dir(&self) -> Option<String> {
        let library_dir = self.config.current().library_dir;
        (!library_dir.trim().is_empty()).then_some(library_dir)
    }

    #[cfg(test)]
    pub fn set_library_dir_for_test(&self, value: String) {
        let mut config = self.config.current();
        config.library_dir = value;
        self.config
            .replace_for_test(config)
            .expect("test config replacement must remain valid");
    }

    pub async fn mutate_config<Mutation>(
        &self,
        mutation: Mutation,
    ) -> Result<AppConfig, ConfigStoreError>
    where
        Mutation: FnOnce(&mut AppConfig) -> Result<(), ConfigStoreError> + Send + 'static,
    {
        let config = self.config.clone();
        let gate = config.acquire_mutation().await?;
        let export_configured = self.telemetry.export_configured();
        let observer = self.config_commit_observer();
        let (completed_tx, completed_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let result = config
                .mutate_with_gate(mutation, export_configured, observer, gate)
                .await;
            let _ = completed_tx.send(result);
        });
        completed_rx.await.map_err(|_| {
            ConfigStoreError::Persistence(std::io::Error::other(
                "application config mutation owner stopped before reporting completion",
            ))
        })?
    }

    pub(crate) async fn close_config(&self) -> Result<(), ConfigStoreError> {
        self.config.close(self.config_commit_observer()).await
    }

    pub async fn mutate_instances<ResultValue, Mutation>(
        &self,
        mutation: Mutation,
    ) -> Result<ResultValue, InstanceStoreError>
    where
        ResultValue: Send + 'static,
        Mutation: FnOnce(
                &mut axial_config::InstanceRegistrySnapshot,
            ) -> Result<ResultValue, InstanceStoreError>
            + Send
            + 'static,
    {
        let instances = self.instances.clone();
        let gate = instances.acquire_mutation().await?;
        let (completed_tx, completed_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let result = instances.mutate_with_gate(mutation, gate).await;
            let _ = completed_tx.send(result);
        });
        completed_rx.await.map_err(|_| {
            InstanceStoreError::Persistence(std::io::Error::other(
                "instance registry mutation owner stopped before reporting completion",
            ))
        })?
    }

    pub(crate) async fn create_instance(
        &self,
        instance: axial_config::Instance,
        library_dir: Option<PathBuf>,
    ) -> Result<axial_config::Instance, InstanceStoreError> {
        let instances = self.instances.clone();
        let gate = instances.acquire_mutation().await?;
        let (completed_tx, completed_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let result = instances
                .create_with_gate(instance, library_dir, gate)
                .await;
            let _ = completed_tx.send(result);
        });
        completed_rx.await.map_err(|_| {
            InstanceStoreError::Persistence(std::io::Error::other(
                "instance creation owner stopped before reporting completion",
            ))
        })?
    }

    pub(crate) async fn duplicate_instance(
        &self,
        source_id: String,
        requested_name: Option<String>,
        library_dir: Option<PathBuf>,
    ) -> Result<axial_config::Instance, InstanceStoreError> {
        let instances = self.instances.clone();
        let gate = instances.acquire_mutation().await?;
        let (completed_tx, completed_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let result = instances
                .duplicate_with_gate(source_id, requested_name, library_dir, gate)
                .await;
            let _ = completed_tx.send(result);
        });
        completed_rx.await.map_err(|_| {
            InstanceStoreError::Persistence(std::io::Error::other(
                "instance duplication owner stopped before reporting completion",
            ))
        })?
    }

    pub(crate) async fn delete_instance(
        &self,
        instance_id: String,
        delete_files: bool,
    ) -> Result<(), InstanceStoreError> {
        if self.sessions.has_active_instance(&instance_id).await {
            return Err(InstanceStoreError::Persistence(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "cannot delete a running instance; stop the game first",
            )));
        }
        let lifecycle = self.acquire_instance_lifecycle(&instance_id).await;
        if self.sessions.has_active_instance(&instance_id).await {
            return Err(InstanceStoreError::Persistence(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "cannot delete a running instance; stop the game first",
            )));
        }
        if self.instances.get(&instance_id).is_none() {
            return Err(instance_not_found_error());
        }
        let retirement = self
            .performance
            .retire_managed(&instance_id)
            .await
            .map_err(|error| {
                InstanceStoreError::Persistence(std::io::Error::other(error.to_string()))
            })?;
        let instances = self.instances.clone();
        let (completed_tx, completed_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let _lifecycle = lifecycle;
            let retained_instance_id = instance_id.clone();
            let result = match instances.acquire_mutation().await {
                Ok(gate) => {
                    instances
                        .delete_with_gate(instance_id, delete_files, gate)
                        .await
                }
                Err(error) => Err(error),
            };
            if result.is_ok() || instances.get(&retained_instance_id).is_none() {
                retirement.commit();
            }
            let _ = completed_tx.send(result);
        });
        completed_rx.await.map_err(|_| {
            InstanceStoreError::Persistence(std::io::Error::other(
                "instance deletion owner stopped before reporting completion",
            ))
        })?
    }

    pub(crate) async fn acquire_instance_lifecycle(
        &self,
        instance_id: &str,
    ) -> tokio::sync::OwnedMutexGuard<()> {
        self.instance_lifecycle_gates.acquire(instance_id).await
    }

    pub(crate) async fn admit_managed_instance(
        &self,
        instance_id: &str,
        mutation: bool,
    ) -> Result<AppManagedCompositionAdmission, ManagedInstanceAdmissionError> {
        if !is_canonical_instance_id(instance_id) {
            return Err(ManagedInstanceAdmissionError::InvalidInstanceIdentity);
        }
        if mutation && self.sessions.has_active_instance(instance_id).await {
            return Err(ManagedInstanceAdmissionError::ActiveSession);
        }
        let lifecycle = self.acquire_instance_lifecycle(instance_id).await;
        let instance = self
            .instances
            .get(instance_id)
            .ok_or(ManagedInstanceAdmissionError::InstanceNotFound)?;
        if instance.id != instance_id || !is_canonical_instance_id(&instance.id) {
            return Err(ManagedInstanceAdmissionError::InvalidInstanceIdentity);
        }
        let active_session = self.sessions.has_active_instance(instance_id).await;
        if mutation && active_session {
            return Err(ManagedInstanceAdmissionError::ActiveSession);
        }
        let managed = self
            .performance
            .admit_managed(instance_id, lifecycle, !active_session)
            .await?;
        if self.instances.get(instance_id).as_ref() != Some(&instance) {
            return Err(ManagedInstanceAdmissionError::InstanceNotFound);
        }
        if mutation && self.sessions.has_active_instance(instance_id).await {
            return Err(ManagedInstanceAdmissionError::ActiveSession);
        }
        Ok(managed)
    }

    pub(crate) async fn inspect_managed_instance(
        &self,
        instance_id: &str,
        plan: Option<axial_performance::CompositionPlan>,
    ) -> Result<axial_performance::ManagedCompositionInspection, ManagedInspectionError> {
        let admitted = self.admit_managed_instance(instance_id, false).await?;
        let (completed_tx, completed_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let result = admitted.inspect(plan.as_ref()).await;
            let _ = completed_tx.send(result);
        });
        completed_rx
            .await
            .map_err(|_| ManagedInspectionError::OwnerStopped)?
            .map_err(ManagedInspectionError::Operation)
    }

    pub(crate) async fn resolve_managed_instance(
        &self,
        instance_id: &str,
        request: axial_performance::ResolutionRequest,
    ) -> Result<axial_performance::ManagedResolvedInspection, ManagedInspectionError> {
        let admitted = self.admit_managed_instance(instance_id, false).await?;
        let (completed_tx, completed_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let result = admitted.resolve_and_inspect(request).await;
            let _ = completed_tx.send(result);
        });
        completed_rx
            .await
            .map_err(|_| ManagedInspectionError::OwnerStopped)?
            .map_err(ManagedInspectionError::Operation)
    }

    pub(crate) async fn close_managed_compositions(
        &self,
    ) -> Result<(), ManagedCompositionCloseError> {
        self.performance.close_managed().await
    }

    pub(crate) async fn close_instance_registry(&self) -> Result<(), InstanceStoreError> {
        self.instances.close().await
    }

    fn config_commit_observer(&self) -> Arc<dyn Fn(AppConfig, AppConfig) + Send + Sync> {
        let telemetry = self.telemetry.clone();
        let changes = self.config_changes.clone();
        Arc::new(move |previous: AppConfig, current: AppConfig| {
            if previous.telemetry_enabled && !current.telemetry_enabled {
                telemetry.clear_queue();
            }
            let _ = changes.send(());
        })
    }

    pub fn flag_enabled(&self, key: &str) -> bool {
        let Some(flag) = find_flag(key) else {
            return false;
        };
        if flag.dev_only && !cfg!(debug_assertions) {
            return false;
        }

        let config = self.config.current();
        let remote_identity = self.remote_flag_identity_for(&config);
        let remote_active = remote_identity.is_some();
        let remote_values = remote_identity
            .as_deref()
            .map(|identity| self.remote_flags.values_snapshot(identity))
            .unwrap_or_default();

        resolve_flag(
            flag,
            &config.feature_overrides,
            remote_active,
            &remote_values,
        )
        .enabled
    }

    pub fn subscribe_config_changes(&self) -> broadcast::Receiver<()> {
        self.config_changes.subscribe()
    }

    pub fn frontend_dir(&self) -> &Path {
        self.frontend_dir.as_path()
    }

    #[cfg(test)]
    pub(crate) fn set_auth_chain_client_override(
        &self,
        client: crate::auth_chain::AuthChainClient,
    ) {
        if let Ok(mut override_slot) = self.auth_chain_client_override.write() {
            *override_slot = Some(client);
        }
    }

    #[cfg(test)]
    pub(crate) fn auth_chain_client_override(&self) -> Option<crate::auth_chain::AuthChainClient> {
        self.auth_chain_client_override
            .read()
            .ok()
            .and_then(|override_slot| override_slot.clone())
    }
}

fn bound_startup_warnings(warnings: Vec<String>) -> Vec<String> {
    warnings
        .into_iter()
        .take(STARTUP_WARNING_LIMIT)
        .map(|warning| warning.chars().take(STARTUP_WARNING_MAX_CHARS).collect())
        .collect()
}
