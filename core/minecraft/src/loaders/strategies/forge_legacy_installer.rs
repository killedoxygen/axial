use super::common::install_from_installer_source;
use crate::download::DownloadProgress;
use crate::known_good::KnownGoodInstallReceipt;
use crate::loaders::types::{LoaderError, LoaderInstallPlan};
use std::path::Path;

pub async fn install<F>(
    library_dir: &Path,
    plan: &LoaderInstallPlan,
    send: &mut F,
) -> Result<KnownGoodInstallReceipt, LoaderError>
where
    F: FnMut(DownloadProgress),
{
    install_from_installer_source(library_dir, plan, send).await
}
