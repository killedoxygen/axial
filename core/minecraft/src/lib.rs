mod artifact_path;
mod asset_index;
pub mod download;
pub mod known_good;
mod known_good_libraries;
mod known_good_reconstruction;
pub mod launch;
pub mod lifecycle;
pub mod loaders;
mod managed_blocking;
mod managed_component_ancestor_journal;
mod managed_component_cache;
mod managed_component_effects;
mod managed_component_lifecycle;
mod managed_component_publication;
mod managed_component_source_spool;
mod managed_component_spool;
mod managed_component_table;
mod managed_fs;

pub mod managed_path {
    pub use crate::managed_fs::{
        AnchoredDirectory, AnchoredFileMoveOutcome, AnchoredFileMoveReceipt,
        AnchoredFileRestoreOutcome,
    };
}
mod managed_publication;
pub mod manifest;
pub mod paths;
pub mod rules;
pub mod runtime;
pub mod types;
pub mod version;
mod version_bundle_publication;
pub mod version_meta;

pub use asset_index::{AssetIndexFlagsError, asset_index_requires_virtual_repair};
pub use download::{DownloadError, DownloadProgress, Downloader};
pub use known_good::{KnownGoodInstallReceipt, KnownGoodReconstructionReceipt};
pub use known_good_reconstruction::{
    KnownGoodReconstructionError, ManagedAssetsCommitReceipt, ManagedAssetsRebuildError,
    ManagedAssetsRollbackEffect, ManagedAssetsRollbackReceipt, ManagedLibrariesCommitReceipt,
    ManagedLibrariesRebuildError, ManagedLibrariesRollbackEffect, ManagedLibrariesRollbackReceipt,
    ManagedVersionBundleCommitReceipt, ManagedVersionBundleRebuildError,
    ManagedVersionBundleRollbackEffect, ManagedVersionBundleRollbackReceipt,
    rebuild_managed_assets, rebuild_managed_libraries, rebuild_managed_version_bundle,
    reconstruct_known_good,
};
#[cfg(feature = "test-support")]
pub use known_good_reconstruction::{
    rebuild_managed_assets_fixture_for_test, rebuild_managed_libraries_fixture_for_test,
    rebuild_managed_version_bundle_fixture_for_test,
    rebuild_managed_version_bundle_rollback_fixture_for_test,
};
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
    LoaderComponentRecord, LoaderError, LoaderGameVersion, LoaderInstallError,
    LoaderInstallFailureKind, LoaderInstallStrategy, LoaderInstallability,
    LoaderPreOperationFailureKind, LoaderProviderFailureKind, LoaderSelectionMeta,
    LoaderSelectionReason, LoaderSelectionSource, LoaderTerm, LoaderTermEvidence, LoaderTermSource,
    LoaderVersionIndex, MaterializedLoaderProfile, build_id_for, fetch_builds, fetch_cached_builds,
    fetch_components, fetch_supported_versions, install_build, installed_version_id_for,
    loader_components, parse_build_id, resolve_build_record_for_install,
    validate_materialized_loader_profile,
};
pub use manifest::{ManifestEntry, VersionManifest, fetch_version_manifest_cached};
pub use paths::{
    cache_dir, create_minecraft_dir, libraries_dir, loader_cache_dir, loader_catalog_dir,
    version_manifest_cache_path, versions_dir,
};
pub use rules::default_environment;
pub use runtime::{
    JavaRuntimeInfo, JavaRuntimeLookupError, JavaRuntimeProbeReceipt, JavaRuntimeProbeResolution,
    JavaRuntimeProbeResolutionError, JavaRuntimeProbeSnapshot, JavaRuntimeResult,
    ManagedRuntimeCache, ManagedRuntimeCommitReceipt, ManagedRuntimeFailureReceipt,
    ManagedRuntimeMutationRefused, ManagedRuntimeQuarantineObligation,
    ManagedRuntimeQuarantineObservation, ManagedRuntimeRebuildError, RuntimeEnsureEvent,
    RuntimeEnsureResult, RuntimeId, RuntimeInstallState, RuntimeOverride, RuntimeProbeSource,
    RuntimeProbeUsage, RuntimeRecord, RuntimeRequirement, RuntimeSource, RuntimeSourceFailure,
    RuntimeSourceFailureKind, ensure_runtime_with_events, is_known_runtime_component,
    list_java_runtimes, managed_runtime_contents_verified_without_probe, parse_runtime_override,
    preferred_runtime_component, probe_java_runtime_receipt, rebuild_managed_runtime_component,
    resolve_java_runtime_probe, runtime_component_executable_present_without_probe,
    runtime_component_structurally_ready_without_probe, runtime_executable_ready_without_probe,
    runtime_requirement, snapshot_java_runtime,
};
#[cfg(feature = "test-support")]
pub use runtime::{
    ensure_runtime_with_persisted_manifest_for_test,
    persist_managed_runtime_source_fixture_for_test, rebuild_managed_runtime_fixture_for_test,
};
pub use types::{VersionEntry, VersionLoaderAttachment, VersionSubjectKind};
pub use version::{
    VersionBundleReadGuard, VersionScanDependencyStamp, VersionScanIssue, VersionScanIssueKind,
    VersionScanReport, VersionScanSnapshot, VersionScanState, scan_versions, scan_versions_report,
    scan_versions_snapshot,
};
pub use version_meta::{
    MinecraftVersionMeta, ReleaseReference, analyze_minecraft_version, compare_version_entries,
    compare_version_like, enrich_loader_game_versions, enrich_version_entries,
    manifest_release_entries, manifest_release_references,
};
