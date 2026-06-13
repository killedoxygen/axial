use crate::paths::AppPaths;
use croopor_minecraft::VersionEntry;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::Path;
use std::sync::RwLock;
use thiserror::Error;

/// `art_seed` is the source of truth for the instance identity tile: the
/// frontend derives the tile hues from it, and "shuffle" rewrites it.
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

pub struct InstanceStoreStartup {
    pub store: InstanceStore,
    pub warnings: Vec<String>,
}

const INSTANCE_REGISTRY_STARTUP_WARNING: &str = "Croopor could not load the instance list, so it started with an empty list. Check app data permissions or restore the instance registry.";

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

        Ok(Self::from_inner(paths, inner))
    }

    pub fn load_for_startup(paths: AppPaths) -> InstanceStoreStartup {
        let (inner, warnings) = match fs::read_to_string(&paths.instances_file) {
            Ok(data) => match serde_json::from_str::<StoredInstances>(&data) {
                Ok(inner) => (inner, Vec::new()),
                Err(_) => (
                    StoredInstances::default(),
                    vec![INSTANCE_REGISTRY_STARTUP_WARNING.to_string()],
                ),
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                (StoredInstances::default(), Vec::new())
            }
            Err(_) => (
                StoredInstances::default(),
                vec![INSTANCE_REGISTRY_STARTUP_WARNING.to_string()],
            ),
        };

        InstanceStoreStartup {
            store: Self::from_inner(paths, inner),
            warnings,
        }
    }

    fn from_inner(paths: AppPaths, inner: StoredInstances) -> Self {
        Self {
            paths,
            inner: RwLock::new(inner),
        }
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
        if inner
            .instances
            .iter()
            .enumerate()
            .any(|(stored_index, instance)| stored_index != index && instance.name == next.name)
        {
            return Err(InstanceStoreError::Read(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "an instance with this name already exists",
            )));
        }

        let previous = inner.instances[index].clone();
        inner.instances[index] = next.clone();
        if let Err(error) = self.persist_locked(&inner) {
            inner.instances[index] = previous;
            return Err(error);
        }
        Ok(next)
    }

    pub fn clear(&self) -> Result<(), InstanceStoreError> {
        let mut inner = self.inner.write().map_err(|_| {
            InstanceStoreError::Read(std::io::Error::other("instance store lock poisoned"))
        })?;
        let previous = inner.clone();
        inner.instances.clear();
        inner.last_instance_id.clear();
        if let Err(error) = self.persist_locked(&inner) {
            *inner = previous;
            return Err(error);
        }
        Ok(())
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

        let removed = inner.instances.remove(index);
        let previous_last_instance_id = inner.last_instance_id.clone();
        if inner.last_instance_id == id {
            inner.last_instance_id.clear();
        }
        if let Err(error) = self.persist_locked(&inner) {
            inner.instances.insert(index, removed);
            inner.last_instance_id = previous_last_instance_id;
            return Err(error);
        }
        drop(inner);

        if delete_files {
            let _ = fs::remove_dir_all(self.paths.instances_dir.join(id));
        }
        Ok(())
    }

    pub fn set_last_instance_id(&self, id: impl Into<String>) -> Result<(), InstanceStoreError> {
        let mut inner = self.inner.write().map_err(|_| {
            InstanceStoreError::Read(std::io::Error::other("instance store lock poisoned"))
        })?;
        let previous = inner.last_instance_id.clone();
        inner.last_instance_id = id.into();
        if let Err(error) = self.persist_locked(&inner) {
            inner.last_instance_id = previous;
            return Err(error);
        }
        Ok(())
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

        if let Err(error) = self.ensure_instance_layout(&instance.id, mc_dir) {
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

    pub fn duplicate(
        &self,
        source_id: &str,
        requested_name: Option<String>,
        mc_dir: Option<&Path>,
    ) -> Result<Instance, InstanceStoreError> {
        let mut inner = self.inner.write().map_err(|_| {
            InstanceStoreError::Read(std::io::Error::other("instance store lock poisoned"))
        })?;
        let source = inner
            .instances
            .iter()
            .find(|instance| instance.id == source_id)
            .cloned()
            .ok_or_else(|| {
                InstanceStoreError::Read(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "instance not found",
                ))
            })?;
        let requested_name = requested_name
            .map(|name| name.trim().to_string())
            .filter(|name| !name.is_empty());
        let name = match requested_name {
            Some(name) => {
                if inner.instances.iter().any(|instance| instance.name == name) {
                    return Err(InstanceStoreError::Read(std::io::Error::new(
                        std::io::ErrorKind::AlreadyExists,
                        "an instance with this name already exists",
                    )));
                }
                name
            }
            None => duplicate_name_for(&source.name, &inner.instances),
        };

        let id = generate_id();
        let art_seed = derive_art_seed(&id, &name, &source.version_id);
        let instance = Instance {
            id,
            name,
            version_id: source.version_id.clone(),
            created_at: chrono::DateTime::<chrono::Utc>::from(std::time::SystemTime::now())
                .to_rfc3339(),
            last_played_at: String::new(),
            art_seed,
            max_memory_mb: source.max_memory_mb,
            min_memory_mb: source.min_memory_mb,
            java_path: source.java_path.clone(),
            window_width: source.window_width,
            window_height: source.window_height,
            jvm_preset: source.jvm_preset.clone(),
            performance_mode: source.performance_mode.clone(),
            extra_jvm_args: source.extra_jvm_args.clone(),
            icon: source.icon.clone(),
            accent: source.accent.clone(),
        };

        inner.instances.push(instance.clone());
        if let Err(error) = self.persist_locked(&inner) {
            inner.instances.retain(|stored| stored.id != instance.id);
            return Err(error);
        }
        drop(inner);

        if let Err(error) = self
            .ensure_instance_layout(source_id, mc_dir)
            .and_then(|_| {
                self.ensure_instance_layout(&instance.id, mc_dir)?;
                copy_instance_files(&self.game_dir(source_id), &self.game_dir(&instance.id))
            })
        {
            let mut inner = self.inner.write().map_err(|_| {
                InstanceStoreError::Read(std::io::Error::other("instance store lock poisoned"))
            })?;
            inner.instances.retain(|stored| stored.id != instance.id);
            let rollback_result = self.persist_locked(&inner);
            let _ = fs::remove_dir_all(self.paths.instances_dir.join(&instance.id));
            return Err(match rollback_result {
                Ok(()) => InstanceStoreError::Read(error),
                Err(rollback_error) => InstanceStoreError::Read(std::io::Error::other(format!(
                    "failed to duplicate instance files: {error}; failed to roll back persisted instance: {rollback_error}"
                ))),
            });
        }

        Ok(instance)
    }

    pub fn ensure_instance_layout(
        &self,
        instance_id: &str,
        mc_dir: Option<&Path>,
    ) -> Result<(), std::io::Error> {
        let game_dir = self.paths.instances_dir.join(instance_id);
        for subdir in [
            "mods",
            "saves",
            "resourcepacks",
            "shaderpacks",
            "config",
            "screenshots",
            "logs",
        ] {
            fs::create_dir_all(game_dir.join(subdir))?;
        }

        if let Some(mc_dir) = mc_dir {
            for file_name in ["options.txt", "servers.dat"] {
                copy_shared_file_if_missing(mc_dir, &game_dir, file_name)?;
            }
        }

        Ok(())
    }

    fn persist_locked(&self, inner: &StoredInstances) -> Result<(), InstanceStoreError> {
        fs::create_dir_all(&self.paths.config_dir)?;
        let data = serde_json::to_string_pretty(inner)?;
        let temp_path = self.paths.instances_file.with_extension("json.tmp");
        fs::write(&temp_path, data)?;
        promote_replacement(&temp_path, &self.paths.instances_file)?;
        Ok(())
    }
}

