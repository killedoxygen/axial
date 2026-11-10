use super::common::{
    FORGE_MAVEN_META, FORGE_PROMOTIONS_URL, apply_forge_promotion_selection,
    extract_forge_loader_version, extract_forge_minecraft_version, fetch_text,
    forge_install_source, infer_loader_build_metadata, is_prerelease_loader_version,
    parse_maven_versions, provider_installed_version_id,
};
use crate::loaders::api::build_id_for;
use crate::loaders::http::fetch_json;
use crate::loaders::types::{
    LoaderBuildRecord, LoaderBuildSubjectKind, LoaderComponentId, LoaderGameVersion,
    LoaderInstallability, LoaderVersionIndex,
};
use crate::types::VersionSubjectKind;
use crate::{LifecycleMeta, version_meta::MinecraftVersionMeta};
use serde::Deserialize;
use std::collections::HashMap;

#[derive(Deserialize, Default)]
struct Promotions {
    #[serde(default)]
    promos: HashMap<String, String>,
}

pub async fn fetch_game_versions()
-> Result<Vec<LoaderGameVersion>, crate::loaders::types::LoaderError> {
    let xml = fetch_text(FORGE_MAVEN_META).await?;
    Ok(parse_game_versions_from_maven_metadata(&xml))
}

fn parse_game_versions_from_maven_metadata(xml: &str) -> Vec<LoaderGameVersion> {
    let mut versions_by_stability = std::collections::HashMap::<String, bool>::new();
    for entry in parse_maven_versions(xml) {
        let minecraft_version = extract_forge_minecraft_version(&entry);
        let loader_version = extract_forge_loader_version(&entry);
        if minecraft_version.is_empty() || loader_version.is_empty() {
            continue;
        }
        let has_stable_build = !is_prerelease_loader_version(&loader_version);
        versions_by_stability
            .entry(minecraft_version)
            .and_modify(|stable| *stable |= has_stable_build)
            .or_insert(has_stable_build);
    }

    let mut versions = Vec::new();
    for (minecraft_version, has_stable_build) in versions_by_stability {
        versions.push(LoaderGameVersion {
            subject_kind: VersionSubjectKind::MinecraftVersion,
            id: minecraft_version,
            release_time: String::new(),
            minecraft_meta: MinecraftVersionMeta::default(),
            lifecycle: LifecycleMeta::default(),
            stable_hint: Some(has_stable_build),
        });
    }
    versions
}

pub async fn fetch_builds(
    minecraft_version: &str,
) -> Result<LoaderVersionIndex, crate::loaders::types::LoaderError> {
    let maven_metadata = fetch_text(FORGE_MAVEN_META);
    let promotions = fetch_json::<Promotions>(FORGE_PROMOTIONS_URL);
    let (maven_metadata, promotions) = tokio::join!(maven_metadata, promotions);

    let xml = maven_metadata?;
    let promotions = promotions.ok().unwrap_or_default();
    let recommended = promotions
        .promos
        .get(&format!("{minecraft_version}-recommended"))
        .cloned();
    let latest = promotions
        .promos
        .get(&format!("{minecraft_version}-latest"))
        .cloned();
    let component_id = LoaderComponentId::Forge;
    let mut builds = Vec::new();

    for entry in parse_maven_versions(&xml) {
        if extract_forge_minecraft_version(&entry) != minecraft_version {
            continue;
        }
        let loader_version = extract_forge_loader_version(&entry);
        if loader_version.is_empty() {
            continue;
        }
        let is_recommended = recommended
            .as_ref()
            .is_some_and(|value| value == &loader_version);
        let is_latest = latest
            .as_ref()
            .is_some_and(|value| value == &loader_version);
        let (strategy, artifact_kind, install_source) =
            forge_install_source(minecraft_version, &loader_version)?;
        let mut build_meta =
            infer_loader_build_metadata(&loader_version, &[], is_recommended, is_latest, None);
        apply_forge_promotion_selection(
            &mut build_meta,
            recommended.is_some(),
            is_recommended,
            is_latest,
        );

        builds.push(LoaderBuildRecord {
            subject_kind: LoaderBuildSubjectKind::LoaderBuild,
            component_id,
            component_name: component_id.display_name().to_string(),
            build_id: build_id_for(component_id, minecraft_version, &loader_version),
            minecraft_version: minecraft_version.to_string(),
            loader_version: loader_version.clone(),
            version_id: provider_installed_version_id(
                component_id,
                minecraft_version,
                &loader_version,
            )?,
            build_meta,
            strategy,
            artifact_kind,
            installability: LoaderInstallability::Installable,
            install_source,
        });
    }

    builds.reverse();
    Ok(LoaderVersionIndex {
        component_id,
        builds,
    })
}

