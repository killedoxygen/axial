use super::discovery::{
    is_known_runtime_component, parse_runtime_override, preferred_runtime_component,
    resolve_axial_cached_runtime, resolve_component_runtime, resolve_managed_runtime,
    resolve_override_runtime, runtime_requirement,
};
use super::install::{
    ManagedRuntimeCommitReceipt, ManagedRuntimeRebuildError, install_ephemeral_processor_runtime,
    publish_staged_managed_runtime, publish_staged_managed_runtime_and_finalize,
    stage_managed_runtime,
};
use super::layout::{ManagedRuntimeCache, runtime_os_arch};
use super::manifest::{RuntimeSourceReceipt, acquire_runtime_source};
use super::model::{
    JavaRuntimeLookupError, RuntimeEnsureEvent, RuntimeEnsureResult, RuntimeId, RuntimeOverride,
    RuntimeProbeUsage, RuntimeRecord, RuntimeRequirement, RuntimeSource,
};
use super::probe::{JavaRuntimeProbeReceipt, probe_java_runtime_receipt};
use crate::launch::JavaVersion;
use std::path::{Path, PathBuf};

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

pub async fn rebuild_managed_runtime_component<F>(
    cache: &ManagedRuntimeCache,
    component: RuntimeId,
    mut observer: F,
) -> Result<ManagedRuntimeCommitReceipt, ManagedRuntimeRebuildError>
where
    F: FnMut(RuntimeEnsureEvent),
{
    if !is_known_runtime_component(component.as_str()) {
        return Err(ManagedRuntimeRebuildError::Preparation(
            JavaRuntimeLookupError::Install(
                "runtime rebuild target is outside the closed managed component vocabulary"
                    .to_string(),
            ),
        ));
    }
    observer(RuntimeEnsureEvent::DownloadingManagedRuntime {
        component: component.as_str().to_string(),
    });
    let source_receipt = acquire_runtime_source(&component, &runtime_os_arch())
        .await
        .map_err(ManagedRuntimeRebuildError::Preparation)?;
    let receipt = rebuild_managed_runtime_component_from_source(
        cache,
        &component,
        source_receipt,
        &mut observer,
    )
    .await?;
    if !receipt.revalidate(cache, &component).await {
        return Err(receipt.into_failure(JavaRuntimeLookupError::Install(
            "rebuilt runtime failed exact commit receipt verification".to_string(),
        )));
    }
    observer(RuntimeEnsureEvent::ManagedRuntimeReady {
        component: component.as_str().to_string(),
    });
    Ok(receipt)
}

#[cfg(feature = "test-support")]
pub async fn rebuild_managed_runtime_fixture_for_test(
    cache: &ManagedRuntimeCache,
    component: RuntimeId,
) -> Result<ManagedRuntimeCommitReceipt, ManagedRuntimeRebuildError> {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    #[cfg(unix)]
    const JAVA_BYTES: &[u8] = br#"#!/bin/sh
if [ "$1" = "-XshowSettings:property" ]; then
  echo 'openjdk version "21.0.3"' >&2
  exit 0
fi
count=0
if [ -f guardian-runtime-process-count ]; then
  count=$(cat guardian-runtime-process-count)
fi
count=$((count + 1))
printf '%s' "$count" > guardian-runtime-process-count
printf '%s\n' '[Render thread/INFO]: Created: 1024x512x4 minecraft:textures/atlas/blocks.png-atlas' >&2
sleep 1
exit 0
"#;
    #[cfg(not(unix))]
    const JAVA_BYTES: &[u8] = b"axial managed runtime fixture";
    if !is_known_runtime_component(component.as_str()) {
        return Err(ManagedRuntimeRebuildError::Preparation(
            JavaRuntimeLookupError::Install(
                "runtime rebuild fixture target is outside the closed component vocabulary"
                    .to_string(),
            ),
        ));
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|error| {
            ManagedRuntimeRebuildError::Preparation(JavaRuntimeLookupError::Install(
                error.to_string(),
            ))
        })?;
    let address = listener.local_addr().map_err(|error| {
        ManagedRuntimeRebuildError::Preparation(JavaRuntimeLookupError::Install(error.to_string()))
    })?;
    tokio::spawn(async move {
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };
        let mut request = [0_u8; 1024];
        let _ = socket.read(&mut request).await;
        let headers = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            JAVA_BYTES.len()
        );
        if socket.write_all(headers.as_bytes()).await.is_ok() {
            let _ = socket.write_all(JAVA_BYTES).await;
        }
    });
    let source = super::manifest::authenticated_runtime_rebuild_fixture_source(
        component.clone(),
        format!("http://{address}/java"),
        JAVA_BYTES,
    )
    .map_err(ManagedRuntimeRebuildError::Preparation)?;
    let inventory = crate::known_good::runtime_inventory_from_source(&source).map_err(|_| {
        ManagedRuntimeRebuildError::Preparation(JavaRuntimeLookupError::Install(
            "runtime rebuild fixture inventory derivation failed".to_string(),
        ))
    })?;
    let mut observer = |_| {};
    let receipt =
        rebuild_managed_runtime_component_from_source(cache, &component, source, &mut observer)
            .await?;
    if !receipt.revalidate(cache, &component).await
        || !receipt.matches_known_good_inventory(&inventory)
    {
        return Err(receipt.into_failure(JavaRuntimeLookupError::Install(
            "runtime rebuild fixture failed sealed postcondition verification".to_string(),
        )));
    }
    Ok(receipt)
}