fn promote_replacement(source: &Path, destination: &Path) -> Result<(), std::io::Error> {
    let first_error = match fs::rename(source, destination) {
        Ok(()) => return Ok(()),
        Err(error) => error,
    };

    match fs::symlink_metadata(source) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Err(first_error),
        Err(error) => return Err(error),
    }

    if destination.exists() && !destination.is_dir() {
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

fn count_entries(path: &Path) -> usize {
    fs::read_dir(path)
        .map(|entries| entries.filter_map(Result::ok).count())
        .unwrap_or(0)
}

fn duplicate_name_for(source_name: &str, instances: &[Instance]) -> String {
    let base = format!("{source_name} copy");
    if !instances.iter().any(|instance| instance.name == base) {
        return base;
    }
    for index in 2.. {
        let candidate = format!("{base} {index}");
        if !instances.iter().any(|instance| instance.name == candidate) {
            return candidate;
        }
    }
    unreachable!("unbounded duplicate name search should always return");
}

fn copy_instance_files(source_dir: &Path, target_dir: &Path) -> io::Result<()> {
    for dir_name in ["mods", "saves", "resourcepacks", "shaderpacks", "config"] {
        copy_dir_contents(&source_dir.join(dir_name), &target_dir.join(dir_name))?;
    }
    for file_name in ["options.txt", "servers.dat"] {
        let source = source_dir.join(file_name);
        if source.is_file() {
            fs::copy(source, target_dir.join(file_name))?;
        }
    }
    Ok(())
}

fn copy_dir_contents(source_dir: &Path, target_dir: &Path) -> io::Result<()> {
    if !source_dir.is_dir() {
        return Ok(());
    }
    fs::create_dir_all(target_dir)?;
    for entry in fs::read_dir(source_dir)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let target = target_dir.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_contents(&entry.path(), &target)?;
        } else if file_type.is_file() {
            fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

fn copy_shared_file_if_missing(
    source_dir: &Path,
    target_dir: &Path,
    file_name: &str,
) -> io::Result<()> {
    let source = source_dir.join(file_name);
    if !source.is_file() {
        return Ok(());
    }

    let target = target_dir.join(file_name);
    let mut input = fs::File::open(source)?;
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&target)
    {
        Ok(mut output) => {
            io::copy(&mut input, &mut output)?;
            Ok(())
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error),
    }
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
    h
}

#[cfg(test)]
mod tests {
    use super::{InstanceStore, StoredInstances};
    use crate::paths::AppPaths;
    use std::path::{Path, PathBuf};
    use std::sync::RwLock;
    use std::{fs, io};

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
    fn load_for_startup_uses_empty_store_and_warning_for_malformed_registry_without_rewriting() {
        let root = test_root("startup-malformed-registry");
        let paths = test_paths(&root);
        fs::create_dir_all(&paths.config_dir).expect("create config dir");
        let malformed = "{not valid json";
        fs::write(&paths.instances_file, malformed).expect("write malformed registry");

        let startup = InstanceStore::load_for_startup(paths.clone());

        assert!(startup.store.list().is_empty());
        assert_eq!(
            startup.warnings,
            vec![super::INSTANCE_REGISTRY_STARTUP_WARNING.to_string()]
        );
        assert_eq!(
            fs::read_to_string(&paths.instances_file).expect("read registry"),
            malformed
        );
        assert!(matches!(
            InstanceStore::load_from(paths.clone()),
            Err(super::InstanceStoreError::Parse(_))
        ));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn load_for_startup_uses_empty_store_without_warning_when_registry_is_missing() {
        let root = test_root("startup-missing-registry");
        let paths = test_paths(&root);

        let startup = InstanceStore::load_for_startup(paths.clone());

        assert!(startup.store.list().is_empty());
        assert!(startup.warnings.is_empty());
        assert!(!paths.instances_file.exists());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn load_for_startup_uses_empty_store_and_warning_for_registry_read_error() {
        let root = test_root("startup-registry-read-error");
        let paths = test_paths(&root);
        fs::create_dir_all(&paths.instances_file).expect("create registry path as directory");

        let startup = InstanceStore::load_for_startup(paths.clone());

        assert!(startup.store.list().is_empty());
        assert_eq!(
            startup.warnings,
            vec![super::INSTANCE_REGISTRY_STARTUP_WARNING.to_string()]
        );
        assert!(matches!(
            InstanceStore::load_from(paths.clone()),
            Err(super::InstanceStoreError::Read(_))
        ));

        let _ = fs::remove_dir_all(root);
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

    #[test]
    fn add_and_ensure_create_layout_and_copy_shared_files_without_overwrite() {
        let root = test_root("layout-ensure");
        let paths = test_paths(&root);
        let mc_dir = root.join("library");
        fs::create_dir_all(&mc_dir).expect("create shared library dir");
        fs::write(mc_dir.join("options.txt"), "shared options").expect("write options");
        fs::write(mc_dir.join("servers.dat"), "shared servers").expect("write servers");

        let store = InstanceStore {
            paths: paths.clone(),
            inner: RwLock::new(StoredInstances::default()),
        };

        let instance = store
            .add(
                "Test".to_string(),
                "1.21.1".to_string(),
                String::new(),
                String::new(),
                Some(&mc_dir),
            )
            .expect("add instance");
        let game_dir = store.game_dir(&instance.id);

        for subdir in [
            "mods",
            "saves",
            "resourcepacks",
            "shaderpacks",
            "config",
            "screenshots",
            "logs",
        ] {
            assert!(game_dir.join(subdir).is_dir(), "{subdir} should exist");
        }
        assert_eq!(
            fs::read_to_string(game_dir.join("options.txt")).expect("read copied options"),
            "shared options"
        );
        assert_eq!(
            fs::read_to_string(game_dir.join("servers.dat")).expect("read copied servers"),
            "shared servers"
        );

        fs::write(game_dir.join("options.txt"), "local options").expect("write local options");
        fs::write(game_dir.join("servers.dat"), "local servers").expect("write local servers");
        fs::write(mc_dir.join("options.txt"), "changed options").expect("change shared options");
        fs::write(mc_dir.join("servers.dat"), "changed servers").expect("change shared servers");

        store
            .ensure_instance_layout(&instance.id, Some(&mc_dir))
            .expect("ensure layout");

        assert_eq!(
            fs::read_to_string(game_dir.join("options.txt")).expect("read local options"),
            "local options"
        );
        assert_eq!(
            fs::read_to_string(game_dir.join("servers.dat")).expect("read local servers"),
            "local servers"
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn update_allows_unchanged_name_but_rejects_exact_name_collision() {
        let root = test_root("update-name-collision");
        let paths = test_paths(&root);
        let store = InstanceStore {
            paths,
            inner: RwLock::new(StoredInstances::default()),
        };
        let alpha = store
            .add(
                "Alpha".to_string(),
                "1.21.1".to_string(),
                String::new(),
                String::new(),
                None,
            )
            .expect("add alpha");
        let beta = store
            .add(
                "Beta".to_string(),
                "1.21.1".to_string(),
                String::new(),
                String::new(),
                None,
            )
            .expect("add beta");

        let mut unchanged = alpha.clone();
        unchanged.version_id = "1.21.2".to_string();
        let updated = store.update(unchanged).expect("update unchanged name");
        assert_eq!(updated.name, "Alpha");
        assert_eq!(updated.version_id, "1.21.2");

        let mut colliding = updated.clone();
        colliding.name = beta.name.clone();
        let error = store.update(colliding).expect_err("reject duplicate name");

        assert!(matches!(error, super::InstanceStoreError::Read(_)));
        let super::InstanceStoreError::Read(error) = error else {
            panic!("expected read error");
        };
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(
            store.get(&alpha.id).expect("alpha remains").name,
            "Alpha".to_string()
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn update_restores_in_memory_instance_when_persist_fails() {
        let root = test_root("update-persist-failure");
        let paths = test_paths(&root);
        let store = InstanceStore {
            paths: paths.clone(),
            inner: RwLock::new(StoredInstances::default()),
        };
        let previous = store
            .add(
                "Stable".to_string(),
                "1.21.1".to_string(),
                String::new(),
                String::new(),
                None,
            )
            .expect("add instance");
        fs::remove_dir_all(&paths.config_dir).expect("remove config dir");
        fs::write(&paths.config_dir, "not a dir").expect("block config dir");

        let mut next = previous.clone();
        next.performance_mode = "managed".to_string();
        let error = store.update(next).expect_err("persist should fail");

        assert!(matches!(error, super::InstanceStoreError::Read(_)));
        assert_eq!(store.get(&previous.id).expect("instance remains"), previous);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn remove_restores_in_memory_instance_and_files_when_persist_fails() {
        let root = test_root("remove-persist-failure");
        let paths = test_paths(&root);
        let store = InstanceStore {
            paths: paths.clone(),
            inner: RwLock::new(StoredInstances::default()),
        };
        let instance = store
            .add(
                "Keep on failure".to_string(),
                "1.21.1".to_string(),
                String::new(),
                String::new(),
                None,
            )
            .expect("add instance");
        store
            .set_last_instance_id(instance.id.clone())
            .expect("set last instance");
        let marker = store
            .game_dir(&instance.id)
            .join("mods")
            .join("example.jar");
        fs::write(&marker, "mod").expect("write marker");
        fs::remove_dir_all(&paths.config_dir).expect("remove config dir");
        fs::write(&paths.config_dir, "not a dir").expect("block config dir");

        let error = store
            .remove(&instance.id, true)
            .expect_err("persist should fail");

        assert!(matches!(error, super::InstanceStoreError::Read(_)));
        assert_eq!(store.get(&instance.id), Some(instance.clone()));
        assert_eq!(
            store.last_instance_id().as_deref(),
            Some(instance.id.as_str())
        );
        assert!(marker.exists());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn clear_restores_in_memory_instances_when_persist_fails() {
        let root = test_root("clear-persist-failure");
        let paths = test_paths(&root);
        let store = InstanceStore {
            paths: paths.clone(),
            inner: RwLock::new(StoredInstances::default()),
        };
        let instance = store
            .add(
                "Clear rollback".to_string(),
                "1.21.1".to_string(),
                String::new(),
                String::new(),
                None,
            )
            .expect("add instance");
        store
            .set_last_instance_id(instance.id.clone())
            .expect("set last instance");
        fs::remove_dir_all(&paths.config_dir).expect("remove config dir");
        fs::write(&paths.config_dir, "not a dir").expect("block config dir");

        let error = store.clear().expect_err("persist should fail");

        assert!(matches!(error, super::InstanceStoreError::Read(_)));
        assert_eq!(store.get(&instance.id), Some(instance.clone()));
        assert_eq!(
            store.last_instance_id().as_deref(),
            Some(instance.id.as_str())
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn set_last_instance_id_restores_previous_value_when_persist_fails() {
        let root = test_root("last-instance-persist-failure");
        let paths = test_paths(&root);
        let store = InstanceStore {
            paths: paths.clone(),
            inner: RwLock::new(StoredInstances::default()),
        };
        let first = store
            .add(
                "First".to_string(),
                "1.21.1".to_string(),
                String::new(),
                String::new(),
                None,
            )
            .expect("add first");
        let second = store
            .add(
                "Second".to_string(),
                "1.21.1".to_string(),
                String::new(),
                String::new(),
                None,
            )
            .expect("add second");
        store
            .set_last_instance_id(first.id.clone())
            .expect("set first as last");
        fs::remove_dir_all(&paths.config_dir).expect("remove config dir");
        fs::write(&paths.config_dir, "not a dir").expect("block config dir");

        let error = store
            .set_last_instance_id(second.id.clone())
            .expect_err("persist should fail");

        assert!(matches!(error, super::InstanceStoreError::Read(_)));
        assert_eq!(store.last_instance_id().as_deref(), Some(first.id.as_str()));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn promote_replacement_replaces_existing_registry_file() {
        let root = test_root("promote-replacement-existing");
        let paths = test_paths(&root);
        fs::create_dir_all(&paths.config_dir).expect("create config dir");
        let temp_path = paths.instances_file.with_extension("json.tmp");
        fs::write(&paths.instances_file, "old registry").expect("write existing registry");
        fs::write(&temp_path, "new registry").expect("write temp registry");

        super::promote_replacement(&temp_path, &paths.instances_file).expect("promote replacement");

        assert_eq!(
            fs::read_to_string(&paths.instances_file).expect("read promoted registry"),
            "new registry"
        );
        assert!(!temp_path.exists());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn promote_replacement_preserves_registry_when_temp_is_missing() {
        let root = test_root("promote-replacement-missing-temp");
        let paths = test_paths(&root);
        fs::create_dir_all(&paths.config_dir).expect("create config dir");
        let temp_path = paths.instances_file.with_extension("json.tmp");
        fs::write(&paths.instances_file, "old registry").expect("write existing registry");

        let error = super::promote_replacement(&temp_path, &paths.instances_file)
            .expect_err("missing temp should fail");

        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
        assert_eq!(
            fs::read_to_string(&paths.instances_file).expect("read old registry"),
            "old registry"
        );
        assert!(!temp_path.exists());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn promote_replacement_preserves_directory_destination_on_retry_failure() {
        let root = test_root("promote-replacement-directory");
        let paths = test_paths(&root);
        fs::create_dir_all(&paths.instances_file).expect("create registry path as directory");
        let temp_path = paths.instances_file.with_extension("json.tmp");
        fs::write(&temp_path, "new registry").expect("write temp registry");

        super::promote_replacement(&temp_path, &paths.instances_file)
            .expect_err("directory destination should fail");

        assert!(paths.instances_file.is_dir());
        assert!(!temp_path.exists());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn duplicate_copies_settings_and_gameplay_files_without_runtime_history() {
        let root = test_root("duplicate");
        let paths = test_paths(&root);
        let mc_dir = root.join("library");
        fs::create_dir_all(&mc_dir).expect("create shared library dir");
        let store = InstanceStore {
            paths,
            inner: RwLock::new(StoredInstances::default()),
        };

        let mut source = store
            .add(
                "Survival".to_string(),
                "1.21.1".to_string(),
                "grass".to_string(),
                "green".to_string(),
                Some(&mc_dir),
            )
            .expect("add source");
        source.max_memory_mb = 6144;
        source.min_memory_mb = 2048;
        source.java_path = "/usr/bin/java".to_string();
        source.window_width = 1280;
        source.window_height = 720;
        source.jvm_preset = "balanced".to_string();
        source.performance_mode = "managed".to_string();
        source.extra_jvm_args = "-Ddemo=true".to_string();
        source.last_played_at = "2026-05-30T00:00:00Z".to_string();
        store.update(source.clone()).expect("update source");

        let source_dir = store.game_dir(&source.id);
        fs::create_dir_all(source_dir.join("mods")).expect("create mods");
        fs::create_dir_all(source_dir.join("saves").join("World")).expect("create world");
        fs::create_dir_all(source_dir.join("config").join("nested")).expect("create config");
        fs::write(source_dir.join("mods").join("sodium.jar"), "mod").expect("write mod");
        fs::write(
            source_dir.join("saves").join("World").join("level.dat"),
            "world",
        )
        .expect("write world");
        fs::write(
            source_dir.join("config").join("nested").join("options.cfg"),
            "config",
        )
        .expect("write config");
        fs::write(source_dir.join("options.txt"), "local options").expect("write options");
        fs::write(source_dir.join("servers.dat"), "local servers").expect("write servers");
        fs::write(source_dir.join("logs").join("latest.log"), "log").expect("write log");

        let copy = store
            .duplicate(&source.id, None, Some(&mc_dir))
            .expect("duplicate source");
        let copy_dir = store.game_dir(&copy.id);

        assert_eq!(copy.name, "Survival copy");
        assert_eq!(copy.version_id, source.version_id);
        assert_eq!(copy.max_memory_mb, source.max_memory_mb);
        assert_eq!(copy.min_memory_mb, source.min_memory_mb);
        assert_eq!(copy.java_path, source.java_path);
        assert_eq!(copy.window_width, source.window_width);
        assert_eq!(copy.window_height, source.window_height);
        assert_eq!(copy.jvm_preset, source.jvm_preset);
        assert_eq!(copy.performance_mode, source.performance_mode);
        assert_eq!(copy.extra_jvm_args, source.extra_jvm_args);
        assert_eq!(copy.icon, source.icon);
        assert_eq!(copy.accent, source.accent);
        assert!(copy.last_played_at.is_empty());
        assert_eq!(
            fs::read_to_string(copy_dir.join("mods").join("sodium.jar")).expect("read mod"),
            "mod"
        );
        assert_eq!(
            fs::read_to_string(copy_dir.join("saves").join("World").join("level.dat"))
                .expect("read world"),
            "world"
        );
        assert_eq!(
            fs::read_to_string(copy_dir.join("config").join("nested").join("options.cfg"))
                .expect("read config"),
            "config"
        );
        assert_eq!(
            fs::read_to_string(copy_dir.join("options.txt")).expect("read options"),
            "local options"
        );
        assert_eq!(
            fs::read_to_string(copy_dir.join("servers.dat")).expect("read servers"),
            "local servers"
        );
        assert!(!copy_dir.join("logs").join("latest.log").exists());

        let second = store
            .duplicate(&source.id, None, Some(&mc_dir))
            .expect("duplicate source again");
        assert_eq!(second.name, "Survival copy 2");

        let _ = fs::remove_dir_all(root);
    }
}
