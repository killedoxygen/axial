use super::discovery::{
    is_known_runtime_component, parse_runtime_override, preferred_runtime_component,
    resolve_axial_cached_runtime, resolve_component_runtime, resolve_managed_runtime,
    resolve_override_runtime, runtime_requirement,
};
use super::file_download::runtime_filesystem_path;
use super::install::{install_ephemeral_processor_runtime, install_managed_runtime};
use super::layout::{runtime_cache_dir, runtime_os_arch};
use super::manifest::{
    COMPONENT_MANIFEST_PROOF_FILE, RuntimeSourceReceipt, acquire_runtime_source,
    component_manifest_proof_bytes,
};
use super::model::{
    JavaRuntimeLookupError, JavaRuntimeResult, RuntimeEnsureAction, RuntimeEnsureEvent,
    RuntimeEnsureResult, RuntimeId, RuntimeOverride, RuntimeProbeUsage, RuntimeRecord,
    RuntimeRequirement, RuntimeSource,
};
use super::probe::{JavaRuntimeProbeReceipt, probe_java_runtime_receipt};
use crate::launch::JavaVersion;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use tokio::sync::Mutex;

pub(crate) struct ProcessorRuntime {
    probe_receipt: JavaRuntimeProbeReceipt,
    _source_receipt: RuntimeSourceReceipt,
}

impl ProcessorRuntime {
    pub(crate) fn revalidate_cli_executable(&self) -> Result<PathBuf, JavaRuntimeLookupError> {
        self.probe_receipt.revalidate_cli_executable()
    }

    pub(crate) fn into_source_receipt(self) -> RuntimeSourceReceipt {
        self._source_receipt
    }
}

pub(crate) async fn materialize_ephemeral_processor_runtime(
    java_version: &JavaVersion,
    source_receipt: RuntimeSourceReceipt,
    install_root: &Path,
    max_entries: usize,
    max_bytes: u64,
) -> Result<ProcessorRuntime, JavaRuntimeLookupError> {
    let requirement = runtime_requirement(java_version);
    let component = requirement.preferred_component;
    if source_receipt.component() != &component || !is_known_runtime_component(component.as_str()) {
        return Err(JavaRuntimeLookupError::Download(
            "processor runtime source does not match the authenticated base requirement"
                .to_string(),
        ));
    }
    let mut observer = |_| {};
    install_ephemeral_processor_runtime(
        &component,
        install_root,
        &source_receipt,
        max_entries,
        max_bytes,
        &mut observer,
    )
    .await?;
    let java_path = super::layout::java_executable(install_root);
    let probe_receipt = tokio::task::spawn_blocking(move || {
        probe_java_runtime_receipt(&java_path, Some("ephemeral-processor-runtime"))
    })
    .await
    .map_err(|_| {
        JavaRuntimeLookupError::Probe(
            "processor runtime probe task stopped unexpectedly".to_string(),
        )
    })??;
    if probe_receipt.validation().into_info().major
        != u32::try_from(java_version.major_version).unwrap_or(u32::MAX)
    {
        return Err(JavaRuntimeLookupError::Probe(
            "processor runtime Java major does not match the authenticated base requirement"
                .to_string(),
        ));
    }
    Ok(ProcessorRuntime {
        probe_receipt,
        _source_receipt: source_receipt,
    })
}

