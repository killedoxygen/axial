use crate::loaders::http::fetch_bytes;
use crate::loaders::types::{
    LoaderBuildMetadata, LoaderError, LoaderSelectionMeta, LoaderSelectionReason,
    LoaderSelectionSource, LoaderTerm, LoaderTermEvidence, LoaderTermSource,
};

pub const FABRIC_META_BASE: &str = "https://meta.fabricmc.net/v2/versions";
pub const QUILT_META_BASE: &str = "https://meta.quiltmc.org/v3/versions";
pub const FORGE_MAVEN_META: &str =
    "https://maven.minecraftforge.net/net/minecraftforge/forge/maven-metadata.xml";
pub const FORGE_PROMOTIONS_URL: &str =
    "https://files.minecraftforge.net/net/minecraftforge/forge/promotions_slim.json";
pub const NEOFORGE_MAVEN_META: &str =
    "https://maven.neoforged.net/releases/net/neoforged/neoforge/maven-metadata.xml";
pub const FORGE_MAVEN_BASE: &str = "https://maven.minecraftforge.net";
pub const NEOFORGE_MAVEN_BASE: &str = "https://maven.neoforged.net/releases";

pub async fn fetch_text(url: &str) -> Result<String, LoaderError> {
    let bytes = fetch_bytes(url, 2 << 20).await?;
    String::from_utf8(bytes).map_err(|_| LoaderError::ProviderDataInvalid {
        kind: crate::loaders::types::LoaderProviderFailureKind::SchemaInvalid,
        status: None,
    })
}

pub fn parse_maven_versions(xml: &str) -> Vec<String> {
    let open = "<version>";
    let close = "</version>";
    let mut versions = Vec::new();
    let mut rest = xml;

    while let Some(start) = rest.find(open) {
        rest = &rest[start + open.len()..];
        let Some(end) = rest.find(close) else {
            break;
        };

        let version = &rest[..end];
        if !version.is_empty() && !version.contains('<') {
            versions.push(version.to_string());
        }
        rest = &rest[end + close.len()..];
    }

    versions
}

pub fn extract_forge_minecraft_version(entry: &str) -> String {
    split_forge_coordinate(entry)
        .map(|(minecraft_version, _)| minecraft_version.to_string())
        .unwrap_or_default()
}

pub fn extract_forge_loader_version(entry: &str) -> String {
    split_forge_coordinate(entry)
        .map(|(_, loader_version)| loader_version.to_string())
        .unwrap_or_default()
}

fn split_forge_coordinate(entry: &str) -> Option<(&str, &str)> {
    entry.match_indices('-').find_map(|(index, _)| {
        let minecraft_version = &entry[..index];
        let loader_version = &entry[index + 1..];
        (!minecraft_version.is_empty() && is_forge_loader_version_suffix(loader_version))
            .then_some((minecraft_version, loader_version))
    })
}

fn is_forge_loader_version_suffix(value: &str) -> bool {
    let Some(first) = value.chars().next() else {
        return false;
    };
    if !first.is_ascii_digit() {
        return false;
    }
    let numeric_prefix = value.split('-').next().unwrap_or(value);
    let mut part_count = 0;
    for part in numeric_prefix.split('.') {
        if part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()) {
            return false;
        }
        part_count += 1;
    }
    part_count >= 2
}

pub fn parse_version_triplet(version: &str) -> Option<Vec<u32>> {
    let mut values = Vec::new();
    for part in version.split('.') {
        if part.is_empty() {
            return None;
        }
        let digits = part
            .chars()
            .take_while(|ch| ch.is_ascii_digit())
            .collect::<String>();
        if digits.is_empty() {
            return None;
        }
        values.push(digits.parse::<u32>().ok()?);
    }
    Some(values)
}

pub fn minecraft_version_at_least(version: &str, target: &[u32]) -> bool {
    let Some(parts) = parse_version_triplet(version) else {
        return false;
    };
    for index in 0..target.len().max(parts.len()) {
        let left = *parts.get(index).unwrap_or(&0);
        let right = *target.get(index).unwrap_or(&0);
        if left != right {
            return left > right;
        }
    }
    true
}

