use crate::paths::AppPaths;
use croopor_minecraft::VersionEntry;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::RwLock;
use thiserror::Error;

/// Deterministic instance-art variants. Order is part of the seed contract and
/// must match `ART_PRESETS` in `frontend/src/art/InstanceArt.tsx`.
///
/// `art_seed` is the artwork source of truth. The preset is derived with
/// `ART_PRESETS[art_seed % ART_PRESETS.len()]`, and every renderer detail is
/// expected to derive from the same seed. `art_preset` is a denormalized label
/// recalculated from the seed whenever an instance is created or updated.
pub const ART_PRESETS: [&str; 9] = [
    "aurora", "silk", "mineral", "ember", "vapor", "topo", "prism", "dune", "orbit",
];

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Instance {
    pub id: String,
    pub name: String,
    pub version_id: String,
    pub created_at: String,
    #[serde(default)]
    pub last_played_at: String,
    #[serde(default)]
    pub art_seed: u32,
    #[serde(default)]
    pub art_preset: String,
    #[serde(default)]
    pub max_memory_mb: i32,
    #[serde(default)]
    pub min_memory_mb: i32,
    #[serde(default)]
    pub java_path: String,
    #[serde(default)]
    pub window_width: i32,
    #[serde(default)]
    pub window_height: i32,
    #[serde(default)]
    pub jvm_preset: String,
    #[serde(default)]
    pub performance_mode: String,
    #[serde(default)]
    pub extra_jvm_args: String,
    #[serde(default)]
    pub icon: String,
    #[serde(default)]
    pub accent: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct EnrichedInstance {
    #[serde(flatten)]
    pub instance: Instance,
    pub launchable: bool,
    #[serde(default)]
    pub status_detail: String,
    #[serde(default)]
    pub needs_install: String,
    #[serde(default)]
    pub java_major: i32,
    pub saves_count: usize,
    pub mods_count: usize,
    pub resource_count: usize,
    pub shader_count: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
struct StoredInstances {
    #[serde(default)]
    instances: Vec<Instance>,
    #[serde(default)]
    last_instance_id: String,
}

pub struct InstanceStore {
    paths: AppPaths,
    inner: RwLock<StoredInstances>,
}

#[derive(Debug, Error)]
pub enum InstanceStoreError {
    #[error("failed to read instances: {0}")]
    Read(#[from] std::io::Error),
    #[error("failed to parse instances: {0}")]
    Parse(#[from] serde_json::Error),
}

impl InstanceStore {
    pub fn load_default() -> Result<Self, InstanceStoreError> {
        Self::load_from(AppPaths::detect())
    }

    pub fn load_from(paths: AppPaths) -> Result<Self, InstanceStoreError> {
        let inner = match fs::read_to_string(&paths.instances_file) {
            Ok(data) => serde_json::from_str::<StoredInstances>(&data)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                StoredInstances::default()
            }
            Err(error) => return Err(InstanceStoreError::Read(error)),
        };

        Ok(Self {
            paths,
            inner: RwLock::new(inner),
        })
    }

    pub fn list(&self) -> Vec<Instance> {
        self.inner
            .read()
            .map(|inner| inner.instances.clone())
            .unwrap_or_default()
    }

    pub fn get(&self, id: &str) -> Option<Instance> {
        self.inner.read().ok().and_then(|inner| {
            inner
                .instances
                .iter()
                .find(|instance| instance.id == id)
                .cloned()
        })
    }

    pub fn last_instance_id(&self) -> Option<String> {
        self.inner.read().ok().and_then(|inner| {
            if inner.last_instance_id.is_empty() {
                None
            } else {
                Some(inner.last_instance_id.clone())
            }
        })
    }

    pub fn enrich(&self, versions: &[VersionEntry]) -> Vec<EnrichedInstance> {
        let version_map: HashMap<&str, &VersionEntry> = versions
            .iter()
            .map(|version| (version.id.as_str(), version))
            .collect();

        self.list()
            .into_iter()
            .map(|instance| {
                let version = version_map.get(instance.version_id.as_str()).copied();
                let game_dir = self.game_dir(&instance.id);

                EnrichedInstance {
                    launchable: version.is_some_and(|entry| entry.launchable),
                    status_detail: version
                        .map(|entry| entry.status_detail.clone())
                        .unwrap_or_else(|| "version not installed".to_string()),
                    needs_install: version
                        .map(|entry| entry.needs_install.clone())
                        .unwrap_or_default(),
                    java_major: version.map(|entry| entry.java_major).unwrap_or_default(),
                    saves_count: count_entries(&game_dir.join("saves")),
                    mods_count: count_entries(&game_dir.join("mods")),
                    resource_count: count_entries(&game_dir.join("resourcepacks")),
                    shader_count: count_entries(&game_dir.join("shaderpacks")),
                    instance,
                }
            })
            .collect()
    }

    pub fn game_dir(&self, id: &str) -> std::path::PathBuf {
        self.paths.instances_dir.join(id)
    }

    pub fn update(&self, next: Instance) -> Result<Instance, InstanceStoreError> {
        let mut inner = self.inner.write().map_err(|_| {
            InstanceStoreError::Read(std::io::Error::other("instance store lock poisoned"))
        })?;
        let Some(index) = inner
            .instances
            .iter()
            .position(|instance| instance.id == next.id)
        else {
            return Err(InstanceStoreError::Read(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "instance not found",
            )));
        };

        inner.instances[index] = next.clone();
        self.persist_locked(&inner)?;
        Ok(next)
    }

    pub fn clear(&self) -> Result<(), InstanceStoreError> {
        let mut inner = self.inner.write().map_err(|_| {
            InstanceStoreError::Read(std::io::Error::other("instance store lock poisoned"))
        })?;
        inner.instances.clear();
        inner.last_instance_id.clear();
        self.persist_locked(&inner)
    }

    pub fn paths(&self) -> &AppPaths {
        &self.paths
    }

    pub fn remove(&self, id: &str, delete_files: bool) -> Result<(), InstanceStoreError> {
        let mut inner = self.inner.write().map_err(|_| {
            InstanceStoreError::Read(std::io::Error::other("instance store lock poisoned"))
        })?;
        let Some(index) = inner
            .instances
            .iter()
            .position(|instance| instance.id == id)
        else {
            return Err(InstanceStoreError::Read(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "instance not found",
            )));
        };

        inner.instances.remove(index);
        if inner.last_instance_id == id {
            inner.last_instance_id.clear();
        }
        if delete_files {
            let _ = fs::remove_dir_all(self.paths.instances_dir.join(id));
        }
        self.persist_locked(&inner)
    }

    pub fn set_last_instance_id(&self, id: impl Into<String>) -> Result<(), InstanceStoreError> {
        let mut inner = self.inner.write().map_err(|_| {
            InstanceStoreError::Read(std::io::Error::other("instance store lock poisoned"))
        })?;
        inner.last_instance_id = id.into();
        self.persist_locked(&inner)
    }

    pub fn add(
        &self,
        name: String,
        version_id: String,
        icon: String,
        accent: String,
        mc_dir: Option<&Path>,
    ) -> Result<Instance, InstanceStoreError> {
        let mut inner = self.inner.write().map_err(|_| {
            InstanceStoreError::Read(std::io::Error::other("instance store lock poisoned"))
        })?;

        if name.trim().is_empty() {
            return Err(InstanceStoreError::Read(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "instance name is required",
            )));
        }
        if version_id.trim().is_empty() {
            return Err(InstanceStoreError::Read(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "version_id is required",
            )));
        }
        if inner.instances.iter().any(|instance| instance.name == name) {
            return Err(InstanceStoreError::Read(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "an instance with this name already exists",
            )));
        }

        let id = generate_id();
        let art_seed = derive_art_seed(&id, &name, &version_id);
        let instance = Instance {
            id,
            name,
            version_id,
            created_at: chrono::DateTime::<chrono::Utc>::from(std::time::SystemTime::now())
                .to_rfc3339(),
            last_played_at: String::new(),
            art_seed,
            art_preset: art_preset_for_seed(art_seed).to_string(),
            max_memory_mb: 0,
            min_memory_mb: 0,
            java_path: String::new(),
            window_width: 0,
            window_height: 0,
            jvm_preset: String::new(),
            performance_mode: String::new(),
            extra_jvm_args: String::new(),
            icon,
            accent,
        };

        inner.instances.push(instance.clone());
        if let Err(error) = self.persist_locked(&inner) {
            inner.instances.retain(|stored| stored.id != instance.id);
            return Err(error);
        }

        if let Err(error) = self.initialize_instance_files(&instance.id, mc_dir) {
            inner.instances.retain(|stored| stored.id != instance.id);
            let rollback_result = self.persist_locked(&inner);
            let _ = fs::remove_dir_all(self.paths.instances_dir.join(&instance.id));
            return Err(match rollback_result {
                Ok(()) => InstanceStoreError::Read(error),
                Err(rollback_error) => InstanceStoreError::Read(std::io::Error::other(format!(
                    "failed to initialize instance files: {error}; failed to roll back persisted instance: {rollback_error}"
                ))),
            });
        }

        Ok(instance)
    }

    fn initialize_instance_files(
        &self,
        instance_id: &str,
        mc_dir: Option<&Path>,
    ) -> Result<(), std::io::Error> {
        let game_dir = self.paths.instances_dir.join(instance_id);
        for subdir in ["saves", "mods", "resourcepacks", "shaderpacks", "config"] {
            fs::create_dir_all(game_dir.join(subdir))?;
        }

        if let Some(mc_dir) = mc_dir {
            let options_path = mc_dir.join("options.txt");
            if let Ok(data) = fs::read(&options_path) {
                let _ = fs::write(game_dir.join("options.txt"), data);
            }
        }

        Ok(())
    }

    fn persist_locked(&self, inner: &StoredInstances) -> Result<(), InstanceStoreError> {
        fs::create_dir_all(&self.paths.config_dir)?;
        let data = serde_json::to_string_pretty(inner)?;
        let temp_path = self.paths.instances_file.with_extension("json.tmp");
        fs::write(&temp_path, data)?;
        fs::rename(temp_path, &self.paths.instances_file)?;
        Ok(())
    }
}

