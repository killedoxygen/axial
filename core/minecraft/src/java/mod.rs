use crate::paths::runtime_dirs;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;
use thiserror::Error;

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

#[derive(Debug, Error)]
pub enum JavaRuntimeLookupError {
    #[error("java runtime not found: {component} (Java {major}) not installed")]
    NotFound { component: String, major: i32 },
    #[error("failed to probe java runtime: {0}")]
    Probe(String),
}

pub fn list_java_runtimes(mc_dir: &Path) -> Vec<JavaRuntimeResult> {
    let components = [
        "java-runtime-epsilon",
        "java-runtime-delta",
        "java-runtime-gamma",
        "java-runtime-beta",
        "java-runtime-alpha",
        "jre-legacy",
    ];
    let mut dirs = runtime_dirs(mc_dir);
    dirs.push(runtime_cache_dir());

    let mut results = Vec::new();
    for dir in dirs {
        for component in components {
            if let Some(runtime) = search_exact_runtime(&dir, component) {
                results.push(runtime);
            }
        }
    }

    results
}

pub fn find_java_runtime(
    mc_dir: &Path,
    java_version: &crate::launch::JavaVersion,
    override_path: &str,
) -> Result<JavaRuntimeResult, JavaRuntimeLookupError> {
    let override_path = override_path.trim();
    if !override_path.is_empty() && Path::new(override_path).is_file() {
        return Ok(JavaRuntimeResult {
            path: override_path.to_string(),
            component: preferred_java_component(java_version),
            source: "override".to_string(),
        });
    }

    let override_component = (!override_path.is_empty()
        && matches!(
            override_path,
            "java-runtime-epsilon"
                | "java-runtime-delta"
                | "java-runtime-gamma"
                | "java-runtime-beta"
                | "java-runtime-alpha"
                | "jre-legacy"
        ))
    .then(|| override_path.to_string());
    if !override_path.is_empty() && override_component.is_none() {
        return Err(JavaRuntimeLookupError::NotFound {
            component: override_path.to_string(),
            major: java_version.major_version,
        });
    }

    let used_override_component = override_component.is_some();
    let component = override_component.unwrap_or_else(|| preferred_java_component(java_version));
    let mut dirs = runtime_dirs(mc_dir);
    dirs.push(runtime_cache_dir());
    for dir in dirs {
        if let Some(runtime) = search_exact_runtime(&dir, &component) {
            return Ok(if !used_override_component {
                runtime
            } else {
                JavaRuntimeResult {
                    source: "override".to_string(),
                    ..runtime
                }
            });
        }
    }

    Err(JavaRuntimeLookupError::NotFound {
        component,
        major: java_version.major_version,
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

fn preferred_java_component(java_version: &crate::launch::JavaVersion) -> String {
    if java_version.component.is_empty() {
        "java-runtime-delta".to_string()
    } else {
        java_version.component.clone()
    }
}

fn search_exact_runtime(base_dir: &Path, component: &str) -> Option<JavaRuntimeResult> {
    if !base_dir.exists() {
        return None;
    }

    let os_arch = runtime_os_arch();
    for candidate in [
        base_dir.join(component).join(&os_arch).join(component),
        base_dir.join(component),
    ] {
        let java_exe = java_executable(&candidate);
        if runtime_executable_ready(&java_exe) {
            let source = if base_dir.to_string_lossy().contains("Packages") {
                "ms-store"
            } else if base_dir.to_string_lossy().contains("croopor") {
                "croopor"
            } else {
                "minecraft-runtime"
            };

            return Some(JavaRuntimeResult {
                path: java_exe.to_string_lossy().to_string(),
                component: component.to_string(),
                source: source.to_string(),
            });
        }
    }

    None
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
