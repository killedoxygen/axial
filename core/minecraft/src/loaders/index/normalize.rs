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
    index
}

fn compare_build_records(left: &LoaderBuildRecord, right: &LoaderBuildRecord) -> Ordering {
    right
        .build_meta
        .selection
        .default_rank
        .cmp(&left.build_meta.selection.default_rank)
        .then_with(|| compare_version_like(&right.loader_version, &left.loader_version))
}

fn compare_supported_versions(
    left: &LoaderGameVersion,
    right: &LoaderGameVersion,
    catalog_order: Option<&HashMap<String, usize>>,
) -> Ordering {
    if let Some(order) = catalog_order {
        let left_rank = order.get(&left.id);
        let right_rank = order.get(&right.id);

        match (left_rank, right_rank) {
            (Some(left_rank), Some(right_rank)) if left_rank != right_rank => {
                return left_rank.cmp(right_rank);
            }
            (Some(_), None) => return Ordering::Less,
            (None, Some(_)) => return Ordering::Greater,
            _ => {}
        }
    }

    compare_version_like(&right.id, &left.id)
}

#[cfg(test)]
mod tests {
    use super::{normalize_build_index, normalize_supported_versions};
    use crate::lifecycle::LifecycleMeta;
    use crate::loaders::types::{
        LoaderArtifactKind, LoaderBuildMetadata, LoaderBuildRecord, LoaderBuildSubjectKind,
        LoaderComponentId, LoaderGameVersion, LoaderInstallSource, LoaderInstallStrategy,
        LoaderInstallability, LoaderSelectionMeta, LoaderSelectionReason, LoaderSelectionSource,
        LoaderTerm, LoaderTermEvidence, LoaderTermSource, LoaderVersionIndex,
    };
    use crate::types::VersionSubjectKind;
    use crate::version_meta::MinecraftVersionMeta;
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
            .map(|entry| entry.id)
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
            .map(|entry| entry.id)
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
            .map(|entry| entry.id)
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
            .map(|entry| entry.id)
            .collect::<Vec<_>>();

        assert_eq!(ordered, vec!["1.20.5", "1.20.5-rc1", "1.20.5-pre1"]);
    }

    #[test]
    fn sorts_builds_by_flags_then_loader_version() {
        let component_id = LoaderComponentId::Forge;
        let normalized = normalize_build_index(LoaderVersionIndex {
            component_id,
            builds: vec![
                build_record(component_id, "40.1.0", 800),
                build_record(component_id, "40.2.0", 900),
                build_record(component_id, "40.3.0", 1_000),
                build_record(component_id, "39.0.0", 600),
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
                build_record(component_id, "26.1.2.12-beta", 650),
                build_record(component_id, "26.1.1.15", 800),
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
        default_rank: i32,
    ) -> LoaderBuildRecord {
        LoaderBuildRecord {
            subject_kind: LoaderBuildSubjectKind::LoaderBuild,
            component_id,
            component_name: component_id.display_name().to_string(),
            build_id: format!("{}:1.18.2:{loader_version}", component_id.short_key()),
            minecraft_version: "1.18.2".to_string(),
            loader_version: loader_version.to_string(),
            version_id: format!("1.18.2-forge-{loader_version}"),
            build_meta: LoaderBuildMetadata {
                terms: if default_rank >= 1_000 {
                    vec![LoaderTerm::Recommended]
                } else if default_rank >= 900 {
                    vec![LoaderTerm::Latest]
                } else if default_rank >= 800 {
                    Vec::new()
                } else {
                    vec![LoaderTerm::Beta]
                },
                evidence: if default_rank >= 1_000 {
                    vec![LoaderTermEvidence {
                        term: LoaderTerm::Recommended,
                        source: LoaderTermSource::PromotionMarker,
                    }]
                } else if default_rank >= 900 {
                    vec![LoaderTermEvidence {
                        term: LoaderTerm::Latest,
                        source: LoaderTermSource::PromotionMarker,
                    }]
                } else if default_rank >= 800 {
                    Vec::new()
                } else {
                    vec![LoaderTermEvidence {
                        term: LoaderTerm::Beta,
                        source: LoaderTermSource::ExplicitVersionLabel,
                    }]
                },
                selection: LoaderSelectionMeta {
                    default_rank,
                    reason: if default_rank >= 1_000 {
                        LoaderSelectionReason::Recommended
                    } else if default_rank >= 900 {
                        LoaderSelectionReason::LatestStable
                    } else if default_rank >= 800 {
                        LoaderSelectionReason::Stable
                    } else {
                        LoaderSelectionReason::Unstable
                    },
                    source: if default_rank >= 900 {
                        LoaderSelectionSource::PromotionMarker
                    } else if default_rank >= 800 {
                        LoaderSelectionSource::ExplicitApiFlag
                    } else {
                        LoaderSelectionSource::ExplicitVersionLabel
                    },
                },
                display_tags: if default_rank >= 1_000 {
                    vec!["recommended".to_string()]
                } else if default_rank >= 900 {
                    vec!["latest".to_string()]
                } else if default_rank >= 800 {
                    Vec::new()
                } else {
                    vec!["beta".to_string()]
                },
            },
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
            subject_kind: VersionSubjectKind::MinecraftVersion,
            id: version.to_string(),
            release_time: String::new(),
            minecraft_meta: MinecraftVersionMeta::default(),
            lifecycle: LifecycleMeta::default(),
            stable_hint: Some(stable),
        }
    }
}
