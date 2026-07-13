mod accounts;
mod auth_logins;
mod auth_persistence;
pub mod benchmark_suite_drivers;
pub mod benchmark_suites;
mod config;
pub mod contracts;
pub mod failure_memory;
mod installed_versions;
mod installs;
mod instance_lifecycle;
mod instance_registry;
mod java_probe_failures;
mod journals;
mod known_good;
mod known_good_rebuilds;
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
    AppConfig, ConfigStore as StartupConfigStore, ConfigStoreError, INSTANCE_REGISTRY_MAX_ENTRIES,
    InstanceStore as StartupInstanceStore, InstanceStoreError, find_flag, is_canonical_instance_id,
};
pub use axial_launcher::{LaunchEvent, LaunchLogEvent, LaunchSessionRecord, LaunchStatusEvent};
use axial_minecraft::ManagedRuntimeCache;
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
pub(crate) use installed_versions::{InstalledVersionsLookup, InstalledVersionsSnapshot};
pub(crate) use installs::InstallInitializationStatus;
pub use installs::{
    ActiveQueuedInstallEntry, InstallProgressRecord, InstallQueueEnqueueOutcome,
    InstallQueuePlacement, InstallQueueSnapshot, InstallQueueSpec, InstallSnapshot, InstallStore,
    QueuedInstallEntry,
};
pub use instance_registry::AppInstanceStore;
pub(crate) use instance_registry::new_instance;
pub(crate) use instance_registry::{ensure_instance_layout, instance_not_found_error};
pub(crate) use java_probe_failures::{
    JavaProbeFailureCache, JavaProbeFailureClaim, JavaProbeFailureKey, JavaProbeFailureKind,
    JavaProbeFailureOwner,
};
pub(crate) use journals::{
    OperationJournalReconciliation, operation_journal_completed_step_is_visible,
    operation_journal_plan_is_visible, operation_journal_terminal_is_visible,
};
pub use journals::{OperationJournalStore, OperationJournalStoreError};
pub(crate) use known_good_rebuilds::KnownGoodRebuildError;
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
pub use sessions::{SessionAdmissionError, SessionStopError, SessionStore, StartupOutcome};
use shutdown::AppShutdownCoordinator;
pub use shutdown::{AppShutdownError, AppShutdownStep};

#[derive(Clone)]
pub struct AppState {
    app_name: String,
    version: String,
    config: Arc<AppConfigStore>,
    managed_runtime_cache: ManagedRuntimeCache,
    instances: Arc<AppInstanceStore>,
    accounts: Arc<LauncherAccountStore>,
    auth_logins: Arc<AuthLoginStore>,
    installs: Arc<InstallStore>,
    failure_memory: Arc<GuardianFailureMemoryStore>,
    journals: Arc<OperationJournalStore>,
    installed_versions: Arc<installed_versions::InstalledVersionsIndex>,
    known_good: Arc<known_good::KnownGoodInventoryStore>,
    known_good_rebuilds: Arc<known_good_rebuilds::KnownGoodRebuildFlights>,
    java_probe_failures: Arc<JavaProbeFailureCache>,
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

struct KnownGoodCandidateAdmission {
    _lifecycle: InstanceLifecycleLease,
    instance_id: String,
    version_id: String,
    created_at: String,
    library_root: PathBuf,
}

pub(crate) struct InstanceLifecycleLease {
    instance_id: String,
    _guard: tokio::sync::OwnedMutexGuard<()>,
}

pub(crate) struct KnownGoodVerificationLease<'a> {
    _lifecycle: &'a InstanceLifecycleLease,
    instance_id: String,
    version_id: String,
    created_at: String,
    library_root: PathBuf,
    managed_runtime_cache: ManagedRuntimeCache,
    inventory: Arc<axial_minecraft::known_good::KnownGoodInventory>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum KnownGoodVerificationUnavailable {
    InstanceNotRegistered,
    LibraryRootUnavailable,
    LiveAuthorityUnavailable,
}

impl InstanceLifecycleLease {
    fn bind(instance_id: &str, guard: tokio::sync::OwnedMutexGuard<()>) -> Self {
        Self {
            instance_id: instance_id.to_string(),
            _guard: guard,
        }
    }

