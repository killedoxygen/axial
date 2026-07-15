mod asset_source;
mod assets;
mod client;
mod facts;
mod install;
mod integrity;
mod libraries;
pub(crate) mod library_source;
mod model;
mod path_safety;
mod plan;
mod promotion;
mod runtime;
mod transfer;
mod transfer_failure;

#[cfg(feature = "test-support")]
pub(crate) use asset_source::AssetSourcePool;
pub(crate) use asset_source::{
    AuthenticatedAssetCacheProofSet, RetainedAssetComponentSource, RetainedAssetSourceSet,
};
pub use assets::repair_virtual_assets_from_index;
pub(crate) use assets::{ASSET_OBJECT_BASE_URL, parse_asset_index};
pub use install::Downloader;
pub(crate) use install::{
    AuthenticatedVanillaInstallSources, AuthenticatedVersionBundleMemberSource,
    AuthenticatedVersionBundleSource, ManagedReconstructionContext, PreparedManagedInstall,
    ReconstructedVanillaAuthority, prepare_local_managed_install, publish_prepared_managed_install,
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
    SelectedDownloadArtifactKind,
};
pub(crate) use transfer::AuthenticatedSelectedArtifactSource;
#[cfg(test)]
pub(crate) use transfer::promote_launcher_managed_artifact_temp_once;
pub(crate) use transfer::write_launcher_managed_artifact_bytes_to_temp;

#[cfg(test)]
mod tests;
