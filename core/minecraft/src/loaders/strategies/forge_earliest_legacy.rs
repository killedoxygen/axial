use super::common::install_from_legacy_archive;
use crate::download::DownloadProgress;
use crate::loaders::types::{LoaderError, LoaderInstallPlan, LoaderInstallSource};
use std::path::Path;

pub async fn install<F>(
    library_dir: &Path,
    plan: &LoaderInstallPlan,
    send: &mut F,
) -> Result<String, LoaderError>
where
    F: FnMut(DownloadProgress),
{
    let LoaderInstallSource::LegacyArchive { url } = &plan.record.install_source else {
        return Err(LoaderError::InvalidProfile(
            "earliest Forge build requires a legacy archive source".to_string(),
        ));
    };
    install_from_legacy_archive(library_dir, plan, url, send).await
}
