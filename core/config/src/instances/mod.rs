use crate::paths::AppPaths;
use axial_minecraft::{LoaderComponentId, VersionEntry};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io;
use std::ops::Deref;
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
    pub auto_optimize: bool,
    #[serde(default)]
    pub icon: String,
    #[serde(default)]
    pub accent: String,
    /// Recorded at creation so the instance can describe itself before its
    /// version profile lands on disk. Empty on instances created before this
    /// was tracked, which fall back to the installed version entry.
    #[serde(default)]
    pub loader_key: String,
    #[serde(default)]
    pub minecraft_version: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct EnrichedInstance {
    #[serde(flatten)]
    pub instance: Instance,
    pub version_display: InstanceVersionDisplay,
    pub launchable: bool,
    pub launch_action: LaunchActionState,
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

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct InstanceVersionDisplay {
    pub loader_key: String,
    pub loader_label: String,
    pub minecraft_label: String,
    #[serde(default)]
    pub loader_version_label: String,
    pub loader_detail_label: String,
    pub summary_label: String,
    pub supports_mods: bool,
}

impl InstanceVersionDisplay {
    /// What the instance is, as opposed to what has finished downloading.
    ///
    /// The installed version entry is the better witness for anything on disk
    /// (the exact loader build, its display tags). But it is a scan artifact: it
    /// is absent before the download starts and can be missing its loader
    /// attachment while one is in flight. The loader and Minecraft version
    /// recorded at creation are immutable and always true, so they fill any gap
    /// the entry leaves — otherwise a brand new modded instance briefly claims to
    /// be vanilla, and content installed into it during that window is refused.
    fn from_version(version: Option<&VersionEntry>, declared: &Instance) -> Self {
        let loader = version
            .and_then(|entry| entry.loader.as_ref())
            .map(|loader| loader.component_id)
            .or_else(|| LoaderComponentId::parse(declared.loader_key.trim()));
        let loader_key = loader
            .map(|id| id.short_key().to_string())
            .unwrap_or_else(|| "vanilla".to_string());
        let loader_label = loader
            .map(|id| id.display_name().to_string())
            .unwrap_or_else(|| "Vanilla".to_string());
        let minecraft_label = version
            .map(minecraft_label_for_version)
            .filter(|value| !value.trim().is_empty())
            .or_else(|| {
                Some(declared.minecraft_version.trim())
                    .filter(|value| !value.is_empty())
                    .map(str::to_string)
            })
            .unwrap_or_else(|| "Unknown".to_string());
        let loader_version_label = version
            .and_then(|entry| entry.loader.as_ref())
            .map(|loader| {
                loader_version_label(&loader.loader_version, &loader.build_meta.display_tags)
            })
            .unwrap_or_default();
        let loader_detail_label = if loader_version_label.trim().is_empty() {
            loader_label.clone()
        } else {
            format!("{loader_label} · {loader_version_label}")
        };
        let supports_mods = loader.is_some();

        Self {
            summary_label: format!("{loader_label} · {minecraft_label}"),
            loader_key,
            loader_label,
            minecraft_label,
            loader_version_label,
            loader_detail_label,
            supports_mods,
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LaunchActionTone {
    Ok,
    Warn,
    Err,
    Mute,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LaunchPrimaryAction {
    Launch,
    Install,
    Blocked,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct LaunchActionState {
    pub state_id: String,
    pub label: String,
    pub tone: LaunchActionTone,
    pub launchable: bool,
    pub primary_action: LaunchPrimaryAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled_reason: Option<String>,
}

impl LaunchActionState {
    pub fn launch_ready() -> Self {
        Self {
            state_id: "launch_ready".to_string(),
            label: "Launch".to_string(),
            tone: LaunchActionTone::Ok,
            launchable: true,
            primary_action: LaunchPrimaryAction::Launch,
            disabled_reason: None,
        }
    }

    pub fn install_required(reason: impl Into<String>) -> Self {
        let reason = reason.into();
        Self {
            state_id: "install_required".to_string(),
            label: "Install".to_string(),
            tone: LaunchActionTone::Warn,
            launchable: false,
            primary_action: LaunchPrimaryAction::Install,
            disabled_reason: Some(reason),
        }
    }

    pub fn repair_required(reason: impl Into<String>) -> Self {
        let reason = reason.into();
        Self {
            state_id: "repair_required".to_string(),
            label: "Repair".to_string(),
            tone: LaunchActionTone::Err,
            launchable: false,
            primary_action: LaunchPrimaryAction::Install,
            disabled_reason: Some(reason),
        }
    }

    pub fn blocked(reason: impl Into<String>) -> Self {
        let reason = reason.into();
        Self {
            state_id: "launch_blocked".to_string(),
            label: "Blocked".to_string(),
            tone: LaunchActionTone::Err,
            launchable: false,
            primary_action: LaunchPrimaryAction::Blocked,
            disabled_reason: Some(reason),
        }
    }

    fn from_readiness(launchable: bool, status_detail: &str, needs_install: &str) -> Self {
        if launchable {
            return Self::launch_ready();
        }

        let disabled_reason = [status_detail, needs_install]
            .into_iter()
            .map(str::trim)
            .find(|value| !value.is_empty())
            .unwrap_or("Version files are not ready.")
            .to_string();

        Self::install_required(disabled_reason)
    }
}

impl EnrichedInstance {
    pub fn from_instance(
        instance: Instance,
        version: Option<&VersionEntry>,
        game_dir: &Path,
    ) -> Self {
        Self::from_instance_with_counts(
            instance,
            version,
            ResourceCounts {
                saves: count_entries(&game_dir.join("saves")),
                mods: count_entries(&game_dir.join("mods")),
                resourcepacks: count_entries(&game_dir.join("resourcepacks")),
                shaderpacks: count_entries(&game_dir.join("shaderpacks")),
            },
        )
    }

    pub fn from_instance_without_resource_counts(
        instance: Instance,
        version: Option<&VersionEntry>,
    ) -> Self {
        Self::from_instance_with_counts(instance, version, ResourceCounts::default())
    }

    fn from_instance_with_counts(
        instance: Instance,
        version: Option<&VersionEntry>,
        counts: ResourceCounts,
    ) -> Self {
        let launchable = version.is_some_and(|entry| entry.launchable);
        let status_detail = version
            .map(|entry| entry.status_detail.clone())
            .unwrap_or_else(|| "version not installed".to_string());
        let needs_install = version
            .map(|entry| entry.needs_install.clone())
            .unwrap_or_default();

        Self {
            version_display: InstanceVersionDisplay::from_version(version, &instance),
            launch_action: LaunchActionState::from_readiness(
                launchable,
                &status_detail,
                &needs_install,
            ),
            launchable,
            status_detail,
            needs_install,
            java_major: version.map(|entry| entry.java_major).unwrap_or_default(),
            saves_count: counts.saves,
            mods_count: counts.mods,
            resource_count: counts.resourcepacks,
            shader_count: counts.shaderpacks,
            instance,
        }
    }
}

fn minecraft_label_for_version(version: &VersionEntry) -> String {
    let inherited = version.inherits_from.trim();
    if !inherited.is_empty() {
        return inherited.to_string();
    }

    [
        version.minecraft_meta.effective_version.as_str(),
        version.minecraft_meta.base_id.as_str(),
        version.minecraft_meta.display_name.as_str(),
        version.minecraft_meta.display_hint.as_str(),
        version.id.as_str(),
    ]
    .into_iter()
    .map(str::trim)
    .find(|value| !value.is_empty())
    .unwrap_or_default()
    .to_string()
}

fn loader_version_label(loader_version: &str, display_tags: &[String]) -> String {
    let loader_version = loader_version.trim();
    if loader_version.is_empty() {
        return String::new();
    }
    if display_tags.is_empty() {
        return loader_version.to_string();
    }
    format!("{} ({})", loader_version, display_tags.join(", "))
}

#[derive(Default)]
struct ResourceCounts {
    saves: usize,
    mods: usize,
    resourcepacks: usize,
    shaderpacks: usize,
}

impl Deref for EnrichedInstance {
    type Target = Instance;

    fn deref(&self) -> &Self::Target {
        &self.instance
    }
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

const INSTANCE_REGISTRY_STARTUP_WARNING: &str = "Axial could not load the instance list, so it started with an empty list. Check app data permissions or restore the instance registry.";

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

    fn from_inner(paths: AppPaths, mut inner: StoredInstances) -> Self {
        normalize_last_instance_id(&mut inner);
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
            if !inner.last_instance_id.is_empty()
                && inner
                    .instances
                    .iter()
                    .any(|instance| instance.id == inner.last_instance_id)
            {
                Some(inner.last_instance_id.clone())
            } else {
                None
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

                EnrichedInstance::from_instance(instance, version, &game_dir)
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
        let id = id.into();
        if !id.is_empty() && !inner.instances.iter().any(|instance| instance.id == id) {
            return Err(InstanceStoreError::Read(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "instance not found",
            )));
        }
        let previous = inner.last_instance_id.clone();
        inner.last_instance_id = id;
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
            auto_optimize: false,
            icon,
            accent,
            loader_key: String::new(),
            minecraft_version: String::new(),
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
            auto_optimize: source.auto_optimize,
            icon: source.icon.clone(),
            accent: source.accent.clone(),
            loader_key: source.loader_key.clone(),
            minecraft_version: source.minecraft_version.clone(),
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

fn normalize_last_instance_id(inner: &mut StoredInstances) {
    if inner.last_instance_id.is_empty() {
        return;
    }
    if !inner
        .instances
        .iter()
        .any(|instance| instance.id == inner.last_instance_id)
    {
        inner.last_instance_id.clear();
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
    use super::{EnrichedInstance, Instance, InstanceStore, StoredInstances};
    use crate::paths::AppPaths;
    use axial_minecraft::{
        LifecycleMeta, LoaderBuildMetadata, LoaderComponentId, MinecraftVersionMeta, VersionEntry,
        VersionLoaderAttachment, VersionSubjectKind,
    };
    use std::path::{Path, PathBuf};
    use std::sync::RwLock;
    use std::{fs, io};

    fn test_root(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "axial-config-{name}-{}",
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

    fn stored_instance(id: &str) -> Instance {
        Instance {
            id: id.to_string(),
            name: format!("Instance {id}"),
            version_id: "1.21.1".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            last_played_at: String::new(),
            art_seed: 1,
            max_memory_mb: 0,
            min_memory_mb: 0,
            java_path: String::new(),
            window_width: 0,
            window_height: 0,
            jvm_preset: String::new(),
            performance_mode: String::new(),
            extra_jvm_args: String::new(),
            auto_optimize: false,
            icon: String::new(),
            accent: String::new(),
            loader_key: String::new(),
            minecraft_version: String::new(),
        }
    }

    fn stored_instance_json(id: &str) -> serde_json::Value {
        serde_json::to_value(stored_instance(id)).expect("serialize stored instance")
    }

    fn version_entry(id: &str) -> VersionEntry {
        VersionEntry {
            subject_kind: VersionSubjectKind::InstalledVersion,
            id: id.to_string(),
            raw_kind: String::new(),
            release_time: String::new(),
            minecraft_meta: MinecraftVersionMeta {
                effective_version: id.to_string(),
                base_id: id.to_string(),
                display_name: id.to_string(),
                ..MinecraftVersionMeta::default()
            },
            lifecycle: LifecycleMeta::default(),
            inherits_from: String::new(),
            launchable: true,
            installed: true,
            status: "installed".to_string(),
            status_detail: String::new(),
            needs_install: String::new(),
            java_component: String::new(),
            java_major: 21,
            manifest_url: String::new(),
            loader: None,
        }
    }

    #[test]
    fn enriched_instance_exposes_backend_authored_vanilla_version_display() {
        let instance = stored_instance("vanilla");
        let version = version_entry("1.21.1");

        let enriched =
            EnrichedInstance::from_instance_without_resource_counts(instance, Some(&version));

        assert_eq!(enriched.version_display.loader_label, "Vanilla");
        assert_eq!(enriched.version_display.loader_key, "vanilla");
        assert_eq!(enriched.version_display.minecraft_label, "1.21.1");
        assert_eq!(enriched.version_display.summary_label, "Vanilla · 1.21.1");
        assert_eq!(enriched.version_display.loader_version_label, "");
        assert_eq!(enriched.version_display.loader_detail_label, "Vanilla");
        assert!(!enriched.version_display.supports_mods);
    }

    #[test]
    fn enriched_instance_exposes_backend_authored_loader_version_display() {
        let mut instance = stored_instance("quilt");
        instance.version_id = "quilt-loader-0.30.0-beta.8-1.21.1".to_string();
        let mut version = version_entry(&instance.version_id);
        version.inherits_from = "1.21.1".to_string();
        version.loader = Some(VersionLoaderAttachment {
            component_id: LoaderComponentId::Quilt,
            component_name: "Quilt".to_string(),
            build_id: "org.quiltmc.quilt-loader:1.21.1:0.30.0-beta.8".to_string(),
            loader_version: "0.30.0-beta.8".to_string(),
            build_meta: LoaderBuildMetadata {
                display_tags: vec!["beta".to_string()],
                ..LoaderBuildMetadata::default()
            },
        });

        let enriched =
            EnrichedInstance::from_instance_without_resource_counts(instance, Some(&version));

        assert_eq!(enriched.version_display.loader_label, "Quilt");
        assert_eq!(enriched.version_display.loader_key, "quilt");
        assert_eq!(enriched.version_display.minecraft_label, "1.21.1");
        assert_eq!(enriched.version_display.summary_label, "Quilt · 1.21.1");
        assert_eq!(
            enriched.version_display.loader_version_label,
            "0.30.0-beta.8 (beta)"
        );
        assert_eq!(
            enriched.version_display.loader_detail_label,
            "Quilt · 0.30.0-beta.8 (beta)"
        );
        assert!(enriched.version_display.supports_mods);
    }

    #[test]
    fn enriched_instance_falls_back_to_declared_loader_while_the_version_downloads() {
        let mut instance = stored_instance("fresh");
        instance.version_id = "fabric-loader-0.17.2-1.21.6".to_string();
        instance.loader_key = "fabric".to_string();
        instance.minecraft_version = "1.21.6".to_string();

        let enriched = EnrichedInstance::from_instance_without_resource_counts(instance, None);

        assert_eq!(enriched.version_display.loader_key, "fabric");
        assert_eq!(enriched.version_display.loader_label, "Fabric");
        assert_eq!(enriched.version_display.minecraft_label, "1.21.6");
        assert!(enriched.version_display.supports_mods);
        assert!(!enriched.launchable);
    }

    #[test]
    fn enriched_instance_without_a_version_or_declaration_stays_unknown() {
        let enriched = EnrichedInstance::from_instance_without_resource_counts(
            stored_instance("legacy"),
            None,
        );

        assert_eq!(enriched.version_display.loader_key, "vanilla");
        assert_eq!(enriched.version_display.minecraft_label, "Unknown");
        assert!(!enriched.version_display.supports_mods);
    }

    /// A version entry that exists but has not had its loader attached yet is a
    /// half-scanned install, not a vanilla instance. Reading it as vanilla made a
    /// freshly created modded instance refuse mods for as long as its download
    /// took.
    #[test]
    fn a_half_installed_version_does_not_make_a_modded_instance_look_vanilla() {
        let mut instance = stored_instance("mid-install");
        instance.version_id = "fabric-loader-0.19.3-1.21.6".to_string();
        instance.loader_key = "fabric".to_string();
        instance.minecraft_version = "1.21.6".to_string();
        let mut version = version_entry(&instance.version_id);
        version.loader = None;

        let enriched =
            EnrichedInstance::from_instance_without_resource_counts(instance, Some(&version));

        assert_eq!(enriched.version_display.loader_key, "fabric");
        assert!(enriched.version_display.supports_mods);
    }

    /// The installed entry still wins where it actually knows more: the exact
    /// loader build behind the instance.
    #[test]
    fn the_installed_entry_supplies_the_loader_build_detail() {
        let mut instance = stored_instance("settled");
        instance.version_id = "quilt-loader-0.30.0-1.21.1".to_string();
        instance.loader_key = "quilt".to_string();
        instance.minecraft_version = "1.21.1".to_string();
        let mut version = version_entry(&instance.version_id);
        version.inherits_from = "1.21.1".to_string();
        version.loader = Some(VersionLoaderAttachment {
            component_id: LoaderComponentId::Quilt,
            component_name: "Quilt".to_string(),
            build_id: "org.quiltmc.quilt-loader:1.21.1:0.30.0".to_string(),
            loader_version: "0.30.0".to_string(),
            build_meta: LoaderBuildMetadata::default(),
        });

        let enriched =
            EnrichedInstance::from_instance_without_resource_counts(instance, Some(&version));

        assert_eq!(enriched.version_display.loader_key, "quilt");
        assert_eq!(enriched.version_display.loader_version_label, "0.30.0");
        assert_eq!(
            enriched.version_display.loader_detail_label,
            "Quilt · 0.30.0"
        );
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
    fn load_from_normalizes_stale_last_instance_id_without_rewriting_registry() {
        let root = test_root("normalize-stale-last-instance");
        let paths = test_paths(&root);
        fs::create_dir_all(&paths.config_dir).expect("create config dir");
        let stored = serde_json::json!({
            "instances": [stored_instance_json("kept")],
            "last_instance_id": "missing"
        });
        fs::write(
            &paths.instances_file,
            serde_json::to_vec_pretty(&stored).expect("serialize registry"),
        )
        .expect("write registry");

        let store = InstanceStore::load_from(paths.clone()).expect("load store");

        assert_eq!(store.last_instance_id(), None);
        let persisted = fs::read_to_string(&paths.instances_file).expect("read registry");
        assert!(
            persisted.contains("\"last_instance_id\": \"missing\""),
            "load normalization should not rewrite registry: {persisted}"
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn last_instance_id_returns_none_when_inner_value_is_stale() {
        let root = test_root("last-instance-accessor-stale");
        let paths = test_paths(&root);
        let store = InstanceStore {
            paths,
            inner: RwLock::new(StoredInstances {
                instances: vec![stored_instance("kept")],
                last_instance_id: "missing".to_string(),
            }),
        };

        assert_eq!(store.last_instance_id(), None);

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
