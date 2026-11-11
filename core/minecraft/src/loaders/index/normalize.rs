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
    use crate::loaders::installed_version_id_for;
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
    fn orders_supported_versions_by_catalog_then_version_shape() {
        let catalog = HashMap::from([
            ("1.20.4".to_string(), 0),
            ("1.20.3".to_string(), 1),
            ("1.20.2".to_string(), 2),
        ]);
        for (input, catalog_order, expected) in [
            (
                vec!["1.18", "26.1", "1.18.2", "1.18.1", "26.1.2"],
                None,
                vec!["26.1.2", "26.1", "1.18.2", "1.18.1", "1.18"],
            ),
            (
                vec!["1.20.2", "1.20.4", "1.20.3"],
                Some(&catalog),
                vec!["1.20.4", "1.20.3", "1.20.2"],
            ),
            (
                vec!["26.1", "1.20.3", "1.20.4"],
                Some(&catalog),
                vec!["1.20.4", "1.20.3", "26.1"],
            ),
            (
                vec!["1.20.5-pre1", "1.20.5", "1.20.5-rc1"],
                None,
                vec!["1.20.5", "1.20.5-rc1", "1.20.5-pre1"],
            ),
        ] {
            let versions = input
                .iter()
                .map(|id| game_version(id, !id.contains('-')))
                .collect();
            let ordered = normalize_supported_versions(versions, catalog_order)
                .into_iter()
                .map(|entry| entry.id)
                .collect::<Vec<_>>();
            assert_eq!(ordered, expected);
        }
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
            version_id: installed_version_id_for(component_id, "1.18.2", loader_version)
                .expect("canonical installed version id"),
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