pub(crate) async fn materialize_preferred_runtime_source<F>(
    java_version: &JavaVersion,
    source_receipt: RuntimeSourceReceipt,
    observer: &mut F,
) -> Result<RuntimeSourceReceipt, JavaRuntimeLookupError>
where
    F: FnMut(RuntimeEnsureEvent),
{
    let component = RuntimeId::from(preferred_runtime_component(java_version));
    if source_receipt.component() != &component || !is_known_runtime_component(component.as_str()) {
        return Err(JavaRuntimeLookupError::Download(
            "runtime source does not match the preferred managed component".to_string(),
        ));
    }
    let install_root = runtime_cache_dir().join(component.as_str());
    let install_lock = runtime_install_lock(component.as_str());
    let _guard = install_lock.lock().await;
    let _file_lock = acquire_runtime_install_file_lock(&install_root).await?;
    let current = resolve_axial_cached_runtime(&component, java_version.major_version).ok();
    let matches_source = match current.as_ref() {
        Some(runtime) => runtime_record_matches_source(runtime, &source_receipt).await,
        None => false,
    };
    if !matches_source {
        observer(RuntimeEnsureEvent::DownloadingManagedRuntime {
            component: component.as_str().to_string(),
        });
        install_managed_runtime(&component, &install_root, &source_receipt, observer).await?;
    }
    let runtime = resolve_axial_cached_runtime(&component, java_version.major_version)?;
    if !runtime_record_matches_source(&runtime, &source_receipt).await {
        return Err(JavaRuntimeLookupError::Download(
            "installed runtime does not match its authenticated source".to_string(),
        ));
    }
    observer(RuntimeEnsureEvent::ManagedRuntimeReady {
        component: component.as_str().to_string(),
    });
    Ok(source_receipt)
}

pub async fn ensure_java_runtime(
    library_dir: &Path,
    java_version: &JavaVersion,
    override_path: &str,
) -> Result<JavaRuntimeResult, JavaRuntimeLookupError> {
    let result = ensure_runtime_with_events(
        library_dir,
        java_version,
        override_path,
        false,
        None,
        |_| {},
    )
    .await?;
    Ok(JavaRuntimeResult {
        path: result.effective.java_path,
        component: result.effective.id.0,
        source: result.effective.source.as_str().to_string(),
    })
}

