use crate::models::{AppConfig, AppConfigValidationError, LAUNCH_AUTH_MODE_OFFLINE};
use crate::paths::AppPaths;
use std::fs;
use std::path::Path;
use std::sync::RwLock;
use thiserror::Error;

pub struct ConfigStore {
    paths: AppPaths,
    config: RwLock<AppConfig>,
}

pub struct ConfigStartupLoad {
    pub store: ConfigStore,
    pub warnings: Vec<String>,
}

const CONFIG_STARTUP_WARNING: &str = "Croopor could not load settings, so it started with safe defaults. Check app data permissions or restore the settings file.";

#[derive(Debug, Error)]
pub enum ConfigStoreError {
    #[error("failed to read config: {0}")]
    Read(#[from] std::io::Error),
    #[error("failed to parse config: {0}")]
    Parse(#[from] serde_json::Error),
    #[error(transparent)]
    Validation(#[from] AppConfigValidationError),
}

impl ConfigStore {
    fn replace_file(source: &Path, destination: &Path) -> Result<(), std::io::Error> {
        if fs::rename(source, destination).is_ok() {
            return Ok(());
        }

        if destination.exists() {
            let _ = fs::remove_file(destination);
        }

        match fs::rename(source, destination) {
            Ok(()) => Ok(()),
            Err(error) => {
                let _ = fs::remove_file(source);
                Err(error)
            }
        }
    }

    pub fn load_default() -> Result<Self, ConfigStoreError> {
        Self::load_from(AppPaths::detect())
    }

    pub fn load_from(paths: AppPaths) -> Result<Self, ConfigStoreError> {
        let config = match fs::read_to_string(&paths.config_file) {
            Ok(data) => serde_json::from_str::<AppConfig>(&data)?.normalized()?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => AppConfig::default(),
            Err(error) => return Err(ConfigStoreError::Read(error)),
        };

        Ok(Self {
            paths,
            config: RwLock::new(config),
        })
    }

    pub fn load_for_startup(paths: AppPaths) -> Result<ConfigStartupLoad, ConfigStoreError> {
        let (config, warnings) = match fs::read_to_string(&paths.config_file) {
            Ok(data) => match load_config_for_startup(&data) {
                Ok(loaded) => loaded,
                Err(ConfigStoreError::Parse(_)) => (
                    AppConfig::default(),
                    vec![CONFIG_STARTUP_WARNING.to_string()],
                ),
                Err(error) => return Err(error),
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                (AppConfig::default(), Vec::new())
            }
            Err(_) => (
                AppConfig::default(),
                vec![CONFIG_STARTUP_WARNING.to_string()],
            ),
        };

        Ok(ConfigStartupLoad {
            store: Self {
                paths,
                config: RwLock::new(config),
            },
            warnings,
        })
    }

    pub fn current(&self) -> AppConfig {
        self.config
            .read()
            .map(|value| value.clone())
            .unwrap_or_else(|_| AppConfig::default())
    }

    pub fn update(&self, next: AppConfig) -> Result<AppConfig, ConfigStoreError> {
        let normalized = next.normalized()?;
        fs::create_dir_all(&self.paths.config_dir)?;
        let data = serde_json::to_string_pretty(&normalized)?;
        let temp_path = self.paths.config_file.with_extension("json.tmp");
        fs::write(&temp_path, data)?;
        Self::replace_file(&temp_path, &self.paths.config_file)?;

        let mut guard = self
            .config
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *guard = normalized.clone();

        Ok(normalized)
    }

    pub fn replace_in_memory(&self, next: AppConfig) -> Result<(), ConfigStoreError> {
        let normalized = next.normalized()?;
        let mut guard = self
            .config
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *guard = normalized;
        Ok(())
    }

    pub fn paths(&self) -> &AppPaths {
        &self.paths
    }
}

fn load_config_for_startup(data: &str) -> Result<(AppConfig, Vec<String>), ConfigStoreError> {
    let parsed = serde_json::from_str::<AppConfig>(data)?;
    match parsed.clone().normalized() {
        Ok(config) => Ok((config, Vec::new())),
        Err(AppConfigValidationError::InvalidLaunchAuthMode(_)) => {
            let mut fallback = parsed;
            fallback.launch_auth_mode = LAUNCH_AUTH_MODE_OFFLINE.to_string();
            let config = fallback.normalized()?;
            Ok((
                config,
                vec![
                    "Croopor ignored an invalid launch auth setting for this session and is using Offline mode."
                        .to_string(),
                ],
            ))
        }
        Err(error) => Err(ConfigStoreError::Validation(error)),
    }
}

#[cfg(test)]
mod tests {
    use super::{ConfigStore, ConfigStoreError};
    use crate::{AppConfig, AppConfigValidationError, AppPaths, LAUNCH_AUTH_MODE_OFFLINE};
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn test_paths(name: &str) -> AppPaths {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after unix epoch")
            .as_nanos();
        let config_dir = std::env::temp_dir().join(format!(
            "croopor-config-store-{name}-{}-{nonce}",
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
    fn load_for_startup_falls_back_only_for_invalid_launch_auth_mode_without_rewriting() {
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
            .expect("startup load should tolerate invalid launch auth mode");
        let config = loaded.store.current();

        assert_eq!(config.launch_auth_mode, LAUNCH_AUTH_MODE_OFFLINE);
        assert_eq!(config.library_dir, library_dir);
        assert_eq!(config.max_memory_mb, 600);
        assert_eq!(config.min_memory_mb, 600);
        assert_eq!(
            loaded.warnings,
            vec![
                "Croopor ignored an invalid launch auth setting for this session and is using Offline mode."
                    .to_string()
            ]
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
        assert!(loaded.warnings.is_empty());
        assert!(!paths.config_file.exists());

        cleanup(&paths.config_dir);
    }

    #[test]
    fn load_for_startup_keeps_rejecting_invalid_username() {
        let paths = test_paths("startup-invalid-username");
        fs::create_dir_all(&paths.config_dir).expect("should create temp config dir");
        let data = serde_json::to_string_pretty(&AppConfig {
            username: "bad name".to_string(),
            launch_auth_mode: "online-ish".to_string(),
            ..AppConfig::default()
        })
        .expect("should serialize config");
        fs::write(&paths.config_file, data).expect("should write temp config");

        let err = match ConfigStore::load_for_startup(paths.clone()) {
            Ok(_) => panic!("startup load should keep rejecting invalid username"),
            Err(err) => err,
        };
        assert!(matches!(
            err,
            ConfigStoreError::Validation(AppConfigValidationError::InvalidUsername(_))
        ));

        cleanup(&paths.config_dir);
    }

    #[test]
    fn update_rejects_invalid_username_without_writing_file() {
        let paths = test_paths("update-invalid-username");
        let store = ConfigStore::load_from(paths.clone()).expect("missing config should load");

        let err = store
            .update(AppConfig {
                username: "bad name".to_string(),
                ..AppConfig::default()
            })
            .expect_err("invalid config should fail");

        assert!(matches!(err, ConfigStoreError::Validation(_)));
        assert!(!paths.config_file.exists());

        cleanup(&paths.config_dir);
    }
}