pub fn is_prerelease_loader_version(version: &str) -> bool {
    let lower = version.to_ascii_lowercase();
    ["alpha", "beta", "snapshot", "pre", "nightly", "dev"]
        .into_iter()
        .any(|marker| lower.contains(marker))
        || contains_release_candidate_marker(&lower)
}

fn contains_release_candidate_marker(version: &str) -> bool {
    version.match_indices("rc").any(|(index, _)| {
        matches!(
            version[..index].chars().next_back(),
            None | Some('-' | '.' | '_')
        )
    })
}

pub fn infer_loader_build_metadata(
    loader_version: &str,
    provider_evidence: &[LoaderTermEvidence],
    recommended: bool,
    latest: bool,
    stable_hint: Option<bool>,
) -> LoaderBuildMetadata {
    let mut evidence = provider_evidence.to_vec();
    let mut terms = provider_evidence
        .iter()
        .map(|entry| entry.term)
        .collect::<Vec<_>>();

    if recommended {
        terms.push(LoaderTerm::Recommended);
        evidence.push(LoaderTermEvidence {
            term: LoaderTerm::Recommended,
            source: LoaderTermSource::PromotionMarker,
        });
    }
    if latest {
        terms.push(LoaderTerm::Latest);
        evidence.push(LoaderTermEvidence {
            term: LoaderTerm::Latest,
            source: LoaderTermSource::PromotionMarker,
        });
    }

    let lower = loader_version.to_ascii_lowercase();
    if lower.contains("nightly") {
        terms.push(LoaderTerm::Nightly);
        evidence.push(LoaderTermEvidence {
            term: LoaderTerm::Nightly,
            source: LoaderTermSource::ExplicitVersionLabel,
        });
    } else if lower.contains("dev") {
        terms.push(LoaderTerm::Dev);
        evidence.push(LoaderTermEvidence {
            term: LoaderTerm::Dev,
            source: LoaderTermSource::ExplicitVersionLabel,
        });
    } else if lower.contains("alpha") {
        terms.push(LoaderTerm::Alpha);
        evidence.push(LoaderTermEvidence {
            term: LoaderTerm::Alpha,
            source: LoaderTermSource::ExplicitVersionLabel,
        });
    } else if lower.contains("beta") {
        terms.push(LoaderTerm::Beta);
        evidence.push(LoaderTermEvidence {
            term: LoaderTerm::Beta,
            source: LoaderTermSource::ExplicitVersionLabel,
        });
    } else if contains_release_candidate_marker(&lower) {
        terms.push(LoaderTerm::ReleaseCandidate);
        evidence.push(LoaderTermEvidence {
            term: LoaderTerm::ReleaseCandidate,
            source: LoaderTermSource::ExplicitVersionLabel,
        });
    } else if lower.contains("pre") {
        terms.push(LoaderTerm::PreRelease);
        evidence.push(LoaderTermEvidence {
            term: LoaderTerm::PreRelease,
            source: LoaderTermSource::ExplicitVersionLabel,
        });
    } else if lower.contains("snapshot") {
        terms.push(LoaderTerm::Snapshot);
        evidence.push(LoaderTermEvidence {
            term: LoaderTerm::Snapshot,
            source: LoaderTermSource::ExplicitVersionLabel,
        });
    }

    terms.sort();
    terms.dedup();

    evidence.sort();
    evidence.dedup();

    let explicit_unstable_term = terms.iter().any(|term| {
        matches!(
            term,
            LoaderTerm::Snapshot
                | LoaderTerm::PreRelease
                | LoaderTerm::ReleaseCandidate
                | LoaderTerm::Beta
                | LoaderTerm::Alpha
                | LoaderTerm::Nightly
                | LoaderTerm::Dev
        )
    });
    let selection = infer_loader_selection(
        terms.contains(&LoaderTerm::Recommended),
        terms.contains(&LoaderTerm::Latest),
        explicit_unstable_term,
        stable_hint,
    );

    LoaderBuildMetadata {
        display_tags: loader_display_tags(&terms),
        terms,
        evidence,
        selection,
    }
}

