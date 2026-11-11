pub use super::common::neoforge_to_minecraft_version;
use super::common::{
    NEOFORGE_MAVEN_META, fetch_text, infer_loader_build_metadata, is_prerelease_loader_version,
    maven_build_record, maven_game_versions, neoforge_install_source, parse_maven_versions,
};
use crate::loaders::types::{LoaderComponentId, LoaderGameVersion, LoaderVersionIndex};

pub async fn fetch_game_versions()
-> Result<Vec<LoaderGameVersion>, crate::loaders::types::LoaderError> {
    let xml = fetch_text(NEOFORGE_MAVEN_META).await?;
    Ok(parse_game_versions_from_maven_metadata(&xml))
}

pub fn parse_game_versions_from_maven_metadata(xml: &str) -> Vec<LoaderGameVersion> {
    maven_game_versions(xml, |entry| {
        neoforge_to_minecraft_version(entry).map(|minecraft| (minecraft, entry.to_string()))
    })
}

pub async fn fetch_builds(
    minecraft_version: &str,
) -> Result<LoaderVersionIndex, crate::loaders::types::LoaderError> {
    let xml = fetch_text(NEOFORGE_MAVEN_META).await?;
    parse_builds_from_maven_metadata(minecraft_version, &xml)
}

/// Normalize provider bytes without performing network or filesystem work.
pub fn parse_builds_from_maven_metadata(
    minecraft_version: &str,
    xml: &str,
) -> Result<LoaderVersionIndex, crate::loaders::types::LoaderError> {
    let component_id = LoaderComponentId::NeoForge;
    let mut builds = Vec::new();

    for entry in parse_maven_versions(xml) {
        let Some(resolved_minecraft_version) = neoforge_to_minecraft_version(&entry) else {
            continue;
        };
        if resolved_minecraft_version != minecraft_version {
            continue;
        }
        let prerelease = is_prerelease_loader_version(&entry);
        let install = neoforge_install_source(&entry)?;
        let build_meta = infer_loader_build_metadata(&entry, &[], false, false, Some(!prerelease));
        builds.push(maven_build_record(
            component_id,
            minecraft_version,
            entry,
            build_meta,
            install,
        )?);
    }

    builds.reverse();
    Ok(LoaderVersionIndex {
        component_id,
        builds,
    })
}

#[cfg(test)]
mod tests {
    use super::{parse_builds_from_maven_metadata, parse_game_versions_from_maven_metadata};
    use crate::loaders::api::parse_build_id;
    use crate::loaders::types::{
        LoaderArtifactKind, LoaderComponentId, LoaderInstallSource, LoaderInstallStrategy,
    };

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

    #[test]
    fn builds_keep_exact_installer_artifacts_for_the_selected_target_only() {
        let xml = "<metadata><versions>\
             <version>21.5.73</version>\
             <version>21.5.74-beta</version>\
             <version>21.4.10</version>\
             </versions></metadata>";
        let index = parse_builds_from_maven_metadata("1.21.5", xml).expect("catalog");
        assert_eq!(index.component_id, LoaderComponentId::NeoForge);
        let builds = index
            .builds
            .iter()
            .map(|build| build.loader_version.as_str())
            .collect::<Vec<_>>();
        assert_eq!(builds, vec!["21.5.74-beta", "21.5.73"]);
        for build in &index.builds {
            assert_eq!(build.minecraft_version, "1.21.5");
            assert_eq!(build.strategy, LoaderInstallStrategy::NeoForgeModern);
            assert_eq!(build.artifact_kind, LoaderArtifactKind::InstallerJar);
            let version = &build.loader_version;
            assert_eq!(
                build.install_source,
                LoaderInstallSource::InstallerJar {
                    url: format!(
                        "https://maven.neoforged.net/releases/net/neoforged/neoforge/{version}/neoforge-{version}-installer.jar"
                    )
                }
            );
            assert_eq!(
                parse_build_id(&build.build_id),
                Some((
                    LoaderComponentId::NeoForge,
                    "1.21.5".to_string(),
                    version.clone()
                ))
            );
        }
        assert!(
            parse_builds_from_maven_metadata("1.21.6", xml)
                .expect("catalog")
                .builds
                .is_empty()
        );
    }
}
