mod common;

pub(crate) use common::{
    AuthenticatedInstallerReconstructionAuthority, AuthenticatedLegacyOverlayAuthority,
};

use crate::download::DownloadProgress;
use crate::known_good::{KnownGoodInstallReceipt, KnownGoodReconstructionReceipt};
use crate::loaders::types::{LoaderError, LoaderInstallPlan, LoaderInstallStrategy};
use crate::runtime::ManagedRuntimeCache;
use std::path::Path;

pub async fn install_build<F>(
    library_dir: &Path,
    runtime_cache: &ManagedRuntimeCache,
    plan: &LoaderInstallPlan,
    mut send: F,
) -> Result<KnownGoodInstallReceipt, LoaderError>
where
    F: FnMut(DownloadProgress),
{
    match plan.record.strategy {
        LoaderInstallStrategy::FabricProfile | LoaderInstallStrategy::QuiltProfile => {
            Box::pin(common::install_from_profile_source(
                library_dir,
                runtime_cache,
                plan,
                &mut send,
            ))
            .await
        }
        LoaderInstallStrategy::ForgeModern
        | LoaderInstallStrategy::ForgeLegacyInstaller
        | LoaderInstallStrategy::NeoForgeModern => {
            Box::pin(common::install_from_installer_source(
                library_dir,
                runtime_cache,
                plan,
                &mut send,
            ))
            .await
        }
        LoaderInstallStrategy::ForgeEarliestLegacy => {
            Box::pin(common::install_from_legacy_archive(
                library_dir,
                runtime_cache,
                plan,
                &mut send,
            ))
            .await
        }
    }
}

pub(crate) async fn reconstruct_build(
    plan: &LoaderInstallPlan,
) -> Result<KnownGoodReconstructionReceipt, LoaderError> {
    match plan.record.strategy {
        LoaderInstallStrategy::FabricProfile | LoaderInstallStrategy::QuiltProfile => {
            Box::pin(common::reconstruct_from_profile_source(plan)).await
        }
        LoaderInstallStrategy::ForgeEarliestLegacy => {
            Box::pin(common::reconstruct_from_legacy_archive(plan)).await
        }
        LoaderInstallStrategy::ForgeModern
        | LoaderInstallStrategy::ForgeLegacyInstaller
        | LoaderInstallStrategy::NeoForgeModern => {
            Box::pin(common::reconstruct_from_installer_source(plan)).await
        }
    }
}
