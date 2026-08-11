use super::cancellation::{
    RuntimeCancellation, RuntimeCancellationSender, runtime_cancellation_channel,
};
#[cfg(test)]
use super::cancellation::{
    RuntimeTestGate, RuntimeTestHookPoint, arm_runtime_test_hook, wait_for_runtime_test_hook,
};
use super::discovery::{
    is_known_runtime_component, parse_runtime_override, preferred_runtime_component,
    resolve_component_runtime, resolve_managed_runtime, resolve_override_runtime,
    runtime_requirement,
};
use super::install::{
    CachedManagedRuntimeVerification, ManagedRuntimeCommitReceipt, ManagedRuntimeRebuildError,
    RuntimeTreeVerificationReason, discard_staged_managed_runtime,
    install_ephemeral_processor_runtime, publish_staged_managed_runtime,
    publish_staged_managed_runtime_and_finalize, stage_managed_runtime,
    stage_managed_runtime_until_cancelled, verify_cached_managed_runtime_until_cancelled,
};
use super::layout::{ManagedRuntimeCache, runtime_os_arch};
use super::manifest::{RuntimeSourceReceipt, acquire_runtime_source};
use super::model::{
    JavaRuntimeLookupError, ManagedRuntimeMutationRefused, RuntimeEnsureEvent, RuntimeEnsureResult,
    RuntimeId, RuntimeOverride, RuntimeProbeUsage, RuntimeRecord, RuntimeRequirement,
    RuntimeSource,
};
use super::probe::{JavaRuntimeProbeReceipt, probe_java_runtime_receipt};
use crate::launch::JavaVersion;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicU8, Ordering},
};

const RUNTIME_MATERIALIZATION_OPEN: u8 = 0;
const RUNTIME_MATERIALIZATION_CANCELLED: u8 = 1;
const RUNTIME_MATERIALIZATION_SETTLING: u8 = 2;
const RUNTIME_MATERIALIZATION_COMPLETE: u8 = 3;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RuntimeMaterializationCancellation {
    Cancelled,
    SettlementRequired,
}

pub(crate) struct RuntimeMaterializationCancelHandle {
    phase: Arc<AtomicU8>,
    cancellation: RuntimeCancellationSender,
}

pub(crate) struct RuntimeMaterializationTaskControl {
    phase: Arc<AtomicU8>,
    cancellation: RuntimeCancellation,
}

pub(crate) fn runtime_materialization_control() -> (
    RuntimeMaterializationCancelHandle,
    RuntimeMaterializationTaskControl,
) {
    let phase = Arc::new(AtomicU8::new(RUNTIME_MATERIALIZATION_OPEN));
    let (cancellation, cancellation_rx) = runtime_cancellation_channel();
    (
        RuntimeMaterializationCancelHandle {
            phase: Arc::clone(&phase),
            cancellation,
        },
        RuntimeMaterializationTaskControl {
            phase,
            cancellation: cancellation_rx,
        },
    )
}

#[cfg(test)]
pub(crate) fn block_runtime_before_publication_claim_for_test(
    install_root: &Path,
) -> RuntimeTestGate {
    arm_runtime_test_hook(RuntimeTestHookPoint::BeforePublicationClaim, install_root)
}

impl RuntimeMaterializationCancelHandle {
    pub(crate) fn cancel_before_publication(&self) -> RuntimeMaterializationCancellation {
        match self.phase.compare_exchange(
            RUNTIME_MATERIALIZATION_OPEN,
            RUNTIME_MATERIALIZATION_CANCELLED,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) | Err(RUNTIME_MATERIALIZATION_CANCELLED) => {
                self.cancellation.cancel();
                RuntimeMaterializationCancellation::Cancelled
            }
            Err(RUNTIME_MATERIALIZATION_SETTLING | RUNTIME_MATERIALIZATION_COMPLETE) => {
                RuntimeMaterializationCancellation::SettlementRequired
            }
            Err(phase) => panic!("invalid runtime materialization phase {phase}"),
        }
    }
}

