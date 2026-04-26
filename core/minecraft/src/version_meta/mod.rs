mod parse;
mod tokenize;

use crate::lifecycle::{LifecycleChannel, LifecycleLabel, LifecycleMeta};
use crate::loaders::types::LoaderGameVersion;
use crate::types::VersionEntry;
use crate::{ManifestEntry, manifest::VersionManifest};
use parse::{ParsedVersionId, VersionShape, parse_version_id};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use tokenize::{TokenKind, tokenize_version_id};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseReference {
    pub id: String,
    pub release_time: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct MinecraftVersionMeta {
    #[serde(default)]
    pub family: String,
    #[serde(default)]
    pub base_id: String,
    #[serde(default)]
    pub effective_version: String,
    #[serde(default)]
    pub variant_of: String,
    #[serde(default)]
    pub variant_kind: String,
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub display_hint: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnalyzedMinecraftVersion {
    pub lifecycle: LifecycleMeta,
    pub minecraft_meta: MinecraftVersionMeta,
}

pub fn manifest_release_references(manifest: &VersionManifest) -> Vec<ReleaseReference> {
    manifest_release_entries(&manifest.versions)
}

pub fn manifest_release_entries(entries: &[ManifestEntry]) -> Vec<ReleaseReference> {
    let mut releases = entries
        .iter()
        .filter(|entry| entry.kind == "release")
        .map(|entry| ReleaseReference {
            id: entry.id.clone(),
            release_time: entry.release_time.clone(),
        })
        .collect::<Vec<_>>();
    releases.sort_by(|left, right| left.release_time.cmp(&right.release_time));
    releases
}

pub fn analyze_minecraft_version(
    id: &str,
    raw_kind: &str,
    release_time: &str,
    stable_hint: Option<bool>,
    releases: &[ReleaseReference],
) -> AnalyzedMinecraftVersion {
    let parsed = parse_version_id(id);
    let family = classify_version_family(&parsed, raw_kind, stable_hint);
    let effective_version = effective_version_for(&parsed, family, release_time, releases);
    let display_name = display_name_for(&parsed);
    let display_hint = display_hint_for(&parsed, family, &effective_version);
    let lifecycle = classify_lifecycle(&parsed, raw_kind, stable_hint);

    AnalyzedMinecraftVersion {
        lifecycle,
        minecraft_meta: MinecraftVersionMeta {
            family: family.to_string(),
            base_id: parsed.base_id.clone(),
            effective_version,
            variant_of: if parsed.variant_kind.is_empty() {
                String::new()
            } else {
                parsed.base_id.clone()
            },
            variant_kind: parsed.variant_kind.clone(),
            display_name,
            display_hint,
        },
    }
}

pub fn enrich_loader_game_versions(
    versions: &mut [LoaderGameVersion],
    manifest_entries: &[ManifestEntry],
    releases: &[ReleaseReference],
) {
    for version in versions {
        let manifest_entry = manifest_entries.iter().find(|entry| entry.id == version.id);
        let metadata = analyze_minecraft_version(
            &version.id,
            manifest_entry
                .map(|entry| entry.kind.as_str())
                .unwrap_or(""),
            manifest_entry
                .map(|entry| entry.release_time.as_str())
                .unwrap_or(&version.release_time),
            version.stable_hint,
            releases,
        );
        if let Some(entry) = manifest_entry {
            version.release_time = entry.release_time.clone();
        }
        version.lifecycle = metadata.lifecycle;
        version.minecraft_meta = metadata.minecraft_meta;
    }
}

pub fn enrich_version_entries(entries: &mut [VersionEntry], releases: &[ReleaseReference]) {
    for entry in entries {
        let analysis = analyze_minecraft_version(
            &entry.id,
            &entry.raw_kind,
            &entry.release_time,
            None,
            releases,
        );
        entry.lifecycle = analysis.lifecycle;
        entry.minecraft_meta = analysis.minecraft_meta;
    }
}

pub fn apply_version_analysis(entry: &mut VersionEntry, releases: &[ReleaseReference]) {
    let analysis = analyze_minecraft_version(
        &entry.id,
        &entry.raw_kind,
        &entry.release_time,
        None,
        releases,
    );
    entry.lifecycle = analysis.lifecycle;
    entry.minecraft_meta = analysis.minecraft_meta;
}

pub fn compare_version_like(left: &str, right: &str) -> Ordering {
    let left_parsed = parse_version_id(left);
    let right_parsed = parse_version_id(right);

    compare_special_shapes(&left_parsed, &right_parsed)
        .then_with(|| compare_tokenized_like(left, right))
        .then_with(|| left.cmp(right))
}

pub fn compare_version_entries(left: &VersionEntry, right: &VersionEntry) -> Ordering {
    version_lifecycle_priority(&left.lifecycle, &left.minecraft_meta)
        .cmp(&version_lifecycle_priority(
            &right.lifecycle,
            &right.minecraft_meta,
        ))
        .then_with(|| compare_release_time_desc(&left.release_time, &right.release_time))
        .then_with(|| compare_version_meta_desc(&left.minecraft_meta, &right.minecraft_meta))
        .then_with(|| compare_version_like(&right.id, &left.id))
}

fn classify_lifecycle(
    parsed: &ParsedVersionId,
    raw_kind: &str,
    stable_hint: Option<bool>,
) -> LifecycleMeta {
    match parsed.shape {
        VersionShape::OldBeta { .. } => LifecycleMeta::new(
            LifecycleChannel::Legacy,
            vec![LifecycleLabel::OldBeta],
            200,
            "BETA",
            vec!["old_beta".to_string()],
        ),
        VersionShape::OldAlpha { .. } => LifecycleMeta::new(
            LifecycleChannel::Legacy,
            vec![LifecycleLabel::OldAlpha],
            100,
            "ALPH",
            vec!["old_alpha".to_string()],
        ),
        VersionShape::PreRelease { .. } => LifecycleMeta::new(
            LifecycleChannel::Preview,
            vec![LifecycleLabel::PreRelease],
            430,
            "PRE",
            vec!["pre_release".to_string()],
        ),
        VersionShape::ReleaseCandidate { .. } => LifecycleMeta::new(
            LifecycleChannel::Preview,
            vec![LifecycleLabel::ReleaseCandidate],
            440,
            "RC",
            vec!["release_candidate".to_string()],
        ),
        VersionShape::CombatTest { .. }
        | VersionShape::ExperimentalSnapshot { .. }
        | VersionShape::DeepDarkExperimentalSnapshot { .. } => LifecycleMeta::new(
            LifecycleChannel::Experimental,
            vec![LifecycleLabel::Snapshot],
            320,
            "EXP",
            vec!["experimental".to_string()],
        ),
        VersionShape::WeeklySnapshot { .. } => LifecycleMeta::new(
            LifecycleChannel::Preview,
            vec![LifecycleLabel::Snapshot],
            400,
            "SNAP",
            vec!["snapshot".to_string()],
        ),
        VersionShape::Release { .. } => LifecycleMeta::new(
            LifecycleChannel::Stable,
            vec![LifecycleLabel::Release],
            500,
            "REL",
            vec!["release".to_string()],
        ),
        VersionShape::Unknown => {
            if raw_kind == "old_beta" {
                LifecycleMeta::new(
                    LifecycleChannel::Legacy,
                    vec![LifecycleLabel::OldBeta],
                    200,
                    "BETA",
                    vec![raw_kind.to_string()],
                )
            } else if raw_kind == "old_alpha" {
                LifecycleMeta::new(
                    LifecycleChannel::Legacy,
                    vec![LifecycleLabel::OldAlpha],
                    100,
                    "ALPH",
                    vec![raw_kind.to_string()],
                )
            } else if raw_kind == "snapshot" {
                LifecycleMeta::new(
                    LifecycleChannel::Preview,
                    vec![LifecycleLabel::Snapshot],
                    400,
                    "SNAP",
                    vec![raw_kind.to_string()],
                )
            } else if raw_kind == "release" {
                LifecycleMeta::new(
                    LifecycleChannel::Stable,
                    vec![LifecycleLabel::Release],
                    500,
                    "REL",
                    vec![raw_kind.to_string()],
                )
            } else if let Some(stable) = stable_hint {
                if stable {
                    LifecycleMeta::new(
                        LifecycleChannel::Stable,
                        vec![LifecycleLabel::Release],
                        500,
                        "REL",
                        Vec::new(),
                    )
                } else {
                    LifecycleMeta::new(
                        LifecycleChannel::Preview,
                        vec![LifecycleLabel::Unknown],
                        250,
                        "PRE",
                        Vec::new(),
                    )
                }
            } else {
                LifecycleMeta::new(
                    LifecycleChannel::Unknown,
                    vec![LifecycleLabel::Unknown],
                    0,
                    "?",
                    if raw_kind.trim().is_empty() {
                        Vec::new()
                    } else {
                        vec![raw_kind.to_string()]
                    },
                )
            }
        }
    }
}

fn classify_version_family(
    parsed: &ParsedVersionId,
    raw_kind: &str,
    stable_hint: Option<bool>,
) -> &'static str {
    match &parsed.shape {
        VersionShape::OldBeta { .. } => "old_beta",
        VersionShape::OldAlpha { .. } => "old_alpha",
        VersionShape::ReleaseCandidate { .. } => "release_candidate",
        VersionShape::PreRelease { .. } => "pre_release",
        VersionShape::CombatTest { .. } => "combat_test",
        VersionShape::DeepDarkExperimentalSnapshot { .. } => "deep_dark_experimental_snapshot",
        VersionShape::ExperimentalSnapshot { .. } => "experimental_snapshot",
        VersionShape::WeeklySnapshot { is_potato, .. } if *is_potato => "potato_snapshot",
        VersionShape::WeeklySnapshot { .. } => "weekly_snapshot",
        VersionShape::Release { .. } => "release",
        VersionShape::Unknown if raw_kind == "snapshot" || matches!(stable_hint, Some(false)) => {
            "snapshot"
        }
        _ => "release",
    }
}

fn effective_version_for(
    parsed: &ParsedVersionId,
    family: &str,
    release_time: &str,
    releases: &[ReleaseReference],
) -> String {
    if let Some(base) = parsed.shape.base_release() {
        return base.to_string();
    }
    if family == "release" || family == "old_beta" || family == "old_alpha" {
        return parsed.base_id.clone();
    }
    if release_time.trim().is_empty() || releases.is_empty() {
        return String::new();
    }

    releases
        .iter()
        .filter(|entry| entry.release_time.as_str() >= release_time)
        .min_by(|left, right| left.release_time.cmp(&right.release_time))
        .or_else(|| {
            releases
                .iter()
                .filter(|entry| entry.release_time.as_str() <= release_time)
                .max_by(|left, right| left.release_time.cmp(&right.release_time))
        })
        .map(|entry| entry.id.clone())
        .unwrap_or_default()
}

fn display_name_for(parsed: &ParsedVersionId) -> String {
    match &parsed.shape {
        VersionShape::OldBeta { raw } => format!("Beta {}", &raw[1..]),
        VersionShape::OldAlpha { raw } => format!("Alpha {}", &raw[1..]),
        VersionShape::PreRelease { release, .. } => release.clone(),
        VersionShape::ReleaseCandidate { release, .. } => release.clone(),
        VersionShape::CombatTest { release, label } => {
            format!("{release} Combat Test {}", normalize_numeric_label(label))
        }
        VersionShape::DeepDarkExperimentalSnapshot { release, label } => {
            if label.is_empty() {
                format!("{release} Deep Dark Experimental Snapshot")
            } else {
                format!(
                    "{release} Deep Dark Experimental Snapshot {}",
                    normalize_numeric_label(label)
                )
            }
        }
        VersionShape::ExperimentalSnapshot { release, label } => format!(
            "{release} Experimental Snapshot {}",
            normalize_numeric_label(label)
        ),
        _ => parsed.base_id.clone(),
    }
}

fn display_hint_for(parsed: &ParsedVersionId, family: &str, effective_version: &str) -> String {
    let mut hints = Vec::new();

    if uses_estimated_target_release(family)
        && !effective_version.is_empty()
        && effective_version != parsed.base_id
    {
        hints.push(format!("~ {effective_version}"));
    }

    match &parsed.shape {
        VersionShape::PreRelease { label, .. } => {
            hints.push(format!("Pre-release {}", normalize_numeric_label(label)));
        }
        VersionShape::ReleaseCandidate { label, .. } => {
            hints.push(format!("RC {}", normalize_numeric_label(label)));
        }
        _ => {}
    }

    if !parsed.variant_kind.is_empty() {
        hints.push(variant_display_label(&parsed.variant_kind));
    }

    hints.join(" · ")
}

fn uses_estimated_target_release(family: &str) -> bool {
    matches!(family, "weekly_snapshot" | "potato_snapshot" | "snapshot")
}

fn variant_display_label(variant_kind: &str) -> String {
    match variant_kind {
        "unobfuscated" => "Unobfuscated".to_string(),
        "original" => "Original".to_string(),
        other => title_case(other),
    }
}

fn normalize_numeric_label(label: &str) -> String {
    label
        .replace(['I', 'i', 'l', 'L'], "1")
        .replace(['O', 'o'], "0")
}

fn title_case(value: &str) -> String {
    value
        .split(['-', '_', ' '])
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => {
                    first.to_ascii_uppercase().to_string() + &chars.as_str().to_ascii_lowercase()
                }
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn compare_special_shapes(left: &ParsedVersionId, right: &ParsedVersionId) -> Ordering {
    compare_weekly_snapshots(left, right)
        .then_with(|| compare_release_anchored_shapes(left, right))
        .then_with(|| left.variant_kind.cmp(&right.variant_kind))
}

fn compare_weekly_snapshots(left: &ParsedVersionId, right: &ParsedVersionId) -> Ordering {
    match (&left.shape, &right.shape) {
        (
            VersionShape::WeeklySnapshot {
                year: left_year,
                week: left_week,
                channel: left_channel,
                ..
            },
            VersionShape::WeeklySnapshot {
                year: right_year,
                week: right_week,
                channel: right_channel,
                ..
            },
        ) => left_year
            .cmp(right_year)
            .then_with(|| left_week.cmp(right_week))
            .then_with(|| compare_label_like(left_channel, right_channel)),
        _ => Ordering::Equal,
    }
}

fn compare_release_anchored_shapes(left: &ParsedVersionId, right: &ParsedVersionId) -> Ordering {
    let left_anchor = release_anchor(left);
    let right_anchor = release_anchor(right);

    match (left_anchor, right_anchor) {
        (Some(left_anchor), Some(right_anchor)) => {
            compare_numeric_series(&left_anchor, &right_anchor)
                .then_with(|| stage_rank(&left.shape).cmp(&stage_rank(&right.shape)))
                .then_with(|| {
                    compare_label_like(left.shape.stage_label(), right.shape.stage_label())
                })
        }
        _ => Ordering::Equal,
    }
}

fn release_anchor(parsed: &ParsedVersionId) -> Option<Vec<u32>> {
    match &parsed.shape {
        VersionShape::Release { components } => Some(components.clone()),
        VersionShape::PreRelease { release, .. }
        | VersionShape::ReleaseCandidate { release, .. }
        | VersionShape::CombatTest { release, .. }
        | VersionShape::ExperimentalSnapshot { release, .. }
        | VersionShape::DeepDarkExperimentalSnapshot { release, .. } => {
            parse_release_components(release)
        }
        VersionShape::OldBeta { raw } | VersionShape::OldAlpha { raw } => {
            parse_release_components(&raw[1..])
        }
        _ => None,
    }
}

fn stage_rank(shape: &VersionShape) -> i32 {
    match shape {
        VersionShape::OldAlpha { .. } => 0,
        VersionShape::OldBeta { .. } => 1,
        VersionShape::WeeklySnapshot { .. } => 2,
        VersionShape::CombatTest { .. } => 3,
        VersionShape::DeepDarkExperimentalSnapshot { .. } => 4,
        VersionShape::ExperimentalSnapshot { .. } => 5,
        VersionShape::PreRelease { .. } => 6,
        VersionShape::ReleaseCandidate { .. } => 7,
        VersionShape::Release { .. } => 8,
        VersionShape::Unknown => 9,
    }
}

fn compare_tokenized_like(left: &str, right: &str) -> Ordering {
    let left_tokens = tokenize_version_id(left)
        .into_iter()
        .filter(|token| !matches!(token.kind, TokenKind::Separator(_)))
        .collect::<Vec<_>>();
    let right_tokens = tokenize_version_id(right)
        .into_iter()
        .filter(|token| !matches!(token.kind, TokenKind::Separator(_)))
        .collect::<Vec<_>>();

    let len = left_tokens.len().max(right_tokens.len());
    for index in 0..len {
        let Some(left_token) = left_tokens.get(index) else {
            return Ordering::Less;
        };
        let Some(right_token) = right_tokens.get(index) else {
            return Ordering::Greater;
        };

        let ordering = match (&left_token.kind, &right_token.kind) {
            (TokenKind::Number, TokenKind::Number) => {
                let left_num = left_token.normalized.parse::<u32>().unwrap_or(0);
                let right_num = right_token.normalized.parse::<u32>().unwrap_or(0);
                left_num.cmp(&right_num)
            }
            (TokenKind::Word, TokenKind::Word) => {
                left_token.normalized.cmp(&right_token.normalized)
            }
            (TokenKind::Number, TokenKind::Word) => Ordering::Greater,
            (TokenKind::Word, TokenKind::Number) => Ordering::Less,
            _ => Ordering::Equal,
        };

        if ordering != Ordering::Equal {
            return ordering;
        }
    }

    Ordering::Equal
}

fn compare_numeric_series(left: &[u32], right: &[u32]) -> Ordering {
    let len = left.len().max(right.len());
    for index in 0..len {
        let left_value = left.get(index).copied().unwrap_or(0);
        let right_value = right.get(index).copied().unwrap_or(0);
        match left_value.cmp(&right_value) {
            Ordering::Equal => continue,
            ordering => return ordering,
        }
    }
    Ordering::Equal
}

fn compare_label_like(left: &str, right: &str) -> Ordering {
    let left = normalize_numeric_label(left);
    let right = normalize_numeric_label(right);
    compare_tokenized_like(&left, &right)
}

fn parse_release_components(value: &str) -> Option<Vec<u32>> {
    let mut components = Vec::new();
    for part in value.split('.') {
        if part.is_empty() {
            return None;
        }
        components.push(part.parse::<u32>().ok()?);
    }
    if components.is_empty() {
        None
    } else {
        Some(components)
    }
}

fn compare_version_meta_desc(
    left: &MinecraftVersionMeta,
    right: &MinecraftVersionMeta,
) -> Ordering {
    compare_version_like(
        effective_version_or_base(right),
        effective_version_or_base(left),
    )
    .then_with(|| family_priority(&left.family).cmp(&family_priority(&right.family)))
    .then_with(|| compare_version_like(&right.base_id, &left.base_id))
    .then_with(|| variant_priority(&left.variant_kind).cmp(&variant_priority(&right.variant_kind)))
}

fn effective_version_or_base(meta: &MinecraftVersionMeta) -> &str {
    if !meta.effective_version.is_empty() {
        meta.effective_version.as_str()
    } else if !meta.base_id.is_empty() {
        meta.base_id.as_str()
    } else {
        ""
    }
}

fn compare_release_time_desc(left: &str, right: &str) -> Ordering {
    match (left.is_empty(), right.is_empty()) {
        (false, false) if left != right => right.cmp(left),
        (false, true) => Ordering::Less,
        (true, false) => Ordering::Greater,
        _ => Ordering::Equal,
    }
}

fn version_lifecycle_priority(lifecycle: &LifecycleMeta, meta: &MinecraftVersionMeta) -> i32 {
    match lifecycle.channel {
        LifecycleChannel::Stable => 0,
        LifecycleChannel::Preview => {
            if lifecycle.has_label(LifecycleLabel::ReleaseCandidate) {
                1
            } else if lifecycle.has_label(LifecycleLabel::PreRelease) {
                2
            } else {
                3
            }
        }
        LifecycleChannel::Experimental => 4,
        LifecycleChannel::Legacy => {
            if meta.family == "old_beta" {
                5
            } else {
                6
            }
        }
        LifecycleChannel::Unknown => 7,
    }
}

fn family_priority(family: &str) -> i32 {
    match family {
        "release" => 0,
        "release_candidate" => 1,
        "pre_release" => 2,
        "weekly_snapshot" => 3,
        "potato_snapshot" => 4,
        "experimental_snapshot" => 5,
        "deep_dark_experimental_snapshot" => 6,
        "combat_test" => 7,
        "snapshot" => 8,
        "old_beta" => 9,
        "old_alpha" => 10,
        _ => 11,
    }
}

fn variant_priority(variant_kind: &str) -> i32 {
    match variant_kind {
        "" => 0,
        "original" => 1,
        "unobfuscated" => 2,
        _ => 3,
    }
}

#[cfg(test)]
mod tests {
    use super::{ReleaseReference, analyze_minecraft_version, compare_version_like};
    use crate::lifecycle::{LifecycleChannel, LifecycleLabel};

    #[test]
    fn classifies_unobfuscated_release_as_release_with_variant_hint() {
        let analysis = analyze_minecraft_version("1.21.11_unobfuscated", "release", "", None, &[]);
        assert_eq!(analysis.lifecycle.channel, LifecycleChannel::Stable);
        assert_eq!(analysis.lifecycle.labels, vec![LifecycleLabel::Release]);
        assert_eq!(analysis.minecraft_meta.variant_kind, "unobfuscated");
        assert_eq!(analysis.minecraft_meta.variant_of, "1.21.11");
        assert_eq!(analysis.minecraft_meta.display_name, "1.21.11");
        assert_eq!(analysis.minecraft_meta.display_hint, "Unobfuscated");
    }

    #[test]
    fn classifies_snapshot_variants_with_release_estimate_and_variant_hint() {
        let analysis = analyze_minecraft_version(
            "25w46a_unobfuscated",
            "snapshot",
            "2026-01-01T00:00:00+00:00",
            None,
            &[ReleaseReference {
                id: "1.21.11".to_string(),
                release_time: "2026-01-02T00:00:00+00:00".to_string(),
            }],
        );
        assert_eq!(analysis.lifecycle.channel, LifecycleChannel::Preview);
        assert_eq!(analysis.lifecycle.labels, vec![LifecycleLabel::Snapshot]);
        assert_eq!(analysis.minecraft_meta.family, "weekly_snapshot");
        assert_eq!(analysis.minecraft_meta.effective_version, "1.21.11");
        assert_eq!(analysis.minecraft_meta.display_name, "25w46a");
        assert_eq!(
            analysis.minecraft_meta.display_hint,
            "~ 1.21.11 · Unobfuscated"
        );
    }

    #[test]
    fn picks_release_hint_from_timeline_without_collapsing_to_latest_release() {
        let releases = vec![
            ReleaseReference {
                id: "26.1.2".to_string(),
                release_time: "2026-02-01T00:00:00+00:00".to_string(),
            },
            ReleaseReference {
                id: "1.21.11".to_string(),
                release_time: "2025-11-20T00:00:00+00:00".to_string(),
            },
        ];

        let analysis = analyze_minecraft_version(
            "25w46a",
            "snapshot",
            "2025-11-10T00:00:00+00:00",
            None,
            &releases,
        );

        assert_eq!(analysis.minecraft_meta.display_hint, "~ 1.21.11");
    }

    #[test]
    fn humanizes_experimental_snapshot_names() {
        let analysis = analyze_minecraft_version("1.18_experimentaI-snapshot-6", "", "", None, &[]);
        assert_eq!(analysis.lifecycle.channel, LifecycleChannel::Experimental);
        assert_eq!(analysis.minecraft_meta.family, "experimental_snapshot");
        assert_eq!(analysis.minecraft_meta.effective_version, "1.18");
        assert_eq!(
            analysis.minecraft_meta.display_name,
            "1.18 Experimental Snapshot 6"
        );
    }

    #[test]
    fn humanizes_combat_test_names() {
        let analysis = analyze_minecraft_version("1.16_combat-3", "", "", None, &[]);
        assert_eq!(analysis.lifecycle.channel, LifecycleChannel::Experimental);
        assert_eq!(analysis.minecraft_meta.family, "combat_test");
        assert_eq!(analysis.minecraft_meta.effective_version, "1.16");
        assert_eq!(analysis.minecraft_meta.display_name, "1.16 Combat Test 3");
    }

    #[test]
    fn orders_release_candidate_between_release_and_pre_release() {
        assert_eq!(
            compare_version_like("1.21.11", "1.21.11-rc3"),
            std::cmp::Ordering::Greater
        );
        assert_eq!(
            compare_version_like("1.21.11-rc3", "1.21.11-pre5"),
            std::cmp::Ordering::Greater
        );
    }

    #[test]
    fn orders_weekly_snapshots_by_year_and_week() {
        assert_eq!(
            compare_version_like("25w46a", "25w45b"),
            std::cmp::Ordering::Greater
        );
        assert_eq!(
            compare_version_like("26w01a", "25w46a"),
            std::cmp::Ordering::Greater
        );
    }

    #[test]
    fn anchors_special_snapshot_families_to_their_release_line() {
        assert_eq!(
            compare_version_like("1.18_experimentaI-snapshot-6", "1.16_combat-3"),
            std::cmp::Ordering::Greater
        );
    }
}
