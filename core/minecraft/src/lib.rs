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
pub mod portable_path;

pub mod managed_path {
    #[cfg(feature = "test-support")]
    pub use crate::managed_fs::ManagedLibraryTestAuthority;
    pub use crate::managed_fs::{
        ManagedContentCancelReceipt, ManagedContentCommitReceipt, ManagedContentCompleteTransfers,
        ManagedContentDeferredManifest, ManagedContentEncodedManifest,
        ManagedContentIssuedTransfer, ManagedContentManifestBindOutcome,
        ManagedContentManifestObservationFailure, ManagedContentMutationPlan,
        ManagedContentObservationError, ManagedContentObservedState, ManagedContentPathMutation,
        ManagedContentPathObservation, ManagedContentPathResult, ManagedContentPayloadId,
        ManagedContentPayloadPlan, ManagedContentPlanError, ManagedContentPlanningBinding,
        ManagedContentPlanningObservationFailure, ManagedContentPlanningSession,
        ManagedContentPreparationError, ManagedContentPreparationOutcome,
        ManagedContentPreparedTransaction, ManagedContentReadyTransaction, ManagedContentRecovery,
        ManagedContentStageOutcome, ManagedContentTransactionFailure,
        ManagedContentTransactionOutcome, ManagedContentTransactionRoot,
        ManagedContentTransactionSession, ManagedContentTransferAdvance,
        ManagedContentTransferBatch, ManagedContentTransferSettlement, ManagedContentTransferStep,
        ManagedContentTransferTask, ManagedLibraryAdmissionRebindFailure, ManagedLibraryBinding,
        ManagedLibraryOperation, ManagedLibraryRetirement, ManagedLibraryRetirementBinding,
        ManagedLibraryRoot, ManagedLibraryWitness, ManagedTreeCopyFailure, ManagedTreeCopyLimits,
        ManagedTreeCopyOutcome, ManagedTreeDirectory, ManagedTreeOperation, ManagedTreeRetirement,
        ManagedTreeRoot, PreparedManagedLibraryAdmissionRebind,
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
    ManagedInstallRollbackEffect, ManagedInstallRolledBackEvidence,
    RegisteredKnownGoodBootstrapVerificationFailure,
    RegisteredKnownGoodBootstrapVerificationFailureKind,
    RegisteredKnownGoodBootstrapVerificationRecovery, VerifiedManagedInstallCheckpointReceipt,
    VerifiedManagedInstallReceipt, VerifiedRegisteredKnownGoodBootstrap,
    classify_managed_install_publication, classify_managed_install_publication_candidates,
    verify_managed_install_loader_base_checkpoint,
    verify_managed_install_publication_evidence_root,
    verify_managed_install_reconstruction_checkpoint, verify_registered_known_good_bootstrap,
};
pub use known_good::{KnownGoodInstallReceipt, KnownGoodReconstructionReceipt};
#[cfg(feature = "test-support")]
pub use known_good::{
    managed_install_reconstruction_receipt_fixture_for_test,
    managed_version_bundle_activation_source_fixture_for_test,
};
pub use known_good_reconstruction::{
    KnownGoodReconstructionError, ManagedAssetsCommitReceipt, ManagedAssetsRebuildError,
    ManagedAssetsRollbackEffect, ManagedAssetsRollbackReceipt, ManagedLibrariesCommitReceipt,
    ManagedLibrariesRebuildError, ManagedLibrariesRollbackEffect, ManagedLibrariesRollbackReceipt,
    ManagedVersionBundleAcknowledgementOutcome, ManagedVersionBundleAcknowledgementRecovery,
    ManagedVersionBundleCommitReceipt, ManagedVersionBundleExpectedSettlement,
    ManagedVersionBundleOrphanOutcome, ManagedVersionBundleOrphanRecovery,
    ManagedVersionBundleOrphanSettlement, ManagedVersionBundleRebuildError,
    ManagedVersionBundleRebuildRecovery, ManagedVersionBundleRollbackEffect,
    ManagedVersionBundleRollbackReceipt, ManagedVersionBundleSettlementOutcome,
    rebuild_managed_assets, rebuild_managed_libraries, rebuild_managed_version_bundle,
    reconstruct_known_good, recover_guardian_version_bundle_orphan,
    recover_managed_version_bundle_acknowledgement,
};
#[cfg(feature = "test-support")]
pub use known_good_reconstruction::{
    rebuild_managed_assets_fixture_for_test, rebuild_managed_libraries_fixture_for_test,
    rebuild_managed_version_bundle_fixture_for_source_test,
    rebuild_managed_version_bundle_fixture_for_test,
    rebuild_managed_version_bundle_rollback_fixture_for_source_test,
    rebuild_managed_version_bundle_rollback_fixture_for_test,
    rebuild_registered_managed_assets_fixture_for_test,
    rebuild_registered_managed_libraries_fixture_for_test,
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
    LoaderBuildMetadata, LoaderBuildRecord, LoaderBuildSubjectKind, LoaderCatalogState,
    LoaderComponentId, LoaderComponentRecord, LoaderError, LoaderGameVersion,
    LoaderInstallBaseActivationError, LoaderInstallBaseCheckpointVerificationFailure,
    LoaderInstallBaseCommit, LoaderInstallBaseCommitVerificationFailure,
    LoaderInstallBaseContinuation, LoaderInstallError, LoaderInstallFailureKind,
    LoaderInstallPublicationOutcome, LoaderInstallStrategy, LoaderInstallability,
    LoaderPreOperationFailureKind, LoaderProviderFailureKind, LoaderSelectionMeta,
    LoaderSelectionReason, LoaderSelectionSource, LoaderTerm, LoaderTermEvidence, LoaderTermSource,
    LoaderVersionIndex, MaterializedLoaderProfile, VerifiedLoaderInstallBaseCheckpoint,
    VerifiedLoaderInstallBaseCommit, build_id_for, continue_install_build_after_base, fetch_builds,
    fetch_cached_builds, fetch_components, fetch_supported_versions, install_build,
    installed_version_id_for, is_canonical_installed_loader_id, loader_components, parse_build_id,
    resolve_build_record_for_install, resume_install_build_after_base,
    validate_materialized_loader_profile,
};
#[cfg(feature = "test-support")]
pub use loaders::{
    persist_loader_build_cache_fixture_for_test,
    persist_loader_supported_versions_cache_fixture_for_test,
};
#[cfg(feature = "test-support")]
pub use manifest::persist_version_manifest_cache_fixture_for_test;
pub use manifest::{ManifestEntry, VersionManifest, fetch_version_manifest_cached};
pub use paths::{libraries_dir, versions_dir};
pub use rules::default_environment;
pub use runtime::{
    JavaRuntimeInfo, JavaRuntimeLookupError, JavaRuntimeProbeReceipt, JavaRuntimeProbeResolution,
    JavaRuntimeProbeResolutionError, JavaRuntimeProbeSnapshot, JavaRuntimeResult,
    ManagedRuntimeCache, ManagedRuntimeCommitReceipt, ManagedRuntimeComponent,
    ManagedRuntimeFailureReceipt, ManagedRuntimeLaunchReceipt, ManagedRuntimeMarkerState,
    ManagedRuntimeMutationRefused, ManagedRuntimeQuarantineObligation,
    ManagedRuntimeQuarantineObservation, ManagedRuntimeRebuildError, RuntimeEnsureEvent,
    RuntimeEnsureResult, RuntimeId, RuntimeInstallState, RuntimeOverride, RuntimeProbeSource,
    RuntimeProbeUsage, RuntimeRecord, RuntimeRequirement, RuntimeSource, RuntimeSourceFailure,
    RuntimeSourceFailureKind, ensure_runtime_with_events, is_known_runtime_component,
    list_java_runtimes, parse_runtime_override, preferred_runtime_component,
    probe_java_runtime_receipt, rebuild_managed_runtime_component, resolve_java_runtime_probe,
    runtime_component_executable_present_without_probe,
    runtime_component_structurally_ready_without_probe, runtime_executable_ready_without_probe,
    runtime_requirement, snapshot_java_runtime,
};
#[cfg(feature = "test-support")]
pub use runtime::{
    ManagedRuntimeRebuildFixture, ensure_runtime_with_persisted_manifest_for_test,
    persist_managed_runtime_source_fixture_for_test,
    prepare_managed_runtime_rebuild_fixture_for_test, rebuild_managed_runtime_fixture_for_test,
    rebuild_managed_runtime_prepared_fixture_for_test, runtime_publication_lock_available_for_test,
};
pub use types::{VersionEntry, VersionLoaderAttachment, VersionSubjectKind};
#[cfg(feature = "test-support")]
pub use version::VersionBundlePublicationGuardForTest;
pub use version::{
    VersionBundleReadGuard, VersionScanDependencyStamp, VersionScanIssue, VersionScanIssueKind,
    VersionScanReport, VersionScanSnapshot, VersionScanState, scan_versions, scan_versions_report,
    scan_versions_snapshot,
};
#[cfg(feature = "test-support")]
pub use version_bundle_publication::fail_after_promotions_for_test;
pub use version_meta::{
    MinecraftVersionMeta, ReleaseReference, analyze_minecraft_version, compare_version_entries,
    compare_version_like, enrich_loader_game_versions, enrich_version_entries,
    manifest_release_entries, manifest_release_references,
};
