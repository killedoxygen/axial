use super::common::{
    QUILT_META_BASE, infer_loader_build_metadata, profile_proof_url, profile_source_url,
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
    hashed: QuiltMappingVersion,
    intermediary: QuiltMappingVersion,
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
        || entry.hashed.version != record.minecraft_version
        || entry.intermediary.version != record.minecraft_version
        || entry.loader.maven != loader_coordinate
        || entry.hashed.maven != hashed_coordinate
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
            "quilt-loader-{}-{}",
            record.loader_version, record.minecraft_version
        ),
        inherits_from: record.minecraft_version.clone(),
        client_main_class: entry.launcher_meta.main_class.client,
        required_libraries: vec![
            profile_library_proof(
                entry.loader.maven,
                entry.loader.hashes.sha1,
                entry.loader.file_size,
            )?,
            profile_library_proof(
                entry.hashed.maven,
                entry.hashed.hashes.sha1,
                entry.hashed.file_size,
            )?,
            profile_library_proof(
                entry.intermediary.maven,
                entry.intermediary.hashes.sha1,
                entry.intermediary.file_size,
            )?,
        ],
    })
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
    use super::{QuiltInstallEntry, profile_library_proof};

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
