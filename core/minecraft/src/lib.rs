mod asset_index;
mod cancellation;
#[cfg(test)]
pub(crate) fn test_temp_root() -> std::path::PathBuf {
    std::fs::canonicalize(std::env::temp_dir()).expect("canonical temporary test parent")
}

pub mod download;
pub mod http;
pub mod known_good;
mod known_good_libraries;
mod known_good_reconstruction;
pub mod launch;
pub mod lifecycle;
pub mod loaders;
mod managed;
pub mod portable_path;
pub mod process;

pub mod managed_path {
    #[cfg(feature = "test-support")]
    pub use crate::managed::fs::ManagedLibraryTestAuthority;
    pub use crate::managed::fs::{
        FileAbsence, FileObservation, ManagedContentCancelReceipt, ManagedContentCheckpointError,
        ManagedContentCommitReceipt, ManagedContentCompleteTransfers,
        ManagedContentEncodedManifest, ManagedContentIssuedTransfer,
        ManagedContentManifestObservationFailure, ManagedContentMutationPlan,
        ManagedContentObservationError, ManagedContentObservedState, ManagedContentPathMutation,
        ManagedContentPathObservation, ManagedContentPathResult, ManagedContentPayloadId,
        ManagedContentPayloadPlan, ManagedContentPlanError,
        ManagedContentPlanningObservationFailure, ManagedContentPlanningSession,
        ManagedContentPreparationError, ManagedContentPreparationOutcome,
        ManagedContentPreparedTransaction, ManagedContentReadyTransaction, ManagedContentRecovery,
        ManagedContentStageOutcome, ManagedContentStagingCheckpoint,
        ManagedContentTransactionFailure, ManagedContentTransactionOutcome,
        ManagedContentTransactionRoot, ManagedContentTransactionSession,
        ManagedContentTransferAdvance, ManagedContentTransferBatch,
        ManagedContentTransferSettlement, ManagedContentTransferStep, ManagedContentTransferTask,
        ManagedLibraryBinding, ManagedLibraryFile, ManagedLibraryFileBatch,
        ManagedLibraryOperation, ManagedLibraryRoot, ManagedLibraryWitness, ManagedNativeDirectory,
        ManagedNativePublicationFailure, ManagedTreeCopyFailure, ManagedTreeCopyLimits,
        ManagedTreeCopyOutcome, ManagedTreeDirectory, ManagedTreeOperation, ManagedTreeRoot,
    };
}
pub mod manifest;
pub mod paths;
pub mod rules;
pub mod runtime;
pub mod types;
pub mod version;
pub mod version_meta;

