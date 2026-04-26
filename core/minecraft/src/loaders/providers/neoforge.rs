use super::common::{
    NEOFORGE_MAVEN_BASE, NEOFORGE_MAVEN_META, fetch_text, infer_loader_build_metadata,
    is_prerelease_loader_version, neoforge_to_minecraft_version, parse_maven_versions,
};
use crate::lifecycle::LifecycleMeta;
use crate::loaders::api::{build_id_for, installed_version_id_for};
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
    let mut seen = std::collections::HashSet::new();
    let mut versions = Vec::new();
    for entry in parse_maven_versions(&xml) {
        let Some(minecraft_version) = neoforge_to_minecraft_version(&entry) else {
            continue;
        };
        if !seen.insert(minecraft_version.clone()) {
            continue;
        }
        versions.push(LoaderGameVersion {
            subject_kind: VersionSubjectKind::MinecraftVersion,
            id: minecraft_version,
            release_time: String::new(),
            minecraft_meta: MinecraftVersionMeta::default(),
            lifecycle: LifecycleMeta::default(),
            stable_hint: Some(true),
        });
    }
    Ok(versions)
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
            version_id: installed_version_id_for(component_id, minecraft_version, &entry),
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
