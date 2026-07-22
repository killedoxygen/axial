use crate::models::{AppConfig, AppConfigValidationError};
use crate::paths::AppPaths;
use crate::AppRootSession;
use axial_fs::{Directory, LeafName};
use std::sync::Arc;
use thiserror::Error;

pub struct ConfigStore {
    paths: AppPaths,
    root_session: Arc<AppRootSession>,
    config: AppConfig,
    mutation_allowed: bool,
    startup_source: StartupFileProvenance,
}

#[derive(Clone, Eq, PartialEq)]
pub enum StartupFileProvenance {
    Missing,
    Accepted(Vec<u8>),
    Rejected,
    Synthetic,
}

pub struct ConfigStartupLoad {
    pub store: ConfigStore,
    pub warnings: Vec<String>,
}

const CONFIG_STARTUP_WARNING: &str = "Axial could not load settings, so it started with safe defaults. Check app data permissions or restore the settings file.";
pub const CONFIG_MAX_BYTES: u64 = 256 * 1024;

#[derive(Debug, Error)]
pub enum ConfigStoreError {
    #[error("failed to read config: {0}")]
    Read(#[from] std::io::Error),
    #[error("failed to parse config: {0}")]
    Parse(#[from] serde_json::Error),
    #[error(transparent)]
    Validation(#[from] AppConfigValidationError),
    #[error("failed to persist config: {0}")]
    Persistence(std::io::Error),
    #[error("failed to open application root: {0}")]
    Root(std::io::Error),
    #[error("config exceeds the maximum persisted size of {max_bytes} bytes")]
    TooLarge { max_bytes: u64 },
}

impl ConfigStore {
    pub fn load_from(
        paths: AppPaths,
        root_session: Arc<AppRootSession>,
    ) -> Result<Self, ConfigStoreError> {
        root_session
            .validate_paths(&paths)
            .map_err(ConfigStoreError::Root)?;
        let root = root_session.root_directory().map_err(ConfigStoreError::Root)?;
        let (config, startup_source) = match read_config(&root) {
            Ok(data) => (
                serde_json::from_slice::<AppConfig>(&data)?.normalized()?,
                StartupFileProvenance::Accepted(data),
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                (AppConfig::default(), StartupFileProvenance::Missing)
            }
            Err(error) => return Err(ConfigStoreError::Read(error)),
        };

        Ok(Self {
            paths,
            root_session,
            config,
            mutation_allowed: true,
            startup_source,
        })
    }

    pub fn load_for_startup(
        paths: AppPaths,
        root_session: Arc<AppRootSession>,
    ) -> Result<ConfigStartupLoad, ConfigStoreError> {
        root_session
            .validate_paths(&paths)
            .map_err(ConfigStoreError::Root)?;
        let root = root_session.root_directory().map_err(ConfigStoreError::Root)?;
        let (config, warnings, mutation_allowed, startup_source) = match read_config(&root) {
            Ok(data) => match load_config_for_startup(&data) {
                Ok(config) => (
                    config,
                    Vec::new(),
                    true,
                    StartupFileProvenance::Accepted(data),
                ),
                Err(ConfigStoreError::Parse(_) | ConfigStoreError::Validation(_)) => (
                    AppConfig::default(),
                    vec![CONFIG_STARTUP_WARNING.to_string()],
                    false,
                    StartupFileProvenance::Rejected,
                ),
                Err(error) => return Err(error),
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                (
                    AppConfig::default(),
                    Vec::new(),
                    true,
                    StartupFileProvenance::Missing,
                )
            }
            Err(_) => (
                AppConfig::default(),
                vec![CONFIG_STARTUP_WARNING.to_string()],
                false,
                StartupFileProvenance::Rejected,
            ),
        };

        Ok(ConfigStartupLoad {
            store: Self {
                paths,
                root_session,
                config,
                mutation_allowed,
                startup_source,
            },
            warnings,
        })
    }

    pub fn current(&self) -> AppConfig {
        self.config.clone()
    }

    pub fn from_config(
        paths: AppPaths,
        root_session: Arc<AppRootSession>,
        config: AppConfig,
    ) -> Result<Self, ConfigStoreError> {
        root_session
            .validate_paths(&paths)
            .map_err(ConfigStoreError::Root)?;
        Ok(Self {
            paths,
            root_session,
            config: config.normalized()?,
            mutation_allowed: true,
            startup_source: StartupFileProvenance::Synthetic,
        })
    }

    pub fn paths(&self) -> &AppPaths {
        &self.paths
    }

    pub fn mutation_allowed(&self) -> bool {
        self.mutation_allowed
    }

    pub fn startup_source(&self) -> &StartupFileProvenance {
        &self.startup_source
    }

    pub fn root_session(&self) -> &Arc<AppRootSession> {
        &self.root_session
    }
}

fn load_config_for_startup(data: &[u8]) -> Result<AppConfig, ConfigStoreError> {
    Ok(serde_json::from_slice::<AppConfig>(data)?.normalized()?)
}

fn read_config(root: &Directory) -> Result<Vec<u8>, std::io::Error> {
    root.open_file(&LeafName::new("config.json").expect("fixed config leaf is valid"))?
        .read_bounded(CONFIG_MAX_BYTES)
}

#[cfg(test)]
mod tests {
    use super::{CONFIG_MAX_BYTES, ConfigStore, ConfigStoreError};
    use crate::{AppConfig, AppConfigValidationError, AppPaths, AppRootSession};
    use std::fs;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TestRoot {
        root: PathBuf,
        paths: AppPaths,
        root_session: Option<Arc<AppRootSession>>,
    }

    impl TestRoot {
        fn new(name: &str) -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock should be after unix epoch")
                .as_nanos();
            let root = std::env::temp_dir().join(format!(
                "axial-config-store-{name}-{}-{nonce}",
                std::process::id()
            ));
            let paths = AppPaths::from_root(root.clone()).expect("absolute test app root");
            let root_session = Arc::new(paths.open_root_session().expect("test root session"));
            Self {
                root,
                paths,
                root_session: Some(root_session),
            }
        }

        fn paths(&self) -> AppPaths {
            self.paths.clone()
        }

        fn root_session(&self) -> Arc<AppRootSession> {
            Arc::clone(
                self.root_session
                    .as_ref()
                    .expect("test root session is retained"),
            )
        }
    }

    impl Drop for TestRoot {
        fn drop(&mut self) {
            drop(self.root_session.take());
            if let Err(error) = fs::remove_dir_all(&self.root)
                && error.kind() != std::io::ErrorKind::NotFound
            {
                if std::thread::panicking() {
                    eprintln!("failed to clean config test root during panic: {error}");
                } else {
                    panic!("failed to clean config test root: {error}");
                }
            }
        }
    }

    #[test]
    fn constructors_reject_reconstructed_paths_without_the_acquisition_lineage() {
        let root = TestRoot::new("root-lineage-mismatch");
        let paths = AppPaths::from_root(root.root.clone()).expect("reconstruct identical paths");
        let root_session = root.root_session();

        assert!(matches!(
            ConfigStore::load_from(paths.clone(), Arc::clone(&root_session)),
            Err(ConfigStoreError::Root(error))
                if error.kind() == std::io::ErrorKind::InvalidInput
        ));
        assert!(matches!(
            ConfigStore::load_for_startup(paths.clone(), Arc::clone(&root_session)),
            Err(ConfigStoreError::Root(error))
                if error.kind() == std::io::ErrorKind::InvalidInput
        ));
        assert!(matches!(
            ConfigStore::from_config(paths.clone(), root_session, AppConfig::default()),
            Err(ConfigStoreError::Root(error))
                if error.kind() == std::io::ErrorKind::InvalidInput
        ));
    }

    #[test]
    fn load_from_rejects_invalid_username() {
        let root = TestRoot::new("load-invalid-username");
        let paths = root.paths();
        let root_session = root.root_session();
        fs::create_dir_all(paths.config_file().parent().expect("config has a parent"))
            .expect("should create temp config dir");
        let data = serde_json::to_string_pretty(&AppConfig {
            username: "bad name".to_string(),
            ..AppConfig::default()
        })
        .expect("should serialize config");
        fs::write(paths.config_file(), data).expect("should write temp config");

        let err = match ConfigStore::load_from(paths.clone(), root_session) {
            Ok(_) => panic!("invalid config should fail"),
            Err(err) => err,
        };
        assert!(matches!(err, ConfigStoreError::Validation(_)));
    }

    #[test]
    fn load_from_rejects_invalid_launch_auth_mode() {
        let root = TestRoot::new("load-invalid-launch-auth-mode");
        let paths = root.paths();
        let root_session = root.root_session();
        fs::create_dir_all(paths.config_file().parent().expect("config has a parent"))
            .expect("should create temp config dir");
        let data = serde_json::to_string_pretty(&AppConfig {
            launch_auth_mode: "online-ish".to_string(),
            ..AppConfig::default()
        })
        .expect("should serialize config");
        fs::write(paths.config_file(), data).expect("should write temp config");

        let err = match ConfigStore::load_from(paths.clone(), root_session) {
            Ok(_) => panic!("invalid config should fail"),
            Err(err) => err,
        };
        assert!(matches!(
            err,
            ConfigStoreError::Validation(AppConfigValidationError::InvalidLaunchAuthMode(_))
        ));
    }

    #[test]
    fn load_paths_preserve_disabled_guardian_mode() {
        let root = TestRoot::new("load-disabled-guardian-mode");
        let paths = root.paths();
        let root_session = root.root_session();
        fs::create_dir_all(paths.config_file().parent().expect("config has a parent"))
            .expect("should create temp config dir");
        let data = serde_json::to_string_pretty(&AppConfig {
            guardian_mode: "disabled".to_string(),
            ..AppConfig::default()
        })
        .expect("should serialize config");
        fs::write(paths.config_file(), data).expect("should write temp config");

        assert_eq!(
            ConfigStore::load_from(paths.clone(), Arc::clone(&root_session))
                .expect("regular load")
                .current()
                .guardian_mode,
            "disabled"
        );
        let startup = ConfigStore::load_for_startup(paths.clone(), root_session)
            .expect("startup load");
        assert!(startup.warnings.is_empty());
        assert_eq!(startup.store.current().guardian_mode, "disabled");
    }

    #[test]
    fn load_for_startup_uses_default_config_and_warning_for_invalid_config_without_rewriting() {
        let root = TestRoot::new("startup-invalid-launch-auth-mode");
        let paths = root.paths();
        let root_session = root.root_session();
        fs::create_dir_all(paths.config_file().parent().expect("config has a parent"))
            .expect("should create temp config dir");
        let library_dir = paths.library_dir().to_string_lossy().to_string();
        let data = serde_json::to_string_pretty(&AppConfig {
            launch_auth_mode: "online-ish".to_string(),
            library_dir: library_dir.clone(),
            max_memory_mb: 600,
            min_memory_mb: 800,
            ..AppConfig::default()
        })
        .expect("should serialize config");
        fs::write(paths.config_file(), &data).expect("should write temp config");

        let loaded = ConfigStore::load_for_startup(paths.clone(), root_session)
            .expect("startup load should tolerate invalid config");

        assert_eq!(loaded.store.current(), AppConfig::default());
        assert!(!loaded.store.mutation_allowed());
        assert_eq!(
            loaded.warnings,
            vec![super::CONFIG_STARTUP_WARNING.to_string()]
        );
        assert_eq!(
            fs::read_to_string(paths.config_file()).expect("config file should remain readable"),
            data
        );
    }

    #[test]
    fn load_for_startup_uses_default_config_and_warning_for_malformed_config_without_rewriting() {
        let root = TestRoot::new("startup-malformed-config");
        let paths = root.paths();
        let root_session = root.root_session();
        fs::create_dir_all(paths.config_file().parent().expect("config has a parent"))
            .expect("should create temp config dir");
        let malformed = "{not valid json";
        fs::write(paths.config_file(), malformed).expect("should write malformed config");

        let loaded = ConfigStore::load_for_startup(paths.clone(), Arc::clone(&root_session))
            .expect("startup load should tolerate malformed config");

        assert_eq!(loaded.store.current(), AppConfig::default());
        assert!(!loaded.store.mutation_allowed());
        assert_eq!(
            loaded.warnings,
            vec![super::CONFIG_STARTUP_WARNING.to_string()]
        );
        assert_eq!(
            fs::read_to_string(paths.config_file()).expect("config file should remain readable"),
            malformed
        );
        assert!(matches!(
            ConfigStore::load_from(paths.clone(), root_session),
            Err(ConfigStoreError::Parse(_))
        ));
    }

    #[test]
    fn load_for_startup_uses_default_config_and_warning_for_config_read_error() {
        let root = TestRoot::new("startup-config-read-error");
        let paths = root.paths();
        let root_session = root.root_session();
        fs::create_dir_all(paths.config_file()).expect("should create config path as directory");

        let loaded = ConfigStore::load_for_startup(paths.clone(), Arc::clone(&root_session))
            .expect("startup load should tolerate config read error");

        assert_eq!(loaded.store.current(), AppConfig::default());
        assert!(!loaded.store.mutation_allowed());
        assert_eq!(
            loaded.warnings,
            vec![super::CONFIG_STARTUP_WARNING.to_string()]
        );
        assert!(paths.config_file().is_dir());
        assert!(matches!(
            ConfigStore::load_from(paths.clone(), root_session),
            Err(ConfigStoreError::Read(_))
        ));
    }

    #[test]
    fn load_for_startup_uses_default_config_without_warning_when_config_is_missing() {
        let root = TestRoot::new("startup-missing-config");
        let paths = root.paths();
        let root_session = root.root_session();

        let loaded = ConfigStore::load_for_startup(paths.clone(), root_session)
            .expect("missing config should load for startup");

        assert_eq!(loaded.store.current(), AppConfig::default());
        assert!(loaded.store.mutation_allowed());
        assert!(loaded.warnings.is_empty());
        assert!(!paths.config_file().exists());
    }

    #[test]
    fn load_for_startup_uses_default_config_and_warning_for_invalid_username() {
        let root = TestRoot::new("startup-invalid-username");
        let paths = root.paths();
        let root_session = root.root_session();
        fs::create_dir_all(paths.config_file().parent().expect("config has a parent"))
            .expect("should create temp config dir");
        let data = serde_json::to_string_pretty(&AppConfig {
            username: "bad name".to_string(),
            launch_auth_mode: "online-ish".to_string(),
            ..AppConfig::default()
        })
        .expect("should serialize config");
        fs::write(paths.config_file(), &data).expect("should write temp config");

        let loaded = ConfigStore::load_for_startup(paths.clone(), root_session)
            .expect("startup should tolerate invalid config");

        assert_eq!(loaded.store.current(), AppConfig::default());
        assert!(!loaded.store.mutation_allowed());
        assert_eq!(
            loaded.warnings,
            vec![super::CONFIG_STARTUP_WARNING.to_string()]
        );
        assert_eq!(
            fs::read_to_string(paths.config_file()).expect("config file should remain readable"),
            data
        );
    }

    #[test]
    fn startup_rejects_unknown_fields_without_rewriting() {
        let root = TestRoot::new("startup-unknown-field");
        let paths = root.paths();
        let root_session = root.root_session();
        fs::create_dir_all(paths.config_file().parent().expect("config has a parent"))
            .expect("should create temp config dir");
        let data = r#"{"username":"Player","removed_legacy_setting":true}"#;
        fs::write(paths.config_file(), data).expect("should write config with unknown field");

        let loaded = ConfigStore::load_for_startup(paths.clone(), root_session)
            .expect("startup should contain schema rejection");

        assert_eq!(loaded.store.current(), AppConfig::default());
        assert!(!loaded.store.mutation_allowed());
        assert_eq!(
            loaded.warnings,
            vec![super::CONFIG_STARTUP_WARNING.to_string()]
        );
        assert_eq!(
            fs::read_to_string(paths.config_file()).expect("rejected config should remain readable"),
            data
        );
    }

    #[test]
    fn startup_rejects_oversized_config_without_reading_or_rewriting_it() {
        let root = TestRoot::new("startup-oversized");
        let paths = root.paths();
        let root_session = root.root_session();
        fs::create_dir_all(paths.config_file().parent().expect("config has a parent"))
            .expect("should create temp config dir");
        let data = vec![b' '; CONFIG_MAX_BYTES as usize + 1];
        fs::write(paths.config_file(), &data).expect("should write oversized config");

        let loaded = ConfigStore::load_for_startup(paths.clone(), Arc::clone(&root_session))
            .expect("startup should contain oversized config rejection");

        assert_eq!(loaded.store.current(), AppConfig::default());
        assert!(!loaded.store.mutation_allowed());
        assert_eq!(
            loaded.warnings,
            vec![super::CONFIG_STARTUP_WARNING.to_string()]
        );
        assert_eq!(
            fs::metadata(paths.config_file())
                .expect("oversized config should remain")
                .len(),
            CONFIG_MAX_BYTES + 1
        );
        assert_eq!(
            fs::read(paths.config_file()).expect("oversized config should remain readable"),
            data
        );
        assert!(matches!(
            ConfigStore::load_from(paths.clone(), root_session),
            Err(ConfigStoreError::Read(error)) if error.kind() == std::io::ErrorKind::InvalidData
        ));
    }

    #[test]
    fn from_config_rejects_invalid_username_without_writing_file() {
        let root = TestRoot::new("update-invalid-username");
        let paths = root.paths();
        let root_session = root.root_session();
        let err = match ConfigStore::from_config(
            paths.clone(),
            root_session,
            AppConfig {
                username: "bad name".to_string(),
                ..AppConfig::default()
            },
        ) {
            Ok(_) => panic!("invalid config should fail"),
            Err(error) => error,
        };

        assert!(matches!(err, ConfigStoreError::Validation(_)));
        assert!(!paths.config_file().exists());
    }
}
