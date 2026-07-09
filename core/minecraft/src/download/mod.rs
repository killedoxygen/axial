mod assets;
mod client;
mod facts;
mod install;
mod integrity;
mod libraries;
mod model;
mod path_safety;
mod plan;
mod runtime;
mod transfer;

pub use assets::{
    asset_object_hash_prefix, repair_virtual_assets_from_index, virtual_asset_destination,
};
pub use install::Downloader;
pub use integrity::{LauncherManagedArtifactReadiness, verify_existing_launcher_managed_artifact};
pub use integrity::{
    jar_contains_signed_metadata,
    verify_existing_launcher_managed_artifact_allowing_missing_checksum,
};
pub use libraries::{
    DownloadJob, download_libraries,
    download_libraries_allowing_missing_checksums_with_facts_and_descriptors,
    download_libraries_with_facts_and_descriptors, library_jobs_for,
};
pub use model::{
    DownloadError, DownloadProgress, ExecutionDownloadError, ExecutionDownloadFact,
    ExecutionDownloadFactKind, ExecutionDownloadOwnership, ExecutionDownloadReport,
    ExpectedIntegrity, SelectedDownloadArtifactDescriptor, SelectedDownloadArtifactKind,
};
pub use transfer::download_file_with_client_report;
#[cfg(test)]
pub(crate) use transfer::promote_launcher_managed_artifact_temp_once;
pub(crate) use transfer::write_launcher_managed_artifact_bytes_to_temp;

#[cfg(test)]
mod tests;
