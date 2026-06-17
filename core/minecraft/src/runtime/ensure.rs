use super::discovery::{
    parse_runtime_override, resolve_component_runtime, resolve_managed_runtime,
    resolve_override_runtime, runtime_requirement,
};
use super::install::install_managed_runtime;
use super::layout::runtime_cache_dir;
use super::model::{
    JavaRuntimeLookupError, JavaRuntimeResult, RuntimeEnsureAction, RuntimeEnsureEvent,
    RuntimeEnsureResult, RuntimeOverride, RuntimeRecord, RuntimeRequirement,
};
use crate::launch::JavaVersion;
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, OnceLock};
use tokio::sync::Mutex;

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
    install_managed_runtime(preferred, &install_root, observer).await?;
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
