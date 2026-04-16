use crate::loaders::types::{LoaderBuildRecord, LoaderGameVersion, LoaderVersionIndex};
use crate::version_meta::compare_version_like;
use std::cmp::Ordering;
use std::collections::HashMap;

pub fn normalize_supported_versions(
    mut versions: Vec<LoaderGameVersion>,
    catalog_order: Option<&HashMap<String, usize>>,
) -> Vec<LoaderGameVersion> {
    versions.sort_by(|left, right| compare_supported_versions(left, right, catalog_order));
    versions
}

pub fn normalize_build_index(mut index: LoaderVersionIndex) -> LoaderVersionIndex {
    index.builds.sort_by(compare_build_records);

    if !index.builds.iter().any(|build| build.latest)
        && let Some(first) = index.builds.first_mut()
    {
        first.latest = true;
    }

    if !index.builds.iter().any(|build| build.recommended)
        && let Some(first_stable) = index.builds.iter_mut().find(|build| build.stable)
    {
        first_stable.recommended = true;
    }

    index
}

fn compare_build_records(left: &LoaderBuildRecord, right: &LoaderBuildRecord) -> Ordering {
    right
        .recommended
        .cmp(&left.recommended)
        .then_with(|| right.stable.cmp(&left.stable))
        .then_with(|| left.prerelease.cmp(&right.prerelease))
        .then_with(|| right.latest.cmp(&left.latest))
        .then_with(|| compare_version_like(&right.loader_version, &left.loader_version))
}

fn compare_supported_versions(
    left: &LoaderGameVersion,
    right: &LoaderGameVersion,
    catalog_order: Option<&HashMap<String, usize>>,
) -> Ordering {
    if let Some(order) = catalog_order {
        let left_rank = order.get(&left.version);
        let right_rank = order.get(&right.version);

        match (left_rank, right_rank) {
            (Some(left_rank), Some(right_rank)) if left_rank != right_rank => {
                return left_rank.cmp(right_rank);
            }
            (Some(_), None) => return Ordering::Less,
            (None, Some(_)) => return Ordering::Greater,
            _ => {}
        }
    }

    compare_version_like(&right.version, &left.version)
}

#[cfg(test)]
mod tests {
    use super::{normalize_build_index, normalize_supported_versions};
    use crate::loaders::types::{
        LoaderArtifactKind, LoaderBuildRecord, LoaderComponentId, LoaderGameVersion,
        LoaderInstallSource, LoaderInstallStrategy, LoaderInstallability, LoaderVersionIndex,
    };
    use crate::version_meta::VersionMeta;
    use std::collections::HashMap;

    #[test]
    fn sorts_supported_versions_descending_across_patch_and_year_formats() {
        let versions = normalize_supported_versions(
            vec![
                game_version("1.18", true),
                game_version("26.1", true),
                game_version("1.18.2", true),
                game_version("1.18.1", true),
                game_version("26.1.2", true),
            ],
            None,
        );

        let ordered = versions
            .into_iter()
            .map(|entry| entry.version)
            .collect::<Vec<_>>();

        assert_eq!(ordered, vec!["26.1.2", "26.1", "1.18.2", "1.18.1", "1.18"]);
    }

    #[test]
    fn prefers_catalog_order_when_manifest_knows_the_versions() {
        let mut catalog_order = HashMap::new();
        catalog_order.insert("1.20.4".to_string(), 0);
        catalog_order.insert("1.20.3".to_string(), 1);
        catalog_order.insert("1.20.2".to_string(), 2);

        let versions = normalize_supported_versions(
            vec![
                game_version("1.20.2", true),
                game_version("1.20.4", true),
                game_version("1.20.3", true),
            ],
            Some(&catalog_order),
        );

        let ordered = versions
            .into_iter()
            .map(|entry| entry.version)
            .collect::<Vec<_>>();

        assert_eq!(ordered, vec!["1.20.4", "1.20.3", "1.20.2"]);
    }

    #[test]
    fn keeps_catalog_known_versions_ahead_of_unknown_fallback_versions() {
        let mut catalog_order = HashMap::new();
        catalog_order.insert("1.20.4".to_string(), 0);
        catalog_order.insert("1.20.3".to_string(), 1);

        let versions = normalize_supported_versions(
            vec![
                game_version("26.1", true),
                game_version("1.20.3", true),
                game_version("1.20.4", true),
            ],
            Some(&catalog_order),
        );

        let ordered = versions
            .into_iter()
            .map(|entry| entry.version)
            .collect::<Vec<_>>();

        assert_eq!(ordered, vec!["1.20.4", "1.20.3", "26.1"]);
    }

    #[test]
    fn keeps_plain_release_ahead_of_prerelease_suffixes() {
        let versions = normalize_supported_versions(
            vec![
                game_version("1.20.5-pre1", false),
                game_version("1.20.5", true),
                game_version("1.20.5-rc1", false),
            ],
            None,
        );

        let ordered = versions
            .into_iter()
            .map(|entry| entry.version)
            .collect::<Vec<_>>();

        assert_eq!(ordered, vec!["1.20.5", "1.20.5-rc1", "1.20.5-pre1"]);
    }

    #[test]
    fn sorts_builds_by_flags_then_loader_version() {
        let component_id = LoaderComponentId::Forge;
        let normalized = normalize_build_index(LoaderVersionIndex {
            component_id,
            builds: vec![
                build_record(component_id, "40.1.0", false, false, true),
                build_record(component_id, "40.2.0", false, true, true),
                build_record(component_id, "40.3.0", true, false, true),
                build_record(component_id, "39.0.0", false, false, false),
            ],
        });

        let ordered = normalized
            .builds
            .into_iter()
            .map(|build| build.loader_version)
            .collect::<Vec<_>>();

        assert_eq!(ordered, vec!["40.3.0", "40.2.0", "40.1.0", "39.0.0"]);
    }

    #[test]
    fn keeps_stable_build_ahead_of_latest_prerelease() {
        let component_id = LoaderComponentId::NeoForge;
        let normalized = normalize_build_index(LoaderVersionIndex {
            component_id,
            builds: vec![
                build_record(component_id, "26.1.2.12-beta", false, true, false),
                build_record(component_id, "26.1.1.15", false, false, true),
            ],
        });

        let ordered = normalized
            .builds
            .into_iter()
            .map(|build| build.loader_version)
            .collect::<Vec<_>>();

        assert_eq!(ordered, vec!["26.1.1.15", "26.1.2.12-beta"]);
    }

    fn build_record(
        component_id: LoaderComponentId,
        loader_version: &str,
        recommended: bool,
        latest: bool,
        stable: bool,
    ) -> LoaderBuildRecord {
        LoaderBuildRecord {
            component_id,
            component_name: component_id.display_name().to_string(),
            build_id: format!("{}:1.18.2:{loader_version}", component_id.short_key()),
            minecraft_version: "1.18.2".to_string(),
            loader_version: loader_version.to_string(),
            version_id: format!("1.18.2-forge-{loader_version}"),
            stable,
            prerelease: !stable,
            recommended,
            latest,
            strategy: LoaderInstallStrategy::ForgeModern,
            artifact_kind: LoaderArtifactKind::InstallerJar,
            installability: LoaderInstallability::Installable,
            install_source: LoaderInstallSource::InstallerJar {
                url: format!("https://example.invalid/{loader_version}.jar"),
            },
        }
    }

    fn game_version(version: &str, stable: bool) -> LoaderGameVersion {
        LoaderGameVersion {
            version: version.to_string(),
            kind: String::new(),
            release_time: String::new(),
            meta: VersionMeta::default(),
            stable,
        }
    }
}