pub(crate) async fn rebuild_managed_runtime_component_from_source(
    cache: &ManagedRuntimeCache,
    component: &RuntimeId,
    source_receipt: RuntimeSourceReceipt,
    observer: &mut impl FnMut(RuntimeEnsureEvent),
) -> Result<ManagedRuntimeCommitReceipt, ManagedRuntimeRebuildError> {
    let staged = stage_managed_runtime(cache, component, source_receipt, observer)
        .await
        .map_err(ManagedRuntimeRebuildError::Preparation)?;
    publish_staged_managed_runtime(staged).await
}

async fn install_managed_runtime_component_from_source(
    cache: &ManagedRuntimeCache,
    component: &RuntimeId,
    source_receipt: RuntimeSourceReceipt,
    observer: &mut impl FnMut(RuntimeEnsureEvent),
) -> Result<ManagedRuntimeCommitReceipt, ManagedRuntimeRebuildError> {
    let staged = stage_managed_runtime(cache, component, source_receipt, observer)
        .await
        .map_err(ManagedRuntimeRebuildError::Preparation)?;
    publish_staged_managed_runtime_and_finalize(staged).await
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
        return Err(JavaRuntimeLookupError::Install(
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
    cache: &ManagedRuntimeCache,
    java_version: &JavaVersion,
    source_receipt: RuntimeSourceReceipt,
    observer: &mut F,
) -> Result<RuntimeSourceReceipt, JavaRuntimeLookupError>
where
    F: FnMut(RuntimeEnsureEvent),
{
    let component = RuntimeId::from(preferred_runtime_component(java_version));
    if source_receipt.component() != &component || !is_known_runtime_component(component.as_str()) {
        return Err(JavaRuntimeLookupError::Install(
            "runtime source does not match the preferred managed component".to_string(),
        ));
    }
    let current = resolve_axial_cached_runtime(cache, &component, java_version.major_version).ok();
    let matches_source = match current.as_ref() {
        Some(runtime) => runtime_record_matches_source(runtime, &source_receipt).await,
        None => false,
    };
    let source_receipt = if matches_source {
        source_receipt
    } else {
        observer(RuntimeEnsureEvent::DownloadingManagedRuntime {
            component: component.as_str().to_string(),
        });
        install_managed_runtime_component_from_source(cache, &component, source_receipt, observer)
            .await
            .map_err(ManagedRuntimeRebuildError::into_lookup_error)?
            .into_source_receipt()
    };
    let runtime = resolve_axial_cached_runtime(cache, &component, java_version.major_version)?;
    if !runtime_record_matches_source(&runtime, &source_receipt).await {
        return Err(JavaRuntimeLookupError::Install(
            "installed runtime does not match its authenticated source".to_string(),
        ));
    }
    observer(RuntimeEnsureEvent::ManagedRuntimeReady {
        component: component.as_str().to_string(),
    });
    Ok(source_receipt)
}

pub async fn ensure_runtime_with_events<F>(
    cache: &ManagedRuntimeCache,
    java_version: &JavaVersion,
    override_path: &str,
    force_managed: bool,
    probe_receipt: Option<&JavaRuntimeProbeReceipt>,
    observer: F,
) -> Result<RuntimeEnsureResult, JavaRuntimeLookupError>
where
    F: FnMut(RuntimeEnsureEvent),
{
    ensure_runtime_with_events_from_source(
        cache,
        java_version,
        override_path,
        force_managed,
        probe_receipt,
        RuntimeEnsureSource::Production,
        observer,
    )
    .await
}

#[cfg(feature = "test-support")]
pub async fn ensure_runtime_with_persisted_manifest_for_test<F>(
    cache: &ManagedRuntimeCache,
    java_version: &JavaVersion,
    override_path: &str,
    force_managed: bool,
    probe_receipt: Option<&JavaRuntimeProbeReceipt>,
    observer: F,
) -> Result<RuntimeEnsureResult, JavaRuntimeLookupError>
where
    F: FnMut(RuntimeEnsureEvent),
{
    ensure_runtime_with_events_from_source(
        cache,
        java_version,
        override_path,
        force_managed,
        probe_receipt,
        RuntimeEnsureSource::PersistedManifest,
        observer,
    )
    .await
}

#[derive(Clone, Copy)]
enum RuntimeEnsureSource {
    Production,
    #[cfg(feature = "test-support")]
    PersistedManifest,
}

async fn ensure_runtime_with_events_from_source<F>(
    cache: &ManagedRuntimeCache,
    java_version: &JavaVersion,
    override_path: &str,
    force_managed: bool,
    probe_receipt: Option<&JavaRuntimeProbeReceipt>,
    source: RuntimeEnsureSource,
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
                    cache,
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
            cache,
            requested_runtime,
            java_version.major_version,
            source,
            &mut observer,
        )
        .await?;
        return Ok(RuntimeEnsureResult {
            requested: Some(requested_record),
            effective: refreshed.effective,
            probe_usage,
        });
    }

    let managed =
        ensure_managed_runtime_with_events(cache, &requirement, source, &mut observer).await?;

    Ok(RuntimeEnsureResult {
        requested,
        effective: managed.effective,
        probe_usage,
    })
}
struct ManagedEnsure {
    effective: RuntimeRecord,
}