pub fn apply_forge_promotion_selection(
    build_meta: &mut LoaderBuildMetadata,
    has_recommended: bool,
    is_recommended: bool,
    is_latest: bool,
) {
    let explicit_unstable_term = has_unstable_loader_terms(&build_meta.terms);
    build_meta.selection = if explicit_unstable_term {
        if is_latest {
            LoaderSelectionMeta {
                default_rank: 650,
                reason: LoaderSelectionReason::LatestUnstable,
                source: LoaderSelectionSource::ExplicitVersionLabel,
            }
        } else {
            LoaderSelectionMeta {
                default_rank: 600,
                reason: LoaderSelectionReason::Unstable,
                source: LoaderSelectionSource::ExplicitVersionLabel,
            }
        }
    } else if has_recommended {
        if is_recommended {
            LoaderSelectionMeta {
                default_rank: 1_000,
                reason: LoaderSelectionReason::Recommended,
                source: LoaderSelectionSource::PromotionMarker,
            }
        } else if is_latest {
            LoaderSelectionMeta {
                default_rank: 900,
                reason: LoaderSelectionReason::LatestStable,
                source: LoaderSelectionSource::PromotionMarker,
            }
        } else {
            LoaderSelectionMeta {
                default_rank: 800,
                reason: LoaderSelectionReason::Stable,
                source: LoaderSelectionSource::PromotionMarker,
            }
        }
    } else if is_latest {
        LoaderSelectionMeta {
            default_rank: 900,
            reason: LoaderSelectionReason::LatestStable,
            source: LoaderSelectionSource::PromotionMarker,
        }
    } else {
        LoaderSelectionMeta {
            default_rank: 800,
            reason: LoaderSelectionReason::Stable,
            source: LoaderSelectionSource::None,
        }
    };
}

fn has_unstable_loader_terms(terms: &[LoaderTerm]) -> bool {
    terms.iter().any(|term| {
        matches!(
            term,
            LoaderTerm::Snapshot
                | LoaderTerm::PreRelease
                | LoaderTerm::ReleaseCandidate
                | LoaderTerm::Beta
                | LoaderTerm::Alpha
                | LoaderTerm::Nightly
                | LoaderTerm::Dev
        )
    })
}

fn infer_loader_selection(
    recommended: bool,
    latest: bool,
    explicit_unstable_term: bool,
    stable_hint: Option<bool>,
) -> LoaderSelectionMeta {
    let (default_rank, reason, source) = if recommended {
        (
            1_000,
            LoaderSelectionReason::Recommended,
            LoaderSelectionSource::PromotionMarker,
        )
    } else if stable_hint == Some(true) && latest {
        (
            900,
            LoaderSelectionReason::LatestStable,
            LoaderSelectionSource::ExplicitApiFlag,
        )
    } else if stable_hint == Some(true) {
        (
            800,
            LoaderSelectionReason::Stable,
            LoaderSelectionSource::ExplicitApiFlag,
        )
    } else if latest && explicit_unstable_term {
        (
            650,
            LoaderSelectionReason::LatestUnstable,
            LoaderSelectionSource::ExplicitVersionLabel,
        )
    } else if latest && stable_hint == Some(false) {
        (
            650,
            LoaderSelectionReason::LatestUnstable,
            LoaderSelectionSource::ExplicitApiFlag,
        )
    } else if latest {
        (
            750,
            LoaderSelectionReason::Latest,
            LoaderSelectionSource::PromotionMarker,
        )
    } else if explicit_unstable_term {
        (
            600,
            LoaderSelectionReason::Unstable,
            LoaderSelectionSource::ExplicitVersionLabel,
        )
    } else if stable_hint == Some(false) {
        (
            600,
            LoaderSelectionReason::Unstable,
            LoaderSelectionSource::ExplicitApiFlag,
        )
    } else {
        (
            700,
            LoaderSelectionReason::Unlabeled,
            LoaderSelectionSource::None,
        )
    };

    LoaderSelectionMeta {
        default_rank,
        reason,
        source,
    }
}

