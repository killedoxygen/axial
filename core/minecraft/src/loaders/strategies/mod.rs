mod common;
mod fabric_profile;
mod forge_earliest_legacy;
mod forge_legacy_installer;
mod forge_modern;
mod neoforge_modern;
mod quilt_profile;

use crate::download::DownloadProgress;
use crate::loaders::types::{LoaderError, LoaderInstallPlan, LoaderInstallStrategy};
use std::path::Path;

pub async fn install_build<F>(
    library_dir: &Path,
    plan: &LoaderInstallPlan,
    mut send: F,
) -> Result<String, LoaderError>
where
    F: FnMut(DownloadProgress),
{
    match plan.record.strategy {
        LoaderInstallStrategy::FabricProfile => {
            Box::pin(fabric_profile::install(library_dir, plan, &mut send)).await
        }
        LoaderInstallStrategy::QuiltProfile => {
            Box::pin(quilt_profile::install(library_dir, plan, &mut send)).await
        }
        LoaderInstallStrategy::ForgeModern => {
            Box::pin(forge_modern::install(library_dir, plan, &mut send)).await
        }
        LoaderInstallStrategy::ForgeLegacyInstaller => {
            Box::pin(forge_legacy_installer::install(
                library_dir,
                plan,
                &mut send,
            ))
            .await
        }
        LoaderInstallStrategy::ForgeEarliestLegacy => {
            Box::pin(forge_earliest_legacy::install(library_dir, plan, &mut send)).await
        }
        LoaderInstallStrategy::NeoForgeModern => {
            Box::pin(neoforge_modern::install(library_dir, plan, &mut send)).await
        }
    }
}
