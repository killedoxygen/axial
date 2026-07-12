pub(crate) mod common;
mod fabric;
mod forge;
mod neoforge;
mod quilt;

use crate::loaders::types::{
    LoaderBuildRecord, LoaderComponentId, LoaderError, LoaderGameVersion,
    LoaderProviderFailureKind, LoaderVersionIndex,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProfileInstallProof {
    pub(crate) canonical_profile_id: String,
    pub(crate) inherits_from: String,
    pub(crate) client_main_class: String,
    pub(crate) required_libraries: Vec<ProfileLibraryProof>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProfileLibraryProof {
    pub(crate) coordinate: String,
    pub(crate) sha1: Option<String>,
    pub(crate) size: Option<u64>,
}

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

pub(crate) async fn fetch_profile_install_proof(
    record: &LoaderBuildRecord,
) -> Result<ProfileInstallProof, LoaderError> {
    match record.component_id {
        LoaderComponentId::Fabric => fabric::fetch_profile_install_proof(record).await,
        LoaderComponentId::Quilt => quilt::fetch_profile_install_proof(record).await,
        LoaderComponentId::Forge | LoaderComponentId::NeoForge => {
            Err(LoaderError::ProviderDataInvalid {
                kind: LoaderProviderFailureKind::SchemaInvalid,
                status: None,
            })
        }
    }
}
