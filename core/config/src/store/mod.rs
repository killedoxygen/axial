use crate::models::{AppConfig, AppConfigValidationError};
use crate::paths::AppPaths;
use std::fs;
use std::io::Read;
use std::path::Path;
use thiserror::Error;

pub struct ConfigStore {
    paths: AppPaths,
    config: AppConfig,
    mutation_allowed: bool,
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
    #[error("config exceeds the maximum persisted size of {max_bytes} bytes")]
    TooLarge { max_bytes: u64 },
}

impl ConfigStore {
    pub fn load_from(paths: AppPaths) -> Result<Self, ConfigStoreError> {
        let config = match read_config(&paths.config_file) {
            Ok(data) => serde_json::from_slice::<AppConfig>(&data)?.normalized()?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => AppConfig::default(),
            Err(error) => return Err(ConfigStoreError::Read(error)),
        };

        Ok(Self {
            paths,
            config,
            mutation_allowed: true,
        })
    }

    pub fn load_for_startup(paths: AppPaths) -> Result<ConfigStartupLoad, ConfigStoreError> {
        let (config, warnings, mutation_allowed) = match read_config(&paths.config_file) {
            Ok(data) => match load_config_for_startup(&data) {
                Ok(config) => (config, Vec::new(), true),
                Err(ConfigStoreError::Parse(_) | ConfigStoreError::Validation(_)) => (
                    AppConfig::default(),
                    vec![CONFIG_STARTUP_WARNING.to_string()],
                    false,
                ),
                Err(error) => return Err(error),
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                (AppConfig::default(), Vec::new(), true)
            }
            Err(_) => (
                AppConfig::default(),
                vec![CONFIG_STARTUP_WARNING.to_string()],
                false,
            ),
        };

        Ok(ConfigStartupLoad {
            store: Self {
                paths,
                config,
                mutation_allowed,
            },
            warnings,
        })
    }

    pub fn current(&self) -> AppConfig {
        self.config.clone()
    }

    pub fn from_config(paths: AppPaths, config: AppConfig) -> Result<Self, ConfigStoreError> {
        Ok(Self {
            paths,
            config: config.normalized()?,
            mutation_allowed: true,
        })
    }

    pub fn paths(&self) -> &AppPaths {
        &self.paths
    }

    pub fn mutation_allowed(&self) -> bool {
        self.mutation_allowed
    }
}

fn load_config_for_startup(data: &[u8]) -> Result<AppConfig, ConfigStoreError> {
    Ok(serde_json::from_slice::<AppConfig>(data)?.normalized()?)
}