impl RuntimeMaterializationTaskControl {
    #[cfg(test)]
    pub(crate) async fn cancelled(&mut self) {
        self.cancellation.cancelled().await;
    }

    fn cancellation(&mut self) -> &mut RuntimeCancellation {
        &mut self.cancellation
    }

    pub(crate) fn claim_publication_settlement(&self) -> bool {
        match self.phase.compare_exchange(
            RUNTIME_MATERIALIZATION_OPEN,
            RUNTIME_MATERIALIZATION_SETTLING,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) | Err(RUNTIME_MATERIALIZATION_SETTLING) => true,
            Err(RUNTIME_MATERIALIZATION_CANCELLED) => false,
            Err(RUNTIME_MATERIALIZATION_COMPLETE) => {
                panic!("completed runtime materialization cannot claim publication")
            }
            Err(phase) => panic!("invalid runtime materialization phase {phase}"),
        }
    }

    pub(crate) fn finish(&self) -> bool {
        loop {
            match self.phase.load(Ordering::Acquire) {
                RUNTIME_MATERIALIZATION_OPEN => {
                    if self
                        .phase
                        .compare_exchange(
                            RUNTIME_MATERIALIZATION_OPEN,
                            RUNTIME_MATERIALIZATION_COMPLETE,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_ok()
                    {
                        return true;
                    }
                }
                RUNTIME_MATERIALIZATION_SETTLING => {
                    self.phase
                        .store(RUNTIME_MATERIALIZATION_COMPLETE, Ordering::Release);
                    return true;
                }
                RUNTIME_MATERIALIZATION_CANCELLED => return false,
                RUNTIME_MATERIALIZATION_COMPLETE => return true,
                phase => panic!("invalid runtime materialization phase {phase}"),
            }
        }
    }
}

pub(crate) struct ProcessorRuntime {
    probe_receipt: JavaRuntimeProbeReceipt,
    install_directory: crate::managed_fs::ManagedDir,
    program_guard: crate::managed_fs::ManagedExecutableGuard,
    program_path: PathBuf,
    _source_receipt: RuntimeSourceReceipt,
}

impl ProcessorRuntime {
    pub(crate) fn cli_executable_path(&self) -> &Path {
        &self.program_path
    }

