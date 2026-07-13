mod assets;
mod client;
mod facts;
mod install;
mod integrity;
mod libraries;
mod library_source;
mod model;
mod path_safety;
mod plan;
mod promotion;
mod runtime;
mod transfer;
mod transfer_failure;

pub(crate) use assets::parse_asset_index;
pub use assets::{
    asset_object_hash_prefix, repair_virtual_assets_from_index, virtual_asset_destination,
};
pub use install::Downloader;
pub(crate) use install::{
    CompletedVanillaInstallAuthority, PendingVanillaInstallSourceAuthority,
    ReconstructedVanillaAuthority,
};
pub(crate) use install::{
    reconstruct_installer_library_declarations, reconstruct_installer_processor_sources,
    reconstruct_profile_library_declarations,
};
pub use integrity::{
    LauncherManagedArtifactReadiness, jar_contains_signed_metadata,
    verify_existing_launcher_managed_artifact,
};
pub(crate) use libraries::DownloadJob;
pub(crate) use libraries::download_installer_libraries_with_declarations_and_facts_and_descriptors;
pub(crate) use libraries::{
    LibraryArtifactPlan, download_profile_libraries_with_declarations_and_facts_and_descriptors,
    library_artifact_plans_for,
};
pub use libraries::{
    LibraryVerificationIntegrity, LibraryVerificationPlan, library_verification_plans_for,
};
pub use model::{
    DownloadError, DownloadProgress, ExecutionDownloadError, ExecutionDownloadFact,
    ExecutionDownloadFactKind, ExecutionDownloadOwnership, ExecutionDownloadReport,
    ExpectedIntegrity, LibraryPlanError, SelectedDownloadArtifactDescriptor,
    SelectedDownloadArtifactKind,
};
pub(crate) use model::{ExactLibraryDownloadProof, MaterializedLibraryIdentity};
pub(crate) use transfer::AuthenticatedSelectedArtifactSource;
pub use transfer::download_file_with_client_report;
#[cfg(test)]
pub(crate) use transfer::promote_launcher_managed_artifact_temp_once;
pub(crate) use transfer::write_launcher_managed_artifact_bytes_to_temp;

#[cfg(test)]
mod tests;
