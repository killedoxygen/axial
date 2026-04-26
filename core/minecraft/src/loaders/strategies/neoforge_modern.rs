use super::common::install_from_installer_source;
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
    let LoaderInstallSource::InstallerJar { url } = &plan.record.install_source else {
        return Err(LoaderError::InvalidProfile(
            "NeoForge build requires an installer jar source".to_string(),
        ));
    };
    install_from_installer_source(library_dir, plan, url, send).await
}
