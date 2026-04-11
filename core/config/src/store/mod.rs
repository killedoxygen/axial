use crate::models::AppConfig;
use crate::paths::AppPaths;
use std::fs;
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
}

impl ConfigStore {
    pub fn load_default() -> Result<Self, ConfigStoreError> {
        Self::load_from(AppPaths::detect())
    }

    pub fn load_from(paths: AppPaths) -> Result<Self, ConfigStoreError> {
        let config = match fs::read_to_string(&paths.config_file) {
            Ok(data) => serde_json::from_str::<AppConfig>(&data)?.normalized(),
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
        let normalized = next.normalized();
        fs::create_dir_all(&self.paths.config_dir)?;
        let data = serde_json::to_string_pretty(&normalized)?;
        let temp_path = self.paths.config_file.with_extension("json.tmp");
        fs::write(&temp_path, data)?;
        fs::rename(temp_path, &self.paths.config_file)?;

        if let Ok(mut guard) = self.config.write() {
            *guard = normalized.clone();
        }

        Ok(normalized)
    }

    pub fn replace_in_memory(&self, next: AppConfig) {
        if let Ok(mut guard) = self.config.write() {
            *guard = next;
        }
    }

    pub fn paths(&self) -> &AppPaths {
        &self.paths
    }
}
