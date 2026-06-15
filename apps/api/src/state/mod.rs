mod accounts;
mod auth_logins;
mod auth_persistence;
pub mod benchmark_suite_drivers;
pub mod benchmark_suites;
pub mod contracts;
pub mod failure_memory;
mod installs;
mod journals;
pub mod launch_reports;
pub mod ownership;
pub mod performance_operations;
pub mod presence;
mod sessions;
pub mod skins;

use croopor_config::{AppConfig, ConfigStore, ConfigStoreError, InstanceStore};
pub use croopor_launcher::{LaunchEvent, LaunchLogEvent, LaunchSessionRecord, LaunchStatusEvent};
pub use croopor_minecraft::download::DownloadProgress;
use croopor_performance::PerformanceManager;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use tokio::sync::broadcast;

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
pub use installs::InstallStore;
pub use journals::OperationJournalStore;
pub use sessions::{SessionStore, StartupOutcome};

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
    benchmark_suite_drivers: Arc<benchmark_suite_drivers::BenchmarkSuiteDriverStore>,
    performance_operations: Arc<performance_operations::PerformanceOperationStore>,
    performance: Arc<PerformanceManager>,
    startup_warnings: Arc<Vec<String>>,
    config_changes: Arc<broadcast::Sender<()>>,
    library_dir: Arc<RwLock<Option<String>>>,
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

impl AppState {
    pub fn new(init: AppStateInit) -> Self {
        let library_dir = init.config.current().library_dir;
        let benchmark_suite_drivers = Arc::new(
            benchmark_suite_drivers::BenchmarkSuiteDriverStore::load_from_paths(
                init.config.paths(),
            ),
        );
        let performance_operations = Arc::new(
            performance_operations::PerformanceOperationStore::load_from_paths(init.config.paths()),
        );
        let skins = Arc::new(skins::SavedSkinStore::load_from_paths(init.config.paths()));
        let accounts = Arc::new(LauncherAccountStore::load_from_paths(init.config.paths()));
        let (config_changes, _) = broadcast::channel(32);

        Self {
            app_name: init.app_name,
            version: init.version,
            config: init.config,
            instances: init.instances,
            accounts,
            auth_logins: Arc::new(AuthLoginStore::load_from_secure_store()),
            installs: init.installs,
            failure_memory: Arc::new(GuardianFailureMemoryStore::new()),
            journals: Arc::new(OperationJournalStore::new()),
            sessions: init.sessions,
            skins,
            benchmark_suite_drivers,
            performance_operations,
            performance: init.performance,
            startup_warnings: Arc::new(bound_startup_warnings(init.startup_warnings)),
            config_changes: Arc::new(config_changes),
            library_dir: Arc::new(RwLock::new(if library_dir.is_empty() {
                None
            } else {
                Some(library_dir)
            })),
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

    pub fn update_config(&self, next: AppConfig) -> Result<AppConfig, ConfigStoreError> {
        let config = self.config.update(next)?;
        self.set_library_dir(config.library_dir.clone());
        let _ = self.config_changes.send(());
        Ok(config)
    }

    pub fn subscribe_config_changes(&self) -> broadcast::Receiver<()> {
        self.config_changes.subscribe()
    }

    pub fn frontend_dir(&self) -> &Path {
        self.frontend_dir.as_path()
    }
}

fn bound_startup_warnings(warnings: Vec<String>) -> Vec<String> {
    warnings
        .into_iter()
        .take(STARTUP_WARNING_LIMIT)
        .map(|warning| warning.chars().take(STARTUP_WARNING_MAX_CHARS).collect())
        .collect()
}