pub fn loader_display_tags(terms: &[LoaderTerm]) -> Vec<String> {
    [
        LoaderTerm::Recommended,
        LoaderTerm::Latest,
        LoaderTerm::Nightly,
        LoaderTerm::Dev,
        LoaderTerm::ReleaseCandidate,
        LoaderTerm::PreRelease,
        LoaderTerm::Beta,
        LoaderTerm::Alpha,
        LoaderTerm::Snapshot,
    ]
    .into_iter()
    .filter(|term| terms.contains(term))
    .map(|term| match term {
        LoaderTerm::Recommended => "recommended".to_string(),
        LoaderTerm::Latest => "latest".to_string(),
        LoaderTerm::Nightly => "nightly".to_string(),
        LoaderTerm::Dev => "dev".to_string(),
        LoaderTerm::ReleaseCandidate => "rc".to_string(),
        LoaderTerm::PreRelease => "pre-release".to_string(),
        LoaderTerm::Beta => "beta".to_string(),
        LoaderTerm::Alpha => "alpha".to_string(),
        LoaderTerm::Snapshot => "snapshot".to_string(),
    })
    .collect()
}

pub fn neoforge_to_minecraft_version(version: &str) -> Option<String> {
    if !version
        .chars()
        .next()
        .is_some_and(|ch| matches!(ch, '1'..='9'))
    {
        return None;
    }

    let numeric_parts = version
        .split('.')
        .map(|part| {
            part.chars()
                .take_while(|ch| ch.is_ascii_digit())
                .collect::<String>()
        })
        .take_while(|part| !part.is_empty())
        .collect::<Vec<_>>();

    let major = numeric_parts.first()?;
    let minor = numeric_parts.get(1)?;

    if major.parse::<u32>().ok()? >= 25 {
        let mut parts = vec![major.clone(), minor.clone()];
        if let Some(patch) = numeric_parts.get(2)
            && patch != "0"
        {
            parts.push(patch.clone());
        }
        return Some(parts.join("."));
    }

    if minor == "0" {
        Some(format!("1.{major}"))
    } else {
        Some(format!("1.{major}.{minor}"))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        apply_forge_promotion_selection, extract_forge_loader_version,
        extract_forge_minecraft_version, infer_loader_build_metadata, is_prerelease_loader_version,
        neoforge_to_minecraft_version, parse_maven_versions,
    };
    use crate::loaders::types::{
        LoaderSelectionReason, LoaderSelectionSource, LoaderTerm, LoaderTermEvidence,
        LoaderTermSource,
    };

    #[test]
    fn detects_common_prerelease_loader_markers() {
        assert!(is_prerelease_loader_version("26.1.2.12-beta"));
        assert!(is_prerelease_loader_version("26.1.0.0-alpha.15+pre-3"));
        assert!(is_prerelease_loader_version("1.0.0-rc1"));
        assert!(!is_prerelease_loader_version("61.1.5"));
        assert!(!is_prerelease_loader_version("1.0.0+source"));
        assert!(!is_prerelease_loader_version("from-src-1.0"));
    }

    #[test]
    fn parses_literal_maven_version_entries() {
        let xml = "<metadata><versions><version>1.20.1-47.4.0</version><version>bad<nested></version><version>1.21.1-52.0.1</version></versions></metadata>";

        assert_eq!(
            parse_maven_versions(xml),
            vec!["1.20.1-47.4.0".to_string(), "1.21.1-52.0.1".to_string()]
        );
    }

    #[test]
    fn splits_forge_coordinates_at_loader_version_suffix() {
        for (entry, minecraft_version, loader_version) in [
            ("26.2-rc-1-65.0.0", "26.2-rc-1", "65.0.0"),
            ("26.1-snapshot-9-62.0.0", "26.1-snapshot-9", "62.0.0"),
            (
                "1.7.10_pre4-10.12.2.1149-prerelease",
                "1.7.10_pre4",
                "10.12.2.1149-prerelease",
            ),
        ] {
            assert_eq!(extract_forge_minecraft_version(entry), minecraft_version);
            assert_eq!(extract_forge_loader_version(entry), loader_version);
        }
    }

    #[test]
    fn maps_recommended_loader_to_explicit_term_and_stable_selection() {
        let metadata = infer_loader_build_metadata("61.1.0", &[], true, false, Some(true));
        assert!(metadata.terms.contains(&LoaderTerm::Recommended));
        assert_eq!(
            metadata.selection.reason,
            LoaderSelectionReason::Recommended
        );
        assert_eq!(
            metadata.selection.source,
            LoaderSelectionSource::PromotionMarker
        );
        assert_eq!(metadata.display_tags, vec!["recommended".to_string()]);
    }

    #[test]
    fn maps_beta_loader_to_explicit_terms_and_unstable_selection() {
        let metadata = infer_loader_build_metadata(
            "26.1.2.12-beta",
            &[LoaderTermEvidence {
                term: LoaderTerm::Beta,
                source: LoaderTermSource::ExplicitVersionLabel,
            }],
            false,
            true,
            Some(false),
        );
        assert!(metadata.terms.contains(&LoaderTerm::Beta));
        assert!(metadata.terms.contains(&LoaderTerm::Latest));
        assert_eq!(
            metadata.selection.reason,
            LoaderSelectionReason::LatestUnstable
        );
        assert_eq!(
            metadata.display_tags,
            vec!["latest".to_string(), "beta".to_string()]
        );
    }

    #[test]
    fn only_maps_release_candidate_for_delimited_rc_markers() {
        let metadata = infer_loader_build_metadata("1.0.0+source", &[], false, false, None);
        assert!(!metadata.terms.contains(&LoaderTerm::ReleaseCandidate));

        let metadata = infer_loader_build_metadata("1.0.0-rc1", &[], false, false, None);
        assert!(metadata.terms.contains(&LoaderTerm::ReleaseCandidate));
    }

    #[test]
    fn keeps_unlabeled_unstable_loader_without_inventing_terms() {
        let metadata = infer_loader_build_metadata("0.16.11", &[], false, false, Some(false));
        assert!(metadata.terms.is_empty());
        assert_eq!(metadata.selection.reason, LoaderSelectionReason::Unstable);
        assert!(metadata.display_tags.is_empty());
    }

    #[test]
    fn maps_legacy_neoforge_versions_to_one_prefixed_minecraft_versions() {
        assert_eq!(
            neoforge_to_minecraft_version("21.0.167"),
            Some("1.21".to_string())
        );
        assert_eq!(
            neoforge_to_minecraft_version("21.11.5-beta"),
            Some("1.21.11".to_string())
        );
        assert_eq!(
            neoforge_to_minecraft_version("20.4.239"),
            Some("1.20.4".to_string())
        );
    }

    #[test]
    fn maps_year_based_neoforge_versions_without_one_prefix() {
        assert_eq!(
            neoforge_to_minecraft_version("26.1.0.7-beta"),
            Some("26.1".to_string())
        );
        assert_eq!(
            neoforge_to_minecraft_version("26.1.2.7-beta"),
            Some("26.1.2".to_string())
        );
    }

    #[test]
    fn ignores_neoforge_loader_versions_without_minecraft_major_prefix() {
        assert_eq!(
            neoforge_to_minecraft_version("0.25w14craftmine.5-beta"),
            None
        );
        assert_eq!(neoforge_to_minecraft_version("0.25.5-beta"), None);
        assert_eq!(neoforge_to_minecraft_version("beta-26.1.0.1"), None);
    }

    #[test]
    fn forge_promotion_override_keeps_latest_without_recommended_stable() {
        let mut metadata = infer_loader_build_metadata("64.0.4", &[], false, true, None);
        apply_forge_promotion_selection(&mut metadata, false, false, true);

        assert_eq!(
            metadata.selection.reason,
            LoaderSelectionReason::LatestStable
        );
        assert_eq!(
            metadata.selection.source,
            LoaderSelectionSource::PromotionMarker
        );
    }

    #[test]
    fn forge_promotion_override_keeps_explicit_beta_unstable() {
        let mut metadata = infer_loader_build_metadata("64.0.4-beta", &[], false, true, None);
        apply_forge_promotion_selection(&mut metadata, true, false, true);

        assert!(metadata.terms.contains(&LoaderTerm::Beta));
        assert_eq!(
            metadata.selection.reason,
            LoaderSelectionReason::LatestUnstable
        );
        assert_eq!(
            metadata.selection.source,
            LoaderSelectionSource::ExplicitVersionLabel
        );
    }
}
