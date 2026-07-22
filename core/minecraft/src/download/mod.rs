mod asset_source;
mod assets;
mod client;
mod content_transfer;
mod facts;
mod install;
mod integrity;
mod libraries;
pub(crate) mod library_source;
mod model;
mod path_safety;
mod plan;
mod runtime;
mod transfer;
mod transient_transfer;

#[cfg(feature = "test-support")]
pub(crate) use asset_source::AssetSourcePool;
pub(crate) use asset_source::{
    AuthenticatedAssetCacheProofSet, RetainedAssetComponentSource, RetainedAssetSourceSet,
};
pub use assets::repair_virtual_assets_from_index;
pub(crate) use assets::{ASSET_OBJECT_BASE_URL, parse_asset_index};
pub use content_transfer::{
    MAX_VERIFIED_CONTENT_STAGING_BYTES, VerifiedStagedContent, VerifiedStagedContentError,
    download_owned_verified_content_to_staging, download_verified_content_to_staging,
};
pub use install::Downloader;
pub(crate) use install::{
    AuthenticatedVanillaInstallSources, AuthenticatedVersionBundleMemberSource,
    AuthenticatedVersionBundleSource, ManagedReconstructionContext, PreparedManagedInstall,
    ReconstructedVanillaAuthority, ReconstructedVanillaAuthorityParts,
    RegisteredVersionBundleSourceError, RetainedVersionBundleReconstructionSources,
    prepare_local_managed_install, publish_prepared_managed_install,
};
pub(crate) use install::{
    reconstruct_installer_library_declarations, reconstruct_installer_processor_sources,
    reconstruct_profile_library_declarations,
};
pub use integrity::LauncherManagedArtifactReadiness;
pub(crate) use libraries::DownloadJob;
pub(crate) use libraries::download_installer_libraries_with_declarations_and_facts;
pub(crate) use libraries::{
    LibraryArtifactPlan, download_profile_retained_libraries_with_declarations_and_facts,
    library_artifact_plans_for,
};
pub use libraries::{
    LibraryVerificationIntegrity, LibraryVerificationPlan, library_verification_plans_for,
};
pub(crate) use model::ExactLibraryDownloadProof;
pub use model::{
    DownloadError, DownloadProgress, ExecutionDownloadError, ExecutionDownloadFact,
    ExecutionDownloadFactKind, ExecutionDownloadReport, ExpectedIntegrity, LibraryPlanError,
    SelectedDownloadArtifactKind, VerifiedContentIntegrity,
};
pub(crate) use transfer::AuthenticatedSelectedArtifactSource;
pub use transient_transfer::{
    CreateOnlyTransferTarget, ExpectedTransferDigests, ManagedTransferAuthority,
    PinnedTransferOrigin, PinnedTransferOriginError, RetryPolicy, RetryPolicyError,
    SourceOnlyTransferTarget, TransferByteContract, TransferCancellation,
    TransferCancellationSender, TransferCleanupObligation, TransferCleanupResolution,
    TransferClient, TransferClientBuildError, TransferClientConfig, TransferClientConfigError,
    TransferContract, TransferContractError, TransferDigestAlgorithm, TransferDigestParseError,
    TransferFailureEvent, TransferFailureKind, TransferFailureReport, TransferOutcome,
    TransferPublicationObligation, TransferPublicationOutcome, TransferReport, TransferTask,
    TransferTargetCancelObligation, TransferTargetCancelOutcome, TransferTimeoutKind,
    TransferOrigin, TransferOriginError, VerifiedCreateOnly, VerifiedSource,
    VerifiedTransferDigests, VerifiedTransferDiscardObligation, VerifiedTransferDiscardOutcome,
    start_create_only_transfer, start_source_transfer, transfer_cancellation_channel,
};

#[cfg(test)]
mod tests;
