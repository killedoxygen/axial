//! Which instance would fit a staged set of content? Given what the user picked
//! before they have an instance, this scores every (loader, Minecraft version)
//! pair the picks between them support and ranks the ones that lose the least.
//! The frontend renders the candidates; the ranking is policy and stays here.

use axial_content::{CanonicalId, ContentKind};
use axial_minecraft::LoaderComponentId;
use serde::Serialize;

const MAX_CANDIDATES: usize = 8;

/// Preference between candidates that are otherwise tied. Fabric first because
/// it is where most of Modrinth lives.
const LOADER_RANK: [LoaderComponentId; 4] = [
    LoaderComponentId::Fabric,
    LoaderComponentId::NeoForge,
    LoaderComponentId::Forge,
    LoaderComponentId::Quilt,
];

pub struct CompatItem {
    pub canonical_id: CanonicalId,
    pub title: String,
    pub kind: ContentKind,
    pub loaders: Vec<String>,
    pub game_versions: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CompatDrop {
    pub canonical_id: CanonicalId,
    pub title: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CompatCandidate {
    /// Empty for vanilla.
    pub loader: String,
    pub loader_label: String,
    pub game_version: String,
    /// What to POST to `/instances` to create this instance. The create API is
    /// addressable by loader and Minecraft version, so no version browsing is
    /// needed to act on a candidate.
    pub selection_id: String,
    pub summary: String,
    pub supported_count: usize,
    pub total_count: usize,
    pub complete: bool,
    pub drops: Vec<CompatDrop>,
}

pub fn rank_candidates(items: &[CompatItem]) -> Vec<CompatCandidate> {
    if items.is_empty() {
        return Vec::new();
    }

    let mut candidates: Vec<CompatCandidate> = Vec::new();
    for loader in candidate_loaders(items) {
        for game_version in candidate_game_versions(items) {
            let drops: Vec<CompatDrop> = items
                .iter()
                .filter(|item| !item_supports(item, loader, &game_version))
                .map(|item| CompatDrop {
                    canonical_id: item.canonical_id.clone(),
                    title: item.title.clone(),
                })
                .collect();
            let supported_count = items.len() - drops.len();
            if supported_count == 0 {
                continue;
            }
            candidates.push(build_candidate(
                loader,
                game_version,
                supported_count,
                items.len(),
                drops,
            ));
        }
    }

    candidates.sort_by(|a, b| {
        b.supported_count
            .cmp(&a.supported_count)
            .then_with(|| version_key(&b.game_version).cmp(&version_key(&a.game_version)))
            .then_with(|| loader_rank(&a.loader).cmp(&loader_rank(&b.loader)))
    });
    candidates.truncate(MAX_CANDIDATES);
    candidates
}

fn build_candidate(
    loader: Option<LoaderComponentId>,
    game_version: String,
    supported_count: usize,
    total_count: usize,
    drops: Vec<CompatDrop>,
) -> CompatCandidate {
    let complete = supported_count == total_count;
    let summary = if complete && total_count == 1 {
        "Works here".to_string()
    } else if complete {
        format!("All {total_count} work here")
    } else {
        format!("{supported_count} of {total_count} work here")
    };
    CompatCandidate {
        loader: loader
            .map(|id| id.short_key().to_string())
            .unwrap_or_default(),
        loader_label: loader
            .map(|id| id.display_name().to_string())
            .unwrap_or_else(|| "Vanilla".to_string()),
        selection_id: match loader {
            Some(id) => format!("loader_version|{}|{}", id.short_key(), game_version),
            None => format!("vanilla|{game_version}"),
        },
        game_version,
        summary,
        supported_count,
        total_count,
        complete,
        drops,
    }
}

fn item_supports(item: &CompatItem, loader: Option<LoaderComponentId>, game_version: &str) -> bool {
    if !item.game_versions.iter().any(|value| value == game_version) {
        return false;
    }
    if !item.kind.filters_by_loader() {
        return true;
    }
    match loader {
        Some(loader) => item
            .loaders
            .iter()
            .filter_map(|value| LoaderComponentId::parse(value))
            .any(|value| value == loader),
        None => false,
    }
}

/// The loaders worth considering: those the picks actually ship for. When
/// nothing needs a loader, vanilla is the only candidate.
fn candidate_loaders(items: &[CompatItem]) -> Vec<Option<LoaderComponentId>> {
    let needs_loader = items.iter().any(|item| item.kind.filters_by_loader());
    if !needs_loader {
        return vec![None];
    }
    let mut loaders: Vec<Option<LoaderComponentId>> = LOADER_RANK
        .iter()
        .copied()
        .filter(|loader| {
            items.iter().any(|item| {
                item.kind.filters_by_loader()
                    && item
                        .loaders
                        .iter()
                        .filter_map(|value| LoaderComponentId::parse(value))
                        .any(|value| value == *loader)
            })
        })
        .map(Some)
        .collect();
    if loaders.is_empty() {
        loaders.push(None);
    }
    loaders
}

fn candidate_game_versions(items: &[CompatItem]) -> Vec<String> {
    let mut versions: Vec<String> = items
        .iter()
        .flat_map(|item| item.game_versions.iter())
        .filter(|value| is_release_version(value))
        .cloned()
        .collect();
    versions.sort_by_key(|value| std::cmp::Reverse(version_key(value)));
    versions.dedup();
    versions
}

/// `1.21.6` yes, `24w14a` or `1.21.6-rc1` no. Snapshots are not somewhere to
/// send a user who just wants their mods to work.
fn is_release_version(value: &str) -> bool {
    !value.is_empty()
        && value
            .split('.')
            .all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()))
}