#[cfg(test)]
mod tests {
    use super::parse_game_versions_from_maven_metadata;
    use crate::loaders::providers::common::forge_install_source;
    use crate::loaders::types::{LoaderArtifactKind, LoaderInstallSource, LoaderInstallStrategy};

    #[test]
    fn game_versions_are_unstable_when_only_prerelease_forge_builds_exist() {
        let versions = parse_game_versions_from_maven_metadata(
            "<metadata><versions>\
             <version>1.7.10_pre4-10.12.2.1149-prerelease</version>\
             <version>1.20.1-47.4.0-beta</version>\
             <version>1.20.1-47.4.0</version>\
             <version>26.2-rc-1-65.0.0</version>\
             </versions></metadata>",
        );

        let prerelease_only = versions
            .iter()
            .find(|version| version.id == "1.7.10_pre4")
            .expect("1.7.10_pre4 row");
        let mixed = versions
            .iter()
            .find(|version| version.id == "1.20.1")
            .expect("1.20.1 row");
        let snapshot_minecraft_with_stable_loader = versions
            .iter()
            .find(|version| version.id == "26.2-rc-1")
            .expect("26.2-rc-1 row");

        assert_eq!(prerelease_only.stable_hint, Some(false));
        assert_eq!(mixed.stable_hint, Some(true));
        assert_eq!(
            snapshot_minecraft_with_stable_loader.stable_hint,
            Some(true)
        );
    }

    #[test]
    fn classifies_forge_install_sources_by_era() {
        use LoaderArtifactKind::{InstallerJar, LegacyArchive};
        use LoaderInstallStrategy::{ForgeEarliestLegacy, ForgeLegacyInstaller, ForgeModern};
        for (minecraft, forge, expected_strategy, expected_kind, suffix) in [
            (
                "1.1",
                "1.3.4.29",
                ForgeEarliestLegacy,
                LegacyArchive,
                "client.zip",
            ),
            (
                "1.2.4",
                "2.0.0.68",
                ForgeEarliestLegacy,
                LegacyArchive,
                "client.zip",
            ),
            (
                "1.3.2",
                "4.3.5.318",
                ForgeEarliestLegacy,
                LegacyArchive,
                "universal.zip",
            ),
            (
                "1.4.7",
                "6.6.2.534",
                ForgeEarliestLegacy,
                LegacyArchive,
                "universal.zip",
            ),
            (
                "1.5.2",
                "7.8.1.738",
                ForgeLegacyInstaller,
                InstallerJar,
                "installer.jar",
            ),
            (
                "1.6.4",
                "9.11.1.1345",
                ForgeLegacyInstaller,
                InstallerJar,
                "installer.jar",
            ),
            (
                "1.7.10_pre4",
                "10.12.2.1149-prerelease",
                ForgeLegacyInstaller,
                InstallerJar,
                "installer.jar",
            ),
            (
                "1.12.2",
                "14.23.5.2859",
                ForgeLegacyInstaller,
                InstallerJar,
                "installer.jar",
            ),
            ("1.13", "25.0.0", ForgeModern, InstallerJar, "installer.jar"),
            (
                "1.21.11",
                "61.1.5",
                ForgeModern,
                InstallerJar,
                "installer.jar",
            ),
            (
                "26.2-rc-1",
                "65.0.0",
                ForgeModern,
                InstallerJar,
                "installer.jar",
            ),
        ] {
            let (strategy, artifact_kind, install_source) =
                forge_install_source(minecraft, forge).expect("source");
            assert_eq!(strategy, expected_strategy, "{minecraft}");
            assert_eq!(artifact_kind, expected_kind, "{minecraft}");
            let url = match (expected_kind, install_source) {
                (LegacyArchive, LoaderInstallSource::LegacyArchive { url })
                | (InstallerJar, LoaderInstallSource::InstallerJar { url }) => url,
                (_, other) => panic!("{minecraft}: unexpected install source: {other:?}"),
            };
            assert_eq!(
                url,
                format!(
                    "https://maven.minecraftforge.net/net/minecraftforge/forge/{minecraft}-{forge}/forge-{minecraft}-{forge}-{suffix}"
                )
            );
        }
    }
}