    pub(crate) fn validate_program(&self, program: &Path) -> Result<(), JavaRuntimeLookupError> {
        if program != self.program_path {
            return Err(JavaRuntimeLookupError::Probe(
                "processor command does not match its retained Java authority".to_string(),
            ));
        }
        self.install_directory
            .validate_absolute_projection(self.install_directory.path())
            .map_err(|error| JavaRuntimeLookupError::Probe(error.to_string()))?;
        if !matches!(
            self.install_directory
                .executable_guard_matches(&self.program_guard),
            Ok(true)
        ) {
            return Err(JavaRuntimeLookupError::Probe(
                "processor Java executable changed after admission".to_string(),
            ));
        }
        let selected = self.probe_receipt.revalidate_cli_executable()?;
        if selected != self.program_path {
            return Err(JavaRuntimeLookupError::Probe(
                "processor Java probe no longer selects the retained executable".to_string(),
            ));
        }
        self.install_directory
            .validate_absolute_projection(self.install_directory.path())
            .map_err(|error| JavaRuntimeLookupError::Probe(error.to_string()))
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
    observer(RuntimeEnsureEvent::ManagedRuntimeReady {
        component: component.as_str().to_string(),
    });
    Ok(receipt)
}

#[cfg(all(feature = "test-support", unix))]
const MANAGED_RUNTIME_FIXTURE_JAVA_BYTES: &[u8] = br#"#!/bin/sh
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
#[cfg(all(feature = "test-support", not(unix)))]
const MANAGED_RUNTIME_FIXTURE_JAVA_BYTES: &[u8] = b"axial managed runtime fixture";

#[cfg(feature = "test-support")]
pub struct ManagedRuntimeRebuildFixture {
    listener: tokio::net::TcpListener,
    source: RuntimeSourceReceipt,
}

#[cfg(feature = "test-support")]
impl ManagedRuntimeRebuildFixture {
    pub fn replace_known_good_runtime_projection(
        &self,
        active: &crate::known_good::KnownGoodInventory,
    ) -> Result<crate::known_good::KnownGoodInventory, crate::known_good::KnownGoodInventoryError>
    {
        let runtime_only = crate::known_good::runtime_inventory_from_source(&self.source)?;
        crate::known_good::replace_runtime_projection(active, runtime_only, self.source.component())
    }
}

#[cfg(feature = "test-support")]
pub async fn prepare_managed_runtime_rebuild_fixture_for_test(
    component: RuntimeId,
) -> Result<ManagedRuntimeRebuildFixture, ManagedRuntimeRebuildError> {
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
    let source = super::manifest::authenticated_runtime_rebuild_fixture_source(
        component,
        format!("http://{address}/java"),
        MANAGED_RUNTIME_FIXTURE_JAVA_BYTES,
    )
    .map_err(ManagedRuntimeRebuildError::Preparation)?;
    crate::known_good::runtime_inventory_from_source(&source).map_err(|_| {
        ManagedRuntimeRebuildError::Preparation(JavaRuntimeLookupError::Install(
            "runtime rebuild fixture inventory derivation failed".to_string(),
        ))
    })?;
    Ok(ManagedRuntimeRebuildFixture { listener, source })
}

#[cfg(feature = "test-support")]
pub async fn rebuild_managed_runtime_prepared_fixture_for_test(
    cache: &ManagedRuntimeCache,
    fixture: ManagedRuntimeRebuildFixture,
) -> Result<ManagedRuntimeCommitReceipt, ManagedRuntimeRebuildError> {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let ManagedRuntimeRebuildFixture { listener, source } = fixture;
    let inventory = crate::known_good::runtime_inventory_from_source(&source).map_err(|_| {
        ManagedRuntimeRebuildError::Preparation(JavaRuntimeLookupError::Install(
            "runtime rebuild fixture inventory derivation failed".to_string(),
        ))
    })?;
    tokio::spawn(async move {
        let expected_requests = if cfg!(windows) { 2 } else { 1 };
        for _ in 0..expected_requests {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut request = [0_u8; 1024];
            let _ = socket.read(&mut request).await;
            let headers = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                MANAGED_RUNTIME_FIXTURE_JAVA_BYTES.len()
            );
            if socket.write_all(headers.as_bytes()).await.is_ok() {
                let _ = socket.write_all(MANAGED_RUNTIME_FIXTURE_JAVA_BYTES).await;
            }
        }
    });
    let component = source.component().clone();
    let mut observer = |_| {};
    let receipt =
        rebuild_managed_runtime_component_from_source(cache, &component, source, &mut observer)
            .await?;
    if !receipt.matches_known_good_inventory(&inventory) {
        return Err(receipt.into_failure(JavaRuntimeLookupError::Install(
            "runtime rebuild fixture failed sealed postcondition verification".to_string(),
        )));
    }
    Ok(receipt)
}

#[cfg(feature = "test-support")]
pub async fn rebuild_managed_runtime_fixture_for_test(
    cache: &ManagedRuntimeCache,
    component: RuntimeId,
) -> Result<ManagedRuntimeCommitReceipt, ManagedRuntimeRebuildError> {
    let fixture = prepare_managed_runtime_rebuild_fixture_for_test(component).await?;
    rebuild_managed_runtime_prepared_fixture_for_test(cache, fixture).await
}

