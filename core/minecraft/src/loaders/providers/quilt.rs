use super::common::{
    QUILT_META_BASE, infer_loader_build_metadata, profile_proof_url, profile_source_url,
    provider_installed_version_id,
};
use crate::launch::{VersionJson, library_merge_key};
use crate::lifecycle::LifecycleMeta;
use crate::loaders::api::build_id_for;
use crate::loaders::compose::LoaderProfileFragment;
use crate::loaders::http::fetch_json;
use crate::loaders::types::{
    LoaderArtifactKind, LoaderBuildRecord, LoaderBuildSubjectKind, LoaderComponentId, LoaderError,
    LoaderGameVersion, LoaderInstallSource, LoaderInstallStrategy, LoaderInstallability,
    LoaderVersionIndex,
};
use crate::types::VersionSubjectKind;
use crate::version_meta::MinecraftVersionMeta;
use serde::Deserialize;

use super::{ProfileInstallProof, ProfileLibraryProof};

#[derive(Deserialize)]
struct QuiltGameEntry {
    version: String,
    stable: bool,
}

#[derive(Deserialize)]
struct QuiltLoaderEntry {
    loader: QuiltLoaderVersion,
}

#[derive(Deserialize)]
struct QuiltLoaderVersion {
    version: String,
    #[serde(default)]
    maven: String,
    #[serde(default)]
    hashes: QuiltHashes,
    #[serde(rename = "file_size", default)]
    file_size: i64,
}

#[derive(Deserialize)]
struct QuiltInstallEntry {
    loader: QuiltLoaderVersion,
    hashed: Option<QuiltMappingVersion>,
    intermediary: Option<QuiltMappingVersion>,
    #[serde(rename = "launcherMeta")]
    launcher_meta: QuiltLauncherMeta,
}

#[derive(Deserialize)]
struct QuiltMappingVersion {
    version: String,
    maven: String,
    #[serde(default)]
    hashes: QuiltHashes,
    #[serde(rename = "file_size", default)]
    file_size: i64,
}

#[derive(Default, Deserialize)]
struct QuiltHashes {
    #[serde(default)]
    sha1: String,
}

#[derive(Deserialize)]
struct QuiltLauncherMeta {
    #[serde(rename = "mainClass")]
    main_class: QuiltMainClass,
}

#[derive(Deserialize)]
struct QuiltMainClass {
    client: String,
}

pub async fn fetch_game_versions()
-> Result<Vec<LoaderGameVersion>, crate::loaders::types::LoaderError> {
    let raw = fetch_json::<Vec<QuiltGameEntry>>(&format!("{QUILT_META_BASE}/game")).await?;
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
        LoaderComponentId::Quilt,
        &record.minecraft_version,
        &record.loader_version,
    )?;
    fetch_profile_install_proof_from_url(record, &url).await
}

async fn fetch_profile_install_proof_from_url(
    record: &LoaderBuildRecord,
    url: &str,
) -> Result<ProfileInstallProof, crate::loaders::types::LoaderError> {
    let entry = fetch_json::<QuiltInstallEntry>(url).await?;
    profile_install_proof_from_entry(record, url, entry)
}

#[cfg(test)]
pub(super) async fn fetch_profile_install_proof_from_url_for_test(
    record: &LoaderBuildRecord,
    url: &str,
) -> Result<ProfileInstallProof, crate::loaders::types::LoaderError> {
    use crate::loaders::http::fetch_json_for_test;

    let entry = fetch_json_for_test::<QuiltInstallEntry>(url).await?;
    profile_install_proof_from_entry(record, url, entry)
}