pub async fn ensure_runtime_with_events<F>(
    library_dir: &Path,
    java_version: &JavaVersion,
    override_path: &str,
    force_managed: bool,
    probe_receipt: Option<&JavaRuntimeProbeReceipt>,
    mut observer: F,
) -> Result<RuntimeEnsureResult, JavaRuntimeLookupError>
where
    F: FnMut(RuntimeEnsureEvent),
{
    let requirement = runtime_requirement(java_version);
    let requested_override = parse_runtime_override(override_path);

    let (requested, probe_usage) = if force_managed {
        (None, RuntimeProbeUsage::default())
    } else {
        match &requested_override {
            RuntimeOverride::None => (None, RuntimeProbeUsage::default()),
            RuntimeOverride::Component(component) => (
                Some(resolve_component_runtime(
                    library_dir,
                    component,
                    java_version.major_version,
                )?),
                RuntimeProbeUsage::default(),
            ),
            RuntimeOverride::ExecutablePath(path) => {
                let path = path.clone();
                let preferred_component = requirement.preferred_component.clone();
                let probe_validation = probe_receipt.map(JavaRuntimeProbeReceipt::validation);
                let resolved = tokio::task::spawn_blocking(move || {
                    resolve_override_runtime(&path, &preferred_component, probe_validation)
                })
                .await
                .map_err(|_| {
                    JavaRuntimeLookupError::Probe(
                        "java runtime probe task stopped unexpectedly".to_string(),
                    )
                })??;
                (Some(resolved.record), resolved.probe_usage)
            }
        }
    };

    if let Some(requested_runtime) = requested.clone() {
        let requested_record = requested_runtime.clone();
        let refreshed = refresh_ready_managed_runtime(
            library_dir,
            requested_runtime,
            java_version.major_version,
            &mut observer,
        )
        .await?;
        return Ok(RuntimeEnsureResult {
            requested: Some(requested_record),
            effective: refreshed.effective,
            bypassed_requested_runtime: false,
            install_performed: refreshed.install_performed,
            action: RuntimeEnsureAction::UseRequested,
            probe_usage,
            source_receipt: refreshed.source_receipt,
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
        probe_usage,
        source_receipt: managed.source_receipt,
    })
}
struct ManagedEnsure {
    effective: RuntimeRecord,
    install_performed: bool,
    source_receipt: Option<RuntimeSourceReceipt>,
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
    match resolve_managed_runtime(library_dir, preferred) {
        Ok(runtime) => {
            let refreshed = refresh_ready_managed_runtime(
                library_dir,
                runtime,
                requirement.required_java.major_version,
                observer,
            )
            .await?;
            if !refreshed.install_performed {
                observer(RuntimeEnsureEvent::ManagedRuntimeReady {
                    component: preferred.as_str().to_string(),
                });
            }
            return Ok(ManagedEnsure {
                effective: refreshed.effective,
                install_performed: refreshed.install_performed,
                source_receipt: refreshed.source_receipt,
            });
        }
        // reinstalling produces the same x86_64 build, so a missing-Rosetta
        // failure can never be repaired by falling through to install
        Err(error @ JavaRuntimeLookupError::RosettaRequired { .. }) => return Err(error),
        Err(_) => {}
    }

    // Acquire and authenticate the source before creating or removing any
    // runtime install paths. The same parsed receipt is consumed below.
    let source_receipt = acquire_runtime_source(preferred, &runtime_os_arch()).await?;

    let install_root = runtime_cache_dir().join(preferred.as_str());
    let install_lock = runtime_install_lock(preferred.as_str());
    let _guard = install_lock.lock().await;
    let _file_lock = acquire_runtime_install_file_lock(&install_root).await?;

    match resolve_managed_runtime(library_dir, preferred) {
        Ok(runtime) => {
            if runtime.source != RuntimeSource::Managed {
                observer(RuntimeEnsureEvent::ManagedRuntimeReady {
                    component: preferred.as_str().to_string(),
                });
                return Ok(ManagedEnsure {
                    effective: runtime,
                    install_performed: false,
                    source_receipt: None,
                });
            }
            if runtime_record_matches_source(&runtime, &source_receipt).await {
                observer(RuntimeEnsureEvent::ManagedRuntimeReady {
                    component: preferred.as_str().to_string(),
                });
                return Ok(ManagedEnsure {
                    effective: runtime,
                    install_performed: false,
                    source_receipt: Some(source_receipt),
                });
            }
        }
        Err(error @ JavaRuntimeLookupError::RosettaRequired { .. }) => return Err(error),
        Err(_) => {}
    }

    observer(RuntimeEnsureEvent::DownloadingManagedRuntime {
        component: preferred.as_str().to_string(),
    });
    install_managed_runtime(preferred, &install_root, &source_receipt, observer).await?;
    let runtime = resolve_component_runtime(
        library_dir,
        preferred,
        requirement.required_java.major_version,
    )?;
    if !runtime_record_matches_source(&runtime, &source_receipt).await {
        return Err(JavaRuntimeLookupError::Download(
            "installed runtime does not match its authenticated source".to_string(),
        ));
    }
    Ok(ManagedEnsure {
        effective: runtime,
        install_performed: true,
        source_receipt: Some(source_receipt),
    })
}

struct RefreshedRuntime {
    effective: RuntimeRecord,
    install_performed: bool,
    source_receipt: Option<RuntimeSourceReceipt>,
}

async fn refresh_ready_managed_runtime<F>(
    library_dir: &Path,
    runtime: RuntimeRecord,
    required_major: i32,
    observer: &mut F,
) -> Result<RefreshedRuntime, JavaRuntimeLookupError>
where
    F: FnMut(RuntimeEnsureEvent),
{
    if runtime.source != RuntimeSource::Managed {
        return Ok(RefreshedRuntime {
            effective: runtime,
            install_performed: false,
            source_receipt: None,
        });
    }
    let source_receipt = acquire_runtime_source(&runtime.id, &runtime_os_arch()).await?;
    let component = runtime.id.clone();
    let install_root = runtime_cache_dir().join(component.as_str());
    let install_lock = runtime_install_lock(component.as_str());
    let _guard = install_lock.lock().await;
    let _file_lock = acquire_runtime_install_file_lock(&install_root).await?;
    if let Ok(current) = resolve_component_runtime(library_dir, &component, required_major) {
        if current.source != RuntimeSource::Managed {
            return Ok(RefreshedRuntime {
                effective: current,
                install_performed: false,
                source_receipt: None,
            });
        }
        if runtime_record_matches_source(&current, &source_receipt).await {
            return Ok(RefreshedRuntime {
                effective: current,
                install_performed: false,
                source_receipt: Some(source_receipt),
            });
        }
    }

    observer(RuntimeEnsureEvent::DownloadingManagedRuntime {
        component: component.as_str().to_string(),
    });
    install_managed_runtime(&component, &install_root, &source_receipt, observer).await?;
    let effective = resolve_component_runtime(library_dir, &component, required_major)?;
    if !runtime_record_matches_source(&effective, &source_receipt).await {
        return Err(JavaRuntimeLookupError::Download(
            "installed runtime does not match its authenticated source".to_string(),
        ));
    }
    Ok(RefreshedRuntime {
        effective,
        install_performed: true,
        source_receipt: Some(source_receipt),
    })
}

async fn runtime_record_matches_source(
    runtime: &RuntimeRecord,
    source: &RuntimeSourceReceipt,
) -> bool {
    if runtime.source != RuntimeSource::Managed || &runtime.id != source.component() {
        return false;
    }
    let Ok(expected) = component_manifest_proof_bytes(source.manifest()) else {
        return false;
    };
    let proof_path = PathBuf::from(&runtime.root_dir).join(COMPONENT_MANIFEST_PROOF_FILE);
    let Ok(metadata) =
        tokio::fs::symlink_metadata(runtime_filesystem_path(&proof_path).as_ref()).await
    else {
        return false;
    };
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.len() != expected.len() as u64
    {
        return false;
    }
    tokio::fs::read(runtime_filesystem_path(&proof_path).as_ref())
        .await
        .is_ok_and(|actual| actual == expected)
}

#[cfg(test)]
pub(super) async fn runtime_record_matches_source_for_test(
    runtime: &RuntimeRecord,
    source: &RuntimeSourceReceipt,
) -> bool {
    runtime_record_matches_source(runtime, source).await
}

pub(super) fn runtime_install_lock(component: &str) -> Arc<Mutex<()>> {
    static LOCKS: OnceLock<std::sync::Mutex<HashMap<String, Arc<Mutex<()>>>>> = OnceLock::new();
    let mutex = LOCKS.get_or_init(|| std::sync::Mutex::new(HashMap::new()));
    runtime_install_lock_from_map(mutex, component)
}

pub(super) fn runtime_install_lock_from_map(
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

struct RuntimeInstallFileLock {
    file: std::fs::File,
}

impl Drop for RuntimeInstallFileLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

async fn acquire_runtime_install_file_lock(
    install_root: &Path,
) -> Result<RuntimeInstallFileLock, JavaRuntimeLookupError> {
    let lock_path = runtime_install_lock_file_path(install_root);
    tokio::task::spawn_blocking(move || {
        if let Some(parent) = lock_path.parent() {
            std::fs::create_dir_all(runtime_filesystem_path(parent).as_ref())?;
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(runtime_filesystem_path(&lock_path).as_ref())?;
        file.lock()?;
        Ok::<_, std::io::Error>(RuntimeInstallFileLock { file })
    })
    .await
    .map_err(|error| JavaRuntimeLookupError::Download(error.to_string()))?
    .map_err(|error| JavaRuntimeLookupError::Download(error.to_string()))
}

pub(super) fn runtime_install_lock_file_path(install_root: &Path) -> PathBuf {
    let mut name = install_root
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("runtime"))
        .to_os_string();
    name.push(".install.lock");
    install_root.with_file_name(name)
}
