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

pub(crate) use assets::parse_asset_index;
pub use assets::repair_virtual_assets_from_index;
pub use install::Downloader;
pub(crate) use install::{
    AuthenticatedVanillaInstallSources, AuthenticatedVersionBundleMemberSource,
    AuthenticatedVersionBundleSource, PreparedVersionBundlePublication,
    ReconstructedVanillaAuthority, acquire_version_bundle_publication_lease,
    prepare_local_version_bundle_publication, publish_prepared_version_bundle_install,
};
pub(crate) use install::{
    reconstruct_installer_library_declarations, reconstruct_installer_processor_sources,
    reconstruct_profile_library_declarations,
};
pub use integrity::LauncherManagedArtifactReadiness;
pub(crate) use libraries::DownloadJob;
pub(crate) use libraries::download_installer_libraries_with_declarations_and_facts;
pub(crate) use libraries::{
    LibraryArtifactPlan, download_profile_libraries_with_declarations_and_facts,
    library_artifact_plans_for,
};
pub use libraries::{
    LibraryVerificationIntegrity, LibraryVerificationPlan, library_verification_plans_for,
};
pub use model::{
    DownloadError, DownloadProgress, ExecutionDownloadError, ExecutionDownloadFact,
    ExecutionDownloadFactKind, ExecutionDownloadOwnership, ExecutionDownloadReport,
    ExpectedIntegrity, LibraryPlanError, SelectedDownloadArtifactKind,
};
pub(crate) use model::{ExactLibraryDownloadProof, MaterializedLibraryIdentity};
pub(crate) use transfer::AuthenticatedSelectedArtifactSource;
#[cfg(test)]
pub(crate) use transfer::promote_launcher_managed_artifact_temp_once;
pub(crate) use transfer::write_launcher_managed_artifact_bytes_to_temp;

#[cfg(test)]
mod tests;
