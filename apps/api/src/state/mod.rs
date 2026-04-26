mod installs;
mod sessions;

use croopor_config::{ConfigStore, InstanceStore};
pub use croopor_launcher::{LaunchEvent, LaunchLogEvent, LaunchSessionRecord, LaunchStatusEvent};
pub use croopor_minecraft::download::DownloadProgress;
use croopor_performance::PerformanceManager;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

pub use installs::InstallStore;
pub use sessions::{SessionStore, StartupOutcome};

#[derive(Clone)]
pub struct AppState {
    app_name: String,
    version: String,
    config: Arc<ConfigStore>,
    instances: Arc<InstanceStore>,
    installs: Arc<InstallStore>,
    sessions: Arc<SessionStore>,
    performance: Arc<PerformanceManager>,
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
    pub frontend_dir: PathBuf,
}

impl AppState {
    pub fn new(init: AppStateInit) -> Self {
        let library_dir = init.config.current().library_dir;

        Self {
            app_name: init.app_name,
            version: init.version,
            config: init.config,
            instances: init.instances,
            installs: init.installs,
            sessions: init.sessions,
            performance: init.performance,
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

    pub fn sessions(&self) -> &Arc<SessionStore> {
        &self.sessions
    }

    pub fn installs(&self) -> &Arc<InstallStore> {
        &self.installs
    }

    pub fn performance(&self) -> &Arc<PerformanceManager> {
        &self.performance
    }

    pub fn library_dir(&self) -> Option<String> {
        self.library_dir.read().ok().and_then(|value| value.clone())
    }

    pub fn set_library_dir(&self, value: String) {
        if let Ok(mut guard) = self.library_dir.write() {
            *guard = if value.is_empty() { None } else { Some(value) };
        }
    }

    pub fn frontend_dir(&self) -> &Path {
        self.frontend_dir.as_path()
    }
}