fn read_config(path: &Path) -> Result<Vec<u8>, std::io::Error> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() || metadata.len() > CONFIG_MAX_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "config file is not a bounded regular file",
        ));
    }
    let mut data = Vec::with_capacity(metadata.len() as usize);
    let mut bounded = fs::File::open(path)?.take(CONFIG_MAX_BYTES + 1);
    bounded.read_to_end(&mut data)?;
    if data.len() as u64 > CONFIG_MAX_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "config file exceeds the maximum size",
        ));
    }
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::{CONFIG_MAX_BYTES, ConfigStore, ConfigStoreError};
    use crate::{AppConfig, AppConfigValidationError, AppPaths};
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn test_paths(name: &str) -> AppPaths {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after unix epoch")
            .as_nanos();
        let config_dir = std::env::temp_dir().join(format!(
            "axial-config-store-{name}-{}-{nonce}",
            std::process::id()
        ));
        AppPaths {
            config_file: config_dir.join("config.json"),
            instances_file: config_dir.join("instances.json"),
            instances_dir: config_dir.join("instances"),
            music_dir: config_dir.join("music"),
            library_dir: config_dir.join("library"),
            config_dir,
        }
    }

    fn cleanup(path: &PathBuf) {
        let _ = fs::remove_dir_all(path);
    }

    #[test]
    fn load_from_rejects_invalid_username() {
        let paths = test_paths("load-invalid-username");
        fs::create_dir_all(&paths.config_dir).expect("should create temp config dir");
        let data = serde_json::to_string_pretty(&AppConfig {
            username: "bad name".to_string(),
            ..AppConfig::default()
        })
        .expect("should serialize config");
        fs::write(&paths.config_file, data).expect("should write temp config");

        let err = match ConfigStore::load_from(paths.clone()) {
            Ok(_) => panic!("invalid config should fail"),
            Err(err) => err,
        };
        assert!(matches!(err, ConfigStoreError::Validation(_)));

        cleanup(&paths.config_dir);
    }

    #[test]
    fn load_from_rejects_invalid_launch_auth_mode() {
        let paths = test_paths("load-invalid-launch-auth-mode");
        fs::create_dir_all(&paths.config_dir).expect("should create temp config dir");
        let data = serde_json::to_string_pretty(&AppConfig {
            launch_auth_mode: "online-ish".to_string(),
            ..AppConfig::default()
        })
        .expect("should serialize config");
        fs::write(&paths.config_file, data).expect("should write temp config");

        let err = match ConfigStore::load_from(paths.clone()) {
            Ok(_) => panic!("invalid config should fail"),
            Err(err) => err,
        };
        assert!(matches!(
            err,
            ConfigStoreError::Validation(AppConfigValidationError::InvalidLaunchAuthMode(_))
        ));

        cleanup(&paths.config_dir);
    }

    #[test]
    fn load_paths_preserve_disabled_guardian_mode() {
        let paths = test_paths("load-disabled-guardian-mode");
        fs::create_dir_all(&paths.config_dir).expect("should create temp config dir");
        let data = serde_json::to_string_pretty(&AppConfig {
            guardian_mode: "disabled".to_string(),
            ..AppConfig::default()
        })
        .expect("should serialize config");
        fs::write(&paths.config_file, data).expect("should write temp config");

        assert_eq!(
            ConfigStore::load_from(paths.clone())
                .expect("regular load")
                .current()
                .guardian_mode,
            "disabled"
        );
        let startup = ConfigStore::load_for_startup(paths.clone()).expect("startup load");
        assert!(startup.warnings.is_empty());
        assert_eq!(startup.store.current().guardian_mode, "disabled");

        cleanup(&paths.config_dir);
    }

    #[test]
    fn load_for_startup_uses_default_config_and_warning_for_invalid_config_without_rewriting() {
        let paths = test_paths("startup-invalid-launch-auth-mode");
        fs::create_dir_all(&paths.config_dir).expect("should create temp config dir");
        let library_dir = paths.library_dir.to_string_lossy().to_string();
        let data = serde_json::to_string_pretty(&AppConfig {
            launch_auth_mode: "online-ish".to_string(),
            library_dir: library_dir.clone(),
            max_memory_mb: 600,
            min_memory_mb: 800,
            ..AppConfig::default()
        })
        .expect("should serialize config");
        fs::write(&paths.config_file, &data).expect("should write temp config");

        let loaded = ConfigStore::load_for_startup(paths.clone())
            .expect("startup load should tolerate invalid config");

        assert_eq!(loaded.store.current(), AppConfig::default());
        assert!(!loaded.store.mutation_allowed());
        assert_eq!(
            loaded.warnings,
            vec![super::CONFIG_STARTUP_WARNING.to_string()]
        );
        assert_eq!(
            fs::read_to_string(&paths.config_file).expect("config file should remain readable"),
            data
        );

        cleanup(&paths.config_dir);
    }

    #[test]
    fn load_for_startup_uses_default_config_and_warning_for_malformed_config_without_rewriting() {
        let paths = test_paths("startup-malformed-config");
        fs::create_dir_all(&paths.config_dir).expect("should create temp config dir");
        let malformed = "{not valid json";
        fs::write(&paths.config_file, malformed).expect("should write malformed config");

        let loaded = ConfigStore::load_for_startup(paths.clone())
            .expect("startup load should tolerate malformed config");

        assert_eq!(loaded.store.current(), AppConfig::default());
        assert!(!loaded.store.mutation_allowed());
        assert_eq!(
            loaded.warnings,
            vec![super::CONFIG_STARTUP_WARNING.to_string()]
        );
        assert_eq!(
            fs::read_to_string(&paths.config_file).expect("config file should remain readable"),
            malformed
        );
        assert!(matches!(
            ConfigStore::load_from(paths.clone()),
            Err(ConfigStoreError::Parse(_))
        ));

        cleanup(&paths.config_dir);
    }

    #[test]
    fn load_for_startup_uses_default_config_and_warning_for_config_read_error() {
        let paths = test_paths("startup-config-read-error");
        fs::create_dir_all(&paths.config_file).expect("should create config path as directory");

        let loaded = ConfigStore::load_for_startup(paths.clone())
            .expect("startup load should tolerate config read error");

        assert_eq!(loaded.store.current(), AppConfig::default());
        assert!(!loaded.store.mutation_allowed());
        assert_eq!(
            loaded.warnings,
            vec![super::CONFIG_STARTUP_WARNING.to_string()]
        );
        assert!(paths.config_file.is_dir());
        assert!(matches!(
            ConfigStore::load_from(paths.clone()),
            Err(ConfigStoreError::Read(_))
        ));

        cleanup(&paths.config_dir);
    }

    #[test]
    fn load_for_startup_uses_default_config_without_warning_when_config_is_missing() {
        let paths = test_paths("startup-missing-config");

        let loaded = ConfigStore::load_for_startup(paths.clone())
            .expect("missing config should load for startup");

        assert_eq!(loaded.store.current(), AppConfig::default());
        assert!(loaded.store.mutation_allowed());
        assert!(loaded.warnings.is_empty());
        assert!(!paths.config_file.exists());

        cleanup(&paths.config_dir);
    }

    #[test]
    fn load_for_startup_uses_default_config_and_warning_for_invalid_username() {
        let paths = test_paths("startup-invalid-username");
        fs::create_dir_all(&paths.config_dir).expect("should create temp config dir");
        let data = serde_json::to_string_pretty(&AppConfig {
            username: "bad name".to_string(),
            launch_auth_mode: "online-ish".to_string(),
            ..AppConfig::default()
        })
        .expect("should serialize config");
        fs::write(&paths.config_file, &data).expect("should write temp config");

        let loaded = ConfigStore::load_for_startup(paths.clone())
            .expect("startup should tolerate invalid config");

        assert_eq!(loaded.store.current(), AppConfig::default());
        assert!(!loaded.store.mutation_allowed());
        assert_eq!(
            loaded.warnings,
            vec![super::CONFIG_STARTUP_WARNING.to_string()]
        );
        assert_eq!(
            fs::read_to_string(&paths.config_file).expect("config file should remain readable"),
            data
        );

        cleanup(&paths.config_dir);
    }

    #[test]
    fn startup_rejects_unknown_fields_without_rewriting() {
        let paths = test_paths("startup-unknown-field");
        fs::create_dir_all(&paths.config_dir).expect("should create temp config dir");
        let data = r#"{"username":"Player","removed_legacy_setting":true}"#;
        fs::write(&paths.config_file, data).expect("should write config with unknown field");

        let loaded = ConfigStore::load_for_startup(paths.clone())
            .expect("startup should contain schema rejection");

        assert_eq!(loaded.store.current(), AppConfig::default());
        assert!(!loaded.store.mutation_allowed());
        assert_eq!(
            loaded.warnings,
            vec![super::CONFIG_STARTUP_WARNING.to_string()]
        );
        assert_eq!(
            fs::read_to_string(&paths.config_file).expect("rejected config should remain readable"),
            data
        );

        cleanup(&paths.config_dir);
    }

    #[test]
    fn startup_rejects_oversized_config_without_reading_or_rewriting_it() {
        let paths = test_paths("startup-oversized");
        fs::create_dir_all(&paths.config_dir).expect("should create temp config dir");
        let data = vec![b' '; CONFIG_MAX_BYTES as usize + 1];
        fs::write(&paths.config_file, &data).expect("should write oversized config");

        let loaded = ConfigStore::load_for_startup(paths.clone())
            .expect("startup should contain oversized config rejection");

        assert_eq!(loaded.store.current(), AppConfig::default());
        assert!(!loaded.store.mutation_allowed());
        assert_eq!(
            loaded.warnings,
            vec![super::CONFIG_STARTUP_WARNING.to_string()]
        );
        assert_eq!(
            fs::metadata(&paths.config_file)
                .expect("oversized config should remain")
                .len(),
            CONFIG_MAX_BYTES + 1
        );
        assert_eq!(
            fs::read(&paths.config_file).expect("oversized config should remain readable"),
            data
        );
        assert!(matches!(
            ConfigStore::load_from(paths.clone()),
            Err(ConfigStoreError::Read(error)) if error.kind() == std::io::ErrorKind::InvalidData
        ));

        cleanup(&paths.config_dir);
    }

    #[test]
    fn from_config_rejects_invalid_username_without_writing_file() {
        let paths = test_paths("update-invalid-username");
        let err = match ConfigStore::from_config(
            paths.clone(),
            AppConfig {
                username: "bad name".to_string(),
                ..AppConfig::default()
            },
        ) {
            Ok(_) => panic!("invalid config should fail"),
            Err(error) => error,
        };

        assert!(matches!(err, ConfigStoreError::Validation(_)));
        assert!(!paths.config_file.exists());

        cleanup(&paths.config_dir);
    }
}