    fn matches(&self, instance_id: &str) -> bool {
        self.instance_id == instance_id
    }
}

impl KnownGoodVerificationLease<'_> {
    pub(crate) fn execution_parts(
        &self,
    ) -> (
        &str,
        &str,
        &str,
        &Path,
        &ManagedRuntimeCache,
        &axial_minecraft::known_good::KnownGoodInventory,
    ) {
        (
            &self.instance_id,
            &self.version_id,
            &self.created_at,
            &self.library_root,
            &self.managed_runtime_cache,
            &self.inventory,
        )
    }

    #[cfg(test)]
    pub(crate) fn exact_identity_for_test(&self) -> (&str, &str, &str, &Path) {
        (
            &self.instance_id,
            &self.version_id,
            &self.created_at,
            &self.library_root,
        )
    }
}

impl KnownGoodCandidateAdmission {
    fn revalidate(&self, state: &AppState) -> std::io::Result<bool> {
        if !matches_known_good_incarnation(
            state.instances.get(&self.instance_id).as_ref(),
            &self.instance_id,
            &self.version_id,
            &self.created_at,
        ) {
            return Ok(false);
        }
        require_matching_known_good_library_root(state.library_dir(), &self.library_root)
            .map(|root| root == self.library_root)
    }

    fn deactivate(&self, state: &AppState) {
        state.known_good.deactivate_exact(
            &self.instance_id,
            &self.version_id,
            &self.created_at,
            &self.library_root,
        );
    }
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
            ManagedRuntimeCache::isolated_for_test()
                .expect("failed to create isolated managed runtime cache"),
        )
        .unwrap_or_else(|error| {
            panic!("failed to initialize known-good inventory persistence: {error}")
        })
    }

    pub async fn load(mut init: AppStateInit) -> std::io::Result<Self> {
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
        #[cfg(not(test))]
        let managed_runtime_cache = ManagedRuntimeCache::canonical()?;
        #[cfg(test)]
        let managed_runtime_cache = ManagedRuntimeCache::isolated_for_test()?;
        let state = tokio::task::spawn_blocking(move || {
            Self::new_with_telemetry_inner(
                init,
                config,
                telemetry,
                Arc::new(auth_logins),
                Arc::new(remote_flags),
                managed_runtime_cache,
            )
        })
        .await
        .map_err(|_| std::io::Error::other("persisted state startup task stopped"))??;
        if state.known_good.retry_retirements().await.is_err() {
            tracing::warn!("known-good restart cleanup remains pending");
        }
        Ok(state)
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
            ManagedRuntimeCache::isolated_for_test()
                .expect("failed to create isolated managed runtime cache"),
        )
        .unwrap_or_else(|error| {
            panic!("failed to initialize known-good inventory persistence: {error}")
        })
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
        managed_runtime_cache: ManagedRuntimeCache,
    ) -> std::io::Result<Self> {
        let instance_registry_authoritative = init.instances.mutation_allowed();
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
        let known_good = Arc::new(known_good::KnownGoodInventoryStore::claim(config.paths())?);
        if instance_registry_authoritative {
            known_good.discover_absent_snapshot_obligations(
                instances.list().into_iter().map(|instance| instance.id),
            )?;
        }
        let (config_changes, _) = broadcast::channel(32);

        Ok(Self {
            app_name: init.app_name,
            version: init.version,
            config,
            managed_runtime_cache,
            instances,
            accounts,
            auth_logins,
            installs: init.installs,
            failure_memory,
            journals,
            installed_versions: Arc::new(installed_versions::InstalledVersionsIndex::default()),
            known_good,
            known_good_rebuilds: Arc::new(known_good_rebuilds::KnownGoodRebuildFlights::default()),
            java_probe_failures: Arc::new(JavaProbeFailureCache::default()),
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
        })
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

    pub(crate) fn managed_runtime_cache(&self) -> &ManagedRuntimeCache {
        &self.managed_runtime_cache
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

    pub(crate) async fn installed_versions_snapshot(
        &self,
        producer: &ProducerLease,
    ) -> Option<InstalledVersionsLookup> {
        let library_dir = self.library_dir().map(PathBuf::from)?;
        Some(self.installed_versions.lookup(library_dir, producer).await)
    }

    pub(crate) fn invalidate_installed_versions(&self) {
        self.installed_versions.invalidate();
    }

    #[cfg(test)]
    pub(crate) fn installed_versions_walk_count(&self) -> usize {
        self.installed_versions.walk_count()
    }

    pub(crate) fn java_probe_failures(&self) -> &Arc<JavaProbeFailureCache> {
        &self.java_probe_failures
    }

    pub(crate) async fn accept_known_good_install_receipt(
        &self,
        installed_library_root: &Path,
        receipt: axial_minecraft::known_good::KnownGoodInstallReceipt,
    ) -> std::io::Result<()> {
        self.activate_known_good_source(installed_library_root, receipt.into_activation_source())
            .await
    }

    async fn activate_known_good_source(
        &self,
        installed_library_root: &Path,
        source: axial_minecraft::known_good::KnownGoodActivationSource,
    ) -> std::io::Result<()> {
        let installed_library_root =
            require_matching_known_good_library_root(self.library_dir(), installed_library_root)?;
        let (version_id, inventory) = source.into_parts();
        let candidates = self
            .instances
            .list()
            .into_iter()
            .filter(|instance| {
                matches_known_good_incarnation(
                    Some(instance),
                    &instance.id,
                    &version_id,
                    &instance.created_at,
                )
            })
            .map(|instance| (instance.id, instance.created_at))
            .take(INSTANCE_REGISTRY_MAX_ENTRIES + 1)
            .collect::<Vec<_>>();
        if candidates.len() > INSTANCE_REGISTRY_MAX_ENTRIES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "known-good activation candidate count exceeds the instance registry limit",
            ));
        }
        let inventory = Arc::new(inventory);
        let version_id = version_id.as_str();
        let installed_library_root = installed_library_root.as_path();
        complete_independent_known_good_fanout(candidates, |(instance_id, created_at)| {
            let inventory = inventory.clone();
            async move {
                self.reconcile_known_good_instance(
                    &instance_id,
                    version_id,
                    &created_at,
                    installed_library_root,
                    inventory,
                )
                .await
            }
        })
        .await
    }

    async fn reconcile_known_good_instance(
        &self,
        instance_id: &str,
        version_id: &str,
        created_at: &str,
        installed_library_root: &Path,
        inventory: Arc<axial_minecraft::known_good::KnownGoodInventory>,
    ) -> std::io::Result<()> {
        let admission = match self
            .admit_known_good_candidate(instance_id, version_id, created_at, installed_library_root)
            .await
        {
            Ok(Some(admission)) => admission,
            Ok(None) => return Ok(()),
            Err(error) => return Err(error),
        };

        if let Err(error) = self
            .known_good
            .reconcile(
                &admission.instance_id,
                &admission.version_id,
                &admission.created_at,
                &admission.library_root,
                inventory,
            )
            .await
        {
            admission.deactivate(self);
            return Err(error);
        }

        match admission.revalidate(self) {
            Ok(true) => {}
            Ok(false) => {
                admission.deactivate(self);
                return Ok(());
            }
            Err(error) => {
                admission.deactivate(self);
                return Err(error);
            }
        }
        if self
            .known_good
            .active_inventory(
                &admission.instance_id,
                &admission.version_id,
                &admission.created_at,
                &admission.library_root,
            )
            .is_none()
        {
            admission.deactivate(self);
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                "known-good live authority was not activated",
            ));
        }
        Ok(())
    }

    async fn admit_known_good_candidate(
        &self,
        instance_id: &str,
        version_id: &str,
        created_at: &str,
        installed_library_root: &Path,
    ) -> std::io::Result<Option<KnownGoodCandidateAdmission>> {
        let lifecycle = self.acquire_instance_lifecycle(instance_id).await;
        if !matches_known_good_incarnation(
            self.instances.get(instance_id).as_ref(),
            instance_id,
            version_id,
            created_at,
        ) {
            self.known_good.deactivate_exact(
                instance_id,
                version_id,
                created_at,
                installed_library_root,
            );
            return Ok(None);
        }
        let library_root = match require_matching_known_good_library_root(
            self.library_dir(),
            installed_library_root,
        ) {
            Ok(root) => root,
            Err(error) => {
                self.known_good.deactivate_exact(
                    instance_id,
                    version_id,
                    created_at,
                    installed_library_root,
                );
                return Err(error);
            }
        };
        Ok(Some(KnownGoodCandidateAdmission {
            _lifecycle: lifecycle,
            instance_id: instance_id.to_string(),
            version_id: version_id.to_string(),
            created_at: created_at.to_string(),
            library_root,
        }))
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
        if config.library_dir != value {
            self.known_good.clear_active();
        }
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
        let result = completed_rx.await.map_err(|_| {
            InstanceStoreError::Persistence(std::io::Error::other(
                "instance registry mutation owner stopped before reporting completion",
            ))
        })?;
        if result.is_ok() {
            self.prune_known_good_inventories();
        }
        result
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
        owner: ProducerLease,
    ) -> Result<(), InstanceStoreError> {
        let state = self.clone();
        owner
            .spawn_joinable(
                async move { state.delete_instance_owned(instance_id, delete_files).await },
            )
            .await
            .map_err(|_| {
                InstanceStoreError::Persistence(std::io::Error::other(
                    "instance deletion owner stopped before reporting completion",
                ))
            })?
    }

    async fn delete_instance_owned(
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
        let known_good_retirement = self
            .known_good
            .reserve_retirement(&instance_id)
            .map_err(InstanceStoreError::Persistence)?;
        let instances = self.instances.clone();
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
        if instances.get(&retained_instance_id).is_none() {
            retirement.commit();
            if known_good_retirement.commit().await.is_err() {
                tracing::warn!(
                    instance_id = retained_instance_id,
                    "known-good retirement cleanup was retained for retry"
                );
            }
        } else if result.is_ok() {
            return Err(InstanceStoreError::Persistence(std::io::Error::other(
                "instance registry reported successful deletion without removing the instance",
            )));
        }
        result
    }

    pub(crate) async fn acquire_instance_lifecycle(
        &self,
        instance_id: &str,
    ) -> InstanceLifecycleLease {
        InstanceLifecycleLease::bind(
            instance_id,
            self.instance_lifecycle_gates.acquire(instance_id).await,
        )
    }

    pub(crate) fn mint_known_good_verification_lease<'a>(
        &self,
        lifecycle: &'a InstanceLifecycleLease,
        expected_library_root: &Path,
    ) -> Result<KnownGoodVerificationLease<'a>, KnownGoodVerificationUnavailable> {
        let instance = self
            .instances
            .get(&lifecycle.instance_id)
            .filter(|instance| {
                instance.id == lifecycle.instance_id && is_canonical_instance_id(&instance.id)
            })
            .ok_or(KnownGoodVerificationUnavailable::InstanceNotRegistered)?;
        let library_root = self
            .library_dir()
            .map(PathBuf::from)
            .and_then(|root| known_good::normalize_library_root(&root).ok())
            .ok_or(KnownGoodVerificationUnavailable::LibraryRootUnavailable)?;
        let expected_library_root = known_good::normalize_library_root(expected_library_root)
            .map_err(|_| KnownGoodVerificationUnavailable::LibraryRootUnavailable)?;
        if library_root != expected_library_root {
            return Err(KnownGoodVerificationUnavailable::LiveAuthorityUnavailable);
        }
        let inventory = self
            .known_good
            .active_inventory(
                &instance.id,
                &instance.version_id,
                &instance.created_at,
                &library_root,
            )
            .ok_or(KnownGoodVerificationUnavailable::LiveAuthorityUnavailable)?;

        Ok(KnownGoodVerificationLease {
            _lifecycle: lifecycle,
            instance_id: instance.id,
            version_id: instance.version_id,
            created_at: instance.created_at,
            library_root,
            managed_runtime_cache: self.managed_runtime_cache.clone(),
            inventory,
        })
    }

    pub(crate) fn known_good_verification_lease_is_current(
        &self,
        lease: &KnownGoodVerificationLease<'_>,
    ) -> bool {
        let Some(instance) = self.instances.get(&lease.instance_id) else {
            return false;
        };
        if instance.id != lease.instance_id
            || instance.version_id != lease.version_id
            || instance.created_at != lease.created_at
        {
            return false;
        }
        let Some(library_root) = self
            .library_dir()
            .map(PathBuf::from)
            .and_then(|root| known_good::normalize_library_root(&root).ok())
        else {
            return false;
        };
        if library_root != lease.library_root {
            return false;
        }
        if self.managed_runtime_cache.root() != lease.managed_runtime_cache.root() {
            return false;
        }
        self.known_good
            .active_inventory(
                &instance.id,
                &instance.version_id,
                &instance.created_at,
                &library_root,
            )
            .is_some_and(|inventory| Arc::ptr_eq(&inventory, &lease.inventory))
    }

    #[cfg(test)]
    pub(crate) fn activate_known_good_inventory_for_test(
        &self,
        instance_id: &str,
        inventory: axial_minecraft::known_good::KnownGoodInventory,
    ) {
        let instance = self.instances.get(instance_id).expect("test instance");
        let library_root = self
            .library_dir()
            .map(PathBuf::from)
            .expect("test library root");
        self.known_good
            .activate_for_test(
                &instance.id,
                &instance.version_id,
                &instance.created_at,
                &library_root,
                Arc::new(inventory),
            )
            .expect("activate test known-good inventory");
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

    pub(crate) async fn close_known_good_inventories(&self) -> std::io::Result<()> {
        self.known_good.close().await
    }

    fn config_commit_observer(&self) -> Arc<dyn Fn(AppConfig, AppConfig) + Send + Sync> {
        let telemetry = self.telemetry.clone();
        let changes = self.config_changes.clone();
        let known_good = self.known_good.clone();
        Arc::new(move |previous: AppConfig, current: AppConfig| {
            if previous.telemetry_enabled && !current.telemetry_enabled {
                telemetry.clear_queue();
            }
            if previous.library_dir != current.library_dir {
                known_good.clear_active();
            }
            let _ = changes.send(());
        })
    }

    fn prune_known_good_inventories(&self) {
        let Some(library_root) = self.library_dir().map(PathBuf::from) else {
            self.known_good.clear_active();
            return;
        };
        self.known_good.retain_active(
            &library_root,
            self.instances
                .list()
                .into_iter()
                .filter(|instance| is_canonical_instance_id(&instance.id))
                .map(|instance| (instance.id, instance.version_id, instance.created_at)),
        );
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

async fn complete_independent_known_good_fanout<C, F, Fut>(
    candidates: Vec<C>,
    mut activate: F,
) -> std::io::Result<()>
where
    F: FnMut(C) -> Fut,
    Fut: std::future::Future<Output = std::io::Result<()>>,
{
    let mut first_error = None;
    for candidate in candidates {
        if let Err(error) = activate(candidate).await
            && first_error.is_none()
        {
            first_error = Some(error);
        }
    }
    first_error.map_or(Ok(()), Err)
}

fn matches_known_good_incarnation(
    instance: Option<&axial_config::Instance>,
    instance_id: &str,
    version_id: &str,
    created_at: &str,
) -> bool {
    instance.is_some_and(|instance| {
        instance.id == instance_id
            && instance.version_id == version_id
            && instance.created_at == created_at
            && is_canonical_instance_id(&instance.id)
    })
}

fn require_matching_known_good_library_root(
    configured_library_root: Option<String>,
    installed_library_root: &Path,
) -> std::io::Result<PathBuf> {
    let configured_library_root = configured_library_root.map(PathBuf::from).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotConnected,
            "known-good library root is not configured",
        )
    })?;
    let configured_library_root = known_good::normalize_library_root(&configured_library_root)?;
    let installed_library_root = known_good::normalize_library_root(installed_library_root)?;
    if configured_library_root != installed_library_root {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "known-good library root changed during installation",
        ));
    }
    Ok(installed_library_root)
}

