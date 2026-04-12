mod common;
mod fabric;
mod forge;
mod neoforge;
mod quilt;

use crate::loaders::types::{
    LoaderComponentId, LoaderError, LoaderGameVersion, LoaderVersionIndex,
};

pub async fn fetch_supported_versions(
    component_id: LoaderComponentId,
) -> Result<Vec<LoaderGameVersion>, LoaderError> {
    match component_id {
        LoaderComponentId::Fabric => fabric::fetch_game_versions().await,
        LoaderComponentId::Quilt => quilt::fetch_game_versions().await,
        LoaderComponentId::Forge => forge::fetch_game_versions().await,
        LoaderComponentId::NeoForge => neoforge::fetch_game_versions().await,
    }
}

pub async fn fetch_build_index(
    component_id: LoaderComponentId,
    minecraft_version: &str,
) -> Result<LoaderVersionIndex, LoaderError> {
    match component_id {
        LoaderComponentId::Fabric => fabric::fetch_builds(minecraft_version).await,
        LoaderComponentId::Quilt => quilt::fetch_builds(minecraft_version).await,
        LoaderComponentId::Forge => forge::fetch_builds(minecraft_version).await,
        LoaderComponentId::NeoForge => neoforge::fetch_builds(minecraft_version).await,
    }
}