#[cfg(feature = "test-support")]
pub fn persist_managed_runtime_source_fixture_for_test(
    cache: &ManagedRuntimeCache,
    component: RuntimeId,
    java_url: String,
    java_bytes: &[u8],
) -> Result<PathBuf, JavaRuntimeLookupError> {
    if !is_known_runtime_component(component.as_str()) {
        return Err(JavaRuntimeLookupError::Install(
            "runtime source fixture target is outside the closed managed component vocabulary"
                .to_string(),
        ));
    }
    let root = cache
        .component_root(component.as_str())
        .ok_or_else(|| JavaRuntimeLookupError::Install("invalid runtime source fixture".into()))?;
    let source = super::manifest::authenticated_runtime_rebuild_fixture_source(
        component, java_url, java_bytes,
    )?;
    let proof = super::manifest::component_manifest_proof_bytes(source.manifest())
        .map_err(|error| JavaRuntimeLookupError::Install(error.to_string()))?;
    std::fs::create_dir_all(&root)
        .map_err(|error| JavaRuntimeLookupError::Install(error.to_string()))?;
    #[cfg(windows)]
    {
        let config = root.join("lib").join("jvm.cfg");
        std::fs::create_dir_all(
            config
                .parent()
                .expect("managed runtime fixture config has a parent"),
        )
        .map_err(|error| JavaRuntimeLookupError::Install(error.to_string()))?;
        std::fs::write(config, java_bytes)
            .map_err(|error| JavaRuntimeLookupError::Install(error.to_string()))?;
    }
    std::fs::write(
        root.join(super::manifest::COMPONENT_MANIFEST_PROOF_FILE),
        proof,
    )
    .map_err(|error| JavaRuntimeLookupError::Install(error.to_string()))?;
    Ok(root)
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
    install_directory: &crate::managed_fs::ManagedDir,
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
        install_directory,
        &source_receipt,
        max_entries,
        max_bytes,
        &mut observer,
    )
    .await?;
    let install_root = install_directory.path();
    install_directory
        .validate_absolute_projection(&install_root)
        .map_err(|error| JavaRuntimeLookupError::Install(error.to_string()))?;
    let java_path = super::layout::java_executable(&install_root);
    admit_processor_program(install_directory, &java_path)?;
    let probe_receipt = tokio::task::spawn_blocking(move || {
        probe_java_runtime_receipt(&java_path, Some("ephemeral-processor-runtime"))
    })
    .await
    .map_err(|_| {
        JavaRuntimeLookupError::Probe(
            "processor runtime probe task stopped unexpectedly".to_string(),
        )
    })?;
    install_directory
        .validate_absolute_projection(&install_root)
        .map_err(|error| JavaRuntimeLookupError::Install(error.to_string()))?;
    let probe_receipt = probe_receipt?;
    let program_path = probe_receipt.revalidate_cli_executable()?;
    let program_guard = admit_processor_program(install_directory, &program_path)?;
    if probe_receipt.validation().into_info().major
        != u32::try_from(java_version.major_version).unwrap_or(u32::MAX)
    {
        return Err(JavaRuntimeLookupError::Probe(
            "processor runtime Java major does not match the authenticated base requirement"
                .to_string(),
        ));
    }
    let runtime = ProcessorRuntime {
        probe_receipt,
        install_directory: install_directory.clone(),
        program_guard,
        program_path,
        _source_receipt: source_receipt,
    };
    runtime.validate_program(runtime.cli_executable_path())?;
    Ok(runtime)
}

fn admit_processor_program(
    install_directory: &crate::managed_fs::ManagedDir,
    program: &Path,
) -> Result<crate::managed_fs::ManagedExecutableGuard, JavaRuntimeLookupError> {
    let root = install_directory.path();
    install_directory
        .validate_absolute_projection(root)
        .map_err(|error| JavaRuntimeLookupError::Install(error.to_string()))?;
    let relative_path = program.strip_prefix(root).map_err(|_| {
        JavaRuntimeLookupError::Install(
            "processor Java executable escaped its retained runtime".to_string(),
        )
    })?;
    let relative =
        crate::portable_path::PortableRelativePath::from_path(relative_path).map_err(|_| {
            JavaRuntimeLookupError::Install(
                "processor Java executable path is not portable".to_string(),
            )
        })?;
    let guard = install_directory
        .inspect_relative_executable(&relative)
        .map_err(|error| JavaRuntimeLookupError::Install(error.to_string()))?
        .ok_or_else(|| {
            JavaRuntimeLookupError::Install("processor Java executable is not retained".to_string())
        })?;
    if !matches!(install_directory.executable_guard_matches(&guard), Ok(true)) {
        return Err(JavaRuntimeLookupError::Install(
            "processor Java executable changed during admission".to_string(),
        ));
    }
    install_directory
        .validate_absolute_projection(root)
        .map_err(|error| JavaRuntimeLookupError::Install(error.to_string()))?;
    Ok(guard)
}