fn version_key(value: &str) -> Vec<u32> {
    value
        .split('.')
        .map(|part| part.parse::<u32>().unwrap_or(0))
        .collect()
}

fn loader_rank(loader: &str) -> usize {
    LoaderComponentId::parse(loader)
        .and_then(|id| LOADER_RANK.iter().position(|entry| *entry == id))
        .unwrap_or(LOADER_RANK.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str, kind: ContentKind, loaders: &[&str], game_versions: &[&str]) -> CompatItem {
        CompatItem {
            canonical_id: CanonicalId(format!("modrinth:{id}")),
            title: id.to_string(),
            kind,
            loaders: loaders.iter().map(|value| value.to_string()).collect(),
            game_versions: game_versions
                .iter()
                .map(|value| value.to_string())
                .collect(),
        }
    }

    #[test]
    fn the_version_every_pick_supports_ranks_first() {
        let items = vec![
            item(
                "sodium",
                ContentKind::Mod,
                &["fabric"],
                &["1.21.4", "1.21.6"],
            ),
            item(
                "lithium",
                ContentKind::Mod,
                &["fabric"],
                &["1.21.4", "1.21.6"],
            ),
            item("iris", ContentKind::Mod, &["fabric"], &["1.21.4"]),
        ];

        let ranked = rank_candidates(&items);
        let top = &ranked[0];

        assert_eq!(top.game_version, "1.21.4");
        assert_eq!(top.loader, "fabric");
        assert!(top.complete);
        assert_eq!(top.supported_count, 3);
        assert_eq!(top.summary, "All 3 work here");
        assert_eq!(top.selection_id, "loader_version|fabric|1.21.4");
        assert!(top.drops.is_empty());
    }

    #[test]
    fn a_partial_candidate_names_what_it_drops() {
        let items = vec![
            item("sodium", ContentKind::Mod, &["fabric"], &["1.21.6"]),
            item("iris", ContentKind::Mod, &["fabric"], &["1.21.4"]),
        ];

        let ranked = rank_candidates(&items);

        assert!(ranked.iter().all(|candidate| !candidate.complete));
        let newest = &ranked[0];
        assert_eq!(newest.game_version, "1.21.6");
        assert_eq!(newest.summary, "1 of 2 work here");
        assert_eq!(newest.drops.len(), 1);
        assert_eq!(newest.drops[0].title, "iris");
    }

    #[test]
    fn resource_packs_alone_land_on_vanilla() {
        let items = vec![item(
            "faithful",
            ContentKind::ResourcePack,
            &["minecraft"],
            &["1.21.6"],
        )];

        let ranked = rank_candidates(&items);

        assert_eq!(ranked[0].loader, "");
        assert_eq!(ranked[0].loader_label, "Vanilla");
        assert_eq!(ranked[0].selection_id, "vanilla|1.21.6");
        assert_eq!(ranked[0].summary, "Works here");
    }

    #[test]
    fn a_pack_rides_along_with_whatever_loader_the_mods_need() {
        let items = vec![
            item("sodium", ContentKind::Mod, &["fabric"], &["1.21.6"]),
            item(
                "faithful",
                ContentKind::ResourcePack,
                &["minecraft"],
                &["1.21.6"],
            ),
        ];

        let ranked = rank_candidates(&items);

        assert_eq!(ranked[0].loader, "fabric");
        assert!(ranked[0].complete, "the pack must not veto the loader");
    }

    #[test]
    fn a_shaders_iris_tag_is_never_mistaken_for_a_loader() {
        let items = vec![item(
            "complementary",
            ContentKind::ShaderPack,
            &["iris", "optifine"],
            &["1.21.6"],
        )];

        let ranked = rank_candidates(&items);

        assert_eq!(ranked[0].loader, "");
        assert!(ranked[0].complete);
    }

    #[test]
    fn snapshots_are_never_offered() {
        let items = vec![item(
            "sodium",
            ContentKind::Mod,
            &["fabric"],
            &["1.21.6", "24w14a", "1.21.6-rc1"],
        )];

        let ranked = rank_candidates(&items);

        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].game_version, "1.21.6");
    }

    #[test]
    fn mods_that_share_no_loader_still_offer_the_best_partial() {
        let items = vec![
            item("fabric-only", ContentKind::Mod, &["fabric"], &["1.21.6"]),
            item("forge-only", ContentKind::Mod, &["forge"], &["1.21.6"]),
        ];

        let ranked = rank_candidates(&items);

        assert!(!ranked.is_empty());
        assert!(
            ranked
                .iter()
                .all(|candidate| candidate.supported_count == 1)
        );
        assert_eq!(ranked[0].loader, "fabric", "fabric outranks forge on a tie");
    }

    #[test]
    fn nothing_selected_yields_nothing() {
        assert!(rank_candidates(&[]).is_empty());
    }
}