fn count_entries(path: &Path) -> usize {
    fs::read_dir(path)
        .map(|entries| entries.filter_map(Result::ok).count())
        .unwrap_or(0)
}

fn generate_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_nanos())
        .unwrap_or_default();
    format!("{:016x}", nanos as u64)
}

fn derive_art_seed(id: &str, name: &str, version_id: &str) -> u32 {
    let mut h = 2166136261u32;
    for byte in id
        .bytes()
        .chain([0])
        .chain(name.bytes())
        .chain([0])
        .chain(version_id.bytes())
    {
        h ^= u32::from(byte);
        h = h.wrapping_mul(16777619);
    }
    if h == 0 { 1 } else { h }
}

pub fn art_preset_for_seed(seed: u32) -> &'static str {
    ART_PRESETS[(seed as usize) % ART_PRESETS.len()]
}

#[cfg(test)]
mod tests {
    use super::{ART_PRESETS, InstanceStore, StoredInstances, art_preset_for_seed};
    use crate::paths::AppPaths;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::RwLock;

    fn test_root(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "croopor-config-{name}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|value| value.as_nanos())
                .unwrap_or_default()
        ));
        fs::create_dir_all(&path).expect("create test root");
        path
    }

    fn test_paths(root: &Path) -> AppPaths {
        let config_dir = root.join("config");
        AppPaths {
            config_file: config_dir.join("config.json"),
            instances_file: config_dir.join("instances.json"),
            instances_dir: root.join("instances"),
            music_dir: root.join("music"),
            library_dir: root.join("library"),
            config_dir,
        }
    }

    #[test]
    fn art_preset_is_derived_from_seed_modulo_preset_order() {
        for (index, preset) in ART_PRESETS.iter().enumerate() {
            assert_eq!(art_preset_for_seed(index as u32), *preset);
            assert_eq!(
                art_preset_for_seed((index + ART_PRESETS.len() * 17) as u32),
                *preset
            );
        }
    }

    #[test]
    fn add_does_not_create_instance_dirs_when_persist_fails() {
        let root = test_root("persist-failure");
        let config_blocker = root.join("config-blocker");
        fs::write(&config_blocker, "not a dir").expect("create config blocker");

        let paths = AppPaths {
            config_file: config_blocker.join("config.json"),
            instances_file: config_blocker.join("instances.json"),
            instances_dir: root.join("instances"),
            music_dir: root.join("music"),
            library_dir: root.join("library"),
            config_dir: config_blocker,
        };
        let store = InstanceStore {
            paths: paths.clone(),
            inner: RwLock::new(StoredInstances::default()),
        };

        let error = store
            .add(
                "Test".to_string(),
                "1.21.1".to_string(),
                String::new(),
                String::new(),
                None,
            )
            .expect_err("persist should fail");

        assert!(matches!(error, super::InstanceStoreError::Read(_)));
        assert!(store.list().is_empty());
        assert!(!paths.instances_dir.exists());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn add_rolls_back_persisted_instance_when_file_setup_fails() {
        let root = test_root("file-setup-failure");
        let paths = test_paths(&root);
        fs::create_dir_all(&paths.config_dir).expect("create config dir");
        fs::write(&paths.instances_dir, "not a dir").expect("create instances blocker");

        let store = InstanceStore {
            paths: paths.clone(),
            inner: RwLock::new(StoredInstances::default()),
        };

        let error = store
            .add(
                "Test".to_string(),
                "1.21.1".to_string(),
                String::new(),
                String::new(),
                None,
            )
            .expect_err("file setup should fail");

        assert!(matches!(error, super::InstanceStoreError::Read(_)));
        assert!(store.list().is_empty());

        let persisted = fs::read_to_string(&paths.instances_file).expect("read persisted store");
        let stored: serde_json::Value =
            serde_json::from_str(&persisted).expect("parse persisted store");
        assert_eq!(
            stored["instances"]
                .as_array()
                .expect("instances array")
                .len(),
            0
        );

        let _ = fs::remove_dir_all(root);
    }
}