pub(crate) async fn materialize_preferred_runtime_source<F>(
    cache: &ManagedRuntimeCache,
    java_version: &JavaVersion,
    source_receipt: RuntimeSourceReceipt,
    observer: &mut F,
    control: &mut RuntimeMaterializationTaskControl,
) -> Result<Option<RuntimeSourceReceipt>, JavaRuntimeLookupError>
where
    F: FnMut(RuntimeEnsureEvent),
{
    let component = RuntimeId::from(preferred_runtime_component(java_version));
    if source_receipt.component() != &component || !is_known_runtime_component(component.as_str()) {
        return Err(JavaRuntimeLookupError::Install(
            "runtime source does not match the preferred managed component".to_string(),
        ));
    }
    let source_receipt = match verify_cached_managed_runtime_until_cancelled(
        cache,
        &component,
        java_version.major_version,
        source_receipt,
        control.cancellation(),
    )
    .await?
    {
        CachedManagedRuntimeVerification::Matched(verified) => {
            let (_runtime, source) = verified.into_parts();
            observer(RuntimeEnsureEvent::ManagedRuntimeReady {
                component: component.as_str().to_string(),
            });
            return Ok(Some(source));
        }
        CachedManagedRuntimeVerification::Mismatched(source) => source,
        CachedManagedRuntimeVerification::Cancelled => return Ok(None),
    };
    observer(RuntimeEnsureEvent::DownloadingManagedRuntime {
        component: component.as_str().to_string(),
    });
    let staged = stage_managed_runtime_until_cancelled(
        cache,
        &component,
        source_receipt,
        observer,
        control.cancellation(),
    )
    .await?;
    let Some(staged) = staged else {
        return Ok(None);
    };
    #[cfg(test)]
    wait_for_runtime_test_hook(
        RuntimeTestHookPoint::BeforePublicationClaim,
        &cache
            .component_root(component.as_str())
            .expect("staged runtime has a managed cache root"),
    )
    .await;
    if !control.claim_publication_settlement() {
        discard_staged_managed_runtime(staged).await?;
        return Ok(None);
    }
    let verified = publish_staged_managed_runtime_and_finalize(staged)
        .await
        .map_err(ManagedRuntimeRebuildError::into_lookup_error)?
        .into_verified_runtime(cache, &component, java_version.major_version)
        .map_err(ManagedRuntimeRebuildError::into_lookup_error)?;
    let (_runtime, source_receipt) = verified.into_parts();
    observer(RuntimeEnsureEvent::ManagedRuntimeReady {
        component: component.as_str().to_string(),
    });
    Ok(Some(source_receipt))
}

pub async fn ensure_runtime_with_events<F, Admit, Permit>(
    cache: &ManagedRuntimeCache,
    java_version: &JavaVersion,
    override_path: &str,
    force_managed: bool,
    probe_receipt: Option<&JavaRuntimeProbeReceipt>,
    admit_managed_mutation: Admit,
    observer: F,
) -> Result<RuntimeEnsureResult, JavaRuntimeLookupError>
where
    F: FnMut(RuntimeEnsureEvent),
    Admit: FnOnce() -> Result<Permit, ManagedRuntimeMutationRefused>,
{
    ensure_runtime_with_events_from_source(
        RuntimeEnsureRequest {
            cache,
            java_version,
            override_path,
            force_managed,
            probe_receipt,
            source: RuntimeEnsureSource::Production,
        },
        admit_managed_mutation,
        observer,
    )
    .await
}

