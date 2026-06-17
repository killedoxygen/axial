mod assets;
mod client;
mod facts;
mod install;
mod integrity;
mod libraries;
mod model;
mod path_safety;
mod runtime;
mod transfer;

pub use install::Downloader;
pub use libraries::download_libraries;
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