async fn ensure_managed_runtime_with_events<F>(
    cache: &ManagedRuntimeCache,
    requirement: &RuntimeRequirement,
    source: RuntimeEnsureSource,
    observer: &mut F,
) -> Result<ManagedEnsure, JavaRuntimeLookupError>
where
    F: FnMut(RuntimeEnsureEvent),
{
    let preferred = &requirement.preferred_component;
    match resolve_managed_runtime(cache, preferred) {
        Ok(runtime) => {
            let refreshed = refresh_ready_managed_runtime(
                cache,
                runtime,
                requirement.required_java.major_version,
                source,
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
            });
        }
        // reinstalling produces the same x86_64 build, so a missing-Rosetta
        // failure can never be repaired by falling through to install
        Err(error @ JavaRuntimeLookupError::RosettaRequired { .. }) => return Err(error),
        Err(_) => {}
    }

    // Acquire and authenticate the source before creating or removing any
    // runtime install paths. The same parsed receipt is consumed below.
    let source_receipt = acquire_runtime_source_for_ensure(cache, preferred, source).await?;

    match resolve_managed_runtime(cache, preferred) {
        Ok(runtime) => {
            if runtime_record_matches_source(&runtime, &source_receipt).await {
                observer(RuntimeEnsureEvent::ManagedRuntimeReady {
                    component: preferred.as_str().to_string(),
                });
                return Ok(ManagedEnsure { effective: runtime });
            }
        }
        Err(error @ JavaRuntimeLookupError::RosettaRequired { .. }) => return Err(error),
        Err(_) => {}
    }

    observer(RuntimeEnsureEvent::DownloadingManagedRuntime {
        component: preferred.as_str().to_string(),
    });
    let source_receipt =
        install_managed_runtime_component_from_source(cache, preferred, source_receipt, observer)
            .await
            .map_err(ManagedRuntimeRebuildError::into_lookup_error)?
            .into_source_receipt();
    let runtime =
        resolve_component_runtime(cache, preferred, requirement.required_java.major_version)?;
    if !runtime_record_matches_source(&runtime, &source_receipt).await {
        return Err(JavaRuntimeLookupError::Install(
            "installed runtime does not match its authenticated source".to_string(),
        ));
    }
    observer(RuntimeEnsureEvent::ManagedRuntimeReady {
        component: preferred.as_str().to_string(),
    });
    Ok(ManagedEnsure { effective: runtime })
}

struct RefreshedRuntime {
    effective: RuntimeRecord,
    install_performed: bool,
}

