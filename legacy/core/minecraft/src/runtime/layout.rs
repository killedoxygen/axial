use super::file_download::runtime_filesystem_path;
use crate::loaders::types::LoaderError;
use crate::managed_fs::{ManagedDir, ManagedExecutableGuard};
use crate::portable_path::PortableRelativePath;
use axial_fs::Directory;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

#[derive(Clone)]
pub struct ManagedRuntimeCache {
    inner: Arc<ManagedRuntimeCacheInner>,
}

#[derive(Clone)]
pub struct ManagedRuntimeComponent {
    pub(super) cache: ManagedRuntimeCache,
    pub(super) component: String,
    pub(super) root: ManagedDir,
    pub(super) root_path: PathBuf,
}

#[derive(Clone)]
pub struct ManagedRuntimeLaunchReceipt {
    inner: Arc<ManagedRuntimeLaunchReceiptInner>,
}

struct ManagedRuntimeLaunchReceiptInner {
    component: ManagedRuntimeComponent,
    java_guard: ManagedExecutableGuard,
    program_path: PathBuf,
}

impl std::fmt::Debug for ManagedRuntimeLaunchReceipt {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ManagedRuntimeLaunchReceipt")
            .field("component", &self.inner.component.component)
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for ManagedRuntimeComponent {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ManagedRuntimeComponent")
            .field("component", &self.component)
            .finish_non_exhaustive()
    }
}

struct ManagedRuntimeCacheInner {
    root_path: PathBuf,
    root: ManagedDir,
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
    pub fn from_directory(directory: Directory, root_path: PathBuf) -> std::io::Result<Self> {
        if !root_path.is_absolute() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "managed runtime projection path must be absolute",
            ));
        }
        directory.validate_absolute_projection(&root_path)?;
        let effects = directory.create_effect_owner()?;
        let root = ManagedDir::from_directory_at_path(directory, effects, root_path.clone())
            .map_err(runtime_cache_io)?;
        root.revalidate().map_err(runtime_cache_io)?;
        Ok(Self {
            inner: Arc::new(ManagedRuntimeCacheInner {
                root_path,
                root,
                install_locks: Mutex::new(HashMap::new()),
                #[cfg(any(test, feature = "test-support"))]
                _test_root: None,
            }),
        })
    }

    pub fn root(&self) -> &Path {
        &self.inner.root_path
    }

    pub fn shares_identity_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn isolated_for_test() -> std::io::Result<Self> {
        let test_root = tempfile::Builder::new()
            .prefix("axial-managed-runtime-")
            .tempdir()?;
        let root_path = test_root.path().to_path_buf();
        let root = ManagedDir::open_root(&root_path).map_err(runtime_cache_io)?;
        Ok(Self {
            inner: Arc::new(ManagedRuntimeCacheInner {
                root_path,
                root,
                install_locks: Mutex::new(HashMap::new()),
                _test_root: Some(test_root),
            }),
        })
    }

    pub(super) fn authority(&self) -> Result<ManagedDir, LoaderError> {
        self.validate_projection()?;
        let root = self.inner.root.clone();
        self.validate_projection()?;
        Ok(root)
    }

    pub(crate) fn validate_projection(&self) -> Result<(), LoaderError> {
        self.inner
            .root
            .validate_absolute_projection(&self.inner.root_path)
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

impl ManagedRuntimeComponent {
    pub fn component(&self) -> &str {
        &self.component
    }

    pub fn root_path(&self) -> &Path {
        &self.root_path
    }

    pub fn java_executable_path(&self) -> PathBuf {
        java_executable(&self.root_path)
    }

    pub fn belongs_to(&self, cache: &ManagedRuntimeCache) -> bool {
        self.cache.shares_identity_with(cache)
    }

    pub fn validate_projection(&self) -> std::io::Result<()> {
        self.root
            .validate_absolute_projection(&self.root_path)
            .map_err(runtime_cache_io)
    }
}

impl ManagedRuntimeLaunchReceipt {
    pub(super) fn new(
        component: ManagedRuntimeComponent,
        java_guard: ManagedExecutableGuard,
    ) -> Self {
        let program_path = component.java_executable_path();
        Self {
            inner: Arc::new(ManagedRuntimeLaunchReceiptInner {
                component,
                java_guard,
                program_path,
            }),
        }
    }

    pub fn validate_program(&self, program: &Path) -> std::io::Result<()> {
        if program != self.inner.program_path {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "managed runtime launch program does not match its retained receipt",
            ));
        }
        self.inner.component.validate_projection()?;
        if !self.inner.component.structurally_ready()
            || !matches!(
                self.inner
                    .component
                    .root
                    .executable_guard_matches(&self.inner.java_guard),
                Ok(true)
            )
        {
            return Err(std::io::Error::other(
                "managed runtime launch receipt no longer matches the retained tree",
            ));
        }
        self.inner.component.validate_projection()
    }
}

fn runtime_cache_io(error: LoaderError) -> std::io::Error {
    std::io::Error::other(error.to_string())
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

pub(super) fn managed_runtime_executable_present(root: &ManagedDir) -> bool {
    PortableRelativePath::new_exact(runtime_java_relative_path())
        .is_ok_and(|relative| matches!(root.inspect_relative_executable(&relative), Ok(Some(_))))
}

pub(super) fn managed_runtime_executable_ready(root: &ManagedDir) -> bool {
    let Ok(relative) = PortableRelativePath::new_exact(runtime_java_relative_path()) else {
        return false;
    };
    let Ok(Some(guard)) = root.inspect_relative_executable(&relative) else {
        return false;
    };
    if !matches!(root.executable_guard_matches(&guard), Ok(true)) {
        return false;
    }
    #[cfg(unix)]
    {
        matches!(root.executable_guard_is_executable(&guard), Ok(true))
    }
    #[cfg(windows)]
    {
        [
            "lib/jvm.cfg",
            "lib/amd64/jvm.cfg",
            "jre/lib/jvm.cfg",
            "jre/lib/amd64/jvm.cfg",
        ]
        .into_iter()
        .filter_map(|relative| PortableRelativePath::new_exact(relative).ok())
        .any(|relative| {
            let Ok(Some(guard)) = root.inspect_relative_regular_file(&relative) else {
                return false;
            };
            matches!(
                root.relative_file_guard_matches(&relative, &guard),
                Ok(true)
            )
        })
    }
    #[cfg(not(any(unix, windows)))]
    {
        true
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
