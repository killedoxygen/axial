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

const MAX_TWEAKER_CLASS_BYTES: usize = 256;

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
    #[serde(default)]
    launchwrapper: Option<FabricLaunchWrapper>,
}

#[derive(Deserialize, Eq, PartialEq)]
struct FabricLaunchWrapper {
    tweakers: FabricTweakers,
}

#[derive(Deserialize, Eq, PartialEq)]
struct FabricTweakers {
    #[serde(default, deserialize_with = "deserialize_client_tweaker")]
    client: Option<String>,
}

fn deserialize_client_tweaker<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct ClientTweaker;

    impl<'de> serde::de::Visitor<'de> for ClientTweaker {
        type Value = Option<String>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a client tweaker array")
        }

        fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
        where
            A: serde::de::SeqAccess<'de>,
        {
            // The historical installer consumes client[0], not trailing tweakers or other sides.
            let client = sequence.next_element::<String>()?;
            while sequence.next_element::<serde::de::IgnoredAny>()?.is_some() {}
            Ok(client)
        }
    }

    deserializer.deserialize_seq(ClientTweaker)
}

#[derive(Deserialize, Eq, PartialEq)]
#[serde(from = "FabricMainClassSource")]
struct FabricMainClass {
    client: String,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum FabricMainClassSource {
    Environment(FabricEnvironmentMainClasses),
    Universal(String),
}

#[derive(Deserialize)]
struct FabricEnvironmentMainClasses {
    #[serde(default)]
    client: String,
}

impl From<FabricMainClassSource> for FabricMainClass {
    fn from(source: FabricMainClassSource) -> Self {
        let client = match source {
            FabricMainClassSource::Environment(main_classes) => main_classes.client,
            FabricMainClassSource::Universal(main_class) => main_class,
        };
        Self { client }
    }
}

pub async fn fetch_game_versions()
-> Result<Vec<LoaderGameVersion>, crate::loaders::types::LoaderError> {
    let raw = fetch_json::<Vec<FabricGameEntry>>(&format!("{FABRIC_META_BASE}/game")).await?;
    Ok(game_versions_from_entries(raw))
}

#[cfg(feature = "test-support")]
pub(crate) async fn fetch_game_versions_from_loopback_for_test(
    url: &reqwest::Url,
) -> Result<Vec<LoaderGameVersion>, crate::loaders::types::LoaderError> {
    let raw = crate::loaders::http::fetch_json_from_loopback_for_test::<Vec<FabricGameEntry>>(url)
        .await?;
    Ok(game_versions_from_entries(raw))
}

fn game_versions_from_entries(raw: Vec<FabricGameEntry>) -> Vec<LoaderGameVersion> {
    raw.into_iter()
        .map(|entry| LoaderGameVersion {
            subject_kind: VersionSubjectKind::MinecraftVersion,
            id: entry.version,
            release_time: String::new(),
            minecraft_meta: MinecraftVersionMeta::default(),
            lifecycle: LifecycleMeta::default(),
            stable_hint: Some(entry.stable),
        })
        .collect()
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
    let client_main_class = (record.component_id == LoaderComponentId::Fabric
        && entry.loader.version == record.loader_version)
        .then(|| compatible_client_main_class(&entry, &record.minecraft_version))
        .flatten()
        .map(str::to_owned);
    let Some(client_main_class) = client_main_class else {
        return Err(crate::loaders::types::LoaderError::ProviderDataInvalid {
            kind: crate::loaders::types::LoaderProviderFailureKind::SchemaInvalid,
            status: None,
        });
    };
    Ok(ProfileInstallProof {
        provider_url: url.to_string(),
        canonical_profile_id: format!(
            "fabric-loader-{}-{}",
            record.loader_version, record.minecraft_version
        ),
        inherits_from: record.minecraft_version.clone(),
        client_main_class,
        launchwrapper_tweaker: entry
            .launcher_meta
            .launchwrapper
            .and_then(|wrapper| wrapper.tweakers.client),
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

#[cfg(feature = "test-support")]
pub(crate) async fn fetch_builds_from_loopback_for_test(
    minecraft_version: &str,
    url: &reqwest::Url,
) -> Result<LoaderVersionIndex, crate::loaders::types::LoaderError> {
    let raw =
        crate::loaders::http::fetch_json_from_loopback_for_test::<Vec<FabricInstallEntry>>(url)
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
        .filter(|entry| compatible_client_main_class(entry, minecraft_version).is_some())
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

fn compatible_client_main_class<'a>(
    entry: &'a FabricInstallEntry,
    minecraft_version: &str,
) -> Option<&'a str> {
    if entry.loader.maven != format!("net.fabricmc:fabric-loader:{}", entry.loader.version)
        || entry.intermediary.version != minecraft_version
        || entry.intermediary.maven != format!("net.fabricmc:intermediary:{minecraft_version}")
    {
        return None;
    }

    let client_main_class = entry.launcher_meta.main_class.client.trim();
    if let Some(wrapper) = &entry.launcher_meta.launchwrapper {
        if client_main_class != "net.minecraft.launchwrapper.Launch" {
            return None;
        }
        let class = wrapper.tweakers.client.as_deref()?;
        if class.is_empty()
            || class.len() > MAX_TWEAKER_CLASS_BYTES
            || !class.split('.').all(|segment| {
                let mut bytes = segment.bytes();
                bytes
                    .next()
                    .is_some_and(|byte| byte.is_ascii_alphabetic() || matches!(byte, b'_' | b'$'))
                    && bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'$'))
            })
        {
            return None;
        }
    }
    (!client_main_class.is_empty()).then_some(client_main_class)
}

#[cfg(test)]
mod tests {
    use super::{FabricInstallEntry, build_index_from_entries, profile_install_proof_from_entry};
    use crate::loaders::types::{LoaderError, LoaderProviderFailureKind};

