pub(crate) mod common;
mod fabric;
mod forge;
mod neoforge;
mod quilt;

use crate::loaders::types::{
    LoaderBuildRecord, LoaderComponentId, LoaderError, LoaderGameVersion,
    LoaderProviderFailureKind, LoaderVersionIndex,
};

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct ProfileInstallProof {
    provider_url: String,
    canonical_profile_id: String,
    inherits_from: String,
    client_main_class: String,
    required_libraries: Vec<ProfileLibraryProof>,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct ProfileLibraryProof {
    coordinate: String,
    sha1: Option<String>,
    size: Option<u64>,
}

impl ProfileInstallProof {
    pub(crate) fn provider_url(&self) -> &str {
        &self.provider_url
    }

    pub(crate) fn identity(&self) -> (&str, &str, &str) {
        (
            &self.canonical_profile_id,
            &self.inherits_from,
            &self.client_main_class,
        )
    }

    pub(crate) fn required_libraries(&self) -> &[ProfileLibraryProof] {
        &self.required_libraries
    }

    #[cfg(test)]
    pub(crate) fn from_test(
        canonical_profile_id: String,
        inherits_from: String,
        client_main_class: String,
        required_libraries: Vec<ProfileLibraryProof>,
    ) -> Self {
        Self {
            provider_url: "https://fixtures.invalid/profile-proof.json".to_string(),
            canonical_profile_id,
            inherits_from,
            client_main_class,
            required_libraries,
        }
    }
}

impl ProfileLibraryProof {
    pub(crate) fn coordinate(&self) -> &str {
        &self.coordinate
    }

    pub(crate) fn exact_integrity(&self) -> Option<(&str, u64)> {
        self.sha1.as_deref().zip(self.size)
    }

    pub(crate) fn has_partial_integrity(&self) -> bool {
        self.sha1.is_some() != self.size.is_some()
    }

    #[cfg(test)]
    pub(crate) fn from_test(coordinate: String, sha1: Option<String>, size: Option<u64>) -> Self {
        Self {
            coordinate,
            sha1,
            size,
        }
    }
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

#[cfg(test)]
pub(crate) async fn fetch_profile_install_proof_from_url_for_test(
    record: &LoaderBuildRecord,
    url: &str,
) -> Result<ProfileInstallProof, LoaderError> {
    match record.component_id {
        LoaderComponentId::Fabric => {
            fabric::fetch_profile_install_proof_from_url_for_test(record, url).await
        }
        LoaderComponentId::Quilt => {
            quilt::fetch_profile_install_proof_from_url_for_test(record, url).await
        }
        LoaderComponentId::Forge | LoaderComponentId::NeoForge => {
            Err(LoaderError::ProviderDataInvalid {
                kind: LoaderProviderFailureKind::SchemaInvalid,
                status: None,
            })
        }
    }
}
