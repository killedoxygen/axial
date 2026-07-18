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
use std::collections::{HashMap, hash_map::Entry};

use super::{ProfileInstallProof, ProfileLibraryProof};

#[derive(Deserialize)]
struct FabricGameEntry {
    version: String,
    stable: bool,
}

#[derive(Deserialize, Eq, PartialEq)]
struct FabricLoaderVersion {
    version: String,
    #[serde(default)]
    stable: bool,
    #[serde(default)]
    maven: String,
}

#[derive(Deserialize, Eq, PartialEq)]
struct FabricInstallEntry {
    loader: FabricLoaderVersion,
    intermediary: FabricIntermediaryVersion,
    #[serde(rename = "launcherMeta")]
    launcher_meta: FabricLauncherMeta,
}

#[derive(Deserialize, Eq, PartialEq)]
struct FabricIntermediaryVersion {
    version: String,
    maven: String,
}

#[derive(Deserialize, Eq, PartialEq)]
struct FabricLauncherMeta {
    #[serde(rename = "mainClass")]
    main_class: FabricMainClass,
}

#[derive(Deserialize, Eq, PartialEq)]
struct FabricMainClass {
    #[serde(default)]
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
    if entry.loader.version != record.loader_version
        || !install_entry_matches_target(&entry, &record.minecraft_version)
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
    let raw = fetch_json::<Vec<FabricInstallEntry>>(&format!(
        "{FABRIC_META_BASE}/loader/{minecraft_version}"
    ))
    .await?;
    build_index_from_entries(minecraft_version, raw)
}

fn build_index_from_entries(
    minecraft_version: &str,
    raw: Vec<FabricInstallEntry>,
) -> Result<LoaderVersionIndex, crate::loaders::types::LoaderError> {
    let component_id = LoaderComponentId::Fabric;
    let mut builds: Vec<LoaderBuildRecord> = Vec::new();
    let mut identities = HashMap::new();

    for entry in raw
        .into_iter()
        .filter(|entry| install_entry_matches_target(entry, minecraft_version))
    {
        let version_id =
            provider_installed_version_id(component_id, minecraft_version, &entry.loader.version)?;
        let record = LoaderBuildRecord {
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
                url: profile_source_url(component_id, minecraft_version, &entry.loader.version)?,
            },
        };

        match identities.entry(record.build_id.clone()) {
            Entry::Occupied(existing) => {
                let (first_entry, first_index) = existing.get();
                if first_entry != &entry || builds[*first_index] != record {
                    return Err(crate::loaders::types::LoaderError::ProviderDataInvalid {
                        kind: crate::loaders::types::LoaderProviderFailureKind::SchemaInvalid,
                        status: None,
                    });
                }
            }
            Entry::Vacant(slot) => {
                slot.insert((entry, builds.len()));
                builds.push(record);
            }
        }
    }

    Ok(LoaderVersionIndex {
        component_id,
        builds,
    })
}

fn install_entry_matches_target(entry: &FabricInstallEntry, minecraft_version: &str) -> bool {
    entry.loader.maven == format!("net.fabricmc:fabric-loader:{}", entry.loader.version)
        && entry.intermediary.version == minecraft_version
        && entry.intermediary.maven == format!("net.fabricmc:intermediary:{minecraft_version}")
        && !entry.launcher_meta.main_class.client.trim().is_empty()
}

#[cfg(test)]
mod tests {
    use super::{FabricInstallEntry, build_index_from_entries};
    use crate::loaders::types::{LoaderError, LoaderProviderFailureKind};

    #[test]
    fn build_catalog_omits_placeholder_and_mismatched_intermediary_entries() {
        let raw = serde_json::from_value::<Vec<FabricInstallEntry>>(serde_json::json!([
            {
                "loader": {
                    "version": "0.19.3",
                    "stable": true,
                    "maven": "net.fabricmc:fabric-loader:0.19.3"
                },
                "intermediary": {
                    "version": "0.0.0",
                    "maven": "net.fabricmc:intermediary:0.0.0"
                },
                "launcherMeta": {
                    "mainClass": { "client": "net.fabricmc.loader.impl.launch.knot.KnotClient" }
                }
            },
            {
                "loader": {
                    "version": "0.19.2",
                    "stable": true,
                    "maven": "net.fabricmc:fabric-loader:0.19.2"
                },
                "intermediary": {
                    "version": "26.2",
                    "maven": "net.fabricmc:intermediary:26.1"
                },
                "launcherMeta": {
                    "mainClass": { "client": "net.fabricmc.loader.impl.launch.knot.KnotClient" }
                }
            },
            {
                "loader": {
                    "version": "0.10.0",
                    "stable": false,
                    "maven": "net.fabricmc:fabric-loader:0.10.0"
                },
                "intermediary": {
                    "version": "26.2",
                    "maven": "net.fabricmc:intermediary:26.2"
                },
                "launcherMeta": {
                    "mainClass": {}
                }
            }
        ]))
        .expect("Fabric loader catalog fixture");

        let index = build_index_from_entries("26.2", raw).expect("normalized Fabric index");

        assert!(index.builds.is_empty());
    }

