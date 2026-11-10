use super::cancellation::{
    RuntimeCancellation, RuntimeCancellationSender, runtime_cancellation_channel,
};
#[cfg(test)]
use super::cancellation::{
    RuntimeTestGate, RuntimeTestHookPoint, arm_runtime_test_hook, wait_for_runtime_test_hook,
};
use super::discovery::{
    is_known_runtime_component, preferred_runtime_component, runtime_requirement,
};
use super::install::{
    CachedManagedRuntimeVerification, ManagedRuntimeCommitReceipt, ManagedRuntimeRebuildError,
    MissingRuntimePreparation, discard_staged_managed_runtime,
    ephemeral_processor_filesystem_failure, install_ephemeral_processor_runtime,
    prepare_missing_runtime_until_cancelled, publish_staged_managed_runtime_and_finalize,
    stage_managed_runtime_until_cancelled, trace_ephemeral_processor_runtime_failure,
    verify_cached_managed_runtime_until_cancelled,
};
use super::layout::ManagedRuntimeCache;
use super::manifest::RuntimeSourceReceipt;
use super::model::{JavaRuntimeLookupError, RuntimeEnsureEvent, RuntimeId};
use super::probe::{JavaRuntimeProbeReceipt, probe_java_runtime_receipt};
use crate::launch::JavaVersion;
use axial_resource::{PhysicalIoClass, PhysicalWorkRequest, process_physical_work};
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
pub enum RuntimeMaterializationCancellation {
    Cancelled,
    SettlementRequired,
}

pub struct RuntimeMaterializationCancelHandle {
    phase: Arc<AtomicU8>,
    cancellation: RuntimeCancellationSender,
}

pub struct RuntimeMaterializationTaskControl {
    phase: Arc<AtomicU8>,
    cancellation: RuntimeCancellation,
}

pub fn runtime_materialization_control() -> (
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
    pub fn cancel_before_publication(&self) -> RuntimeMaterializationCancellation {
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

    pub fn finish(&self) -> bool {
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
        let error = JavaRuntimeLookupError::Install(
            "processor runtime source does not match the authenticated base requirement"
                .to_string(),
        );
        trace_ephemeral_processor_runtime_failure("source_requirement", &error);
        return Err(error);
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
        .validate_absolute_projection(install_root)
        .map_err(|error| ephemeral_processor_filesystem_failure("installed_projection", error))?;
    let java_path = super::layout::java_executable(install_root);
    admit_processor_program(install_directory, &java_path).inspect_err(|error| {
        trace_ephemeral_processor_runtime_failure("executable_admission", error);
    })?;
    let probe_receipt = run_runtime_probe_work(
        "processor runtime probe task stopped unexpectedly",
        move || probe_java_runtime_receipt(&java_path, Some("ephemeral-processor-runtime")),
    )
    .await
    .inspect_err(|error| {
        trace_ephemeral_processor_runtime_failure("probe_worker", error);
    })?;
    install_directory
        .validate_absolute_projection(install_root)
        .map_err(|error| ephemeral_processor_filesystem_failure("post_probe_projection", error))?;
    let probe_receipt = probe_receipt.inspect_err(|error| {
        trace_ephemeral_processor_runtime_failure("probe", error);
    })?;
    let program_path = probe_receipt
        .revalidate_cli_executable()
        .inspect_err(|error| {
            trace_ephemeral_processor_runtime_failure("probe_receipt_validation", error);
        })?;
    let program_guard =
        admit_processor_program(install_directory, &program_path).inspect_err(|error| {
            trace_ephemeral_processor_runtime_failure("probed_executable_admission", error);
        })?;
    let observed_major = probe_receipt.major();
    let expected_major = u32::try_from(java_version.major_version).unwrap_or(u32::MAX);
    if observed_major != expected_major {
        let error = JavaRuntimeLookupError::Probe(
            "processor runtime Java major does not match the authenticated base requirement"
                .to_string(),
        );
        tracing::warn!(
            stage = "probed_major_validation",
            category = "probe",
            expected_major,
            observed_major,
            "Ephemeral processor runtime preparation failed"
        );
        return Err(error);
    }
    let runtime = ProcessorRuntime {
        probe_receipt,
        install_directory: install_directory.clone(),
        program_guard,
        program_path,
        _source_receipt: source_receipt,
    };
    runtime
        .validate_program(runtime.cli_executable_path())
        .inspect_err(|error| {
            trace_ephemeral_processor_runtime_failure("final_validation", error);
        })?;
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

pub async fn materialize_missing_runtime_source<F>(
    cache: &ManagedRuntimeCache,
    java_version: &JavaVersion,
    source: RuntimeSourceReceipt,
    observer: &mut F,
    control: &mut RuntimeMaterializationTaskControl,
) -> Result<Option<ManagedRuntimeCommitReceipt>, ManagedRuntimeRebuildError>
where
    F: FnMut(RuntimeEnsureEvent),
{
    let component = RuntimeId::from(preferred_runtime_component(java_version));
    if source.component() != &component || !is_known_runtime_component(component.as_str()) {
        return Err(JavaRuntimeLookupError::ManagedMutationRefused.into());
    }
    let prepared = prepare_missing_runtime_until_cancelled(
        cache,
        &component,
        source,
        observer,
        control.cancellation(),
    )
    .await?;
    let receipt = match prepared {
        None => return Ok(None),
        Some(MissingRuntimePreparation::Ready(receipt)) => receipt,
        Some(MissingRuntimePreparation::Staged(staged)) => {
            #[cfg(test)]
            wait_for_runtime_test_hook(
                RuntimeTestHookPoint::BeforePublicationClaim,
                &cache
                    .component_root(component.as_str())
                    .expect("admitted runtime root"),
            )
            .await;
            if !control.claim_publication_settlement() {
                discard_staged_managed_runtime(staged).await?;
                return Ok(None);
            }
            publish_staged_managed_runtime_and_finalize(staged).await?
        }
    };
    observer(RuntimeEnsureEvent::ManagedRuntimeReady {
        component: component.as_str().to_owned(),
    });
    Ok(Some(receipt))
}

pub async fn materialize_preferred_runtime_source<F>(
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

async fn run_runtime_probe_work<T, Work>(
    stopped: &'static str,
    work: Work,
) -> Result<T, JavaRuntimeLookupError>
where
    T: Send + 'static,
    Work: FnOnce() -> T + Send + 'static,
{
    let admission = process_physical_work()
        .admit(PhysicalWorkRequest::foreground(PhysicalIoClass::Heavy, 0))
        .await
        .map_err(|_| JavaRuntimeLookupError::Probe(stopped.to_string()))?;
    admission
        .run(move |_| work())
        .await
        .map_err(|_| JavaRuntimeLookupError::Probe(stopped.to_string()))
}
