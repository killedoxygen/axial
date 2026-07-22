use crate::paths::AppPaths;
use crate::store::StartupFileProvenance;
use crate::AppRootSession;
use axial_fs::{Directory, LeafName};
use axial_minecraft::{LoaderComponentId, VersionEntry};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::ops::Deref;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use thiserror::Error;

pub const INSTANCE_LAYOUT_DIRS: [&str; 7] = [
    "mods",
    "saves",
    "resourcepacks",
    "shaderpacks",
    "config",
    "screenshots",
    "logs",
];
pub const SHARED_INSTANCE_FILES: [&str; 2] = ["options.txt", "servers.dat"];

/// `art_seed` is the source of truth for the instance identity tile: the
/// frontend derives the tile hues from it, and "shuffle" rewrites it.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Instance {
    pub id: String,
    pub name: String,
    pub version_id: String,
    pub created_at: String,
    pub last_played_at: String,
    pub art_seed: u32,
    pub max_memory_mb: i32,
    pub min_memory_mb: i32,
    pub java_path: String,
    pub window_width: i32,
    pub window_height: i32,
    pub jvm_preset: String,
    pub performance_mode: String,
    pub extra_jvm_args: String,
    pub auto_optimize: bool,
    pub icon: String,
    pub accent: String,
    pub loader_key: String,
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
    fn from_version(version: Option<&VersionEntry>, declared: &Instance) -> Self {
        let loader = version
            .and_then(|entry| entry.loader.as_ref())
            .map(|loader| loader.component_id)
            .or_else(|| loader_component_from_short_key(declared.loader_key.trim()));
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
                let declared = declared.minecraft_version.trim();
                (!declared.is_empty()).then(|| declared.to_string())
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
            format!("{loader_label} {loader_version_label}")
        };
        let supports_mods = loader.is_some();

        Self {
            summary_label: format!("{loader_label} {minecraft_label}"),
            loader_key,
            loader_label,
            minecraft_label,
            loader_version_label,
            loader_detail_label,
            supports_mods,
        }
    }
}