#[cfg(test)]
mod known_good_identity_tests {
    use super::*;
    use std::sync::Mutex;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn known_good_state_fixture(root: &Path) -> AppState {
        let config_dir = root.join("config");
        let paths = axial_config::AppPaths {
            config_file: config_dir.join("config.json"),
            instances_file: config_dir.join("instances.json"),
            instances_dir: root.join("instances"),
            music_dir: root.join("music"),
            library_dir: root.join("library"),
            config_dir,
        };
        let config = Arc::new(
            axial_config::ConfigStore::load_from(paths.clone()).expect("load test config"),
        );
        let instances = Arc::new(
            axial_config::InstanceStore::from_snapshot(
                paths.clone(),
                axial_config::InstanceRegistrySnapshot::default(),
            )
            .expect("load test instances"),
        );
        AppState::new(AppStateInit {
            app_name: "Axial".to_string(),
            version: "test".to_string(),
            config,
            instances,
            installs: Arc::new(InstallStore::new()),
            sessions: Arc::new(SessionStore::new()),
            performance: Arc::new(
                axial_performance::PerformanceManager::load_for_startup(&paths.config_dir)
                    .expect("load test performance state"),
            ),
            startup_warnings: Vec::new(),
            frontend_dir: root.join("frontend"),
        })
    }

