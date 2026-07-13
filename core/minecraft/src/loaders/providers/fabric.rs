use super::common::{
    FABRIC_META_BASE, infer_loader_build_metadata, profile_proof_url, profile_source_url,
    provider_installed_version_id,
};
use crate::lifecycle::LifecycleMeta;
use crate::loaders::api::build_id_for;
use crate::loaders::http::fetch_json;
use crate::loaders::types::{
    LoaderArtifactKind, LoaderBuildRecord, LoaderBuildSubjectKind, LoaderComponentId,
    LoaderGameVersion, LoaderInstallSource, LoaderInstallStrategy, LoaderInstallability,
    LoaderVersionIndex,
};
use crate::types::VersionSubjectKind;
use crate::version_meta::MinecraftVersionMeta;
use serde::Deserialize;

use super::{ProfileInstallProof, ProfileLibraryProof};

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
    #[serde(default)]
    stable: bool,
    #[serde(default)]
    maven: String,
}

#[derive(Deserialize)]
struct FabricInstallEntry {
    loader: FabricLoaderVersion,
    intermediary: FabricIntermediaryVersion,
    #[serde(rename = "launcherMeta")]
    launcher_meta: FabricLauncherMeta,
}

#[derive(Deserialize)]
struct FabricIntermediaryVersion {
    version: String,
    maven: String,
}

#[derive(Deserialize)]
struct FabricLauncherMeta {
    #[serde(rename = "mainClass")]
    main_class: FabricMainClass,
}

#[derive(Deserialize)]
struct FabricMainClass {
    client: String,
}

pub async fn fetch_game_versions()
-> Result<Vec<LoaderGameVersion>, crate::loaders::types::LoaderError> {
    let raw = fetch_json::<Vec<FabricGameEntry>>(&format!("{FABRIC_META_BASE}/game")).await?;
    Ok(raw
        .into_iter()
        .map(|entry| LoaderGameVersion {
            subject_kind: VersionSubjectKind::MinecraftVersion,
            id: entry.version,
            release_time: String::new(),
            minecraft_meta: MinecraftVersionMeta::default(),
            lifecycle: LifecycleMeta::default(),
            stable_hint: Some(entry.stable),
        })
        .collect())
}

pub(crate) async fn fetch_profile_install_proof(
    record: &LoaderBuildRecord,
) -> Result<ProfileInstallProof, crate::loaders::types::LoaderError> {
    let url = profile_proof_url(
        LoaderComponentId::Fabric,
        &record.minecraft_version,
        &record.loader_version,
    )?;
    fetch_profile_install_proof_from_url(record, &url).await
}

async fn fetch_profile_install_proof_from_url(
    record: &LoaderBuildRecord,
    url: &str,
) -> Result<ProfileInstallProof, crate::loaders::types::LoaderError> {
    let entry = fetch_json::<FabricInstallEntry>(url).await?;
    profile_install_proof_from_entry(record, url, entry)
}

#[cfg(test)]
pub(super) async fn fetch_profile_install_proof_from_url_for_test(
    record: &LoaderBuildRecord,
    url: &str,
) -> Result<ProfileInstallProof, crate::loaders::types::LoaderError> {
    use crate::loaders::http::fetch_json_for_test;

    let entry = fetch_json_for_test::<FabricInstallEntry>(url).await?;
    profile_install_proof_from_entry(record, url, entry)
}

fn profile_install_proof_from_entry(
    record: &LoaderBuildRecord,
    url: &str,
    entry: FabricInstallEntry,
) -> Result<ProfileInstallProof, crate::loaders::types::LoaderError> {
    let loader_coordinate = format!("net.fabricmc:fabric-loader:{}", record.loader_version);
    let intermediary_coordinate = format!("net.fabricmc:intermediary:{}", record.minecraft_version);
    if entry.loader.version != record.loader_version
        || entry.intermediary.version != record.minecraft_version
        || entry.loader.maven != loader_coordinate
        || entry.intermediary.maven != intermediary_coordinate
        || entry.launcher_meta.main_class.client.trim().is_empty()
    {
        return Err(crate::loaders::types::LoaderError::ProviderDataInvalid {
            kind: crate::loaders::types::LoaderProviderFailureKind::SchemaInvalid,
            status: None,
        });
    }
    Ok(ProfileInstallProof {
        provider_url: url.to_string(),
        canonical_profile_id: format!(
            "fabric-loader-{}-{}",
            record.loader_version, record.minecraft_version
        ),
        inherits_from: record.minecraft_version.clone(),
        client_main_class: entry.launcher_meta.main_class.client,
        required_libraries: vec![
            ProfileLibraryProof {
                coordinate: entry.loader.maven,
                sha1: None,
                size: None,
            },
            ProfileLibraryProof {
                coordinate: entry.intermediary.maven,
                sha1: None,
                size: None,
            },
        ],
    })
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
            .map(|entry| {
                let version_id = provider_installed_version_id(
                    component_id,
                    minecraft_version,
                    &entry.loader.version,
                )?;
                Ok(LoaderBuildRecord {
                    subject_kind: LoaderBuildSubjectKind::LoaderBuild,
                    component_id,
                    component_name: component_id.display_name().to_string(),
                    build_id: build_id_for(component_id, minecraft_version, &entry.loader.version),
                    minecraft_version: minecraft_version.to_string(),
                    loader_version: entry.loader.version.clone(),
                    version_id,
                    build_meta: infer_loader_build_metadata(
                        &entry.loader.version,
                        &[],
                        false,
                        false,
                        Some(entry.loader.stable),
                    ),
                    strategy: LoaderInstallStrategy::FabricProfile,
                    artifact_kind: LoaderArtifactKind::ProfileJson,
                    installability: LoaderInstallability::Installable,
                    install_source: LoaderInstallSource::ProfileJson {
                        url: profile_source_url(
                            component_id,
                            minecraft_version,
                            &entry.loader.version,
                        )?,
                    },
                })
            })
            .collect::<Result<Vec<_>, crate::loaders::types::LoaderError>>()?,
    })
}
