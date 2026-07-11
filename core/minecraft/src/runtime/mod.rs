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
pub use ensure::{ensure_java_runtime, ensure_runtime_with_events};
pub use model::{
    JavaRuntimeInfo, JavaRuntimeLookupError, JavaRuntimeResult, RuntimeEnsureAction,
    RuntimeEnsureEvent, RuntimeEnsureResult, RuntimeId, RuntimeInstallState, RuntimeOverride,
    RuntimeProbeSource, RuntimeProbeUsage, RuntimeRecord, RuntimeRequirement, RuntimeSource,
};
pub use probe::{JavaRuntimeProbeReceipt, probe_java_runtime_info, probe_java_runtime_receipt};

#[cfg(test)]
use discovery::{detect_runtime_state, resolve_component_runtime_from_roots};
#[cfg(test)]
use ensure::{runtime_install_lock_file_path, runtime_install_lock_from_map};
#[cfg(test)]
use file_download::{
    RuntimeDownloadActual, RuntimeDownloadEvidence, RuntimeDownloadIntegrityError,
    component_manifest_destination, fetch_runtime_file, runtime_download_client,
    runtime_file_download_concurrency_for, runtime_windows_verbatim_path_string,
    verify_runtime_download,
};
#[cfg(test)]
use install::{
    install_managed_runtime_from_manifest_url, install_runtime_manifest_file,
    install_runtime_manifest_files, plan_runtime_manifest_files, remove_runtime_install_path,
    remove_runtime_install_path_async, select_runtime_manifest_url,
};
#[cfg(test)]
use layout::{java_executable, java_executable_for_os, runtime_os_arch_for};
#[cfg(test)]
use manifest::{
    ComponentManifest, ComponentManifestDownload, ComponentManifestDownloads,
    ComponentManifestFile, MAX_RUNTIME_MANIFEST_BYTES, RuntimeManifest, fetch_runtime_json,
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