    #[test]
    fn build_catalog_omits_placeholder_and_mismatched_intermediary_entries() {
        let raw = serde_json::from_value::<Vec<FabricInstallEntry>>(serde_json::json!([
            {
                "loader": {
                    "version": "0.2.0.71",
                    "stable": true,
                    "maven": "net.fabricmc:fabric-loader:0.2.0.71"
                },
                "intermediary": {
                    "version": "0.0.0",
                    "maven": "net.fabricmc:intermediary:0.0.0"
                },
                "launcherMeta": {
                    "mainClass": "net.minecraft.launchwrapper.Launch"
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
            },
            {
                "loader": {
                    "version": "0.2.0.70",
                    "stable": false,
                    "maven": "net.fabricmc:fabric-loader:0.2.0.70"
                },
                "intermediary": {
                    "version": "26.2",
                    "maven": "net.fabricmc:intermediary:26.2"
                },
                "launcherMeta": {
                    "mainClass": "   "
                }
            }
        ]))
        .expect("Fabric loader catalog fixture");

        let index = build_index_from_entries("26.2", raw).expect("normalized Fabric index");

        assert!(index.builds.is_empty());
    }

    #[test]
    fn build_catalog_and_exact_proof_accept_universal_main_class_shape() {
        let fixture = serde_json::json!({
            "loader": {
                "version": "0.2.0.71",
                "stable": true,
                "maven": "net.fabricmc:fabric-loader:0.2.0.71"
            },
            "intermediary": {
                "version": "1.14",
                "maven": "net.fabricmc:intermediary:1.14"
            },
            "launcherMeta": {
                "mainClass": "  net.minecraft.launchwrapper.Launch  ",
                "launchwrapper": { "tweakers": {
                    "common": ["example.CommonTweaker"],
                    "client": ["net.fabricmc.loader.launch.FabricClientTweaker", "example.ExtraTweaker", null],
                    "server": ["net.fabricmc.loader.launch.FabricServerTweaker"]
                } }
            }
        });
        let catalog_entry = serde_json::from_value::<FabricInstallEntry>(fixture.clone())
            .expect("historical Fabric loader catalog fixture");
        let mut ignored_fields = fixture.clone();
        ignored_fields["launcherMeta"]["launchwrapper"]["tweakers"]["common"] =
            serde_json::json!({"ignored": true});
        ignored_fields["launcherMeta"]["launchwrapper"]["tweakers"]["client"][1] =
            serde_json::json!({"ignored": true});
        ignored_fields["launcherMeta"]["launchwrapper"]["tweakers"]["server"] =
            serde_json::json!(null);
        let duplicate = serde_json::from_value::<FabricInstallEntry>(ignored_fields)
            .expect("unused sides and trailing values do not define the client launch");
        let index = build_index_from_entries("1.14", vec![catalog_entry, duplicate])
            .expect("normalized historical Fabric index");

        assert_eq!(index.builds.len(), 1);
        let proof_entry = serde_json::from_value::<FabricInstallEntry>(fixture)
            .expect("historical Fabric profile fixture");
        let proof = profile_install_proof_from_entry(
            &index.builds[0],
            "https://meta.fabricmc.net/v2/versions/loader/1.14/0.2.0.71/profile/json",
            proof_entry,
        )
        .expect("historical Fabric exact proof");

        assert_eq!(
            proof.client_main_class,
            "net.minecraft.launchwrapper.Launch"
        );
        assert_eq!(
            proof.launchwrapper_tweaker(),
            Some("net.fabricmc.loader.launch.FabricClientTweaker"),
        );
    }

    #[test]
    fn launchwrapper_proof_rejects_foreign_identity_and_invalid_declarations() {
        let entry = || {
            compatible_entry(
                "1.14",
                "0.2.0.71",
                false,
                "net.minecraft.launchwrapper.Launch",
            )
        };
        let record = build_index_from_entries("1.14", vec![entry()])
            .unwrap()
            .builds
            .remove(0);
        for classes in [
            vec![],
            vec![String::new()],
            vec![" example.Tweaker".to_string()],
            vec!["example/Tweaker".to_string()],
            vec!["example..Tweaker".to_string()],
            vec!["x".repeat(257)],
        ] {
            let mut invalid = entry();
            invalid.launcher_meta.launchwrapper = Some(
                serde_json::from_value(serde_json::json!({"tweakers": {"client": classes}}))
                    .unwrap(),
            );
            assert!(matches!(
                profile_install_proof_from_entry(
                    &record,
                    "https://fixtures.invalid/proof",
                    invalid
                ),
                Err(LoaderError::ProviderDataInvalid {
                    kind: LoaderProviderFailureKind::SchemaInvalid,
                    status: None,
                })
            ));
        }
        for case in 0..3 {
            let mut invalid = entry();
            invalid.launcher_meta.launchwrapper = Some(
                serde_json::from_value(serde_json::json!({
                    "tweakers": {"client": ["example.Tweaker"]},
                }))
                .unwrap(),
            );
            let mut record = record.clone();
            match case {
                0 => record.component_id = crate::loaders::types::LoaderComponentId::Quilt,
                1 => invalid.loader.maven = "org.quiltmc:quilt-loader:0.2.0.71".to_string(),
                2 => {
                    invalid.launcher_meta.main_class.client = "example.NotLaunchWrapper".to_string()
                }
                _ => unreachable!(),
            }
            assert!(matches!(
                profile_install_proof_from_entry(
                    &record,
                    "https://fixtures.invalid/proof",
                    invalid
                ),
                Err(LoaderError::ProviderDataInvalid {
                    kind: LoaderProviderFailureKind::SchemaInvalid,
                    status: None,
                })
            ));
        }
        assert!(
            serde_json::from_value::<super::FabricLaunchWrapper>(
                serde_json::json!({"tweakers": {"client": "example.Tweaker"}}),
            )
            .is_err()
        );
        assert!(
            serde_json::from_value::<super::FabricLaunchWrapper>(
                serde_json::json!({"tweakers": {"client": [null, "example.Tweaker"]}}),
            )
            .is_err()
        );
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
