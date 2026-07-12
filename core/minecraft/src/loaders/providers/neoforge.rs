use super::common::{
    NEOFORGE_MAVEN_BASE, NEOFORGE_MAVEN_META, fetch_text, infer_loader_build_metadata,
    is_prerelease_loader_version, neoforge_to_minecraft_version, parse_maven_versions,
    provider_installed_version_id,
};
use crate::lifecycle::LifecycleMeta;
use crate::loaders::api::build_id_for;
use crate::loaders::types::{
    LoaderArtifactKind, LoaderBuildRecord, LoaderBuildSubjectKind, LoaderComponentId,
    LoaderGameVersion, LoaderInstallSource, LoaderInstallStrategy, LoaderInstallability,
    LoaderVersionIndex,
};
use crate::types::VersionSubjectKind;
use crate::version_meta::MinecraftVersionMeta;

pub async fn fetch_game_versions()
-> Result<Vec<LoaderGameVersion>, crate::loaders::types::LoaderError> {
    let xml = fetch_text(NEOFORGE_MAVEN_META).await?;
    Ok(parse_game_versions_from_maven_metadata(&xml))
}

fn parse_game_versions_from_maven_metadata(xml: &str) -> Vec<LoaderGameVersion> {
    let mut versions_by_stability = std::collections::HashMap::<String, bool>::new();
    for entry in parse_maven_versions(xml) {
        let Some(minecraft_version) = neoforge_to_minecraft_version(&entry) else {
            continue;
        };
        let has_stable_build = !is_prerelease_loader_version(&entry);
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
    let xml = fetch_text(NEOFORGE_MAVEN_META).await?;
    let component_id = LoaderComponentId::NeoForge;
    let mut builds = Vec::new();

    for entry in parse_maven_versions(&xml) {
        let Some(resolved_minecraft_version) = neoforge_to_minecraft_version(&entry) else {
            continue;
        };
        if resolved_minecraft_version != minecraft_version {
            continue;
        }
        let prerelease = is_prerelease_loader_version(&entry);
        builds.push(LoaderBuildRecord {
            subject_kind: LoaderBuildSubjectKind::LoaderBuild,
            component_id,
            component_name: component_id.display_name().to_string(),
            build_id: build_id_for(component_id, minecraft_version, &entry),
            minecraft_version: minecraft_version.to_string(),
            loader_version: entry.clone(),
            version_id: provider_installed_version_id(component_id, minecraft_version, &entry)?,
            build_meta: infer_loader_build_metadata(&entry, &[], false, false, Some(!prerelease)),
            strategy: LoaderInstallStrategy::NeoForgeModern,
            artifact_kind: LoaderArtifactKind::InstallerJar,
            installability: LoaderInstallability::Installable,
            install_source: LoaderInstallSource::InstallerJar {
                url: format!(
                    "{NEOFORGE_MAVEN_BASE}/net/neoforged/neoforge/{0}/neoforge-{0}-installer.jar",
                    entry
                ),
            },
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

    #[test]
    fn game_versions_are_unstable_when_only_beta_builds_exist() {
        let versions = parse_game_versions_from_maven_metadata(
            "<metadata><versions>\
             <version>26.2.0.3-beta</version>\
             <version>26.2.0.4-beta</version>\
             <version>26.1.2.10</version>\
             <version>26.1.2.11-beta</version>\
             </versions></metadata>",
        );

        let only_beta = versions
            .iter()
            .find(|version| version.id == "26.2")
            .expect("26.2 row");
        let has_stable = versions
            .iter()
            .find(|version| version.id == "26.1.2")
            .expect("26.1.2 row");

        assert_eq!(only_beta.stable_hint, Some(false));
        assert_eq!(has_stable.stable_hint, Some(true));
    }
}
