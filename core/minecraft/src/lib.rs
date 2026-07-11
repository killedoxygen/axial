mod asset_index;
pub mod download;
pub mod integrity;
pub mod launch;
pub mod lifecycle;
pub mod loaders;
pub mod manifest;
pub mod paths;
pub mod profiles;
pub mod rules;
pub mod runtime;
pub mod types;
pub mod version;
pub mod version_meta;

pub use asset_index::{AssetIndexFlagsError, asset_index_requires_virtual_repair};
pub use download::{DownloadError, DownloadProgress, Downloader};
pub use launch::{
    JavaVersion, LaunchModelError, LaunchVars, ResolvedLibrary, VersionJson, build_classpath,
    client_jar_path, effective_java_version_for, java_component_for_major,
    java_major_for_component, load_version_json, offline_uuid, resolve_arguments,
    resolve_libraries, resolve_version,
};
pub use lifecycle::{LifecycleChannel, LifecycleLabel, LifecycleMeta};
pub use loaders::{
    LOADER_CATALOG_SCHEMA_VERSION, LoaderArtifactKind, LoaderAvailability, LoaderBuildId,
    LoaderBuildMetadata, LoaderBuildRecord, LoaderCatalogState, LoaderComponentId,
    LoaderComponentRecord, LoaderError, LoaderGameVersion, LoaderInstallFailureKind,
    LoaderInstallStrategy, LoaderInstallability, LoaderProviderFailureKind, LoaderSelectionMeta,
    LoaderSelectionReason, LoaderSelectionSource, LoaderTerm, LoaderTermEvidence, LoaderTermSource,
    LoaderVersionIndex, build_id_for, fetch_builds, fetch_cached_builds, fetch_components,
    fetch_supported_versions, install_build, installed_version_id_for, loader_components,
    parse_build_id, resolve_build_record,
};
pub use manifest::{
    ManifestEntry, VersionManifest, fetch_version_manifest, fetch_version_manifest_cached,
};
pub use paths::{
    cache_dir, create_minecraft_dir, default_minecraft_dir, libraries_dir, loader_artifacts_dir,
    loader_cache_dir, loader_catalog_dir, loader_work_dir, runtime_dirs, validate_installation,
    version_manifest_cache_path, versions_dir,
};
pub use profiles::ensure_launcher_profiles;
pub use rules::{
    Environment, Rule, current_os_arch, current_os_name, default_environment, evaluate_rules,
    is_native_library, native_classifier_key,
};
pub use runtime::{
    JavaRuntimeInfo, JavaRuntimeLookupError, JavaRuntimeProbeReceipt, JavaRuntimeResult,
    RuntimeEnsureAction, RuntimeEnsureEvent, RuntimeEnsureResult, RuntimeId, RuntimeInstallState,
    RuntimeOverride, RuntimeProbeSource, RuntimeProbeUsage, RuntimeRecord, RuntimeRequirement,
    RuntimeSource, ensure_java_runtime, ensure_runtime_with_events, find_java_runtime,
    is_known_runtime_component, list_java_runtimes, list_runtime_records,
    managed_runtime_contents_verified_without_probe, parse_runtime_override,
    preferred_runtime_component, probe_java_runtime_info, probe_java_runtime_receipt,
    runtime_component_executable_present_without_probe, runtime_component_ready_without_probe,
    runtime_executable_ready_without_probe, runtime_requirement,
};
pub use types::{VersionEntry, VersionLoaderAttachment, VersionSubjectKind};
pub use version::{
    VersionScanIssue, VersionScanIssueKind, VersionScanReport, VersionScanState, scan_versions,
    scan_versions_report,
};
pub use version_meta::{
    MinecraftVersionMeta, ReleaseReference, analyze_minecraft_version, apply_version_analysis,
    compare_version_entries, compare_version_like, enrich_loader_game_versions,
    enrich_version_entries, manifest_release_entries, manifest_release_references,
};
