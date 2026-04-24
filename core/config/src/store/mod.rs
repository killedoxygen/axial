use crate::models::{AppConfig, AppConfigValidationError};
use crate::paths::AppPaths;
use std::fs;
use std::path::Path;
use std::sync::RwLock;
use thiserror::Error;

pub struct ConfigStore {
    paths: AppPaths,
    config: RwLock<AppConfig>,
}

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

        if let Ok(mut guard) = self.config.write() {
            *guard = normalized.clone();
        }

        Ok(normalized)
    }

    pub fn replace_in_memory(&self, next: AppConfig) -> Result<(), ConfigStoreError> {
        let normalized = next.normalized()?;
        if let Ok(mut guard) = self.config.write() {
            *guard = normalized;
        }
        Ok(())
    }

    pub fn paths(&self) -> &AppPaths {
        &self.paths
    }
}

#[cfg(test)]
mod tests {
    use super::{ConfigStore, ConfigStoreError};
    use crate::{AppConfig, AppPaths};
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
