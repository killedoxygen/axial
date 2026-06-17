use super::layout::{
    java_executable, runtime_cache_dir, runtime_executable_ready, runtime_os_arch,
};
use super::model::{
    JavaRuntimeInfo, JavaRuntimeLookupError, JavaRuntimeResult, RuntimeId, RuntimeInstallState,
    RuntimeOverride, RuntimeRecord, RuntimeRequirement, RuntimeSource,
};
use super::probe::probe_java_runtime_info;
use crate::launch::JavaVersion;
use crate::paths::runtime_dirs;
use std::path::{Path, PathBuf};

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

pub(super) fn resolve_component_runtime(
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

pub(super) fn component_runtime_ready_without_probe(base_dir: &Path, component: &str) -> bool {
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

pub(super) fn resolve_managed_runtime(
    library_dir: &Path,
    component: &RuntimeId,
) -> Result<RuntimeRecord, JavaRuntimeLookupError> {
    resolve_component_runtime(library_dir, component, 0)
}

pub(super) fn resolve_override_runtime(
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
pub(super) fn inspect_component_runtime(base_dir: &Path, component: &str) -> Option<RuntimeRecord> {
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

pub(super) fn runtime_requires_ready_marker(base_dir: &Path) -> bool {
    base_dir == runtime_cache_dir()
}

pub(super) fn classify_runtime_source(base_dir: &Path) -> RuntimeSource {
    let label = base_dir.to_string_lossy();
    if label.contains("Packages") {
        RuntimeSource::MicrosoftStore
    } else if label.contains("croopor") {
        RuntimeSource::Managed
    } else {
        RuntimeSource::MinecraftBundled
    }
}

pub(super) fn detect_runtime_state(
    runtime_root: &Path,
    require_ready_marker: bool,
) -> RuntimeInstallState {
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