    #[test]
    fn build_catalog_keeps_only_complete_exact_provider_compatibility() {
        let raw = serde_json::from_value::<Vec<FabricInstallEntry>>(serde_json::json!([
            {
                "loader": {
                    "version": "0.19.3",
                    "stable": true,
                    "maven": "net.fabricmc:fabric-loader:0.19.3"
                },
                "intermediary": {
                    "version": "26.1",
                    "maven": "net.fabricmc:intermediary:26.1"
                },
                "launcherMeta": {
                    "mainClass": { "client": "net.fabricmc.loader.impl.launch.knot.KnotClient" }
                }
            },
            {
                "loader": {
                    "version": "0.19.2",
                    "stable": true,
                    "maven": "net.fabricmc:fabric-loader:0.19.2"
                },
                "intermediary": {
                    "version": "26.1",
                    "maven": "net.fabricmc:intermediary:26.1"
                },
                "launcherMeta": {
                    "mainClass": { "client": "   " }
                }
            }
        ]))
        .expect("Fabric loader catalog fixture");

        let index = build_index_from_entries("26.1", raw).expect("normalized Fabric index");

        assert_eq!(index.builds.len(), 1);
        assert_eq!(index.builds[0].minecraft_version, "26.1");
        assert_eq!(index.builds[0].loader_version, "0.19.3");
        assert_eq!(
            index.builds[0].installability,
            crate::loaders::types::LoaderInstallability::Installable
        );
    }

    #[test]
    fn build_catalog_collapses_exact_duplicates_in_first_record_order() {
        let raw = vec![
            compatible_entry("26.1", "0.19.3", true, "example.fabric.Main"),
            compatible_entry("26.1", "0.19.2", true, "example.fabric.Main"),
            compatible_entry("26.1", "0.19.3", true, "example.fabric.Main"),
        ];

        let index = build_index_from_entries("26.1", raw).expect("deduplicated Fabric index");
        let loader_versions = index
            .builds
            .iter()
            .map(|record| record.loader_version.as_str())
            .collect::<Vec<_>>();

        assert_eq!(loader_versions, vec!["0.19.3", "0.19.2"]);
    }

    #[test]
    fn build_catalog_rejects_conflicting_duplicate_identity() {
        let conflicts = [
            vec![
                compatible_entry("26.1", "0.19.3", true, "example.fabric.Main"),
                compatible_entry("26.1", "0.19.3", false, "example.fabric.Main"),
            ],
            vec![
                compatible_entry("26.1", "0.19.3", true, "example.fabric.Main"),
                compatible_entry("26.1", "0.19.3", true, "example.fabric.OtherMain"),
            ],
        ];

        for raw in conflicts {
            let error = build_index_from_entries("26.1", raw)
                .expect_err("conflicting Fabric identity should fail closed");

            assert!(matches!(
                error,
                LoaderError::ProviderDataInvalid {
                    kind: LoaderProviderFailureKind::SchemaInvalid,
                    status: None,
                }
            ));
        }
    }

    fn compatible_entry(
        minecraft_version: &str,
        loader_version: &str,
        stable: bool,
        client_main_class: &str,
    ) -> FabricInstallEntry {
        serde_json::from_value(serde_json::json!({
            "loader": {
                "version": loader_version,
                "stable": stable,
                "maven": format!("net.fabricmc:fabric-loader:{loader_version}")
            },
            "intermediary": {
                "version": minecraft_version,
                "maven": format!("net.fabricmc:intermediary:{minecraft_version}")
            },
            "launcherMeta": {
                "mainClass": { "client": client_main_class }
            }
        }))
        .expect("compatible Fabric entry")
    }
}
