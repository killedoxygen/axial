//! Execution-owned Java runtime discovery, probing, and managed runtime facade.
//!
//! Child modules own the separate runtime capability axes: public model types,
//! runtime layout detection, Java probing, local discovery, managed runtime
//! ensure orchestration, manifest fetching, file download/integrity, and tests.

mod discovery;
mod ensure;
mod file_download;
mod install;
mod layout;
mod manifest;
mod model;
mod probe;
mod rosetta;

pub use discovery::{
    find_java_runtime, is_known_runtime_component, list_java_runtimes, list_runtime_records,
    managed_runtime_contents_verified_without_probe, parse_runtime_override,
    preferred_runtime_component, runtime_component_executable_present_without_probe,
    runtime_component_ready_without_probe, runtime_executable_ready_without_probe,
    runtime_requirement,
};
pub(crate) use ensure::{
    ProcessorRuntime, materialize_ephemeral_processor_runtime, materialize_preferred_runtime_source,
};
pub use ensure::{ensure_java_runtime, ensure_runtime_with_events};
pub use model::{
    JavaRuntimeInfo, JavaRuntimeLookupError, JavaRuntimeResult, RuntimeEnsureAction,
    RuntimeEnsureEvent, RuntimeEnsureResult, RuntimeId, RuntimeInstallState, RuntimeOverride,
    RuntimeProbeSource, RuntimeProbeUsage, RuntimeRecord, RuntimeRequirement, RuntimeSource,
};
pub use probe::{
    JavaRuntimeProbeReceipt, JavaRuntimeProbeResolution, JavaRuntimeProbeResolutionError,
    JavaRuntimeProbeSnapshot, probe_java_runtime_receipt, resolve_java_runtime_probe,
    snapshot_java_runtime,
};

#[cfg(test)]
use discovery::{detect_runtime_state, resolve_component_runtime_from_roots};
#[cfg(test)]
use ensure::runtime_record_matches_source_for_test;
#[cfg(test)]
use ensure::{runtime_install_lock_file_path, runtime_install_lock_from_map};
#[cfg(test)]
use file_download::{
    RuntimeDownloadActual, RuntimeDownloadEvidence, RuntimeDownloadIntegrityError,
    component_manifest_destination, fetch_runtime_file, runtime_download_client,
    runtime_file_download_concurrency_for, runtime_windows_verbatim_path_string,
    verify_runtime_download,
};
pub(crate) use install::plan_runtime_manifest_files;
#[cfg(test)]
use install::{
    install_managed_runtime, install_runtime_manifest_file, install_runtime_manifest_files,
    remove_runtime_install_path, remove_runtime_install_path_async,
    validate_ephemeral_processor_manifest_for_test,
};
#[cfg(test)]
use layout::{java_executable, java_executable_for_os, runtime_os_arch_for};
pub(crate) use manifest::{
    COMPONENT_MANIFEST_PROOF_FILE, ComponentManifest, RuntimeSourceReceipt,
    component_manifest_proof_bytes,
};

pub(crate) async fn acquire_preferred_runtime_source(
    java_version: &crate::launch::JavaVersion,
) -> Result<RuntimeSourceReceipt, JavaRuntimeLookupError> {
    let component = RuntimeId::from(preferred_runtime_component(java_version));
    if !is_known_runtime_component(component.as_str()) {
        return Err(JavaRuntimeLookupError::Download(
            "preferred runtime component is not in the closed managed-runtime vocabulary"
                .to_string(),
        ));
    }
    manifest::acquire_runtime_source(&component, &layout::runtime_os_arch()).await
}

#[cfg(test)]
#[derive(Clone)]
pub(crate) struct TestRuntimeSourceDescriptor {
    pub(crate) component: RuntimeId,
    pub(crate) url: String,
    pub(crate) sha1: String,
    pub(crate) size: u64,
}

#[cfg(test)]
pub(crate) async fn acquire_test_runtime_source(
    java_version: &crate::launch::JavaVersion,
    descriptor: &TestRuntimeSourceDescriptor,
) -> Result<RuntimeSourceReceipt, JavaRuntimeLookupError> {
    let preferred = RuntimeId::from(preferred_runtime_component(java_version));
    if descriptor.component != preferred || !is_known_runtime_component(preferred.as_str()) {
        return Err(JavaRuntimeLookupError::Download(
            "test runtime source does not match the preferred managed component".to_string(),
        ));
    }
    manifest::acquire_runtime_source_for_test(
        preferred,
        manifest::RuntimeDownloadManifest {
            url: descriptor.url.clone(),
            sha1: descriptor.sha1.clone(),
            size: descriptor.size,
        },
    )
    .await
}

#[cfg(test)]
pub(crate) fn authenticated_test_runtime_source(
    java_version: &crate::launch::JavaVersion,
) -> Result<RuntimeSourceReceipt, JavaRuntimeLookupError> {
    let preferred = RuntimeId::from(preferred_runtime_component(java_version));
    if !is_known_runtime_component(preferred.as_str()) {
        return Err(JavaRuntimeLookupError::Download(
            "test runtime source does not match the closed managed-runtime vocabulary".to_string(),
        ));
    }
    manifest::authenticated_runtime_source_fixture_for_test(preferred)
}
#[cfg(test)]
pub(crate) use manifest::{
    ComponentManifestDownload, ComponentManifestDownloads, ComponentManifestFile,
};
#[cfg(test)]
use manifest::{
    MAX_RUNTIME_MANIFEST_BYTES, RuntimeDownloadManifest, RuntimeManifest,
    acquire_runtime_source_for_test, fetch_runtime_manifest_bytes_for_test,
    runtime_source_url_is_secure_for_test, select_runtime_manifest,
};
#[cfg(test)]
use probe::detect_distribution;
#[cfg(test)]
use rosetta::{
    MachOArm64Compatibility, RosettaRuntimeDecision, parse_mach_o_arm64_compatibility,
    rosetta_requirement_for_managed_runtime,
};

#[cfg(test)]
mod tests;
