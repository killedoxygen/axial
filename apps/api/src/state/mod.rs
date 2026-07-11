mod accounts;
mod auth_logins;
mod auth_persistence;
pub mod benchmark_suite_drivers;
pub mod benchmark_suites;
pub mod contracts;
pub mod failure_memory;
mod installs;
mod journals;
pub(crate) mod launch_reports;
pub mod ownership;
pub mod performance_operations;
pub mod presence;
mod remote_flags;
mod sessions;
pub mod skins;

use axial_config::{AppConfig, ConfigStore, ConfigStoreError, InstanceStore, find_flag};
pub use axial_launcher::{LaunchEvent, LaunchLogEvent, LaunchSessionRecord, LaunchStatusEvent};
pub use axial_minecraft::download::DownloadProgress;
use axial_performance::PerformanceManager;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
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
pub use failure_memory::GuardianFailureMemoryStore;
pub(crate) use installs::InstallInitializationStatus;
pub use installs::{
    ActiveQueuedInstallEntry, InstallProgressRecord, InstallQueueEnqueueOutcome,
    InstallQueuePlacement, InstallQueueSnapshot, InstallQueueSpec, InstallSnapshot, InstallStore,
    QueuedInstallEntry,
};
pub(crate) use journals::{
    OperationJournalReconciliation, operation_journal_completed_step_is_visible,
    operation_journal_plan_is_visible, operation_journal_terminal_is_visible,
};
pub use journals::{OperationJournalStore, OperationJournalStoreError};
pub(crate) use remote_flags::{
    RemoteFlagRefreshOutcome, RemoteFlagStore, ResolvedFlagSource, resolve_flag,
};
pub(crate) use sessions::{LaunchFailureTermination, LaunchFailureTerminationErrorClass};
pub use sessions::{SessionAdmissionError, SessionStore, StartupOutcome};

#[derive(Clone)]
pub struct AppState {
    app_name: String,
    version: String,
    config: Arc<ConfigStore>,
    instances: Arc<InstanceStore>,
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
    performance: Arc<PerformanceManager>,
    telemetry: Arc<TelemetryHub>,
    remote_flags: Arc<RemoteFlagStore>,
    launch_reports: Arc<launch_reports::LaunchReportStore>,
    startup_warnings: Arc<Vec<String>>,
    config_changes: Arc<broadcast::Sender<()>>,
    library_dir: Arc<RwLock<Option<String>>>,
    #[cfg(test)]
    auth_chain_client_override: Arc<RwLock<Option<crate::auth_chain::AuthChainClient>>>,
    frontend_dir: Arc<PathBuf>,
}

pub struct AppStateInit {
    pub app_name: String,
    pub version: String,
    pub config: Arc<ConfigStore>,
    pub instances: Arc<InstanceStore>,
    pub installs: Arc<InstallStore>,
    pub sessions: Arc<SessionStore>,
    pub performance: Arc<PerformanceManager>,
    pub startup_warnings: Vec<String>,
    pub frontend_dir: PathBuf,
}

#[derive(Debug, thiserror::Error)]
#[error("secure authentication cleanup is incomplete")]
pub struct SecureAuthCloseError;

#[derive(Debug, thiserror::Error)]
#[error("remote feature flag persistence is incomplete")]
pub struct RemoteFlagCloseError;

#[derive(Debug, thiserror::Error)]
#[error("launch report persistence is incomplete")]
pub struct LaunchReportCloseError;

impl AppState {
    #[cfg(test)]
    pub fn new(init: AppStateInit) -> Self {
        let telemetry = Arc::new(TelemetryHub::from_env(init.config.clone()));
        Self::new_with_telemetry_inner(
            init,
            telemetry,
            Arc::new(AuthLoginStore::new()),
            Arc::new(RemoteFlagStore::default()),
        )
    }

    pub async fn load(init: AppStateInit) -> Self {
        let telemetry = Arc::new(TelemetryHub::from_env(init.config.clone()));
        let remote_flags_config_dir = init.config.paths().config_dir.clone();
        let (auth_logins, remote_flags) = tokio::join!(
            AuthLoginStore::load_from_secure_store(),
            RemoteFlagStore::load_from_config_dir(remote_flags_config_dir),
        );
        tokio::task::spawn_blocking(move || {
            Self::new_with_telemetry_inner(
                init,
                telemetry,
                Arc::new(auth_logins),
                Arc::new(remote_flags),
            )
        })
        .await
        .unwrap_or_else(|_| panic!("persisted state startup task stopped"))
    }

    pub async fn close_secure_auth(&self) -> Result<(), SecureAuthCloseError> {
        self.auth_logins
            .close()
            .await
            .map_err(|_| SecureAuthCloseError)
    }

