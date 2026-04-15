use super::common::FABRIC_META_BASE;
use crate::loaders::api::{build_id_for, installed_version_id_for};
use crate::loaders::http::fetch_json;
use crate::loaders::types::{
    LoaderArtifactKind, LoaderBuildRecord, LoaderComponentId, LoaderGameVersion,
    LoaderInstallSource, LoaderInstallStrategy, LoaderInstallability, LoaderVersionIndex,
};
use crate::version_meta::VersionMeta;
use serde::Deserialize;

#[derive(Deserialize)]
struct FabricGameEntry {
    version: String,
    stable: bool,
}

#[derive(Deserialize)]
struct FabricLoaderEntry {
    loader: FabricLoaderVersion,
}

#[derive(Deserialize)]
struct FabricLoaderVersion {
    version: String,
    stable: bool,
}

pub async fn fetch_game_versions()
-> Result<Vec<LoaderGameVersion>, crate::loaders::types::LoaderError> {
    let raw = fetch_json::<Vec<FabricGameEntry>>(&format!("{FABRIC_META_BASE}/game")).await?;
    Ok(raw
        .into_iter()
        .map(|entry| LoaderGameVersion {
            version: entry.version,
            kind: String::new(),
            release_time: String::new(),
            meta: VersionMeta::default(),
            stable: entry.stable,
        })
        .collect())
}

pub async fn fetch_builds(
    minecraft_version: &str,
) -> Result<LoaderVersionIndex, crate::loaders::types::LoaderError> {
    let raw = fetch_json::<Vec<FabricLoaderEntry>>(&format!(
        "{FABRIC_META_BASE}/loader/{minecraft_version}"
    ))
    .await?;
    let component_id = LoaderComponentId::Fabric;

    Ok(LoaderVersionIndex {
        component_id,
        builds: raw
            .into_iter()
            .map(|entry| LoaderBuildRecord {
                component_id,
                component_name: component_id.display_name().to_string(),
                build_id: build_id_for(component_id, minecraft_version, &entry.loader.version),
                minecraft_version: minecraft_version.to_string(),
                loader_version: entry.loader.version.clone(),
                version_id: installed_version_id_for(
                    component_id,
                    minecraft_version,
                    &entry.loader.version,
                ),
                stable: entry.loader.stable,
                recommended: false,
                latest: false,
                strategy: LoaderInstallStrategy::FabricProfile,
                artifact_kind: LoaderArtifactKind::ProfileJson,
                installability: LoaderInstallability::Installable,
                install_source: LoaderInstallSource::ProfileJson {
                    url: format!(
                        "{FABRIC_META_BASE}/loader/{minecraft_version}/{}/profile/json",
                        entry.loader.version
                    ),
                },
            })
            .collect(),
    })
}
