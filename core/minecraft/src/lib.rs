pub mod download;
pub mod integrity;
pub mod java;
pub mod launch;
pub mod loaders;
pub mod manifest;
pub mod paths;
pub mod profiles;
pub mod rules;
pub mod types;
pub mod version;

pub use download::{DownloadError, DownloadProgress, Downloader};
pub use java::{
    JavaRuntimeInfo, JavaRuntimeLookupError, JavaRuntimeResult, find_java_runtime,
    list_java_runtimes, probe_java_runtime_info,
};
pub use launch::{
    JavaVersion, LaunchModelError, LaunchVars, ResolvedLibrary, VersionJson, build_classpath,
    client_jar_path, load_version_json, offline_uuid, resolve_arguments, resolve_libraries,
    resolve_version,
};
pub use loaders::{
    GameVersion, LoaderError, LoaderType, LoaderVersion, fetch_game_versions,
    fetch_loader_versions, install_loader,
};
pub use manifest::{ManifestEntry, VersionManifest, fetch_version_manifest};
pub use paths::{
    create_minecraft_dir, default_minecraft_dir, is_legacy_assets, libraries_dir, runtime_dirs,
    validate_installation, versions_dir,
};
pub use profiles::ensure_launcher_profiles;
pub use rules::{
    Environment, Rule, current_os_arch, current_os_name, default_environment, evaluate_rules,
    is_native_library, native_classifier_key,
};
pub use types::VersionEntry;
pub use version::scan_versions;