pub use asset_index::{
    AssetIndexError, LegacyAssetFile, LegacyAssetLayout, LegacyAssets, legacy_assets,
};
#[cfg(feature = "test-support")]
pub use download::publish_managed_install_fixture_for_test;
pub use download::{
    DownloadError, DownloadFileFailureClass, DownloadProgress, Downloader,
    KnownGoodActivationRejected, ManagedInstallAcknowledgementOutcome,
    ManagedInstallAcknowledgementRecovery, ManagedInstallActivationContractId,
    ManagedInstallActivationContractIdError, ManagedInstallCheckpointVerificationFailure,
    ManagedInstallCommittedEvidence, ManagedInstallDurableOutcome, ManagedInstallDurableRecovery,
    ManagedInstallPostActivationAcknowledgement, ManagedInstallPublicationCandidates,
    ManagedInstallPublicationCandidatesError, ManagedInstallPublicationEvidenceId,
    ManagedInstallPublicationEvidenceIdError, ManagedInstallReceiptVerificationFailure,
    ManagedInstallRollbackEffect, ManagedInstallRolledBackEvidence, RecordedVersionMetadata,
    VerifiedManagedInstallCheckpointReceipt, VerifiedManagedInstallReceipt,
    classify_managed_install_publication, classify_managed_install_publication_candidates,
    verify_managed_install_loader_base_checkpoint,
    verify_managed_install_publication_evidence_root,
    verify_managed_install_reconstruction_checkpoint,
};
pub use known_good::{KnownGoodInstallReceipt, KnownGoodReconstructionReceipt};
#[cfg(feature = "test-support")]
pub use known_good::{
    managed_install_reconstruction_receipt_fixture_for_test,
    managed_version_bundle_activation_source_fixture_for_test,
};
pub use known_good_reconstruction::{KnownGoodReconstructionError, reconstruct_known_good};
pub use launch::{
    JavaVersion, LaunchVars, ResolvedLibrary, VersionJson, build_classpath,
    effective_java_version_for, java_component_for_major, java_major_for_component, offline_uuid,
    resolve_arguments, resolve_libraries,
};
pub use lifecycle::LifecycleChannel;
pub use loaders::{
    LOADER_CATALOG_SCHEMA_VERSION, LoaderArtifactKind, LoaderAvailability, LoaderBuildId,
    LoaderBuildMetadata, LoaderBuildRecord, LoaderBuildSubjectKind, LoaderCatalogState,
    LoaderComponentId, LoaderComponentRecord, LoaderError, LoaderGameVersion,
    LoaderInstallBaseActivationError, LoaderInstallBaseCheckpointVerificationFailure,
    LoaderInstallBaseCommit, LoaderInstallBaseCommitVerificationFailure,
    LoaderInstallBaseContinuation, LoaderInstallError, LoaderInstallFailureKind,
    LoaderInstallPublicationOutcome, LoaderInstallStrategy, LoaderInstallability,
    LoaderPreOperationFailureKind, LoaderProviderFailureKind, LoaderSelectionMeta,
    LoaderSelectionReason, LoaderSelectionSource, LoaderTerm, LoaderTermEvidence, LoaderTermSource,
    LoaderVersionIndex, MaterializedLoaderProfile, VerifiedLoaderInstallBaseCheckpoint,
    VerifiedLoaderInstallBaseCommit, build_id_for, continue_install_build_after_base,
    fetch_components, install_build, installed_version_id_for, is_canonical_installed_loader_id,
    loader_components, parse_build_id, resolve_build_record_for_install,
    resume_install_build_after_base, validate_materialized_loader_profile,
};
#[cfg(feature = "test-support")]
pub use loaders::{
    fetch_cached_builds, persist_loader_build_cache_fixture_for_test,
    persist_loader_supported_versions_cache_fixture_for_test,
};
#[cfg(feature = "test-support")]
pub use manifest::persist_version_manifest_cache_fixture_for_test;
pub use manifest::{ManifestEntry, VersionManifest};
pub use paths::libraries_dir;
pub use rules::default_environment;
#[cfg(feature = "test-support")]
pub use runtime::runtime_publication_lock_available_for_test;
pub use runtime::{
    JavaArchitecture, JavaProbeError, JavaRequirement, JavaRequirementRefusal, JavaProbeReceipt, JavaRuntimeInfo, JavaRuntimeLookupError,
    JavaRuntimeResult, ManagedRuntimeCache, ManagedRuntimeCommitReceipt, ManagedRuntimeComponent,
    ManagedRuntimeFailureReceipt, ManagedRuntimeLaunchReceipt, ManagedRuntimeMarkerState,
    ManagedRuntimeQuarantineObligation, ManagedRuntimeQuarantineObservation,
    ManagedRuntimeRebuildError, RuntimeEnsureEvent, RuntimeId, RuntimeInstallState,
    RuntimeOverride, RuntimeRecord, RuntimeRequirement, RuntimeSource, RuntimeSourceFailure,
    RuntimeSourceFailureKind, is_known_runtime_component, list_java_runtimes,
    parse_runtime_override, preferred_runtime_component, probe_java, runtime_requirement,
};
pub use types::{VersionEntry, VersionLoaderAttachment, VersionSubjectKind};
#[cfg(feature = "test-support")]
pub use version::VersionBundlePublicationGuardForTest;
pub use version::{
    VersionBundleReadGuard, VersionScanDependencyStamp, VersionScanIssue, VersionScanIssueKind,
    VersionScanReport, VersionScanSnapshot, VersionScanState, scan_versions_snapshot,
};
pub use version_meta::{
    MinecraftVersionMeta, ReleaseReference, analyze_minecraft_version, compare_version_entries,
    compare_version_like, enrich_loader_game_versions, enrich_version_entries,
    manifest_release_entries, manifest_release_references,
};
