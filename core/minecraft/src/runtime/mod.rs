use crate::launch::JavaVersion;
use crate::paths::runtime_dirs;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, OnceLock};
use thiserror::Error;
use tokio::sync::Mutex;

const RUNTIME_MANIFEST_URL: &str = "https://launchermeta.mojang.com/v1/products/java-runtime/2ec0cc96c44e5a76b9c8b7c39df7210883d12871/all.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JavaRuntimeInfo {
    pub id: String,
    pub major: u32,
    #[serde(default)]
    pub update: u32,
    #[serde(default)]
    pub distribution: String,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JavaRuntimeResult {
    pub path: String,
    pub component: String,
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RuntimeId(pub String);

impl RuntimeId {
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl From<String> for RuntimeId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for RuntimeId {
    fn from(value: &str) -> Self {
        Self(value.to_string())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeSource {
    Managed,
    MinecraftBundled,
    MicrosoftStore,
    ExternalOverride,
}

impl RuntimeSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Managed => "managed",
            Self::MinecraftBundled => "minecraft-runtime",
            Self::MicrosoftStore => "ms-store",
            Self::ExternalOverride => "override",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeInstallState {
    Missing,
    Installing,
    Ready,
    Broken,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeRecord {
    pub id: RuntimeId,
    pub java_path: String,
    pub info: JavaRuntimeInfo,
    pub source: RuntimeSource,
    pub install_state: RuntimeInstallState,
    pub root_dir: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeRequirement {
    pub required_java: JavaVersion,
    pub preferred_component: RuntimeId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeOverride {
    None,
    Component(RuntimeId),
    ExecutablePath(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeEnsureAction {
    UseRequested,
    UseManaged,
    BypassRequested,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeEnsureResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requested: Option<RuntimeRecord>,
    pub effective: RuntimeRecord,
    pub bypassed_requested_runtime: bool,
    pub install_performed: bool,
    pub action: RuntimeEnsureAction,
}

#[derive(Debug, Error)]
pub enum JavaRuntimeLookupError {
    #[error("java runtime not found: {component} (Java {major}) not installed")]
    NotFound { component: String, major: i32 },
    #[error("failed to install java runtime: {0}")]
    Download(String),
    #[error("failed to probe java runtime: {0}")]
    Probe(String),
}

pub fn runtime_requirement(java_version: &JavaVersion) -> RuntimeRequirement {
    RuntimeRequirement {
        required_java: java_version.clone(),
        preferred_component: RuntimeId(preferred_runtime_component(java_version)),
    }
}

pub fn parse_runtime_override(value: &str) -> RuntimeOverride {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        RuntimeOverride::None
    } else if is_known_runtime_component(trimmed) {
        RuntimeOverride::Component(RuntimeId(trimmed.to_string()))
    } else {
        RuntimeOverride::ExecutablePath(PathBuf::from(trimmed))
    }
}

pub fn list_runtime_records(library_dir: &Path) -> Vec<RuntimeRecord> {
    let components = known_runtime_components();
    let mut dirs = runtime_dirs(library_dir);
    dirs.push(runtime_cache_dir());

    let mut results = Vec::new();
    for dir in dirs {
        for component in &components {
            if let Some(runtime) = inspect_component_runtime(&dir, component)
                && runtime.install_state == RuntimeInstallState::Ready
                && !results.iter().any(|entry: &RuntimeRecord| {
                    entry.id == runtime.id && entry.java_path == runtime.java_path
                })
            {
                results.push(runtime);
            }
        }
    }

    results
}

pub fn list_java_runtimes(library_dir: &Path) -> Vec<JavaRuntimeResult> {
    list_runtime_records(library_dir)
        .into_iter()
        .filter(|record| record.install_state == RuntimeInstallState::Ready)
        .map(|record| JavaRuntimeResult {
            path: record.java_path,
            component: record.id.0,
            source: record.source.as_str().to_string(),
        })
        .collect()
}

pub fn find_java_runtime(
    library_dir: &Path,
    java_version: &JavaVersion,
    override_path: &str,
) -> Result<JavaRuntimeResult, JavaRuntimeLookupError> {
    let requirement = runtime_requirement(java_version);
    let runtime_override = parse_runtime_override(override_path);
    let record = match runtime_override {
        RuntimeOverride::None => {
            resolve_managed_runtime(library_dir, &requirement.preferred_component)?
        }
        RuntimeOverride::Component(component) => {
            resolve_component_runtime(library_dir, &component, java_version.major_version)?
        }
        RuntimeOverride::ExecutablePath(path) => {
            resolve_override_runtime(&path, &requirement.preferred_component)?
        }
    };

    Ok(JavaRuntimeResult {
        path: record.java_path,
        component: record.id.0,
        source: record.source.as_str().to_string(),
    })
}

pub async fn ensure_java_runtime(
    library_dir: &Path,
    java_version: &JavaVersion,
    override_path: &str,
) -> Result<JavaRuntimeResult, JavaRuntimeLookupError> {
    let result = ensure_runtime(library_dir, java_version, override_path, false).await?;
    Ok(JavaRuntimeResult {
        path: result.effective.java_path,
        component: result.effective.id.0,
        source: result.effective.source.as_str().to_string(),
    })
}

pub async fn ensure_runtime(
    library_dir: &Path,
    java_version: &JavaVersion,
    override_path: &str,
    force_managed: bool,
) -> Result<RuntimeEnsureResult, JavaRuntimeLookupError> {
    let requirement = runtime_requirement(java_version);
    let requested_override = parse_runtime_override(override_path);

    let requested = if force_managed {
        None
    } else {
        match &requested_override {
            RuntimeOverride::None => None,
            RuntimeOverride::Component(component) => Some(resolve_component_runtime(
                library_dir,
                component,
                java_version.major_version,
            )?),
            RuntimeOverride::ExecutablePath(path) => Some(resolve_override_runtime(
                path,
                &requirement.preferred_component,
            )?),
        }
    };

    if let Some(requested_runtime) = requested.clone()
        && !should_bypass_requested_runtime(java_version, &requested_runtime)
    {
        return Ok(RuntimeEnsureResult {
            requested: Some(requested_runtime.clone()),
            effective: requested_runtime,
            bypassed_requested_runtime: false,
            install_performed: false,
            action: RuntimeEnsureAction::UseRequested,
        });
    }

    let managed = ensure_managed_runtime(library_dir, &requirement).await?;
    let bypassed_requested_runtime = requested.is_some();

    Ok(RuntimeEnsureResult {
        requested,
        effective: managed.effective,
        bypassed_requested_runtime,
        install_performed: managed.install_performed,
        action: if bypassed_requested_runtime {
            RuntimeEnsureAction::BypassRequested
        } else {
            RuntimeEnsureAction::UseManaged
        },
    })
}

pub fn probe_java_runtime_info(
    java_path: &Path,
    id_hint: Option<&str>,
) -> Result<JavaRuntimeInfo, JavaRuntimeLookupError> {
    let exec_path = java_probe_executable(java_path);
    let output = Command::new(&exec_path)
        .args(["-XshowSettings:property", "-version"])
        .output()
        .map_err(|error| JavaRuntimeLookupError::Probe(error.to_string()))?;

    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
    let (major, update) = parse_java_version(&text);
    Ok(JavaRuntimeInfo {
        id: id_hint.unwrap_or_default().to_string(),
        major,
        update,
        distribution: detect_distribution(&text),
        path: java_path.to_string_lossy().to_string(),
    })
}

pub fn preferred_runtime_component(java_version: &JavaVersion) -> String {
    if java_version.component.is_empty() {
        "java-runtime-delta".to_string()
    } else {
        java_version.component.clone()
    }
}

pub fn is_known_runtime_component(value: &str) -> bool {
    known_runtime_components()
        .iter()
        .any(|component| *component == value.trim())
}

fn known_runtime_components() -> [&'static str; 6] {
    [
        "java-runtime-epsilon",
        "java-runtime-delta",
        "java-runtime-gamma",
        "java-runtime-beta",
        "java-runtime-alpha",
        "jre-legacy",
    ]
}

fn should_bypass_requested_runtime(java_version: &JavaVersion, runtime: &RuntimeRecord) -> bool {
    if runtime.source != RuntimeSource::ExternalOverride {
        return false;
    }
    if runtime.info.major == 0 || java_version.major_version == 0 {
        return false;
    }
    if runtime.info.major as i32 != java_version.major_version {
        return true;
    }
    runtime.info.major == 8
}

fn resolve_component_runtime(
    library_dir: &Path,
    component: &RuntimeId,
    required_major: i32,
) -> Result<RuntimeRecord, JavaRuntimeLookupError> {
    let mut dirs = runtime_dirs(library_dir);
    dirs.push(runtime_cache_dir());
    for dir in dirs {
        if let Some(record) = inspect_component_runtime(&dir, component.as_str())
            && record.install_state == RuntimeInstallState::Ready
        {
            return Ok(record);
        }
    }

    Err(JavaRuntimeLookupError::NotFound {
        component: component.0.clone(),
        major: required_major,
    })
}

fn resolve_managed_runtime(
    library_dir: &Path,
    component: &RuntimeId,
) -> Result<RuntimeRecord, JavaRuntimeLookupError> {
    resolve_component_runtime(library_dir, component, 0)
}

fn resolve_override_runtime(
    path: &Path,
    preferred_component: &RuntimeId,
) -> Result<RuntimeRecord, JavaRuntimeLookupError> {
    if !path.is_file() {
        return Err(JavaRuntimeLookupError::NotFound {
            component: path.to_string_lossy().to_string(),
            major: 0,
        });
    }

    let info = probe_java_runtime_info(path, Some(preferred_component.as_str()))?;
    Ok(RuntimeRecord {
        id: preferred_component.clone(),
        java_path: path.to_string_lossy().to_string(),
        info,
        source: RuntimeSource::ExternalOverride,
        install_state: RuntimeInstallState::Ready,
        root_dir: path
            .parent()
            .and_then(Path::parent)
            .unwrap_or_else(|| Path::new(""))
            .to_string_lossy()
            .to_string(),
    })
}

struct ManagedEnsure {
    effective: RuntimeRecord,
    install_performed: bool,
}

async fn ensure_managed_runtime(
    library_dir: &Path,
    requirement: &RuntimeRequirement,
) -> Result<ManagedEnsure, JavaRuntimeLookupError> {
    let preferred = &requirement.preferred_component;
    if let Ok(runtime) = resolve_managed_runtime(library_dir, preferred) {
        return Ok(ManagedEnsure {
            effective: runtime,
            install_performed: false,
        });
    }

    let install_root = runtime_cache_dir().join(preferred.as_str());
    let install_lock = runtime_install_lock(preferred.as_str());
    let _guard = install_lock.lock().await;

    if let Ok(runtime) = resolve_managed_runtime(library_dir, preferred) {
        return Ok(ManagedEnsure {
            effective: runtime,
            install_performed: false,
        });
    }

    install_managed_runtime(preferred, &install_root).await?;
    let runtime = resolve_component_runtime(
        library_dir,
        preferred,
        requirement.required_java.major_version,
    )?;
    Ok(ManagedEnsure {
        effective: runtime,
        install_performed: true,
    })
}

fn runtime_install_lock(component: &str) -> Arc<Mutex<()>> {
    static LOCKS: OnceLock<std::sync::Mutex<HashMap<String, Arc<Mutex<()>>>>> = OnceLock::new();
    let mutex = LOCKS.get_or_init(|| std::sync::Mutex::new(HashMap::new()));
    let mut guard = mutex.lock().expect("runtime install lock poisoned");
    guard
        .entry(component.to_string())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

fn inspect_component_runtime(base_dir: &Path, component: &str) -> Option<RuntimeRecord> {
    if !base_dir.exists() {
        return None;
    }

    let os_arch = runtime_os_arch();
    for candidate in [
        base_dir.join(component).join(&os_arch).join(component),
        base_dir.join(component),
    ] {
        let state = detect_runtime_state(&candidate);
        if state == RuntimeInstallState::Missing {
            continue;
        }

        let java_exe = java_executable(&candidate);
        let source = classify_runtime_source(base_dir);
        let info = if state == RuntimeInstallState::Ready {
            probe_java_runtime_info(&java_exe, Some(component)).unwrap_or(JavaRuntimeInfo {
                id: component.to_string(),
                major: 0,
                update: 0,
                distribution: "unknown".to_string(),
                path: java_exe.to_string_lossy().to_string(),
            })
        } else {
            JavaRuntimeInfo {
                id: component.to_string(),
                major: 0,
                update: 0,
                distribution: "unknown".to_string(),
                path: java_exe.to_string_lossy().to_string(),
            }
        };

        return Some(RuntimeRecord {
            id: RuntimeId(component.to_string()),
            java_path: java_exe.to_string_lossy().to_string(),
            info,
            source,
            install_state: state,
            root_dir: candidate.to_string_lossy().to_string(),
        });
    }

    None
}

fn classify_runtime_source(base_dir: &Path) -> RuntimeSource {
    let label = base_dir.to_string_lossy();
    if label.contains("Packages") {
        RuntimeSource::MicrosoftStore
    } else if label.contains("croopor") {
        RuntimeSource::Managed
    } else {
        RuntimeSource::MinecraftBundled
    }
}

fn detect_runtime_state(runtime_root: &Path) -> RuntimeInstallState {
    let installing_marker = runtime_root.join(".croopor-installing");
    let ready_marker = runtime_root.join(".croopor-ready");
    let java_exe = java_executable(runtime_root);

    if runtime_executable_ready(&java_exe) {
        return RuntimeInstallState::Ready;
    }
    if installing_marker.exists() {
        return RuntimeInstallState::Installing;
    }
    if ready_marker.exists() || runtime_root.exists() {
        return RuntimeInstallState::Broken;
    }
    RuntimeInstallState::Missing
}

async fn install_managed_runtime(
    component: &RuntimeId,
    dest_dir: &Path,
) -> Result<(), JavaRuntimeLookupError> {
    let os_arch = runtime_os_arch();
    let parent_dir = dest_dir.parent().ok_or_else(|| {
        JavaRuntimeLookupError::Download("invalid runtime destination".to_string())
    })?;
    fs::create_dir_all(parent_dir)
        .map_err(|error| JavaRuntimeLookupError::Download(error.to_string()))?;

    let temp_dir = dest_dir.with_extension("installing");
    if temp_dir.exists() {
        let _ = fs::remove_dir_all(&temp_dir);
    }
    if dest_dir.exists() {
        let _ = fs::remove_dir_all(dest_dir);
    }
    fs::create_dir_all(&temp_dir)
        .map_err(|error| JavaRuntimeLookupError::Download(error.to_string()))?;
    fs::write(temp_dir.join(".croopor-installing"), b"installing")
        .map_err(|error| JavaRuntimeLookupError::Download(error.to_string()))?;

    let all_runtimes = fetch_runtime_json::<RuntimeManifest>(RUNTIME_MANIFEST_URL).await?;

    let os_runtimes = all_runtimes.get(&os_arch).ok_or_else(|| {
        JavaRuntimeLookupError::Download(format!("no runtimes available for {os_arch}"))
    })?;
    let entries = os_runtimes.get(component.as_str()).ok_or_else(|| {
        JavaRuntimeLookupError::Download(format!(
            "runtime {} is not available for {os_arch}",
            component.as_str()
        ))
    })?;
    let manifest_url = entries
        .first()
        .map(|entry| entry.manifest.url.as_str())
        .ok_or_else(|| {
            JavaRuntimeLookupError::Download(format!(
                "runtime {} has no downloadable manifest for {os_arch}",
                component.as_str()
            ))
        })?;

    let component_manifest = fetch_runtime_json::<ComponentManifest>(manifest_url).await?;

    for (relative_path, file) in component_manifest.files {
        let destination = temp_dir.join(relative_path.replace('/', std::path::MAIN_SEPARATOR_STR));
        if file.kind == "directory" {
            fs::create_dir_all(&destination)
                .map_err(|error| JavaRuntimeLookupError::Download(error.to_string()))?;
            continue;
        }
        if file.kind != "file" {
            continue;
        }
        let Some(raw) = file.downloads.and_then(|downloads| downloads.raw) else {
            continue;
        };

        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| JavaRuntimeLookupError::Download(error.to_string()))?;
        }

        let bytes = fetch_runtime_bytes(&raw.url).await?;
        let temp_path = destination.with_extension("tmp");
        fs::write(&temp_path, &bytes)
            .map_err(|error| JavaRuntimeLookupError::Download(error.to_string()))?;
        fs::rename(&temp_path, &destination)
            .map_err(|error| JavaRuntimeLookupError::Download(error.to_string()))?;
        #[cfg(unix)]
        if file.executable {
            use std::os::unix::fs::PermissionsExt;

            let metadata = fs::metadata(&destination)
                .map_err(|error| JavaRuntimeLookupError::Download(error.to_string()))?;
            let mut permissions = metadata.permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&destination, permissions)
                .map_err(|error| JavaRuntimeLookupError::Download(error.to_string()))?;
        }
    }

    let java_exe = java_executable(&temp_dir);
    if !runtime_executable_ready(&java_exe) {
        let _ = fs::remove_dir_all(&temp_dir);
        return Err(JavaRuntimeLookupError::Download(format!(
            "installed runtime {} is incomplete",
            component.as_str()
        )));
    }

    let _ = fs::remove_file(temp_dir.join(".croopor-installing"));
    fs::write(temp_dir.join(".croopor-ready"), b"ready")
        .map_err(|error| JavaRuntimeLookupError::Download(error.to_string()))?;
    fs::rename(&temp_dir, dest_dir)
        .map_err(|error| JavaRuntimeLookupError::Download(error.to_string()))?;

    Ok(())
}

async fn fetch_runtime_json<T>(url: &str) -> Result<T, JavaRuntimeLookupError>
where
    T: serde::de::DeserializeOwned + Send + 'static,
{
    let url = url.to_string();
    tokio::task::spawn_blocking(move || {
        let response = ureq::AgentBuilder::new()
            .timeout(std::time::Duration::from_secs(30))
            .user_agent("croopor/0.3")
            .build()
            .get(&url)
            .call()
            .map_err(|error| JavaRuntimeLookupError::Download(error.to_string()))?;
        response
            .into_json::<T>()
            .map_err(|error| JavaRuntimeLookupError::Download(error.to_string()))
    })
    .await
    .map_err(|error| JavaRuntimeLookupError::Download(error.to_string()))?
}

async fn fetch_runtime_bytes(url: &str) -> Result<Vec<u8>, JavaRuntimeLookupError> {
    let url = url.to_string();
    tokio::task::spawn_blocking(move || {
        let response = ureq::AgentBuilder::new()
            .timeout(std::time::Duration::from_secs(300))
            .user_agent("croopor/0.3")
            .build()
            .get(&url)
            .call()
            .map_err(|error| JavaRuntimeLookupError::Download(error.to_string()))?;
        let mut reader = response.into_reader();
        let mut bytes = Vec::new();
        reader
            .read_to_end(&mut bytes)
            .map_err(|error| JavaRuntimeLookupError::Download(error.to_string()))?;
        Ok(bytes)
    })
    .await
    .map_err(|error| JavaRuntimeLookupError::Download(error.to_string()))?
}

fn runtime_cache_dir() -> PathBuf {
    if cfg!(target_os = "windows") {
        std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .map(|path| path.join("croopor").join("runtimes"))
            .unwrap_or_else(|| PathBuf::from(".croopor").join("runtimes"))
    } else {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .map(|path| path.join(".croopor").join("runtimes"))
            .unwrap_or_else(|| PathBuf::from(".croopor").join("runtimes"))
    }
}

#[derive(Debug, Deserialize)]
struct RuntimeManifestEntry {
    manifest: RuntimeDownloadManifest,
}

#[derive(Debug, Deserialize)]
struct RuntimeDownloadManifest {
    url: String,
}

type RuntimeManifest = HashMap<String, HashMap<String, Vec<RuntimeManifestEntry>>>;

#[derive(Debug, Deserialize)]
struct ComponentManifest {
    files: HashMap<String, ComponentManifestFile>,
}

#[derive(Debug, Deserialize)]
struct ComponentManifestFile {
    #[serde(rename = "type")]
    kind: String,
    #[cfg_attr(not(unix), allow(dead_code))]
    #[serde(default)]
    executable: bool,
    #[serde(default)]
    downloads: Option<ComponentManifestDownloads>,
}

#[derive(Debug, Deserialize)]
struct ComponentManifestDownloads {
    #[serde(default)]
    raw: Option<ComponentManifestDownload>,
}

#[derive(Debug, Deserialize)]
struct ComponentManifestDownload {
    url: String,
}

fn java_probe_executable(java_path: &Path) -> PathBuf {
    if !cfg!(target_os = "windows") {
        return java_path.to_path_buf();
    }

    if java_path
        .file_name()
        .map(|name| name.to_string_lossy().eq_ignore_ascii_case("javaw.exe"))
        .unwrap_or(false)
    {
        let candidate = java_path.with_file_name("java.exe");
        if candidate.is_file() {
            return candidate;
        }
    }

    java_path.to_path_buf()
}

fn parse_java_version(text: &str) -> (u32, u32) {
    let Some(version) = text
        .lines()
        .find_map(|line| line.split('"').nth(1))
        .or_else(|| {
            text.split_whitespace()
                .find(|token| token.chars().next().is_some_and(|ch| ch.is_ascii_digit()))
        })
    else {
        return (0, 0);
    };

    let parts = version
        .split(['.', '_', '-', '+'])
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    if parts.is_empty() {
        return (0, 0);
    }

    if parts[0] == "1" {
        let major = parts
            .get(1)
            .and_then(|part| part.parse::<u32>().ok())
            .unwrap_or_default();
        let update = parts
            .get(2)
            .and_then(|part| part.parse::<u32>().ok())
            .unwrap_or_default();
        return (major, update);
    }

    let major = parts[0].parse::<u32>().ok().unwrap_or_default();
    let update = parts
        .get(2)
        .and_then(|part| part.parse::<u32>().ok())
        .unwrap_or_else(|| {
            parts
                .get(1)
                .and_then(|part| part.parse::<u32>().ok())
                .unwrap_or_default()
        });
    (major, update)
}

fn detect_distribution(text: &str) -> String {
    let vendor = text
        .lines()
        .find_map(|line| {
            let line = line.trim();
            line.strip_prefix("java.vendor =")
                .map(str::trim)
                .map(str::to_uppercase)
        })
        .unwrap_or_default();

    match () {
        _ if vendor.contains("GRAALVM") => "graalvm".to_string(),
        _ if vendor.contains("OPENJ9") || vendor.contains("SEMERU") || vendor.contains("IBM") => {
            "openj9".to_string()
        }
        _ if vendor.contains("TEMURIN") || vendor.contains("ECLIPSE") => "temurin".to_string(),
        _ if vendor.contains("ORACLE") => "oracle".to_string(),
        _ if vendor.is_empty() => "unknown".to_string(),
        _ => "openjdk".to_string(),
    }
}

fn runtime_os_arch() -> String {
    let os_name = if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "macos") {
        "mac-os"
    } else {
        "linux"
    };

    let arch_name = if cfg!(target_arch = "x86_64") {
        "x64"
    } else if cfg!(target_arch = "x86") {
        "x86"
    } else if cfg!(target_arch = "aarch64") {
        "arm64"
    } else {
        std::env::consts::ARCH
    };

    format!("{os_name}-{arch_name}")
}

fn java_executable(runtime_root: &Path) -> PathBuf {
    if cfg!(target_os = "windows") {
        runtime_root.join("bin").join("javaw.exe")
    } else {
        runtime_root.join("bin").join("java")
    }
}

fn runtime_executable_ready(java_exe: &Path) -> bool {
    if !java_exe.is_file() {
        return false;
    }

    if !cfg!(target_os = "windows") {
        return true;
    }

    let runtime_root = java_exe
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or_default();
    runtime_config_candidates(&runtime_root)
        .into_iter()
        .any(|candidate| candidate.is_file())
}

fn runtime_config_candidates(runtime_root: &Path) -> Vec<PathBuf> {
    vec![
        runtime_root.join("lib").join("jvm.cfg"),
        runtime_root.join("lib").join("amd64").join("jvm.cfg"),
        runtime_root.join("jre").join("lib").join("jvm.cfg"),
        runtime_root
            .join("jre")
            .join("lib")
            .join("amd64")
            .join("jvm.cfg"),
    ]
}