fn profile_install_proof_from_entry(
    record: &LoaderBuildRecord,
    url: &str,
    entry: QuiltInstallEntry,
) -> Result<ProfileInstallProof, crate::loaders::types::LoaderError> {
    let loader_coordinate = format!("org.quiltmc:quilt-loader:{}", record.loader_version);
    let hashed_coordinate = format!("org.quiltmc:hashed:{}", record.minecraft_version);
    let intermediary_coordinate = format!("net.fabricmc:intermediary:{}", record.minecraft_version);
    if entry.loader.version != record.loader_version
        || entry.loader.maven != loader_coordinate
        || entry.launcher_meta.main_class.client.trim().is_empty()
    {
        return Err(crate::loaders::types::LoaderError::ProviderDataInvalid {
            kind: crate::loaders::types::LoaderProviderFailureKind::SchemaInvalid,
            status: None,
        });
    }
    let mut required_libraries = vec![profile_library_proof(
        entry.loader.maven,
        entry.loader.hashes.sha1,
        entry.loader.file_size,
    )?];
    for (mapping, coordinate) in [
        (entry.hashed, hashed_coordinate),
        (entry.intermediary, intermediary_coordinate),
    ] {
        let Some(mapping) = mapping else { continue };
        if mapping.version != record.minecraft_version || mapping.maven != coordinate {
            return Err(crate::loaders::types::LoaderError::ProviderDataInvalid {
                kind: crate::loaders::types::LoaderProviderFailureKind::SchemaInvalid,
                status: None,
            });
        }
        required_libraries.push(profile_library_proof(
            mapping.maven,
            mapping.hashes.sha1,
            mapping.file_size,
        )?);
    }
    Ok(ProfileInstallProof {
        provider_url: url.to_string(),
        canonical_profile_id: format!(
            "quilt-loader-{}-{}",
            record.loader_version, record.minecraft_version
        ),
        inherits_from: record.minecraft_version.clone(),
        client_main_class: entry.launcher_meta.main_class.client,
        required_libraries,
    })
}

pub(crate) fn validate_profile_mappings(
    fragment: &LoaderProfileFragment,
    record: &LoaderBuildRecord,
    proof: &ProfileInstallProof,
    authenticated_base: &VersionJson,
) -> Result<(), LoaderError> {
    if authenticated_base.id != record.minecraft_version {
        return Err(LoaderError::InvalidProfile(
            "Quilt profile base does not match authenticated Minecraft identity".to_string(),
        ));
    }
    let mut mappings_missing = false;
    for coordinate in [
        format!("org.quiltmc:hashed:{}", record.minecraft_version),
        format!("net.fabricmc:intermediary:{}", record.minecraft_version),
    ] {
        let key = library_merge_key(&coordinate);
        let required = proof
            .required_libraries()
            .iter()
            .find(|library| library_merge_key(library.coordinate()) == key)
            .map(ProfileLibraryProof::coordinate);
        let mut declared = fragment
            .libraries
            .iter()
            .filter(|library| library_merge_key(&library.name) == key);
        if required.is_some_and(|required| required != coordinate)
            || declared.next().map(|library| library.name.as_str()) != required
            || declared.next().is_some()
        {
            return Err(LoaderError::InvalidProfile(
                "Quilt profile mappings do not match its live provider proof".to_string(),
            ));
        }
        mappings_missing |= required.is_none();
    }
    if mappings_missing {
        let release_time = chrono::DateTime::parse_from_rfc3339(&authenticated_base.release_time)
            .map_err(|_| {
            LoaderError::InvalidProfile(
                "Quilt mapping omission requires an authenticated base release time".to_string(),
            )
        })?;
        // Match Quilt Meta's MinecraftMeta.isObfuscated release-time boundary.
        let unobfuscated_since = chrono::DateTime::parse_from_rfc3339("2025-12-16T00:00:00Z")
            .expect("valid Quilt unobfuscated release boundary");
        if release_time < unobfuscated_since {
            return Err(LoaderError::InvalidProfile(
                "Quilt profile requires both mappings for an obfuscated Minecraft base".to_string(),
            ));
        }
    }
    Ok(())
}