#[cfg(feature = "test-support")]
pub async fn ensure_runtime_with_persisted_manifest_for_test<F, Admit, Permit>(
    cache: &ManagedRuntimeCache,
    java_version: &JavaVersion,
    override_path: &str,
    force_managed: bool,
    probe_receipt: Option<&JavaRuntimeProbeReceipt>,
    admit_managed_mutation: Admit,
    observer: F,
) -> Result<RuntimeEnsureResult, JavaRuntimeLookupError>
where
    F: FnMut(RuntimeEnsureEvent),
    Admit: FnOnce() -> Result<Permit, ManagedRuntimeMutationRefused>,
{
    ensure_runtime_with_events_from_source(
        RuntimeEnsureRequest {
            cache,
            java_version,
            override_path,
            force_managed,
            probe_receipt,
            source: RuntimeEnsureSource::PersistedManifest,
        },
        admit_managed_mutation,
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

struct RuntimeEnsureRequest<'a> {
    cache: &'a ManagedRuntimeCache,
    java_version: &'a JavaVersion,
    override_path: &'a str,
    force_managed: bool,
    probe_receipt: Option<&'a JavaRuntimeProbeReceipt>,
    source: RuntimeEnsureSource,
}

async fn ensure_runtime_with_events_from_source<F, Admit, Permit>(
    request: RuntimeEnsureRequest<'_>,
    admit_managed_mutation: Admit,
    mut observer: F,
) -> Result<RuntimeEnsureResult, JavaRuntimeLookupError>
where
    F: FnMut(RuntimeEnsureEvent),
    Admit: FnOnce() -> Result<Permit, ManagedRuntimeMutationRefused>,
{
    let RuntimeEnsureRequest {
        cache,
        java_version,
        override_path,
        force_managed,
        probe_receipt,
        source,
    } = request;
    let mut mutation_admission = Some(admit_managed_mutation);
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
        let managed_launch = if requested_runtime.source == RuntimeSource::Managed {
            Some(managed_runtime_launch_receipt(cache, &requested_runtime)?)
        } else {
            None
        };
        if requested_runtime.source == RuntimeSource::Managed {
            observer(RuntimeEnsureEvent::ManagedRuntimeReady {
                component: requested_runtime.id.as_str().to_string(),
            });
        }
        return Ok(RuntimeEnsureResult {
            requested: Some(requested_runtime.clone()),
            effective: requested_runtime,
            probe_usage,
            managed_launch,
        });
    }

    let managed = ensure_managed_runtime_with_events(
        cache,
        &requirement,
        source,
        &mut mutation_admission,
        &mut observer,
    )
    .await?;

    Ok(RuntimeEnsureResult {
        requested,
        effective: managed.effective,
        probe_usage,
        managed_launch: Some(managed.launch_receipt),
    })
}
struct ManagedEnsure {
    effective: RuntimeRecord,
    launch_receipt: super::layout::ManagedRuntimeLaunchReceipt,
}

