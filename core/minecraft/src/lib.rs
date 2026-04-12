pub mod download;
pub mod integrity;
pub mod java;
pub mod launch;
pub mod loaders;
pub mod manifest;
pub mod paths;
pub mod profiles;
pub mod rules;
pub mod runtime;
pub mod types;
pub mod version;

pub use download::{DownloadError, DownloadProgress, Downloader};
pub use java::{
    JavaRuntimeInfo, JavaRuntimeLookupError, JavaRuntimeResult, ensure_java_runtime,
    find_java_runtime, is_known_runtime_component, list_java_runtimes, preferred_runtime_component,
    probe_java_runtime_info,
};
pub use launch::{
    JavaVersion, LaunchModelError, LaunchVars, ResolvedLibrary, VersionJson, build_classpath,
    client_jar_path, load_version_json, offline_uuid, resolve_arguments, resolve_libraries,
    resolve_version,
};
pub use loaders::{
    LoaderArtifactKind, LoaderAvailability, LoaderBuildId, LoaderBuildRecord, LoaderCatalogState,
    LoaderComponentId, LoaderComponentRecord, LoaderError, LoaderGameVersion,
    LoaderInstallFailureKind, LoaderInstallStrategy, LoaderInstallability, LoaderVersionIndex,
    build_id_for, fetch_builds, fetch_components, fetch_supported_versions,
    infer_build_from_version_id, infer_neoforge_minecraft_version, install_build,
    installed_version_id_for, loader_components, parse_build_id, resolve_build_record,
};
pub use manifest::{ManifestEntry, VersionManifest, fetch_version_manifest};
pub use paths::{
    cache_dir, create_minecraft_dir, default_minecraft_dir, is_legacy_assets, libraries_dir,
    loader_artifacts_dir, loader_cache_dir, loader_catalog_dir, loader_work_dir, runtime_dirs,
    validate_installation, versions_dir,
};
pub use profiles::ensure_launcher_profiles;
pub use rules::{
    Environment, Rule, current_os_arch, current_os_name, default_environment, evaluate_rules,
    is_native_library, native_classifier_key,
};
pub use runtime::{
    RuntimeEnsureAction, RuntimeEnsureResult, RuntimeId, RuntimeInstallState, RuntimeOverride,
    RuntimeRecord, RuntimeRequirement, RuntimeSource, ensure_runtime, list_runtime_records,
    parse_runtime_override, runtime_requirement,
};
pub use types::VersionEntry;
pub use version::scan_versions;