fn profile_library_proof(
    coordinate: String,
    sha1: String,
    file_size: i64,
) -> Result<ProfileLibraryProof, crate::loaders::types::LoaderError> {
    let sha1 = (!sha1.is_empty()).then_some(sha1);
    let size = u64::try_from(file_size).ok().filter(|size| *size > 0);
    if sha1
        .as_deref()
        .is_some_and(|sha1| sha1.len() != 40 || !sha1.bytes().all(|byte| byte.is_ascii_hexdigit()))
        || sha1.is_some() != size.is_some()
    {
        return Err(crate::loaders::types::LoaderError::ProviderDataInvalid {
            kind: crate::loaders::types::LoaderProviderFailureKind::SchemaInvalid,
            status: None,
        });
    }
    Ok(ProfileLibraryProof {
        coordinate,
        sha1: sha1.map(|sha1| sha1.to_ascii_lowercase()),
        size,
    })
}

pub async fn fetch_builds(
    minecraft_version: &str,
) -> Result<LoaderVersionIndex, crate::loaders::types::LoaderError> {
    let raw = fetch_json::<Vec<QuiltLoaderEntry>>(&format!(
        "{QUILT_META_BASE}/loader/{minecraft_version}"
    ))
    .await?;
    let component_id = LoaderComponentId::Quilt;

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
                        None,
                    ),
                    strategy: LoaderInstallStrategy::QuiltProfile,
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

#[cfg(test)]
mod tests {
    use super::{
        QuiltInstallEntry, profile_install_proof_from_entry, profile_library_proof,
        validate_profile_mappings,
    };
    use crate::launch::VersionJson;
    use crate::loaders::compose::LoaderProfileFragment;
    use crate::loaders::providers::ProfileInstallProof;
    use crate::loaders::types::{
        LoaderArtifactKind, LoaderBuildRecord, LoaderBuildSubjectKind, LoaderComponentId,
        LoaderInstallSource, LoaderInstallStrategy, LoaderInstallability,
    };
    use crate::loaders::{build_id_for, installed_version_id_for};