    #[tokio::test]
    async fn lifecycle_queued_identity_drift_isolated_from_an_exact_candidate() {
        let root = std::env::temp_dir().join(format!(
            "axial-known-good-identity-drift-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let state = known_good_state_fixture(&root);
        let library_root = root.join("library");
        std::fs::create_dir_all(&library_root).expect("library root");
        state.set_library_dir_for_test(library_root.to_string_lossy().into_owned());
        let exact = state
            .instances()
            .insert_for_test("Exact", "1.21.5")
            .expect("exact instance");
        let drifted = state
            .instances()
            .insert_for_test("Drifted", "1.21.5")
            .expect("drifted instance");
        let exact_id = exact.id.clone();
        let exact_created_at = exact.created_at.clone();
        let drifted_id = drifted.id.clone();
        let drifted_created_at = drifted.created_at.clone();
        let fanout_candidates = vec![
            (exact_id.clone(), exact_created_at),
            (drifted_id.clone(), drifted_created_at),
        ];
        let lifecycle = state.acquire_instance_lifecycle(&drifted.id).await;
        let activated = Arc::new(Mutex::new(Vec::new()));
        let first_activated = Arc::new(tokio::sync::Notify::new());
        let fanout_state = state.clone();
        let fanout_root = library_root.clone();
        let fanout_activated = activated.clone();
        let fanout_first_activated = first_activated.clone();
        let fanout = tokio::spawn(async move {
            complete_independent_known_good_fanout(
                fanout_candidates,
                |(instance_id, created_at)| {
                    let state = fanout_state.clone();
                    let library_root = fanout_root.clone();
                    let activated = fanout_activated.clone();
                    let first_activated = fanout_first_activated.clone();
                    async move {
                        if let Some(admission) = state
                            .admit_known_good_candidate(
                                &instance_id,
                                "1.21.5",
                                &created_at,
                                &library_root,
                            )
                            .await?
                        {
                            activated
                                .lock()
                                .expect("activated candidates")
                                .push(admission.instance_id.clone());
                            first_activated.notify_one();
                        }
                        Ok(())
                    }
                },
            )
            .await
        });

        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            first_activated.notified(),
        )
        .await
        .expect("first exact candidate should activate before blocked drift candidate");

        let mut replacement = state
            .instances()
            .get(&drifted_id)
            .expect("drifted instance remains registered");
        replacement.version_id = "1.21.6".to_string();
        state
            .instances()
            .replace_for_test(replacement)
            .expect("replace drifted identity");
        drop(lifecycle);

        fanout
            .await
            .expect("fanout task")
            .expect("identity drift is an isolated skip");
        assert_eq!(
            *activated.lock().expect("activated candidates"),
            vec![exact_id],
            "the exact candidate remains activated and the drifted candidate is skipped"
        );
        state
            .close_known_good_inventories()
            .await
            .expect("close known-good store");
        drop(state);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn lifecycle_queued_root_drift_rejects_candidate_admission() {
        let root = std::env::temp_dir().join(format!(
            "axial-known-good-root-drift-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let state = known_good_state_fixture(&root);
        let installed_root = root.join("library");
        let changed_root = root.join("changed-library");
        std::fs::create_dir_all(&installed_root).expect("installed library root");
        std::fs::create_dir_all(&changed_root).expect("changed library root");
        state.set_library_dir_for_test(installed_root.to_string_lossy().into_owned());
        let instance = state
            .instances()
            .insert_for_test("Root drift", "1.21.5")
            .expect("root-drift instance");
        let lifecycle = state.acquire_instance_lifecycle(&instance.id).await;
        let admission_state = state.clone();
        let admission_id = instance.id.clone();
        let admission_created_at = instance.created_at.clone();
        let admission_root = installed_root.clone();
        let admission = tokio::spawn(async move {
            admission_state
                .admit_known_good_candidate(
                    &admission_id,
                    "1.21.5",
                    &admission_created_at,
                    &admission_root,
                )
                .await
        });

        state.set_library_dir_for_test(changed_root.to_string_lossy().into_owned());
        drop(lifecycle);
        let error = match admission.await.expect("queued root admission") {
            Err(error) => error,
            Ok(_) => panic!("root drift must reject admission"),
        };
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        state
            .close_known_good_inventories()
            .await
            .expect("close known-good store");
        drop(state);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn admitted_candidate_revalidation_rejects_later_root_drift() {
        let root = std::env::temp_dir().join(format!(
            "axial-known-good-post-admission-root-drift-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let state = known_good_state_fixture(&root);
        let installed_root = root.join("library");
        let changed_root = root.join("changed-library");
        std::fs::create_dir_all(&installed_root).expect("installed library root");
        std::fs::create_dir_all(&changed_root).expect("changed library root");
        state.set_library_dir_for_test(installed_root.to_string_lossy().into_owned());
        let instance = state
            .instances()
            .insert_for_test("Post-admission root drift", "1.21.5")
            .expect("post-admission instance");
        let admission = state
            .admit_known_good_candidate(
                &instance.id,
                "1.21.5",
                &instance.created_at,
                &installed_root,
            )
            .await
            .expect("admit exact root")
            .expect("exact candidate admission");

        state.set_library_dir_for_test(changed_root.to_string_lossy().into_owned());
        let error = admission
            .revalidate(&state)
            .expect_err("post-admission root drift must fail revalidation");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        admission.deactivate(&state);
        drop(admission);
        state
            .close_known_good_inventories()
            .await
            .expect("close known-good store");
        drop(state);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn first_failed_activation_does_not_block_a_later_candidate() {
        let activated = Arc::new(Mutex::new(Vec::new()));
        let result = complete_independent_known_good_fanout(
            vec!["first".to_string(), "second".to_string()],
            |candidate| {
                let activated = activated.clone();
                async move {
                    if candidate == "first" {
                        Err(std::io::Error::other("first activation failed"))
                    } else {
                        activated
                            .lock()
                            .expect("activated candidates")
                            .push(candidate);
                        Ok(())
                    }
                }
            },
        )
        .await;

        assert_eq!(
            result.expect_err("first error is retained").to_string(),
            "first activation failed"
        );
        assert_eq!(
            *activated.lock().expect("activated candidates"),
            vec!["second"]
        );
    }

    #[tokio::test]
    async fn later_failed_activation_does_not_undo_an_earlier_candidate() {
        let activated = Arc::new(Mutex::new(Vec::new()));
        let result = complete_independent_known_good_fanout(
            vec!["first".to_string(), "second".to_string()],
            |candidate| {
                let activated = activated.clone();
                async move {
                    if candidate == "second" {
                        Err(std::io::Error::other("second activation failed"))
                    } else {
                        activated
                            .lock()
                            .expect("activated candidates")
                            .push(candidate);
                        Ok(())
                    }
                }
            },
        )
        .await;

        assert_eq!(
            result.expect_err("later error is retained").to_string(),
            "second activation failed"
        );
        assert_eq!(
            *activated.lock().expect("activated candidates"),
            vec!["first"]
        );
    }

    #[test]
    fn stale_raw_known_good_activation_entrypoints_are_absent() {
        let source = include_str!("mod.rs");
        assert!(!source.contains(concat!("reconcile_registered_", "known_good_instance")));
        assert!(!source.contains(concat!(
            "pub(crate) async fn reconcile_",
            "known_good_instance"
        )));
        assert!(!source.contains(concat!(
            "pub(crate) async fn activate_",
            "known_good_source"
        )));
    }

    #[test]
    fn unrelated_instance_changes_preserve_known_good_incarnation() {
        let mut instance = new_instance(
            "0000000000000042".to_string(),
            "Before".to_string(),
            "1.21.5".to_string(),
            String::new(),
            String::new(),
        );
        let created_at = instance.created_at.clone();
        assert!(matches_known_good_incarnation(
            Some(&instance),
            &instance.id,
            "1.21.5",
            &created_at,
        ));

        instance.name = "After".to_string();
        instance.max_memory_mb = 8_192;
        instance.icon = "grass".to_string();
        assert!(matches_known_good_incarnation(
            Some(&instance),
            &instance.id,
            "1.21.5",
            &created_at,
        ));

        instance.version_id = "1.21.6".to_string();
        assert!(!matches_known_good_incarnation(
            Some(&instance),
            &instance.id,
            "1.21.5",
            &created_at,
        ));
        assert!(!matches_known_good_incarnation(
            None,
            "0000000000000042",
            "1.21.5",
            &created_at,
        ));
        instance.version_id = "1.21.5".to_string();
        instance.created_at.push_str("-replacement");
        assert!(!matches_known_good_incarnation(
            Some(&instance),
            &instance.id,
            "1.21.5",
            &created_at,
        ));
        instance.created_at = created_at.clone();
        instance.id = "not-canonical".to_string();
        assert!(!matches_known_good_incarnation(
            Some(&instance),
            "not-canonical",
            "1.21.5",
            &created_at,
        ));
    }

    #[test]
    fn receipt_acceptance_requires_the_exact_current_library_root() {
        let root = std::env::temp_dir().join(format!(
            "axial-known-good-root-contract-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let configured = root.join("configured");
        let changed = root.join("changed");
        std::fs::create_dir_all(&configured).expect("configured root");
        std::fs::create_dir_all(&changed).expect("changed root");

        assert_eq!(
            require_matching_known_good_library_root(None, &configured)
                .expect_err("missing root must fail")
                .kind(),
            std::io::ErrorKind::NotConnected
        );
        assert_eq!(
            require_matching_known_good_library_root(
                Some(configured.to_string_lossy().into_owned()),
                &changed,
            )
            .expect_err("changed root must fail")
            .kind(),
            std::io::ErrorKind::InvalidInput
        );
        let normalized = require_matching_known_good_library_root(
            Some(configured.to_string_lossy().into_owned()),
            &configured,
        )
        .expect("exact root");
        assert_eq!(
            normalized,
            std::fs::canonicalize(&configured).expect("root")
        );

        let _ = std::fs::remove_dir_all(root);
    }
}
