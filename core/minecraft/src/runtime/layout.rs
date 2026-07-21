use super::file_download::runtime_filesystem_path;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

#[derive(Clone)]
pub struct ManagedRuntimeCache {
    inner: Arc<ManagedRuntimeCacheInner>,
}

struct ManagedRuntimeCacheInner {
    root: PathBuf,
    install_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    #[cfg(any(test, feature = "test-support"))]
    _test_root: Option<tempfile::TempDir>,
}

impl std::fmt::Debug for ManagedRuntimeCache {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ManagedRuntimeCache")
            .finish_non_exhaustive()
    }
}

impl ManagedRuntimeCache {
    pub fn from_root(root: impl Into<PathBuf>) -> std::io::Result<Self> {
        let root = root.into();
        if !root.is_absolute() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "managed runtime root must be absolute",
            ));
        }
        Ok(Self {
            inner: Arc::new(ManagedRuntimeCacheInner {
                root,
                install_locks: Mutex::new(HashMap::new()),
                #[cfg(any(test, feature = "test-support"))]
                _test_root: None,
            }),
        })
    }

    pub fn root(&self) -> &Path {
        &self.inner.root
    }

    pub fn shares_identity_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn isolated_for_test() -> std::io::Result<Self> {
        let test_root = tempfile::Builder::new()
            .prefix("axial-managed-runtime-")
            .tempdir()?;
        let root = test_root.path().to_path_buf();
        Ok(Self {
            inner: Arc::new(ManagedRuntimeCacheInner {
                root,
                install_locks: Mutex::new(HashMap::new()),
                _test_root: Some(test_root),
            }),
        })
    }

    pub(super) fn install_lock(&self, component: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = match self.inner.install_locks.lock() {
            Ok(locks) => locks,
            Err(poisoned) => poisoned.into_inner(),
        };
        locks
            .entry(component.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
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

pub(crate) fn runtime_java_relative_path() -> &'static str {
    if cfg!(target_os = "windows") {
        "bin/javaw.exe"
    } else if cfg!(target_os = "macos") {
        "jre.bundle/Contents/Home/bin/java"
    } else {
        "bin/java"
    }
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