async fn ensure_managed_runtime_with_events<F, Admit, Permit>(
    cache: &ManagedRuntimeCache,
    requirement: &RuntimeRequirement,
    source: RuntimeEnsureSource,
    mutation_admission: &mut Option<Admit>,
    observer: &mut F,
) -> Result<ManagedEnsure, JavaRuntimeLookupError>
where
    F: FnMut(RuntimeEnsureEvent),
    Admit: FnOnce() -> Result<Permit, ManagedRuntimeMutationRefused>,
{
    let preferred = &requirement.preferred_component;
    match resolve_managed_runtime(cache, preferred) {
        Ok(runtime) => {
            let launch_receipt = managed_runtime_launch_receipt(cache, &runtime)?;
            observer(RuntimeEnsureEvent::ManagedRuntimeReady {
                component: preferred.as_str().to_string(),
            });
            return Ok(ManagedEnsure {
                effective: runtime,
                launch_receipt,
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
            if runtime_record_matches_source(cache, &runtime, &source_receipt).await {
                let launch_receipt = managed_runtime_launch_receipt(cache, &runtime)?;
                observer(RuntimeEnsureEvent::ManagedRuntimeReady {
                    component: preferred.as_str().to_string(),
                });
                return Ok(ManagedEnsure {
                    effective: runtime,
                    launch_receipt,
                });
            }
        }
        Err(error @ JavaRuntimeLookupError::RosettaRequired { .. }) => return Err(error),
        Err(_) => {}
    }

    let mutation_permit = admit_managed_runtime_mutation(mutation_admission)?;
    observer(RuntimeEnsureEvent::DownloadingManagedRuntime {
        component: preferred.as_str().to_string(),
    });
    let verified =
        install_managed_runtime_component_from_source(cache, preferred, source_receipt, observer)
            .await
            .map_err(ManagedRuntimeRebuildError::into_lookup_error)?
            .into_verified_runtime(cache, preferred, requirement.required_java.major_version)
            .map_err(ManagedRuntimeRebuildError::into_lookup_error)?;
    let (runtime, _source_receipt) = verified.into_parts();
    observer(RuntimeEnsureEvent::ManagedRuntimeReady {
        component: preferred.as_str().to_string(),
    });
    let launch_receipt = managed_runtime_launch_receipt(cache, &runtime)?;
    drop(mutation_permit);
    Ok(ManagedEnsure {
        effective: runtime,
        launch_receipt,
    })
}

fn managed_runtime_launch_receipt(
    cache: &ManagedRuntimeCache,
    runtime: &RuntimeRecord,
) -> Result<super::layout::ManagedRuntimeLaunchReceipt, JavaRuntimeLookupError> {
    if runtime.source != RuntimeSource::Managed || !is_known_runtime_component(runtime.id.as_str())
    {
        return Err(JavaRuntimeLookupError::Install(
            "managed launch receipt target is outside the runtime cache".to_string(),
        ));
    }
    let component = cache
        .admit_component(runtime.id.as_str())
        .map_err(|error| JavaRuntimeLookupError::Install(error.to_string()))?
        .ok_or_else(|| {
            JavaRuntimeLookupError::Install(
                "managed launch receipt component is unavailable".to_string(),
            )
        })?;
    let expected_java = component.java_executable_path();
    if Path::new(&runtime.java_path) != expected_java
        || Path::new(&runtime.root_dir) != component.root_path()
    {
        return Err(JavaRuntimeLookupError::Install(
            "managed launch record does not match its retained component".to_string(),
        ));
    }
    component
        .launch_receipt()
        .map_err(|error| JavaRuntimeLookupError::Install(error.to_string()))
}

fn admit_managed_runtime_mutation<Admit, Permit>(
    admission: &mut Option<Admit>,
) -> Result<Permit, JavaRuntimeLookupError>
where
    Admit: FnOnce() -> Result<Permit, ManagedRuntimeMutationRefused>,
{
    let admit = admission
        .take()
        .ok_or(JavaRuntimeLookupError::ManagedMutationRefused)?;
    admit().map_err(|_| JavaRuntimeLookupError::ManagedMutationRefused)
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
    cache: &ManagedRuntimeCache,
    runtime: &RuntimeRecord,
    source: &RuntimeSourceReceipt,
) -> bool {
    if runtime.source != RuntimeSource::Managed || &runtime.id != source.component() {
        return false;
    }
    let Ok(canonical) = cache
        .authority()
        .and_then(|root| root.open_child(source.component().as_str()))
    else {
        return false;
    };
    super::install::runtime_tree_matches_source(
        &canonical,
        Path::new(&runtime.root_dir),
        source,
        RuntimeTreeVerificationReason::EnsureSourceMatch,
    )
    .await
}

#[cfg(test)]
pub(super) async fn runtime_record_matches_source_for_test(
    cache: &ManagedRuntimeCache,
    runtime: &RuntimeRecord,
    source: &RuntimeSourceReceipt,
) -> bool {
    runtime_record_matches_source(cache, runtime, source).await
}