fn loader_component_from_short_key(loader_key: &str) -> Option<LoaderComponentId> {
    match loader_key {
        "fabric" => Some(LoaderComponentId::Fabric),
        "quilt" => Some(LoaderComponentId::Quilt),
        "forge" => Some(LoaderComponentId::Forge),
        "neoforge" => Some(LoaderComponentId::NeoForge),
        _ => None,
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
    pub fn from_instance_without_resource_counts(
        instance: Instance,
        version: Option<&VersionEntry>,
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
            saves_count: 0,
            mods_count: 0,
            resource_count: 0,
            shader_count: 0,
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

impl Deref for EnrichedInstance {
    type Target = Instance;

    fn deref(&self) -> &Self::Target {
        &self.instance
    }
}

pub const INSTANCE_REGISTRY_SCHEMA_VERSION: u32 = 3;
pub const INSTANCE_REGISTRY_MAX_BYTES: u64 = 1024 * 1024;
pub const INSTANCE_REGISTRY_MAX_ENTRIES: usize = 1024;
const INSTANCE_TOMBSTONE_NAME_PREFIX: &str = ".axial-instance-tombstone-v1-";
const INSTANCE_TOMBSTONE_HASH_DOMAIN: &[u8] = b"axial.instance-tombstone.v1";
const INSTANCE_NAME_MAX_CHARS: usize = 128;
const INSTANCE_VERSION_ID_MAX_CHARS: usize = 256;
const INSTANCE_TIMESTAMP_MAX_CHARS: usize = 64;
const INSTANCE_JAVA_PATH_MAX_CHARS: usize = 4096;
const INSTANCE_JVM_ARGS_MAX_CHARS: usize = 8192;
const INSTANCE_TOKEN_MAX_CHARS: usize = 128;
const INSTANCE_MEMORY_MAX_MB: i32 = 1024 * 1024;
const INSTANCE_WINDOW_MAX_PIXELS: i32 = 16_384;
const INSTANCE_REGISTRY_STARTUP_WARNING: &str = "Axial could not load the instance list, so it started with an empty list. Check app data permissions or restore the instance registry.";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PendingInstanceDeletion {
    pub instance_id: String,
    pub created_at: String,
    pub tombstone_name: String,
}

impl PendingInstanceDeletion {
    pub fn new(
        instance_id: impl Into<String>,
        created_at: impl Into<String>,
    ) -> Result<Self, InstanceStoreError> {
        let instance_id = instance_id.into();
        let created_at = created_at.into();
        let tombstone_name = derive_instance_tombstone_name(&instance_id, &created_at)?;
        Ok(Self {
            instance_id,
            created_at,
            tombstone_name,
        })
    }

    fn validate(&self) -> Result<(), InstanceStoreError> {
        let expected = derive_instance_tombstone_name(&self.instance_id, &self.created_at)?;
        if self.tombstone_name != expected {
            return Err(InstanceStoreError::Validation(
                "instance registry pending deletion tombstone name is invalid",
            ));
        }
        Ok(())
    }
}

pub fn derive_instance_tombstone_name(
    instance_id: &str,
    created_at: &str,
) -> Result<String, InstanceStoreError> {
    if !is_canonical_instance_id(instance_id) {
        return Err(InstanceStoreError::Validation(
            "instance registry pending deletion id is invalid",
        ));
    }
    if !is_valid_timestamp(created_at, false) {
        return Err(InstanceStoreError::Validation(
            "instance registry pending deletion timestamp is invalid",
        ));
    }

    let mut hasher = Sha256::new();
    hasher.update(INSTANCE_TOMBSTONE_HASH_DOMAIN);
    hasher.update([0]);
    hasher.update(instance_id.as_bytes());
    hasher.update([0]);
    hasher.update(created_at.as_bytes());

    const LOWER_HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = hasher.finalize();
    let mut name = String::with_capacity(
        INSTANCE_TOMBSTONE_NAME_PREFIX.len() + instance_id.len() + 1 + digest.len() * 2,
    );
    name.push_str(INSTANCE_TOMBSTONE_NAME_PREFIX);
    name.push_str(instance_id);
    name.push('-');
    for byte in digest {
        name.push(char::from(LOWER_HEX[usize::from(byte >> 4)]));
        name.push(char::from(LOWER_HEX[usize::from(byte & 0x0f)]));
    }
    Ok(name)
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct InstanceRegistrySnapshot {
    pub schema_version: u32,
    pub instances: Vec<Instance>,
    pub last_instance_id: String,
    pub pending_deletions: Vec<PendingInstanceDeletion>,
}

impl Default for InstanceRegistrySnapshot {
    fn default() -> Self {
        Self {
            schema_version: INSTANCE_REGISTRY_SCHEMA_VERSION,
            instances: Vec::new(),
            last_instance_id: String::new(),
            pending_deletions: Vec::new(),
        }
    }
}

impl InstanceRegistrySnapshot {
    pub fn new(
        instances: Vec<Instance>,
        last_instance_id: String,
        pending_deletions: Vec<PendingInstanceDeletion>,
    ) -> Result<Self, InstanceStoreError> {
        let snapshot = Self {
            schema_version: INSTANCE_REGISTRY_SCHEMA_VERSION,
            instances,
            last_instance_id,
            pending_deletions,
        };
        snapshot.validate()?;
        Ok(snapshot)
    }

    pub fn validate(&self) -> Result<(), InstanceStoreError> {
        if self.schema_version != INSTANCE_REGISTRY_SCHEMA_VERSION {
            return Err(InstanceStoreError::Validation(
                "unsupported instance registry schema version",
            ));
        }
        if self
            .instances
            .len()
            .checked_add(self.pending_deletions.len())
            .is_none_or(|total| total > INSTANCE_REGISTRY_MAX_ENTRIES)
        {
            return Err(InstanceStoreError::Validation(
                "instance registry contains too many ownership records",
            ));
        }

        let mut ids = HashSet::with_capacity(self.instances.len());
        let mut names = HashSet::with_capacity(self.instances.len());
        for instance in &self.instances {
            validate_instance(instance)?;
            if !ids.insert(instance.id.as_str()) {
                return Err(InstanceStoreError::Validation(
                    "instance registry contains duplicate instance ids",
                ));
            }
            if !names.insert(instance.name.as_str()) {
                return Err(InstanceStoreError::Validation(
                    "instance registry contains duplicate instance names",
                ));
            }
        }

        if !self.last_instance_id.is_empty()
            && (!is_canonical_instance_id(&self.last_instance_id)
                || !ids.contains(self.last_instance_id.as_str()))
        {
            return Err(InstanceStoreError::Validation(
                "instance registry last instance id is invalid",
            ));
        }

        let mut pending_ids = HashSet::with_capacity(self.pending_deletions.len());
        let mut tombstone_names = HashSet::with_capacity(self.pending_deletions.len());
        for pending in &self.pending_deletions {
            pending.validate()?;
            if ids.contains(pending.instance_id.as_str()) {
                return Err(InstanceStoreError::Validation(
                    "instance registry pending deletion is still live",
                ));
            }
            if !pending_ids.insert(pending.instance_id.as_str()) {
                return Err(InstanceStoreError::Validation(
                    "instance registry contains duplicate pending deletion ids",
                ));
            }
            if !tombstone_names.insert(pending.tombstone_name.as_str()) {
                return Err(InstanceStoreError::Validation(
                    "instance registry contains duplicate pending deletion names",
                ));
            }
        }
        Ok(())
    }

    pub fn encode(&self) -> Result<Vec<u8>, InstanceStoreError> {
        self.validate()?;
        let encoded = serde_json::to_vec_pretty(self)?;
        if encoded.len() as u64 > INSTANCE_REGISTRY_MAX_BYTES {
            return Err(InstanceStoreError::TooLarge {
                max_bytes: INSTANCE_REGISTRY_MAX_BYTES,
            });
        }
        Ok(encoded)
    }
}

pub struct InstanceStore {
    paths: AppPaths,
    root_session: Arc<AppRootSession>,
    snapshot: InstanceRegistrySnapshot,
    mutation_allowed: bool,
    startup_source: StartupFileProvenance,
}

pub struct InstanceStoreStartup {
    pub store: InstanceStore,
    pub warnings: Vec<String>,
}

#[derive(Debug, Error)]
pub enum InstanceStoreError {
    #[error("failed to read instances: {0}")]
    Read(#[from] std::io::Error),
    #[error("failed to parse instances: {0}")]
    Parse(#[from] serde_json::Error),
    #[error("invalid instance registry: {0}")]
    Validation(&'static str),
    #[error("instance registry exceeds the maximum persisted size of {max_bytes} bytes")]
    TooLarge { max_bytes: u64 },
    #[error("failed to persist instance registry: {0}")]
    Persistence(std::io::Error),
    #[error("failed to open application root: {0}")]
    Root(std::io::Error),
}

impl InstanceStore {
    pub fn load_for_startup(
        paths: AppPaths,
        root_session: Arc<AppRootSession>,
    ) -> Result<InstanceStoreStartup, InstanceStoreError> {
        root_session
            .validate_paths(&paths)
            .map_err(InstanceStoreError::Root)?;
        let root = root_session
            .root_directory()
            .map_err(InstanceStoreError::Root)?;
        let loaded = read_registry(&root);
        let (snapshot, warnings, mutation_allowed, startup_source) = match loaded {
            Ok(data) => match load_snapshot(&data) {
                Ok(snapshot) => (
                    snapshot,
                    Vec::new(),
                    true,
                    StartupFileProvenance::Accepted(data),
                ),
                Err(_) => rejected_startup_snapshot(),
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                (
                    InstanceRegistrySnapshot::default(),
                    Vec::new(),
                    true,
                    StartupFileProvenance::Missing,
                )
            }
            Err(_) => rejected_startup_snapshot(),
        };

        Ok(InstanceStoreStartup {
            store: Self {
                paths,
                root_session,
                snapshot,
                mutation_allowed,
                startup_source,
            },
            warnings,
        })
    }

    pub fn from_snapshot(
        paths: AppPaths,
        root_session: Arc<AppRootSession>,
        snapshot: InstanceRegistrySnapshot,
    ) -> Result<Self, InstanceStoreError> {
        root_session
            .validate_paths(&paths)
            .map_err(InstanceStoreError::Root)?;
        snapshot.validate()?;
        Ok(Self {
            paths,
            root_session,
            snapshot,
            mutation_allowed: true,
            startup_source: StartupFileProvenance::Synthetic,
        })
    }

    pub fn current(&self) -> InstanceRegistrySnapshot {
        self.snapshot.clone()
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

pub fn is_canonical_instance_id(value: &str) -> bool {
    value.len() == 16
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

pub fn generate_instance_id() -> String {
    static LAST_ID: AtomicU64 = AtomicU64::new(0);
    let observed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_nanos() as u64)
        .unwrap_or_default();
    let mut previous = LAST_ID.load(Ordering::Relaxed);
    loop {
        let next = observed.max(previous.saturating_add(1));
        match LAST_ID.compare_exchange_weak(previous, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return format!("{next:016x}"),
            Err(current) => previous = current,
        }
    }
}

pub fn derive_instance_art_seed(id: &str, name: &str, version_id: &str) -> u32 {
    let mut hash = 2166136261u32;
    for byte in id
        .bytes()
        .chain([0])
        .chain(name.bytes())
        .chain([0])
        .chain(version_id.bytes())
    {
        hash ^= u32::from(byte);
        hash = hash.wrapping_mul(16777619);
    }
    hash
}

fn rejected_startup_snapshot(
) -> (
    InstanceRegistrySnapshot,
    Vec<String>,
    bool,
    StartupFileProvenance,
) {
    (
        InstanceRegistrySnapshot::default(),
        vec![INSTANCE_REGISTRY_STARTUP_WARNING.to_string()],
        false,
        StartupFileProvenance::Rejected,
    )
}

fn load_snapshot(data: &[u8]) -> Result<InstanceRegistrySnapshot, InstanceStoreError> {
    let snapshot = serde_json::from_slice::<InstanceRegistrySnapshot>(data)?;
    snapshot.validate()?;
    Ok(snapshot)
}

fn read_registry(root: &Directory) -> Result<Vec<u8>, std::io::Error> {
    root.open_file(
        &LeafName::new("instances.json").expect("fixed instance registry leaf is valid"),
    )?
    .read_bounded(INSTANCE_REGISTRY_MAX_BYTES)
}

fn validate_instance(instance: &Instance) -> Result<(), InstanceStoreError> {
    if !is_canonical_instance_id(&instance.id) {
        return Err(InstanceStoreError::Validation("instance id is invalid"));
    }
    if !is_bounded_display_text(&instance.name, INSTANCE_NAME_MAX_CHARS, false)
        || instance.name.trim() != instance.name
    {
        return Err(InstanceStoreError::Validation("instance name is invalid"));
    }
    if !is_safe_version_id(&instance.version_id) {
        return Err(InstanceStoreError::Validation(
            "instance version id is invalid",
        ));
    }
    if !is_valid_timestamp(&instance.created_at, false)
        || !is_valid_timestamp(&instance.last_played_at, true)
    {
        return Err(InstanceStoreError::Validation(
            "instance timestamp is invalid",
        ));
    }
    if !(0..=INSTANCE_MEMORY_MAX_MB).contains(&instance.max_memory_mb)
        || !(0..=INSTANCE_MEMORY_MAX_MB).contains(&instance.min_memory_mb)
    {
        return Err(InstanceStoreError::Validation(
            "instance memory bounds are invalid",
        ));
    }
    if !(0..=INSTANCE_WINDOW_MAX_PIXELS).contains(&instance.window_width)
        || !(0..=INSTANCE_WINDOW_MAX_PIXELS).contains(&instance.window_height)
    {
        return Err(InstanceStoreError::Validation(
            "instance window bounds are invalid",
        ));
    }
    if !is_bounded_display_text(&instance.java_path, INSTANCE_JAVA_PATH_MAX_CHARS, true)
        || !is_bounded_display_text(&instance.extra_jvm_args, INSTANCE_JVM_ARGS_MAX_CHARS, true)
        || !is_bounded_token(&instance.jvm_preset, INSTANCE_TOKEN_MAX_CHARS, true)
        || !is_bounded_token(&instance.performance_mode, INSTANCE_TOKEN_MAX_CHARS, true)
        || !is_bounded_display_text(&instance.icon, INSTANCE_TOKEN_MAX_CHARS, true)
        || !is_bounded_display_text(&instance.accent, INSTANCE_TOKEN_MAX_CHARS, true)
        || !is_bounded_token(&instance.loader_key, INSTANCE_TOKEN_MAX_CHARS, true)
    {
        return Err(InstanceStoreError::Validation(
            "instance text field is invalid",
        ));
    }
    if !instance.minecraft_version.is_empty() && !is_safe_version_id(&instance.minecraft_version) {
        return Err(InstanceStoreError::Validation(
            "instance Minecraft version is invalid",
        ));
    }
    if !instance.loader_key.is_empty()
        && loader_component_from_short_key(&instance.loader_key).is_none()
    {
        return Err(InstanceStoreError::Validation(
            "instance loader key is invalid",
        ));
    }
    Ok(())
}

fn is_safe_version_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= INSTANCE_VERSION_ID_MAX_CHARS
        && value != "."
        && value != ".."
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'+'))
}

fn is_valid_timestamp(value: &str, allow_empty: bool) -> bool {
    if value.is_empty() {
        return allow_empty;
    }
    value.chars().count() <= INSTANCE_TIMESTAMP_MAX_CHARS
        && chrono::DateTime::parse_from_rfc3339(value).is_ok()
}

fn is_bounded_display_text(value: &str, max_chars: usize, allow_empty: bool) -> bool {
    (allow_empty || !value.is_empty())
        && value.chars().count() <= max_chars
        && !value.chars().any(char::is_control)
}

fn is_bounded_token(value: &str, max_chars: usize, allow_empty: bool) -> bool {
    (allow_empty || !value.is_empty())
        && value.len() <= max_chars
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
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
                .expect("clock after unix epoch")
                .as_nanos();
            let root = std::env::temp_dir().join(format!(
                "axial-instance-registry-{name}-{}-{nonce}",
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
                    eprintln!("failed to clean instance-store test root during panic: {error}");
                } else {
                    panic!("failed to clean instance-store test root: {error}");
                }
            }
        }
    }

    fn instance(id: &str, name: &str) -> Instance {
        Instance {
            id: id.to_string(),
            name: name.to_string(),
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

    fn pending(id: &str) -> PendingInstanceDeletion {
        PendingInstanceDeletion::new(id, "2026-01-01T00:00:00Z")
            .expect("valid pending deletion")
    }

    fn write_registry(paths: &AppPaths, data: &[u8]) {
        fs::create_dir_all(
            paths
                .instances_file()
                .parent()
                .expect("instance registry has a parent"),
        )
        .expect("create app root");
        fs::write(paths.instances_file(), data).expect("write registry");
    }

    fn assert_rejected_without_rewrite(name: &str, data: &[u8]) {
        let root = TestRoot::new(name);
        let paths = root.paths();
        let root_session = root.root_session();
        write_registry(&paths, data);

        let startup = InstanceStore::load_for_startup(paths.clone(), root_session)
            .expect("load startup registry");

        assert_eq!(startup.store.current(), InstanceRegistrySnapshot::default());
        assert!(!startup.store.mutation_allowed());
        assert_eq!(startup.warnings.len(), 1);
        assert_eq!(
            fs::read(paths.instances_file()).expect("read registry"),
            data
        );
    }

    #[test]
    fn constructors_reject_reconstructed_paths_without_the_acquisition_lineage() {
        let root = TestRoot::new("root-lineage-mismatch");
        let paths = AppPaths::from_root(root.root.clone()).expect("reconstruct identical paths");
        let root_session = root.root_session();

        assert!(matches!(
            InstanceStore::load_for_startup(paths.clone(), Arc::clone(&root_session)),
            Err(InstanceStoreError::Root(error))
                if error.kind() == std::io::ErrorKind::InvalidInput
        ));
        assert!(matches!(
            InstanceStore::from_snapshot(
                paths.clone(),
                root_session,
                InstanceRegistrySnapshot::default(),
            ),
            Err(InstanceStoreError::Root(error))
                if error.kind() == std::io::ErrorKind::InvalidInput
        ));
    }

    #[test]
    fn strict_snapshot_round_trips_current_schema() {
        let snapshot = InstanceRegistrySnapshot::new(
            vec![instance("0000000000000001", "Primary")],
            "0000000000000001".to_string(),
            vec![pending("0000000000000002")],
        )
        .expect("valid snapshot");

        let encoded = snapshot.encode().expect("encode snapshot");
        let decoded = load_snapshot(&encoded).expect("decode snapshot");

        assert_eq!(decoded, snapshot);
    }

    #[test]
    fn declared_loader_identity_is_available_before_install_completion() {
        let mut instance = instance("0000000000000001", "Fabric");
        instance.version_id = "fabric-loader-0.17.2-1.21.6".to_string();
        instance.loader_key = "fabric".to_string();
        instance.minecraft_version = "1.21.6".to_string();

        let enriched = EnrichedInstance::from_instance_without_resource_counts(instance, None);

        assert_eq!(enriched.version_display.loader_key, "fabric");
        assert_eq!(enriched.version_display.loader_label, "Fabric");
        assert_eq!(enriched.version_display.minecraft_label, "1.21.6");
        assert_eq!(enriched.version_display.summary_label, "Fabric 1.21.6");
        assert!(enriched.version_display.supports_mods);
        assert!(!enriched.launchable);
    }

    #[test]
    fn missing_registry_is_admitted_as_empty() {
        let root = TestRoot::new("missing");
        let paths = root.paths();
        let root_session = root.root_session();

        let startup = InstanceStore::load_for_startup(paths.clone(), root_session)
            .expect("load missing startup registry");

        assert_eq!(startup.store.current(), InstanceRegistrySnapshot::default());
        assert!(startup.store.mutation_allowed());
        assert!(startup.warnings.is_empty());
        assert!(!paths.instances_file().exists());
    }

    #[test]
    fn malformed_registry_is_rejected_without_rewrite() {
        assert_rejected_without_rewrite("malformed", b"{not valid json");
    }

    #[test]
    fn unknown_or_missing_fields_are_rejected_without_rewrite() {
        let unknown = br#"{"schema_version":3,"instances":[],"last_instance_id":"","pending_deletions":[],"legacy":true}"#;
        assert_rejected_without_rewrite("unknown-field", unknown);

        let missing = br#"{"schema_version":3,"instances":[],"last_instance_id":""}"#;
        assert_rejected_without_rewrite("missing-field", missing);

        let nested_unknown = br#"{"schema_version":3,"instances":[],"last_instance_id":"","pending_deletions":[{"instance_id":"0000000000000002","created_at":"2026-01-01T00:00:00Z","tombstone_name":"invalid","legacy":true}]}"#;
        assert_rejected_without_rewrite("nested-unknown-field", nested_unknown);
    }

    #[test]
    fn schema_v2_is_rejected_without_migration_or_rewrite() {
        let v2 = br#"{"schema_version":2,"instances":[],"last_instance_id":"","pending_deletions":[]}"#;
        assert!(matches!(
            load_snapshot(v2),
            Err(InstanceStoreError::Validation(
                "unsupported instance registry schema version"
            ))
        ));
        assert_rejected_without_rewrite("schema-v2", v2);
    }

    #[test]
    fn semantically_invalid_registry_is_rejected_without_rewrite() {
        let data = serde_json::to_vec_pretty(&InstanceRegistrySnapshot {
            schema_version: INSTANCE_REGISTRY_SCHEMA_VERSION,
            instances: vec![instance("../../outside-root", "Unsafe")],
            last_instance_id: String::new(),
            pending_deletions: Vec::new(),
        })
        .expect("serialize invalid registry");

        assert_rejected_without_rewrite("semantic-invalid", &data);
    }

    #[test]
    fn oversized_registry_is_rejected_without_rewrite() {
        let data = vec![b' '; INSTANCE_REGISTRY_MAX_BYTES as usize + 1];
        assert_rejected_without_rewrite("oversized", &data);
    }

    #[test]
    fn nonregular_registry_is_rejected_without_replacement() {
        let root = TestRoot::new("nonregular");
        let paths = root.paths();
        let root_session = root.root_session();
        fs::create_dir_all(paths.instances_file()).expect("create registry directory");

        let startup = InstanceStore::load_for_startup(paths.clone(), root_session)
            .expect("load nonregular startup registry");

        assert_eq!(startup.store.current(), InstanceRegistrySnapshot::default());
        assert!(!startup.store.mutation_allowed());
        assert_eq!(startup.warnings.len(), 1);
        assert!(paths.instances_file().is_dir());
    }

    #[test]
    fn snapshot_rejects_duplicate_or_ambiguous_ownership() {
        let duplicate_id = InstanceRegistrySnapshot {
            schema_version: INSTANCE_REGISTRY_SCHEMA_VERSION,
            instances: vec![
                instance("0000000000000001", "First"),
                instance("0000000000000001", "Second"),
            ],
            last_instance_id: String::new(),
            pending_deletions: Vec::new(),
        };
        assert!(matches!(
            duplicate_id.validate(),
            Err(InstanceStoreError::Validation(_))
        ));

        let duplicate_name = InstanceRegistrySnapshot {
            schema_version: INSTANCE_REGISTRY_SCHEMA_VERSION,
            instances: vec![
                instance("0000000000000001", "Same"),
                instance("0000000000000002", "Same"),
            ],
            last_instance_id: String::new(),
            pending_deletions: Vec::new(),
        };
        assert!(matches!(
            duplicate_name.validate(),
            Err(InstanceStoreError::Validation(_))
        ));

        let live_deletion = InstanceRegistrySnapshot {
            schema_version: INSTANCE_REGISTRY_SCHEMA_VERSION,
            instances: vec![instance("0000000000000001", "Live")],
            last_instance_id: String::new(),
            pending_deletions: vec![pending("0000000000000001")],
        };
        assert!(matches!(
            live_deletion.validate(),
            Err(InstanceStoreError::Validation(_))
        ));
    }

    #[test]
    fn snapshot_rejects_stale_last_instance_and_duplicate_pending_deletions() {
        let stale_last = InstanceRegistrySnapshot {
            schema_version: INSTANCE_REGISTRY_SCHEMA_VERSION,
            instances: vec![instance("0000000000000001", "Live")],
            last_instance_id: "0000000000000002".to_string(),
            pending_deletions: Vec::new(),
        };
        assert!(matches!(
            stale_last.validate(),
            Err(InstanceStoreError::Validation(_))
        ));

        let duplicate_pending = InstanceRegistrySnapshot {
            schema_version: INSTANCE_REGISTRY_SCHEMA_VERSION,
            instances: Vec::new(),
            last_instance_id: String::new(),
            pending_deletions: vec![
                pending("0000000000000002"),
                pending("0000000000000002"),
            ],
        };
        assert!(matches!(
            duplicate_pending.validate(),
            Err(InstanceStoreError::Validation(_))
        ));
    }

    #[test]
    fn pending_deletion_name_is_deterministic_and_identity_bound() {
        let deletion = pending("0000000000000002");
        assert_eq!(
            deletion.tombstone_name,
            ".axial-instance-tombstone-v1-0000000000000002-ef61993acb0a3ca2eeb8140882b3dfc47fa27cbed38f1c7c6e925867ead9683b"
        );
        assert_ne!(
            deletion.tombstone_name,
            derive_instance_tombstone_name(
                "0000000000000002",
                "2026-01-01T00:00:00+00:00"
            )
            .expect("alternate valid spelling")
        );
    }

    #[test]
    fn pending_deletion_rejects_hostile_identity_and_persisted_names() {
        assert!(matches!(
            PendingInstanceDeletion::new("../../outside-root", "2026-01-01T00:00:00Z"),
            Err(InstanceStoreError::Validation(_))
        ));
        assert!(matches!(
            PendingInstanceDeletion::new("0000000000000002", ""),
            Err(InstanceStoreError::Validation(_))
        ));
        assert!(matches!(
            PendingInstanceDeletion::new("0000000000000002", "not-a-timestamp"),
            Err(InstanceStoreError::Validation(_))
        ));
        assert!(matches!(
            PendingInstanceDeletion::new(
                "0000000000000002",
                format!("2026-01-01T00:00:00Z{}", "0".repeat(INSTANCE_TIMESTAMP_MAX_CHARS))
            ),
            Err(InstanceStoreError::Validation(_))
        ));

        let mut forged = pending("0000000000000002");
        forged.tombstone_name.make_ascii_uppercase();
        let snapshot = InstanceRegistrySnapshot {
            schema_version: INSTANCE_REGISTRY_SCHEMA_VERSION,
            instances: Vec::new(),
            last_instance_id: String::new(),
            pending_deletions: vec![forged],
        };
        assert!(matches!(
            snapshot.validate(),
            Err(InstanceStoreError::Validation(
                "instance registry pending deletion tombstone name is invalid"
            ))
        ));
    }

    #[test]
    fn snapshot_rejects_unbounded_fields_counts_and_numerics() {
        let mut invalid = instance("0000000000000001", "Invalid");
        invalid.extra_jvm_args = "x".repeat(INSTANCE_JVM_ARGS_MAX_CHARS + 1);
        assert!(matches!(
            InstanceRegistrySnapshot::new(vec![invalid], String::new(), Vec::new()),
            Err(InstanceStoreError::Validation(_))
        ));

        let too_many =
            vec![instance("0000000000000001", "Repeated"); INSTANCE_REGISTRY_MAX_ENTRIES + 1];
        assert!(matches!(
            InstanceRegistrySnapshot::new(too_many, String::new(), Vec::new()),
            Err(InstanceStoreError::Validation(_))
        ));

        let full = (0..INSTANCE_REGISTRY_MAX_ENTRIES)
            .map(|index| {
                instance(
                    &format!("{index:016x}"),
                    &format!("Instance {index}"),
                )
            })
            .collect();
        assert!(matches!(
            InstanceRegistrySnapshot::new(
                full,
                String::new(),
                vec![pending("0000000000000400")]
            ),
            Err(InstanceStoreError::Validation(
                "instance registry contains too many ownership records"
            ))
        ));

        let mut invalid = instance("0000000000000001", "Invalid numeric");
        invalid.window_width = INSTANCE_WINDOW_MAX_PIXELS + 1;
        assert!(matches!(
            InstanceRegistrySnapshot::new(vec![invalid], String::new(), Vec::new()),
            Err(InstanceStoreError::Validation(_))
        ));

        let oversized = (0..129)
            .map(|index| {
                let mut entry = instance(
                    &format!("{index:016x}"),
                    &format!("Large {index}"),
                );
                entry.extra_jvm_args = "x".repeat(INSTANCE_JVM_ARGS_MAX_CHARS);
                entry
            })
            .collect();
        let oversized = InstanceRegistrySnapshot::new(oversized, String::new(), Vec::new())
            .expect("oversized canonical snapshot remains semantically valid");
        assert!(matches!(
            oversized.encode(),
            Err(InstanceStoreError::TooLarge {
                max_bytes: INSTANCE_REGISTRY_MAX_BYTES
            })
        ));
    }

    #[test]
    fn from_snapshot_exposes_immutable_fixture_state_and_paths() {
        let root = TestRoot::new("fixture");
        let paths = root.paths();
        let root_session = root.root_session();
        let snapshot = InstanceRegistrySnapshot::new(
            vec![instance("0000000000000001", "Fixture")],
            "0000000000000001".to_string(),
            Vec::new(),
        )
        .expect("valid snapshot");

        let store = InstanceStore::from_snapshot(paths.clone(), root_session, snapshot.clone())
            .expect("fixture store");

        assert_eq!(store.current(), snapshot);
        assert_eq!(store.paths().instances_file(), paths.instances_file());
        assert!(store.mutation_allowed());
    }

    #[test]
    fn generated_ids_are_canonical_and_distinct() {
        let first = generate_instance_id();
        let second = generate_instance_id();

        assert!(is_canonical_instance_id(&first));
        assert!(is_canonical_instance_id(&second));
        assert_ne!(first, second);
        assert_ne!(
            derive_instance_art_seed(&first, "First", "1.21.1"),
            derive_instance_art_seed(&second, "First", "1.21.1")
        );
    }
}