async fn refresh_ready_managed_runtime<F>(
    cache: &ManagedRuntimeCache,
    runtime: RuntimeRecord,
    required_major: i32,
    source: RuntimeEnsureSource,
    observer: &mut F,
) -> Result<RefreshedRuntime, JavaRuntimeLookupError>
where
    F: FnMut(RuntimeEnsureEvent),
{
    if runtime.source != RuntimeSource::Managed {
        return Ok(RefreshedRuntime {
            effective: runtime,
            install_performed: false,
        });
    }

    let source_receipt = acquire_runtime_source_for_ensure(cache, &runtime.id, source).await?;
    let component = runtime.id.clone();
    if let Ok(current) = resolve_component_runtime(cache, &component, required_major)
        && runtime_record_matches_source(&current, &source_receipt).await
    {
        return Ok(RefreshedRuntime {
            effective: current,
            install_performed: false,
        });
    }

    observer(RuntimeEnsureEvent::DownloadingManagedRuntime {
        component: component.as_str().to_string(),
    });
    let source_receipt =
        install_managed_runtime_component_from_source(cache, &component, source_receipt, observer)
            .await
            .map_err(ManagedRuntimeRebuildError::into_lookup_error)?
            .into_source_receipt();
    let effective = resolve_component_runtime(cache, &component, required_major)?;
    if !runtime_record_matches_source(&effective, &source_receipt).await {
        return Err(JavaRuntimeLookupError::Install(
            "installed runtime does not match its authenticated source".to_string(),
        ));
    }
    observer(RuntimeEnsureEvent::ManagedRuntimeReady {
        component: component.as_str().to_string(),
    });
    Ok(RefreshedRuntime {
        effective,
        install_performed: true,
    })
}

async fn acquire_runtime_source_for_ensure(
    _cache: &ManagedRuntimeCache,
    component: &RuntimeId,
    source: RuntimeEnsureSource,
) -> Result<RuntimeSourceReceipt, JavaRuntimeLookupError> {
    match source {
        RuntimeEnsureSource::Production => {
            acquire_runtime_source(component, &runtime_os_arch()).await
        }
        #[cfg(feature = "test-support")]
        RuntimeEnsureSource::PersistedManifest => {
            acquire_persisted_runtime_source_for_test(_cache, component).await
        }
    }
}

#[cfg(feature = "test-support")]
async fn acquire_persisted_runtime_source_for_test(
    cache: &ManagedRuntimeCache,
    component: &RuntimeId,
) -> Result<RuntimeSourceReceipt, JavaRuntimeLookupError> {
    use super::manifest::{COMPONENT_MANIFEST_PROOF_FILE, ComponentManifest};
    use tokio::io::AsyncReadExt as _;

    let runtime_root = cache.component_root(component.as_str()).ok_or_else(|| {
        JavaRuntimeLookupError::Install(
            "runtime component is outside the managed cache vocabulary".to_string(),
        )
    })?;
    let proof_path = runtime_root.join(COMPONENT_MANIFEST_PROOF_FILE);
    let file = tokio::fs::File::open(proof_path)
        .await
        .map_err(|error| JavaRuntimeLookupError::Install(error.to_string()))?;
    let mut bytes = Vec::new();
    file.take(super::manifest::MAX_RUNTIME_MANIFEST_BYTES + 1)
        .read_to_end(&mut bytes)
        .await
        .map_err(|error| JavaRuntimeLookupError::Install(error.to_string()))?;
    if bytes.len() as u64 > super::manifest::MAX_RUNTIME_MANIFEST_BYTES {
        return Err(JavaRuntimeLookupError::Install(
            "persisted runtime manifest proof is too large".to_string(),
        ));
    }
    let manifest = serde_json::from_slice::<ComponentManifest>(&bytes)
        .map_err(|error| JavaRuntimeLookupError::Install(error.to_string()))?;
    let canonical = super::manifest::component_manifest_proof_bytes(&manifest)
        .map_err(|error| JavaRuntimeLookupError::Install(error.to_string()))?;
    if bytes != canonical {
        return Err(JavaRuntimeLookupError::Install(
            "persisted runtime manifest proof is not canonical".to_string(),
        ));
    }
    super::manifest::authenticated_runtime_source_from_manifest_for_test(
        component.clone(),
        manifest,
    )
}

async fn runtime_record_matches_source(
    runtime: &RuntimeRecord,
    source: &RuntimeSourceReceipt,
) -> bool {
    if runtime.source != RuntimeSource::Managed || &runtime.id != source.component() {
        return false;
    }
    super::install::runtime_tree_matches_source(Path::new(&runtime.root_dir), source).await
}

#[cfg(test)]
pub(super) async fn runtime_record_matches_source_for_test(
    runtime: &RuntimeRecord,
    source: &RuntimeSourceReceipt,
) -> bool {
    runtime_record_matches_source(runtime, source).await
}