    #[test]
    fn official_unobfuscated_install_metadata_decodes_without_mapping_entries() {
        // https://meta.quiltmc.org/v3/versions/loader/26.3/0.31.0-beta.4, 2026-10-02.
        let source = serde_json::from_str::<serde_json::Value>(
            r#"{
                "loader": {
                    "maven": "org.quiltmc:quilt-loader:0.31.0-beta.4",
                    "version": "0.31.0-beta.4",
                    "build": 4,
                    "separator": ".",
                    "file_size": 3220161,
                    "hashes": {
                        "sha1": "5b164d50fd9dc0c05828e49ef0ca782a50659f33",
                        "sha256": "74acd473e2b8ff9921f0ff180400410453e89340e8d403445bc3cb51403052fa",
                        "sha512": "71a58498318b865ea0f49bb7d73c408816bf98fd91a2a7d8f0fe1da5671ec86af332952944a4e1dfb5501ec0ddebc5f599b128695f7acd821e3be859ce8cdcf7"
                    }
                },
                "launcherMeta": {
                    "version": 2,
                    "libraries": {
                        "client": [],
                        "common": [
                            {"name":"net.fabricmc:sponge-mixin:0.17.4+mixin.0.8.7","url":"https://maven.fabricmc.net/"},
                            {"name":"org.quiltmc:quilt-json5:1.0.4+final","url":"https://maven.quiltmc.org/repository/release/"},
                            {"name":"org.ow2.asm:asm:9.10.1","url":"https://maven.fabricmc.net/"},
                            {"name":"org.ow2.asm:asm-analysis:9.10.1","url":"https://maven.fabricmc.net/"},
                            {"name":"org.ow2.asm:asm-commons:9.10.1","url":"https://maven.fabricmc.net/"},
                            {"name":"org.ow2.asm:asm-tree:9.10.1","url":"https://maven.fabricmc.net/"},
                            {"name":"org.ow2.asm:asm-util:9.10.1","url":"https://maven.fabricmc.net/"},
                            {"name":"org.quiltmc:quilt-config:1.3.3","url":"https://maven.quiltmc.org/repository/release/"}
                        ],
                        "server": [],
                        "development": [
                            {"name":"io.github.llamalad7:mixinextras-fabric:0.5.5","url":"https://maven.fabricmc.net/"}
                        ]
                    },
                    "mainClass": {
                        "client": "org.quiltmc.loader.impl.launch.knot.KnotClient",
                        "server": "org.quiltmc.loader.impl.launch.knot.KnotServer",
                        "serverLauncher": "org.quiltmc.loader.impl.launch.server.QuiltServerLauncher"
                    },
                    "min_java_version": 8
                }
            }"#,
        )
        .expect("captured official metadata");
        assert!(source.get("hashed").is_none());
        assert!(source.get("intermediary").is_none());
        let entry: QuiltInstallEntry = serde_json::from_value(source)
            .expect("official unobfuscated Quilt metadata may omit both mapping entries");
        assert_eq!(entry.loader.version, "0.31.0-beta.4");
        assert_eq!(entry.loader.maven, "org.quiltmc:quilt-loader:0.31.0-beta.4");
        assert_eq!(
            entry.loader.hashes.sha1,
            "5b164d50fd9dc0c05828e49ef0ca782a50659f33"
        );
        assert_eq!(entry.loader.file_size, 3_220_161);
        assert_eq!(
            entry.launcher_meta.main_class.client,
            "org.quiltmc.loader.impl.launch.knot.KnotClient"
        );
        assert!(entry.hashed.is_none());
        assert!(entry.intermediary.is_none());
        let proof =
            profile_install_proof_from_entry(&record(), "https://meta.quiltmc.org/fixture", entry)
                .expect("official omitted mapping proof");
        assert_eq!(proof.required_libraries().len(), 1);
        assert_eq!(
            proof.required_libraries()[0].exact_integrity(),
            Some(("5b164d50fd9dc0c05828e49ef0ca782a50659f33", 3220161))
        );
    }

    #[test]
    fn nullable_mappings_preserve_present_identity_and_integrity() {
        for (hashed, intermediary) in [(false, false), (true, false), (false, true), (true, true)] {
            for explicit_null in [false, true] {
                let mut source = metadata();
                let mut fragment = fragment();
                for (field, present, coordinate) in [
                    ("hashed", hashed, "org.quiltmc:hashed:26.3"),
                    (
                        "intermediary",
                        intermediary,
                        "net.fabricmc:intermediary:26.3",
                    ),
                ] {
                    if !present {
                        if explicit_null {
                            source[field] = serde_json::Value::Null;
                        } else {
                            source
                                .as_object_mut()
                                .expect("metadata object")
                                .remove(field);
                        }
                        fragment
                            .libraries
                            .retain(|library| library.name != coordinate);
                    }
                }
                let proof = proof(source);
                assert_eq!(
                    proof.required_libraries().len(),
                    1 + usize::from(hashed) + usize::from(intermediary)
                );
                if hashed {
                    assert_eq!(
                        proof.required_libraries()[1].exact_integrity(),
                        Some(("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb", 43))
                    );
                }
                if intermediary {
                    assert_eq!(
                        proof
                            .required_libraries()
                            .last()
                            .expect("intermediary")
                            .exact_integrity(),
                        None
                    );
                }
                validate_profile_mappings(
                    &fragment,
                    &record(),
                    &proof,
                    &base("2025-12-16T00:00:00Z"),
                )
                .expect("nullable mapping agreement for unobfuscated base");
                assert_eq!(
                    validate_profile_mappings(
                        &fragment,
                        &record(),
                        &proof,
                        &base("2025-12-15T23:59:59Z")
                    )
                    .is_ok(),
                    hashed && intermediary,
                    "obfuscated base still requires both mappings"
                );
            }
        }
    }

    #[test]
    fn present_install_entries_reject_identity_drift_and_partial_integrity() {
        for field in ["loader", "hashed", "intermediary"] {
            for (member, value) in [
                ("version", serde_json::json!("wrong")),
                ("maven", serde_json::json!("wrong:coordinate:1")),
                ("hashes", serde_json::json!({"sha1": "invalid"})),
                ("file_size", serde_json::json!(0)),
                ("file_size", serde_json::json!(-1)),
                ("hashes", serde_json::json!({})),
            ] {
                let mut source = metadata();
                source[field]["file_size"] = serde_json::json!(43);
                source[field]["hashes"] =
                    serde_json::json!({"sha1": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"});
                source[field][member] = value;
                let entry = serde_json::from_value(source).expect("decodable malformed entry");
                assert!(
                    profile_install_proof_from_entry(
                        &record(),
                        "https://meta.quiltmc.org/fixture",
                        entry
                    )
                    .is_err(),
                    "{field}.{member} must not lose identity or integrity validation"
                );
            }
        }
        for field in ["hashed", "intermediary"] {
            let mut source = metadata();
            source[field] = serde_json::json!({"version": "26.3"});
            assert!(serde_json::from_value::<QuiltInstallEntry>(source).is_err());
        }
    }

    #[test]
    fn mapping_omission_uses_authenticated_base_identity_and_utc_release_time() {
        let mut source = metadata();
        source.as_object_mut().expect("metadata").remove("hashed");
        source
            .as_object_mut()
            .expect("metadata")
            .remove("intermediary");
        let proof = proof(source);
        let mut fragment = fragment();
        fragment.libraries.truncate(1);
        fragment.release_time = "2099-01-01T00:00:00Z".to_string();
        for release_time in [
            "2025-12-16T00:00:00Z",
            "2025-12-16T01:00:00+01:00",
            "2025-12-15T19:00:00-05:00",
            "2026-01-01T00:00:00Z",
        ] {
            validate_profile_mappings(&fragment, &record(), &proof, &base(release_time))
                .expect("at or after UTC boundary");
        }
        for release_time in [
            "2025-12-15T23:59:59.999999999Z",
            "2025-12-16T00:59:59+01:00",
            "2025-12-16",
            "not-a-time",
            "",
        ] {
            assert!(
                validate_profile_mappings(&fragment, &record(), &proof, &base(release_time))
                    .is_err(),
                "profile time cannot admit missing mappings for base time {release_time:?}"
            );
        }
        let mut wrong_base = base("2026-01-01T00:00:00Z");
        wrong_base.id = "other-minecraft".to_string();
        assert!(validate_profile_mappings(&fragment, &record(), &proof, &wrong_base).is_err());
        validate_profile_mappings(
            &self::fragment(),
            &record(),
            &self::proof(metadata()),
            &base(""),
        )
        .expect("complete historical mapping metadata does not need a release-time fallback");
    }

    #[test]
    fn mapping_declarations_must_match_provider_proof_without_downgrade() {
        let complete = proof(metadata());
        for field in ["hashed", "intermediary"] {
            let mut source = metadata();
            source.as_object_mut().expect("metadata").remove(field);
            assert!(
                validate_profile_mappings(
                    &fragment(),
                    &record(),
                    &proof(source),
                    &base("2026-01-01T00:00:00Z")
                )
                .is_err()
            );
        }
        for coordinate in ["org.quiltmc:hashed:26.3", "net.fabricmc:intermediary:26.3"] {
            let mut missing = fragment();
            missing
                .libraries
                .retain(|library| library.name != coordinate);
            let mut wrong_version = fragment();
            wrong_version
                .libraries
                .iter_mut()
                .find(|library| library.name == coordinate)
                .expect("mapping")
                .name
                .push_str("-wrong");
            let mut duplicate = fragment();
            duplicate.libraries.push(
                duplicate
                    .libraries
                    .iter()
                    .find(|library| library.name == coordinate)
                    .expect("mapping")
                    .clone(),
            );
            for fragment in [missing, wrong_version, duplicate] {
                assert!(
                    validate_profile_mappings(
                        &fragment,
                        &record(),
                        &complete,
                        &base("2026-01-01T00:00:00Z")
                    )
                    .is_err()
                );
            }
        }
    }

    fn record() -> LoaderBuildRecord {
        let component = LoaderComponentId::Quilt;
        LoaderBuildRecord {
            subject_kind: LoaderBuildSubjectKind::LoaderBuild,
            component_id: component,
            component_name: "Quilt".to_string(),
            build_id: build_id_for(component, "26.3", "0.31.0-beta.4"),
            minecraft_version: "26.3".to_string(),
            loader_version: "0.31.0-beta.4".to_string(),
            version_id: installed_version_id_for(component, "26.3", "0.31.0-beta.4")
                .expect("version id"),
            build_meta: Default::default(),
            strategy: LoaderInstallStrategy::QuiltProfile,
            artifact_kind: LoaderArtifactKind::ProfileJson,
            installability: LoaderInstallability::Installable,
            install_source: LoaderInstallSource::ProfileJson {
                url: "https://meta.quiltmc.org/fixture".to_string(),
            },
        }
    }

    fn metadata() -> serde_json::Value {
        serde_json::json!({
            "loader":{"version":"0.31.0-beta.4","maven":"org.quiltmc:quilt-loader:0.31.0-beta.4","file_size":42,"hashes":{"sha1":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"}},
            "hashed":{"version":"26.3","maven":"org.quiltmc:hashed:26.3","file_size":43,"hashes":{"sha1":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}},
            "intermediary":{"version":"26.3","maven":"net.fabricmc:intermediary:26.3"},
            "launcherMeta":{"mainClass":{"client":"org.quiltmc.loader.impl.launch.knot.KnotClient"}}
        })
    }

    fn proof(source: serde_json::Value) -> ProfileInstallProof {
        profile_install_proof_from_entry(
            &record(),
            "https://meta.quiltmc.org/fixture",
            serde_json::from_value(source).expect("metadata"),
        )
        .expect("valid proof")
    }

    fn fragment() -> LoaderProfileFragment {
        serde_json::from_value(serde_json::json!({
            "id": "quilt-loader-0.31.0-beta.4-26.3",
            "inheritsFrom": "26.3",
            "mainClass": "org.quiltmc.loader.impl.launch.knot.KnotClient",
            "libraries": [
                {"name": "org.quiltmc:quilt-loader:0.31.0-beta.4"},
                {"name": "org.quiltmc:hashed:26.3"},
                {"name": "net.fabricmc:intermediary:26.3"}
            ]
        }))
        .expect("profile fragment")
    }

    fn base(release_time: &str) -> VersionJson {
        serde_json::from_value(serde_json::json!({"id": "26.3", "releaseTime": release_time}))
            .expect("base version")
    }

    #[test]
    fn install_metadata_reads_nested_hashes_as_exact_integrity() {
        let entry: QuiltInstallEntry = serde_json::from_str(
            r#"{
                "loader":{"version":"0.29.2","maven":"org.quiltmc:quilt-loader:0.29.2","file_size":42,"hashes":{"sha1":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"}},
                "hashed":{"version":"1.21.5","maven":"org.quiltmc:hashed:1.21.5","file_size":43,"hashes":{"sha1":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}},
                "intermediary":{"version":"1.21.5","maven":"net.fabricmc:intermediary:1.21.5"},
                "launcherMeta":{"mainClass":{"client":"org.quiltmc.loader.impl.launch.knot.KnotClient"}}
            }"#,
        )
        .expect("Quilt metadata");

        let proof = profile_library_proof(
            entry.loader.maven,
            entry.loader.hashes.sha1,
            entry.loader.file_size,
        )
        .expect("loader integrity");
        assert_eq!(
            proof.sha1.as_deref(),
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        );
        assert_eq!(proof.size, Some(42));
    }

    #[test]
    fn profile_integrity_accepts_only_absent_or_complete_positive_pairs() {
        let absent = profile_library_proof("example:absent:1".to_string(), String::new(), 0)
            .expect("absent integrity");
        assert_eq!(absent.exact_integrity(), None);
        assert!(!absent.has_partial_integrity());

        for (sha1, size) in [
            ("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", 0),
            ("", 7),
            ("not-a-sha1", 7),
            ("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", -1),
        ] {
            assert!(
                profile_library_proof("example:invalid:1".to_string(), sha1.to_string(), size)
                    .is_err()
            );
        }
    }
}
