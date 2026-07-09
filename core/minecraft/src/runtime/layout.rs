use super::file_download::runtime_filesystem_path;
use std::path::{Path, PathBuf};

pub(super) fn runtime_cache_dir() -> PathBuf {
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
pub(super) fn runtime_os_arch() -> String {
    runtime_os_arch_for(std::env::consts::OS, std::env::consts::ARCH)
}

pub(super) fn runtime_os_arch_for(target_os: &str, target_arch: &str) -> String {
    match target_os {
        "windows" => format!("windows-{}", runtime_arch_name(target_arch)),
        "macos" => match target_arch {
            "aarch64" => "mac-os-arm64".to_string(),
            _ => "mac-os".to_string(),
        },
        _ => match target_arch {
            "x86" => "linux-i386".to_string(),
            _ => "linux".to_string(),
        },
    }
}

pub(super) fn runtime_platform_fallbacks(primary_platform: &str) -> &'static [&'static str] {
    match primary_platform {
        "mac-os-arm64" => &["mac-os"],
        "windows-arm64" => &["windows-x64"],
        _ => &[],
    }
}

fn runtime_arch_name(target_arch: &str) -> &str {
    match target_arch {
        "x86_64" => "x64",
        "x86" => "x86",
        "aarch64" => "arm64",
        other => other,
    }
}

pub(super) fn java_executable(runtime_root: &Path) -> PathBuf {
    java_executable_for_os(runtime_root, std::env::consts::OS)
}

pub(super) fn java_executable_for_os(runtime_root: &Path, target_os: &str) -> PathBuf {
    match target_os {
        "windows" => runtime_root.join("bin").join("javaw.exe"),
        "macos" => runtime_root
            .join("jre.bundle")
            .join("Contents")
            .join("Home")
            .join("bin")
            .join("java"),
        _ => runtime_root.join("bin").join("java"),
    }
}

pub(super) fn runtime_executable_ready(java_exe: &Path) -> bool {
    if !runtime_filesystem_path(java_exe).as_ref().is_file() {
        return false;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        runtime_filesystem_path(java_exe)
            .as_ref()
            .metadata()
            .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }

    #[cfg(windows)]
    {
        let runtime_root = java_exe
            .parent()
            .and_then(Path::parent)
            .map(Path::to_path_buf)
            .unwrap_or_default();
        return runtime_config_candidates(&runtime_root)
            .into_iter()
            .any(|candidate| runtime_filesystem_path(&candidate).as_ref().is_file());
    }

    #[cfg(not(any(unix, windows)))]
    {
        true
    }
}

#[cfg(windows)]
pub(super) fn runtime_config_candidates(runtime_root: &Path) -> Vec<PathBuf> {
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