    pub async fn close_remote_flags(&self) -> Result<(), RemoteFlagCloseError> {
        self.remote_flags
            .close()
            .await
            .map_err(|_| RemoteFlagCloseError)
    }

    pub async fn close_launch_reports(&self) -> Result<(), LaunchReportCloseError> {
        self.launch_reports
            .close()
            .await
            .map_err(|_| LaunchReportCloseError)
    }

    #[cfg(test)]
    pub(crate) fn new_with_telemetry(init: AppStateInit, telemetry: Arc<TelemetryHub>) -> Self {
        Self::new_with_telemetry_inner(
            init,
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
        telemetry: Arc<TelemetryHub>,
        auth_logins: Arc<AuthLoginStore>,
        remote_flags: Arc<RemoteFlagStore>,
    ) -> Self {
        let library_dir = init.config.current().library_dir;
        let benchmark_suite_retention_claims =
            benchmark_suites::BenchmarkSuiteRetentionClaims::default();
        let benchmark_suite_drivers =
            benchmark_suite_drivers::BenchmarkSuiteDriverStore::prepare_load_from_paths(
                init.config.paths(),
                benchmark_suite_retention_claims.clone(),
            );
        let benchmark_suites = Arc::new(benchmark_suites::BenchmarkSuiteStore::load_from_paths(
            init.config.paths(),
            benchmark_suite_retention_claims,
        ));
        let launch_reports = Arc::new(launch_reports::LaunchReportStore::load_from_paths(
            init.config.paths(),
            benchmark_suites.proof_retention_handle(),
        ));
        let benchmark_suite_drivers =
            Arc::new(benchmark_suite_drivers.bind(benchmark_suites.retention_handle()));
        let performance_operations = Arc::new(
            performance_operations::PerformanceOperationStore::load_from_paths(init.config.paths()),
        );
        let skins = Arc::new(skins::SavedSkinStore::load_from_paths(init.config.paths()));
        let accounts = Arc::new(LauncherAccountStore::load_from_paths(init.config.paths()));
        let failure_memory = Arc::new(GuardianFailureMemoryStore::load_from_paths(
            init.config.paths(),
        ));
        let journals = Arc::new(OperationJournalStore::load_from_paths(init.config.paths()));
        let (config_changes, _) = broadcast::channel(32);

        Self {
            app_name: init.app_name,
            version: init.version,
            config: init.config,
            instances: init.instances,
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
            performance: init.performance,
            telemetry,
            remote_flags,
            launch_reports,
            startup_warnings: Arc::new(bound_startup_warnings(init.startup_warnings)),
            config_changes: Arc::new(config_changes),
            library_dir: Arc::new(RwLock::new(if library_dir.is_empty() {
                None
            } else {
                Some(library_dir)
            })),
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

    pub fn config(&self) -> &Arc<ConfigStore> {
        &self.config
    }

    pub fn instances(&self) -> &Arc<InstanceStore> {
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

    pub fn performance(&self) -> &Arc<PerformanceManager> {
        &self.performance
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

    pub(crate) fn remote_flags_active_for(&self, config: &AppConfig) -> bool {
        config.telemetry_enabled && self.telemetry.export_configured()
    }

    pub fn startup_warnings(&self) -> Vec<String> {
        self.startup_warnings.as_ref().clone()
    }

    pub fn library_dir(&self) -> Option<String> {
        self.library_dir.read().ok().and_then(|value| value.clone())
    }

    pub fn set_library_dir(&self, value: String) {
        if let Ok(mut guard) = self.library_dir.write() {
            *guard = if value.is_empty() { None } else { Some(value) };
        }
    }

    pub fn update_config(&self, mut next: AppConfig) -> Result<AppConfig, ConfigStoreError> {
        let previous = self.config.current();
        let telemetry_disabled = previous.telemetry_enabled && !next.telemetry_enabled;
        if telemetry_disabled {
            next.telemetry_install_id.clear();
        }
        let config = self.config.update(next)?;
        self.set_library_dir(config.library_dir.clone());
        if telemetry_disabled {
            self.telemetry.clear_queue();
        }
        let _ = self.config_changes.send(());
        Ok(config)
    }

    pub fn flag_enabled(&self, key: &str) -> bool {
        let Some(flag) = find_flag(key) else {
            return false;
        };
        if flag.dev_only && !cfg!(debug_assertions) {
            return false;
        }

        let config = self.config.current();
        let remote_active = self.remote_flags_active_for(&config);
        let remote_values = if remote_active {
            self.remote_flags.values_snapshot()
        } else {
            Default::default()
        };

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
