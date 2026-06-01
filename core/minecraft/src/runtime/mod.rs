use crate::launch::JavaVersion;
use crate::paths::runtime_dirs;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use sha1::{Digest as _, Sha1};
use std::collections::HashMap;
use std::ffi::OsStr;
#[cfg(test)]
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, OnceLock};
use thiserror::Error;
use tokio::fs as async_fs;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;
use tokio::task::JoinSet;

const RUNTIME_MANIFEST_URL: &str = "https://launchermeta.mojang.com/v1/products/java-runtime/2ec0cc96c44e5a76b9c8b7c39df7210883d12871/all.json";
const MIN_RUNTIME_FILE_DOWNLOAD_CONCURRENCY: usize = 2;
const MAX_RUNTIME_FILE_DOWNLOAD_CONCURRENCY: usize = 8;
const RUNTIME_FILE_DOWNLOADS_PER_CORE: usize = 2;
const MAX_RUNTIME_MANIFEST_BYTES: u64 = 16 << 20;

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeEnsureEvent {
    DownloadingManagedRuntime { component: String },
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

pub fn runtime_component_ready_without_probe(library_dir: &Path, component: &str) -> bool {
    let mut dirs = runtime_dirs(library_dir);
    dirs.push(runtime_cache_dir());
    dirs.into_iter()
        .any(|dir| component_runtime_ready_without_probe(&dir, component))
}

pub fn runtime_executable_ready_without_probe(java_exe: &Path) -> bool {
    runtime_executable_ready(java_exe)
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
    ensure_runtime_with_events(
        library_dir,
        java_version,
        override_path,
        force_managed,
        |_| {},
    )
    .await
}

pub async fn ensure_runtime_with_events<F>(
    library_dir: &Path,
    java_version: &JavaVersion,
    override_path: &str,
    force_managed: bool,
    mut observer: F,
) -> Result<RuntimeEnsureResult, JavaRuntimeLookupError>
where
    F: FnMut(RuntimeEnsureEvent),
{
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

    if let Some(requested_runtime) = requested.clone() {
        return Ok(RuntimeEnsureResult {
            requested: Some(requested_runtime.clone()),
            effective: requested_runtime,
            bypassed_requested_runtime: false,
            install_performed: false,
            action: RuntimeEnsureAction::UseRequested,
        });
    }

    let managed =
        ensure_managed_runtime_with_events(library_dir, &requirement, &mut observer).await?;

    Ok(RuntimeEnsureResult {
        requested,
        effective: managed.effective,
        bypassed_requested_runtime: false,
        install_performed: managed.install_performed,
        action: RuntimeEnsureAction::UseManaged,
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

fn component_runtime_ready_without_probe(base_dir: &Path, component: &str) -> bool {
    if !base_dir.exists() {
        return false;
    }

    let os_arch = runtime_os_arch();
    [
        base_dir.join(component).join(&os_arch).join(component),
        base_dir.join(component),
    ]
    .into_iter()
    .any(|candidate| {
        detect_runtime_state(&candidate, runtime_requires_ready_marker(base_dir))
            == RuntimeInstallState::Ready
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

async fn ensure_managed_runtime_with_events<F>(
    library_dir: &Path,
    requirement: &RuntimeRequirement,
    observer: &mut F,
) -> Result<ManagedEnsure, JavaRuntimeLookupError>
where
    F: FnMut(RuntimeEnsureEvent),
{
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

    observer(RuntimeEnsureEvent::DownloadingManagedRuntime {
        component: preferred.as_str().to_string(),
    });
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
    runtime_install_lock_from_map(mutex, component)
}

fn runtime_install_lock_from_map(
    mutex: &std::sync::Mutex<HashMap<String, Arc<Mutex<()>>>>,
    component: &str,
) -> Arc<Mutex<()>> {
    let mut guard = match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
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
        let state = detect_runtime_state(&candidate, runtime_requires_ready_marker(base_dir));
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

fn runtime_requires_ready_marker(base_dir: &Path) -> bool {
    base_dir == runtime_cache_dir()
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

fn detect_runtime_state(runtime_root: &Path, require_ready_marker: bool) -> RuntimeInstallState {
    let installing_marker = runtime_root.join(".croopor-installing");
    let ready_marker = runtime_root.join(".croopor-ready");
    let java_exe = java_executable(runtime_root);

    if require_ready_marker {
        if installing_marker.exists() {
            return RuntimeInstallState::Installing;
        }
        if ready_marker.is_file() && runtime_executable_ready(&java_exe) {
            return RuntimeInstallState::Ready;
        }
        if ready_marker.exists() || runtime_root.exists() {
            return RuntimeInstallState::Broken;
        }
        return RuntimeInstallState::Missing;
    }

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
    async_fs::create_dir_all(parent_dir)
        .await
        .map_err(|error| JavaRuntimeLookupError::Download(error.to_string()))?;

    let temp_dir = dest_dir.with_extension("installing");
    remove_runtime_install_path_async(&temp_dir)
        .await
        .map_err(|error| JavaRuntimeLookupError::Download(error.to_string()))?;
    remove_runtime_install_path_async(dest_dir)
        .await
        .map_err(|error| JavaRuntimeLookupError::Download(error.to_string()))?;
    async_fs::create_dir_all(&temp_dir)
        .await
        .map_err(|error| JavaRuntimeLookupError::Download(error.to_string()))?;
    async_fs::write(temp_dir.join(".croopor-installing"), b"installing")
        .await
        .map_err(|error| JavaRuntimeLookupError::Download(error.to_string()))?;

    let install_result = async {
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
            .map(|entry| entry.manifest.url.clone())
            .ok_or_else(|| {
                JavaRuntimeLookupError::Download(format!(
                    "runtime {} has no downloadable manifest for {os_arch}",
                    component.as_str()
                ))
            })?;

        let component_manifest = fetch_runtime_json::<ComponentManifest>(&manifest_url).await?;

        install_runtime_manifest_files(&temp_dir, component_manifest.files).await?;

        let java_exe = java_executable(&temp_dir);
        if !runtime_executable_ready(&java_exe) {
            return Err(JavaRuntimeLookupError::Download(format!(
                "installed runtime {} is incomplete",
                component.as_str()
            )));
        }

        Ok(())
    }
    .await;

    if let Err(error) = install_result {
        let _ = async_fs::remove_dir_all(&temp_dir).await;
        return Err(error);
    }

    let _ = async_fs::remove_file(temp_dir.join(".croopor-installing")).await;
    if let Err(error) = async_fs::write(temp_dir.join(".croopor-ready"), b"ready").await {
        let _ = async_fs::remove_dir_all(&temp_dir).await;
        return Err(JavaRuntimeLookupError::Download(error.to_string()));
    }
    if let Err(error) = async_fs::rename(&temp_dir, dest_dir).await {
        let _ = async_fs::remove_dir_all(&temp_dir).await;
        return Err(JavaRuntimeLookupError::Download(error.to_string()));
    }

    Ok(())
}

#[cfg(test)]
fn remove_runtime_install_path(path: &Path) -> std::io::Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };

    if metadata.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

async fn remove_runtime_install_path_async(path: &Path) -> std::io::Result<()> {
    let metadata = match async_fs::symlink_metadata(path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };

    if metadata.is_dir() {
        async_fs::remove_dir_all(path).await
    } else {
        async_fs::remove_file(path).await
    }
}

async fn install_runtime_manifest_files(
    temp_dir: &Path,
    files: HashMap<String, ComponentManifestFile>,
) -> Result<(), JavaRuntimeLookupError> {
    let plan = plan_runtime_manifest_files(files);
    let download_client = runtime_download_client();

    for (relative_path, file) in plan.directory_entries.into_iter().chain(plan.other_entries) {
        install_runtime_manifest_file(download_client.clone(), temp_dir, &relative_path, file)
            .await?;
    }

    let mut entries = plan.file_entries.into_iter();
    let mut tasks = JoinSet::new();
    for _ in 0..runtime_file_download_concurrency() {
        let Some(entry) = entries.next() else {
            break;
        };
        spawn_runtime_manifest_file_install(&mut tasks, download_client.clone(), temp_dir, entry);
    }

    let mut first_error = None;
    while let Some(result) = tasks.join_next().await {
        match result {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
            Err(error) => {
                if first_error.is_none() {
                    first_error = Some(JavaRuntimeLookupError::Download(error.to_string()));
                }
            }
        }

        if first_error.is_none()
            && let Some(entry) = entries.next()
        {
            spawn_runtime_manifest_file_install(
                &mut tasks,
                download_client.clone(),
                temp_dir,
                entry,
            );
        }
    }

    if let Some(error) = first_error {
        return Err(error);
    }

    Ok(())
}

fn spawn_runtime_manifest_file_install(
    tasks: &mut JoinSet<Result<(), JavaRuntimeLookupError>>,
    download_client: reqwest::Client,
    temp_dir: &Path,
    entry: (String, ComponentManifestFile),
) {
    let temp_dir = temp_dir.to_path_buf();
    let (relative_path, file) = entry;
    tasks.spawn(async move {
        Box::pin(install_runtime_manifest_file(
            download_client,
            &temp_dir,
            &relative_path,
            file,
        ))
        .await
    });
}

#[derive(Debug, Default)]
struct RuntimeManifestInstallPlan {
    directory_entries: Vec<(String, ComponentManifestFile)>,
    file_entries: Vec<(String, ComponentManifestFile)>,
    other_entries: Vec<(String, ComponentManifestFile)>,
}

fn plan_runtime_manifest_files(
    files: HashMap<String, ComponentManifestFile>,
) -> RuntimeManifestInstallPlan {
    let mut entries = files.into_iter().collect::<Vec<_>>();
    entries.sort_by(|(left, _), (right, _)| left.cmp(right));

    let mut plan = RuntimeManifestInstallPlan::default();
    for (relative_path, file) in entries {
        match file.kind.as_str() {
            "directory" => plan.directory_entries.push((relative_path, file)),
            "file" => plan.file_entries.push((relative_path, file)),
            _ => plan.other_entries.push((relative_path, file)),
        }
    }

    plan
}

async fn install_runtime_manifest_file(
    download_client: reqwest::Client,
    temp_dir: &Path,
    relative_path: &str,
    file: ComponentManifestFile,
) -> Result<(), JavaRuntimeLookupError> {
    let destination = component_manifest_destination(temp_dir, relative_path)?;
    if file.kind == "directory" {
        async_fs::create_dir_all(&destination)
            .await
            .map_err(|error| JavaRuntimeLookupError::Download(error.to_string()))?;
        return Ok(());
    }
    if file.kind != "file" {
        return Ok(());
    }
    let Some(raw) = file.downloads.and_then(|downloads| downloads.raw) else {
        return Ok(());
    };

    if let Some(parent) = destination.parent() {
        async_fs::create_dir_all(parent)
            .await
            .map_err(|error| JavaRuntimeLookupError::Download(error.to_string()))?;
    }

    let temp_path = runtime_download_temp_path(&destination);
    let expected = RuntimeDownloadEvidence::from(&raw);
    Box::pin(fetch_runtime_file(
        &download_client,
        &raw.url,
        &temp_path,
        expected,
        relative_path,
    ))
    .await?;
    if let Err(error) = async_fs::rename(&temp_path, &destination).await {
        let _ = async_fs::remove_file(&temp_path).await;
        return Err(JavaRuntimeLookupError::Download(error.to_string()));
    }
    #[cfg(unix)]
    if file.executable {
        use std::os::unix::fs::PermissionsExt;

        let metadata = async_fs::metadata(&destination)
            .await
            .map_err(|error| JavaRuntimeLookupError::Download(error.to_string()))?;
        let mut permissions = metadata.permissions();
        permissions.set_mode(0o755);
        async_fs::set_permissions(&destination, permissions)
            .await
            .map_err(|error| JavaRuntimeLookupError::Download(error.to_string()))?;
    }

    Ok(())
}

fn runtime_file_download_concurrency() -> usize {
    runtime_file_download_concurrency_for(available_runtime_parallelism())
}

fn available_runtime_parallelism() -> usize {
    std::thread::available_parallelism()
        .map(|value| value.get())
        .unwrap_or(MIN_RUNTIME_FILE_DOWNLOAD_CONCURRENCY)
}

fn runtime_file_download_concurrency_for(cores: usize) -> usize {
    cores.saturating_mul(RUNTIME_FILE_DOWNLOADS_PER_CORE).clamp(
        MIN_RUNTIME_FILE_DOWNLOAD_CONCURRENCY,
        MAX_RUNTIME_FILE_DOWNLOAD_CONCURRENCY,
    )
}

fn component_manifest_destination(
    temp_dir: &Path,
    relative_path: &str,
) -> Result<PathBuf, JavaRuntimeLookupError> {
    if relative_path.is_empty() || has_unsafe_path_component(Path::new(relative_path)) {
        return Err(JavaRuntimeLookupError::Download(format!(
            "unsafe runtime manifest path: {}",
            bounded_manifest_file_label(relative_path)
        )));
    }

    let mut destination = temp_dir.to_path_buf();
    for segment in relative_path.split(['/', '\\']) {
        if segment.is_empty()
            || segment.contains(':')
            || has_unsafe_path_component(Path::new(segment))
        {
            return Err(JavaRuntimeLookupError::Download(format!(
                "unsafe runtime manifest path: {}",
                bounded_manifest_file_label(relative_path)
            )));
        }
        destination.push(segment);
    }

    Ok(destination)
}

fn has_unsafe_path_component(path: &Path) -> bool {
    path.components()
        .any(|component| !matches!(component, std::path::Component::Normal(_)))
}

fn runtime_download_temp_path(destination: &Path) -> PathBuf {
    let mut name = destination
        .file_name()
        .unwrap_or_else(|| OsStr::new("runtime-download"))
        .to_os_string();
    name.push(".croopor-tmp");
    destination.with_file_name(name)
}

async fn fetch_runtime_file(
    download_client: &reqwest::Client,
    url: &str,
    temp_path: &Path,
    expected: RuntimeDownloadEvidence,
    relative_path: &str,
) -> Result<(), JavaRuntimeLookupError> {
    let result =
        stream_runtime_file_to_temp(download_client, url, temp_path, &expected, relative_path)
            .await;

    if result.is_err() {
        let _ = async_fs::remove_file(temp_path).await;
    }

    result
}

async fn stream_runtime_file_to_temp(
    download_client: &reqwest::Client,
    url: &str,
    temp_path: &Path,
    expected: &RuntimeDownloadEvidence,
    relative_path: &str,
) -> Result<(), JavaRuntimeLookupError> {
    let response = download_client
        .get(url)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|error| JavaRuntimeLookupError::Download(error.to_string()))?;
    if let Some(expected_size) = expected.size
        && let Some(content_length) = response.content_length()
        && content_length > expected_size
    {
        return Err(JavaRuntimeLookupError::Download(
            RuntimeDownloadIntegrityError::SizeMismatch {
                file: bounded_manifest_file_label(relative_path),
                expected: expected_size,
                actual: content_length,
            }
            .to_string(),
        ));
    }
    let mut output = async_fs::File::create(temp_path)
        .await
        .map_err(|error| JavaRuntimeLookupError::Download(error.to_string()))?;
    let mut stream = response.bytes_stream();
    let mut hasher = Sha1::new();
    let mut actual_size = 0_u64;

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| JavaRuntimeLookupError::Download(error.to_string()))?;
        let next_size = actual_size.saturating_add(chunk.len() as u64);
        if let Some(expected_size) = expected.size
            && next_size > expected_size
        {
            return Err(JavaRuntimeLookupError::Download(
                RuntimeDownloadIntegrityError::SizeMismatch {
                    file: bounded_manifest_file_label(relative_path),
                    expected: expected_size,
                    actual: next_size,
                }
                .to_string(),
            ));
        }
        output
            .write_all(&chunk)
            .await
            .map_err(|error| JavaRuntimeLookupError::Download(error.to_string()))?;
        hasher.update(&chunk);
        actual_size = next_size;
    }
    output
        .flush()
        .await
        .map_err(|error| JavaRuntimeLookupError::Download(error.to_string()))?;
    output
        .sync_all()
        .await
        .map_err(|error| JavaRuntimeLookupError::Download(error.to_string()))?;

    let actual = RuntimeDownloadActual {
        size: actual_size,
        sha1: format!("{:x}", hasher.finalize()),
    };
    verify_runtime_download(relative_path, expected, &actual)
        .map_err(|error| JavaRuntimeLookupError::Download(error.to_string()))
}

fn runtime_download_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .user_agent("croopor/0.3")
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RuntimeDownloadEvidence {
    size: Option<u64>,
    sha1: Option<String>,
}

impl From<&ComponentManifestDownload> for RuntimeDownloadEvidence {
    fn from(download: &ComponentManifestDownload) -> Self {
        Self {
            size: download.size,
            sha1: download.sha1.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RuntimeDownloadActual {
    size: u64,
    sha1: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RuntimeDownloadIntegrityError {
    SizeMismatch {
        file: String,
        expected: u64,
        actual: u64,
    },
    Sha1Mismatch {
        file: String,
        expected: String,
        actual: String,
    },
}

impl std::fmt::Display for RuntimeDownloadIntegrityError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SizeMismatch {
                file,
                expected,
                actual,
            } => write!(
                formatter,
                "runtime file {file} size mismatch: expected {expected}, got {actual}"
            ),
            Self::Sha1Mismatch {
                file,
                expected,
                actual,
            } => write!(
                formatter,
                "runtime file {file} sha1 mismatch: expected {expected}, got {actual}"
            ),
        }
    }
}

fn verify_runtime_download(
    relative_path: &str,
    expected: &RuntimeDownloadEvidence,
    actual: &RuntimeDownloadActual,
) -> Result<(), RuntimeDownloadIntegrityError> {
    let file = bounded_manifest_file_label(relative_path);
    if let Some(expected_size) = expected.size
        && actual.size != expected_size
    {
        return Err(RuntimeDownloadIntegrityError::SizeMismatch {
            file,
            expected: expected_size,
            actual: actual.size,
        });
    }

    if let Some(expected_sha1) = expected.sha1.as_deref() {
        let expected_sha1 = expected_sha1.trim();
        if !actual.sha1.eq_ignore_ascii_case(expected_sha1) {
            return Err(RuntimeDownloadIntegrityError::Sha1Mismatch {
                file,
                expected: expected_sha1.to_string(),
                actual: actual.sha1.clone(),
            });
        }
    }

    Ok(())
}

fn bounded_manifest_file_label(relative_path: &str) -> String {
    const MAX_LABEL_CHARS: usize = 120;
    let sanitized = relative_path.replace(['\r', '\n'], "?");
    let mut chars = sanitized.chars();
    let label = chars.by_ref().take(MAX_LABEL_CHARS).collect::<String>();
    if chars.next().is_some() {
        format!("{label}...")
    } else {
        label
    }
}

async fn fetch_runtime_json<T>(url: &str) -> Result<T, JavaRuntimeLookupError>
where
    T: serde::de::DeserializeOwned,
{
    let response = runtime_manifest_client()
        .get(url)
        .send()
        .await
        .map_err(|error| JavaRuntimeLookupError::Download(error.to_string()))?;
    let status = response.status();
    if !status.is_success() {
        return Err(JavaRuntimeLookupError::Download(format!("HTTP {status}")));
    }
    if response
        .content_length()
        .is_some_and(|content_length| content_length > MAX_RUNTIME_MANIFEST_BYTES)
    {
        return Err(JavaRuntimeLookupError::Download(
            "runtime manifest response too large".to_string(),
        ));
    }

    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| JavaRuntimeLookupError::Download(error.to_string()))?;
        if body.len() as u64 + chunk.len() as u64 > MAX_RUNTIME_MANIFEST_BYTES {
            return Err(JavaRuntimeLookupError::Download(
                "runtime manifest response too large".to_string(),
            ));
        }
        body.extend_from_slice(&chunk);
    }

    serde_json::from_slice::<T>(&body)
        .map_err(|error| JavaRuntimeLookupError::Download(error.to_string()))
}

fn runtime_manifest_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .user_agent("croopor/0.3")
            .build()
            .unwrap_or_else(|_| reqwest::Client::new())
    })
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

#[derive(Debug, Clone, Deserialize)]
struct ComponentManifestFile {
    #[serde(rename = "type")]
    kind: String,
    #[cfg_attr(not(unix), allow(dead_code))]
    #[serde(default)]
    executable: bool,
    #[serde(default)]
    downloads: Option<ComponentManifestDownloads>,
}

#[derive(Debug, Clone, Deserialize)]
struct ComponentManifestDownloads {
    #[serde(default)]
    raw: Option<ComponentManifestDownload>,
}

#[derive(Debug, Clone, Deserialize)]
struct ComponentManifestDownload {
    url: String,
    #[serde(default)]
    sha1: Option<String>,
    #[serde(default)]
    size: Option<u64>,
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
    const IDENTITY_PROPERTIES: [&str; 6] = [
        "java.vendor",
        "java.vm.vendor",
        "java.vm.name",
        "java.runtime.name",
        "java.runtime.version",
        "java.vm.version",
    ];

    let identities = text
        .lines()
        .filter_map(|line| line.trim().split_once('='))
        .filter_map(|(key, value)| {
            let key = key.trim();
            IDENTITY_PROPERTIES
                .iter()
                .any(|property| key.eq_ignore_ascii_case(property))
                .then(|| value.trim().to_uppercase())
        })
        .collect::<Vec<_>>();

    let contains_identity = |needles: &[&str]| {
        identities
            .iter()
            .any(|identity| needles.iter().any(|needle| identity.contains(needle)))
    };

    match () {
        _ if contains_identity(&["GRAALVM"]) => "graalvm".to_string(),
        _ if contains_identity(&["OPENJ9", "SEMERU", "IBM"]) => "openj9".to_string(),
        _ if contains_identity(&["TEMURIN", "ECLIPSE", "ADOPTIUM"]) => "temurin".to_string(),
        _ if contains_identity(&["ORACLE"]) => "oracle".to_string(),
        _ if contains_identity(&["OPENJDK"]) => "openjdk".to_string(),
        _ => "unknown".to_string(),
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

#[cfg(test)]
mod tests {
    use super::{
        ComponentManifestDownload, ComponentManifestDownloads, ComponentManifestFile,
        JavaRuntimeLookupError, RuntimeDownloadActual, RuntimeDownloadEvidence,
        RuntimeDownloadIntegrityError, RuntimeInstallState, component_manifest_destination,
        detect_distribution, detect_runtime_state, ensure_java_runtime, fetch_runtime_file,
        fetch_runtime_json, install_runtime_manifest_file, java_executable,
        plan_runtime_manifest_files, remove_runtime_install_path,
        remove_runtime_install_path_async, runtime_download_client,
        runtime_file_download_concurrency_for, runtime_install_lock_from_map,
        verify_runtime_download,
    };
    use crate::JavaVersion;
    use serde::Deserialize;
    use std::collections::HashMap;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn expected(size: Option<u64>, sha1: Option<&str>) -> RuntimeDownloadEvidence {
        RuntimeDownloadEvidence {
            size,
            sha1: sha1.map(str::to_string),
        }
    }

    fn actual(size: u64, sha1: &str) -> RuntimeDownloadActual {
        RuntimeDownloadActual {
            size,
            sha1: sha1.to_string(),
        }
    }

    fn manifest_file(kind: &str) -> ComponentManifestFile {
        ComponentManifestFile {
            kind: kind.to_string(),
            executable: false,
            downloads: None,
        }
    }

    fn downloadable_manifest_file(url: &str, size: u64, sha1: &str) -> ComponentManifestFile {
        ComponentManifestFile {
            kind: "file".to_string(),
            executable: false,
            downloads: Some(ComponentManifestDownloads {
                raw: Some(ComponentManifestDownload {
                    url: url.to_string(),
                    sha1: Some(sha1.to_string()),
                    size: Some(size),
                }),
            }),
        }
    }

    fn planned_paths(entries: &[(String, ComponentManifestFile)]) -> Vec<&str> {
        entries
            .iter()
            .map(|(relative_path, _)| relative_path.as_str())
            .collect()
    }

    fn unsafe_manifest_path_message(result: Result<PathBuf, JavaRuntimeLookupError>) -> String {
        match result {
            Err(JavaRuntimeLookupError::Download(message)) => message,
            other => panic!("expected unsafe manifest path error, got {other:?}"),
        }
    }

    fn assert_runtime_distribution(text: &str, expected: &str) {
        assert_eq!(detect_distribution(text), expected);
    }

    #[test]
    fn detect_runtime_distribution_favors_graalvm_identity() {
        assert_runtime_distribution(
            r#"
                java.vendor = Oracle Corporation
                java.vm.name = GraalVM 64-Bit Server VM
            "#,
            "graalvm",
        );
        assert_runtime_distribution(
            r#"
                java.vendor = OpenJDK
                java.runtime.name = GraalVM Runtime Environment
            "#,
            "graalvm",
        );
    }

    #[test]
    fn detect_runtime_distribution_classifies_openj9_identity() {
        for text in [
            "java.vm.name = Eclipse OpenJ9 VM",
            "java.runtime.name = IBM Semeru Runtime Open Edition",
            "java.vm.vendor = IBM Corporation",
        ] {
            assert_runtime_distribution(text, "openj9");
        }
    }

    #[test]
    fn detect_runtime_distribution_classifies_temurin_identity() {
        for text in [
            "java.runtime.name = OpenJDK Runtime Environment Temurin-21.0.2+13",
            "java.vendor = Eclipse Adoptium",
            "java.vm.vendor = Eclipse Foundation",
        ] {
            assert_runtime_distribution(text, "temurin");
        }
    }

    #[test]
    fn detect_runtime_distribution_classifies_oracle_identity() {
        assert_runtime_distribution(
            r#"
                java.vendor   =   Oracle Corporation
                java.vm.name = Java HotSpot(TM) 64-Bit Server VM
            "#,
            "oracle",
        );
    }

    #[test]
    fn detect_runtime_distribution_classifies_generic_openjdk_identity() {
        assert_runtime_distribution(
            r#"
                java.vendor = Debian
                java.vm.name = OpenJDK 64-Bit Server VM
                java.runtime.version = 21.0.5+11-Debian-1
            "#,
            "openjdk",
        );
    }

    #[test]
    fn detect_runtime_distribution_classifies_missing_identity_as_unknown() {
        assert_runtime_distribution(
            r#"
                java.home = /opt/java
                sun.arch.data.model = 64
            "#,
            "unknown",
        );
    }

    #[test]
    fn component_manifest_destination_accepts_safe_nested_path() {
        let temp_dir = Path::new("runtime-temp");
        let destination = component_manifest_destination(temp_dir, "bin/java").unwrap();

        assert_eq!(destination, temp_dir.join("bin").join("java"));
    }

    #[test]
    fn component_manifest_destination_rejects_traversal() {
        let temp_dir = Path::new("runtime-temp");
        let message =
            unsafe_manifest_path_message(component_manifest_destination(temp_dir, "bin/../java"));

        assert!(message.contains("unsafe runtime manifest path"));
        assert!(message.contains("bin/../java"));
        assert!(!message.contains("runtime-temp"));
    }

    #[test]
    fn component_manifest_destination_rejects_absolute_path() {
        let temp_dir = Path::new("runtime-temp");
        let absolute_path = if cfg!(windows) {
            r"\Windows\System32"
        } else {
            "/etc/passwd"
        };
        let message =
            unsafe_manifest_path_message(component_manifest_destination(temp_dir, absolute_path));

        assert!(message.contains("unsafe runtime manifest path"));
        assert!(message.contains(absolute_path));
        assert!(!message.contains("runtime-temp"));
    }

    #[test]
    fn component_manifest_destination_rejects_drive_like_path_with_slashes() {
        let temp_dir = Path::new("runtime-temp");
        let message = unsafe_manifest_path_message(component_manifest_destination(
            temp_dir,
            "C:/Windows/System32",
        ));

        assert!(message.contains("unsafe runtime manifest path"));
        assert!(message.contains("C:/Windows/System32"));
        assert!(!message.contains("runtime-temp"));
    }

    #[test]
    fn component_manifest_destination_rejects_drive_like_path_with_backslashes() {
        let temp_dir = Path::new("runtime-temp");
        let message = unsafe_manifest_path_message(component_manifest_destination(
            temp_dir,
            r"C:\Windows\System32",
        ));

        assert!(message.contains("unsafe runtime manifest path"));
        assert!(message.contains(r"C:\Windows\System32"));
        assert!(!message.contains("runtime-temp"));
    }

    #[test]
    fn runtime_file_download_concurrency_is_adaptive_and_bounded() {
        assert_eq!(runtime_file_download_concurrency_for(0), 2);
        assert_eq!(runtime_file_download_concurrency_for(1), 2);
        assert_eq!(runtime_file_download_concurrency_for(2), 4);
        assert_eq!(runtime_file_download_concurrency_for(3), 6);
        assert_eq!(runtime_file_download_concurrency_for(4), 8);
        assert_eq!(runtime_file_download_concurrency_for(64), 8);
    }

    #[test]
    fn runtime_manifest_install_plan_sorts_directories_before_files() {
        let mut files = HashMap::new();
        files.insert("lib/server/libjvm.so".to_string(), manifest_file("file"));
        files.insert("bin/java".to_string(), manifest_file("file"));
        files.insert("lib/server".to_string(), manifest_file("directory"));
        files.insert("bin".to_string(), manifest_file("directory"));
        files.insert("ignored-entry".to_string(), manifest_file("link"));

        let plan = plan_runtime_manifest_files(files);

        assert_eq!(
            planned_paths(&plan.directory_entries),
            vec!["bin", "lib/server"]
        );
        assert_eq!(
            planned_paths(&plan.file_entries),
            vec!["bin/java", "lib/server/libjvm.so"]
        );
        assert_eq!(planned_paths(&plan.other_entries), vec!["ignored-entry"]);
    }

    #[tokio::test]
    async fn runtime_manifest_json_fetch_reads_async_http_body() {
        #[derive(Debug, Deserialize)]
        struct SampleRuntimeManifest {
            ok: bool,
        }

        let url = serve_runtime_json(200, r#"{"ok":true}"#.as_bytes().to_vec(), None).await;

        let manifest = fetch_runtime_json::<SampleRuntimeManifest>(&url)
            .await
            .expect("runtime manifest json");

        assert!(manifest.ok);
    }

    #[tokio::test]
    async fn runtime_manifest_json_fetch_rejects_http_errors() {
        let url = serve_runtime_json(503, b"unavailable".to_vec(), None).await;

        let error = fetch_runtime_json::<serde_json::Value>(&url)
            .await
            .expect_err("HTTP error should fail");

        assert!(error.to_string().contains("HTTP 503"), "{error}");
    }

    #[tokio::test]
    async fn runtime_manifest_json_fetch_rejects_oversized_content_length() {
        let url = serve_runtime_json(
            200,
            b"ignored".to_vec(),
            Some(super::MAX_RUNTIME_MANIFEST_BYTES + 1),
        )
        .await;

        let error = fetch_runtime_json::<serde_json::Value>(&url)
            .await
            .expect_err("oversized manifest should fail");

        assert_eq!(
            error.to_string(),
            "failed to install java runtime: runtime manifest response too large"
        );
    }

    #[test]
    fn runtime_install_lock_recovers_from_poisoned_map_lock() {
        let locks = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let seeded_install_lock = Arc::new(tokio::sync::Mutex::new(()));
        let poison_target = Arc::clone(&locks);
        let poison_seed = Arc::clone(&seeded_install_lock);

        let _ = std::thread::spawn(move || {
            let mut guard = poison_target.lock().unwrap();
            guard.insert("java-runtime-delta".to_string(), poison_seed);
            panic!("poison runtime lock map");
        })
        .join();

        assert!(locks.is_poisoned());
        let recovered_lock = runtime_install_lock_from_map(&locks, "java-runtime-delta");

        assert!(Arc::ptr_eq(&recovered_lock, &seeded_install_lock));
    }

    #[test]
    fn managed_runtime_requires_ready_marker_even_when_java_exists() {
        let root = unique_temp_root("croopor-managed-runtime-ready-marker-test");
        write_runtime_executable_fixture(&root);

        assert_eq!(
            detect_runtime_state(&root, true),
            RuntimeInstallState::Broken
        );

        fs::write(root.join(".croopor-installing"), b"installing").expect("installing marker");
        assert_eq!(
            detect_runtime_state(&root, true),
            RuntimeInstallState::Installing
        );

        fs::remove_file(root.join(".croopor-installing")).expect("remove installing marker");
        fs::write(root.join(".croopor-ready"), b"ready").expect("ready marker");
        assert_eq!(
            detect_runtime_state(&root, true),
            RuntimeInstallState::Ready
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn runtime_install_cleanup_removes_stale_directory_destination() {
        let root = unique_temp_root("croopor-runtime-cleanup-dir-test");
        fs::create_dir_all(root.join("bin")).expect("create stale runtime dir");
        fs::write(root.join("bin").join("java"), b"stale").expect("write stale java");

        remove_runtime_install_path(&root).expect("remove stale runtime dir");

        assert!(!root.exists());
    }

    #[test]
    fn runtime_install_cleanup_removes_stale_file_destination() {
        let root = unique_temp_root("croopor-runtime-cleanup-file-test");
        fs::write(&root, b"blocking file").expect("write stale runtime file");

        remove_runtime_install_path(&root).expect("remove stale runtime file");

        assert!(!root.exists());
    }

    #[test]
    fn runtime_install_cleanup_accepts_missing_destination() {
        let root = unique_temp_root("croopor-runtime-cleanup-missing-test");

        remove_runtime_install_path(&root).expect("missing runtime path is clean");

        assert!(!root.exists());
    }

    #[tokio::test]
    async fn async_runtime_install_cleanup_removes_stale_directory_destination() {
        let root = unique_temp_root("croopor-runtime-async-cleanup-dir-test");
        fs::create_dir_all(root.join("bin")).expect("create stale runtime dir");
        fs::write(root.join("bin").join("java"), b"stale").expect("write stale java");

        remove_runtime_install_path_async(&root)
            .await
            .expect("remove stale runtime dir");

        assert!(!root.exists());
    }

    #[tokio::test]
    async fn async_runtime_install_cleanup_removes_stale_file_destination() {
        let root = unique_temp_root("croopor-runtime-async-cleanup-file-test");
        fs::write(&root, b"blocking file").expect("write stale runtime file");

        remove_runtime_install_path_async(&root)
            .await
            .expect("remove stale runtime file");

        assert!(!root.exists());
    }

    #[tokio::test]
    async fn async_runtime_install_cleanup_accepts_missing_destination() {
        let root = unique_temp_root("croopor-runtime-async-cleanup-missing-test");

        remove_runtime_install_path_async(&root)
            .await
            .expect("missing runtime path is clean");

        assert!(!root.exists());
    }

    #[test]
    fn bundled_runtime_keeps_executable_readiness_without_marker() {
        let root = unique_temp_root("croopor-bundled-runtime-ready-test");
        write_runtime_executable_fixture(&root);

        assert_eq!(
            detect_runtime_state(&root, false),
            RuntimeInstallState::Ready
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn runtime_download_verification_accepts_matching_metadata() {
        let result = verify_runtime_download(
            "bin/java",
            &expected(Some(5), Some("AAF4C61DDCC5E8A2DABEDE0F3B482CD9AEA9434D")),
            &actual(5, "aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d"),
        );

        assert_eq!(result, Ok(()));
    }

    #[test]
    fn runtime_download_verification_rejects_size_mismatch() {
        let result = verify_runtime_download(
            "bin/java",
            &expected(Some(6), None),
            &actual(5, "aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d"),
        );

        assert_eq!(
            result,
            Err(RuntimeDownloadIntegrityError::SizeMismatch {
                file: "bin/java".to_string(),
                expected: 6,
                actual: 5,
            })
        );
    }

    #[test]
    fn runtime_download_verification_rejects_sha1_mismatch() {
        let result = verify_runtime_download(
            "bin/java",
            &expected(None, Some("0000000000000000000000000000000000000000")),
            &actual(5, "aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d"),
        );

        assert_eq!(
            result,
            Err(RuntimeDownloadIntegrityError::Sha1Mismatch {
                file: "bin/java".to_string(),
                expected: "0000000000000000000000000000000000000000".to_string(),
                actual: "aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d".to_string(),
            })
        );
    }

    #[test]
    fn runtime_download_verification_accepts_missing_metadata() {
        let result = verify_runtime_download(
            "bin/java",
            &expected(None, None),
            &actual(5, "aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d"),
        );

        assert_eq!(result, Ok(()));
    }

    #[tokio::test]
    async fn runtime_file_download_streams_and_verifies_to_temp() {
        let root = unique_temp_root("croopor-runtime-download-stream-test");
        fs::create_dir_all(&root).expect("download root");
        let temp_path = root.join("java.croopor-tmp");
        let url = serve_runtime_download(b"hello".to_vec()).await;
        let client = runtime_download_client();

        fetch_runtime_file(
            &client,
            &url,
            &temp_path,
            expected(Some(5), Some("aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d")),
            "bin/java",
        )
        .await
        .expect("runtime download");

        assert_eq!(fs::read(&temp_path).expect("downloaded file"), b"hello");
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn runtime_file_download_removes_temp_on_verification_error() {
        let root = unique_temp_root("croopor-runtime-download-cleanup-test");
        fs::create_dir_all(&root).expect("download root");
        let temp_path = root.join("java.croopor-tmp");
        let url = serve_runtime_download(b"hello".to_vec()).await;
        let client = runtime_download_client();

        let result = fetch_runtime_file(
            &client,
            &url,
            &temp_path,
            expected(Some(6), None),
            "bin/java",
        )
        .await;

        assert!(matches!(&result, Err(JavaRuntimeLookupError::Download(_))));
        assert!(!temp_path.exists());
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn runtime_file_download_rejects_oversized_content_length() {
        let root = unique_temp_root("croopor-runtime-download-content-length-test");
        fs::create_dir_all(&root).expect("download root");
        let temp_path = root.join("java.croopor-tmp");
        let url = serve_runtime_response(200, b"hello".to_vec(), Some(6), "/runtime.bin").await;
        let client = runtime_download_client();

        let result = fetch_runtime_file(
            &client,
            &url,
            &temp_path,
            expected(Some(5), None),
            "bin/java",
        )
        .await;

        assert!(matches!(&result, Err(JavaRuntimeLookupError::Download(_))));
        assert!(!temp_path.exists());
        assert!(
            result
                .expect_err("oversized content length should fail")
                .to_string()
                .contains("runtime file bin/java size mismatch: expected 5, got 6")
        );
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn runtime_file_download_rejects_stream_past_expected_size_and_removes_temp() {
        let root = unique_temp_root("croopor-runtime-download-stream-bound-test");
        fs::create_dir_all(&root).expect("download root");
        let temp_path = root.join("java.croopor-tmp");
        let url = serve_runtime_response(200, b"hello!".to_vec(), None, "/runtime.bin").await;
        let client = runtime_download_client();

        let result = fetch_runtime_file(
            &client,
            &url,
            &temp_path,
            expected(Some(5), None),
            "bin/java",
        )
        .await;

        assert!(matches!(&result, Err(JavaRuntimeLookupError::Download(_))));
        assert!(!temp_path.exists());
        assert!(
            result
                .expect_err("oversized stream should fail")
                .to_string()
                .contains("runtime file bin/java size mismatch: expected 5, got 6")
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn runtime_install_futures_stay_small_enough_for_tokio_workers() {
        let root = Path::new("/tmp/croopor-runtime-future-size");
        let client = runtime_download_client();
        let expected = expected(Some(8), Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"));
        let file = downloadable_manifest_file(
            "https://example.test/runtime.bin",
            8,
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        );
        let spawned_client = client.clone();
        let spawned_root = root.to_path_buf();
        let spawned_file = file.clone();
        let spawned_future = async move {
            Box::pin(install_runtime_manifest_file(
                spawned_client,
                &spawned_root,
                "bin/java",
                spawned_file,
            ))
            .await
        };

        assert!(
            std::mem::size_of_val(&fetch_runtime_file(
                &client,
                "https://example.test/runtime.bin",
                &root.join("java.croopor-tmp"),
                expected,
                "bin/java",
            )) < 4096,
            "runtime file download future should stay small"
        );
        assert!(
            std::mem::size_of_val(&install_runtime_manifest_file(
                client.clone(),
                root,
                "bin/java",
                file.clone(),
            )) < 4096,
            "runtime manifest file install future should stay small"
        );
        assert!(
            std::mem::size_of_val(&spawned_future) < 4096,
            "spawned runtime manifest file install future should stay small"
        );
        assert!(
            std::mem::size_of_val(&ensure_java_runtime(
                root,
                &JavaVersion {
                    component: "java-runtime-delta".to_string(),
                    major_version: 21,
                },
                "",
            )) < 4096,
            "managed-runtime ensure future should stay small"
        );
    }

    fn unique_temp_root(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "{label}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ))
    }

    fn write_runtime_executable_fixture(root: &Path) {
        let java = java_executable(root);
        fs::create_dir_all(java.parent().expect("java parent")).expect("java parent dir");
        fs::write(&java, b"java").expect("java executable");
        if cfg!(target_os = "windows") {
            let config = root.join("lib").join("jvm.cfg");
            fs::create_dir_all(config.parent().expect("config parent")).expect("config parent dir");
            fs::write(config, b"jvm").expect("runtime config");
        }
    }

    async fn serve_runtime_download(body: Vec<u8>) -> String {
        let content_length = body.len() as u64;
        serve_runtime_response(200, body, Some(content_length), "/runtime.bin").await
    }

    async fn serve_runtime_json(status: u16, body: Vec<u8>, content_length: Option<u64>) -> String {
        let content_length = content_length.unwrap_or(body.len() as u64);
        serve_runtime_response(status, body, Some(content_length), "/runtime.json").await
    }

    async fn serve_runtime_response(
        status: u16,
        body: Vec<u8>,
        content_length: Option<u64>,
        path: &str,
    ) -> String {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("runtime test listener");
        let address = listener
            .local_addr()
            .expect("runtime test listener address");
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("runtime test connection");
            let mut request = [0_u8; 1024];
            let _ = socket.read(&mut request).await;
            let reason = if status == 200 { "OK" } else { "Error" };
            let headers = if let Some(content_length) = content_length {
                format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Length: {content_length}\r\nConnection: close\r\n\r\n"
                )
            } else {
                format!("HTTP/1.1 {status} {reason}\r\nConnection: close\r\n\r\n")
            };
            socket
                .write_all(headers.as_bytes())
                .await
                .expect("runtime test response headers");
            socket
                .write_all(&body)
                .await
                .expect("runtime test response body");
        });
        format!("http://{address}{path}")
    }
}
