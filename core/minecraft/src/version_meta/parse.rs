use super::tokenize::{TokenKind, tokenize_version_id};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParsedVersionId {
    pub base_id: String,
    pub variant_kind: String,
    pub shape: VersionShape,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum VersionShape {
    OldBeta {
        raw: String,
    },
    OldAlpha {
        raw: String,
    },
    Release {
        components: Vec<u32>,
    },
    PreRelease {
        release: String,
        label: String,
    },
    ReleaseCandidate {
        release: String,
        label: String,
    },
    WeeklySnapshot {
        year: u32,
        week: u32,
        channel: String,
        is_potato: bool,
    },
    CombatTest {
        release: String,
        label: String,
    },
    ExperimentalSnapshot {
        release: String,
        label: String,
    },
    DeepDarkExperimentalSnapshot {
        release: String,
        label: String,
    },
    Unknown,
}

impl VersionShape {
    pub(crate) fn is_snapshot_like(&self) -> bool {
        matches!(
            self,
            Self::PreRelease { .. }
                | Self::ReleaseCandidate { .. }
                | Self::WeeklySnapshot { .. }
                | Self::CombatTest { .. }
                | Self::ExperimentalSnapshot { .. }
                | Self::DeepDarkExperimentalSnapshot { .. }
        )
    }

    pub(crate) fn base_release(&self) -> Option<&str> {
        match self {
            Self::PreRelease { release, .. }
            | Self::ReleaseCandidate { release, .. }
            | Self::CombatTest { release, .. }
            | Self::ExperimentalSnapshot { release, .. }
            | Self::DeepDarkExperimentalSnapshot { release, .. } => Some(release.as_str()),
            _ => None,
        }
    }

    pub(crate) fn stage_label(&self) -> &str {
        match self {
            Self::PreRelease { label, .. }
            | Self::ReleaseCandidate { label, .. }
            | Self::CombatTest { label, .. }
            | Self::ExperimentalSnapshot { label, .. }
            | Self::DeepDarkExperimentalSnapshot { label, .. } => label.as_str(),
            Self::WeeklySnapshot { channel, .. } => channel.as_str(),
            _ => "",
        }
    }
}

pub(crate) fn parse_version_id(raw_id: &str) -> ParsedVersionId {
    let (base_id, variant_kind) = strip_variant_suffix(raw_id);
    let shape = parse_shape(&base_id);
    ParsedVersionId {
        base_id,
        variant_kind,
        shape,
    }
}

fn parse_shape(base_id: &str) -> VersionShape {
    parse_old_channel(base_id)
        .or_else(|| parse_release_stage(base_id))
        .or_else(|| parse_special_snapshot_family(base_id))
        .or_else(|| parse_weekly_snapshot(base_id))
        .or_else(|| parse_release(base_id).map(|components| VersionShape::Release { components }))
        .unwrap_or(VersionShape::Unknown)
}

fn parse_old_channel(base_id: &str) -> Option<VersionShape> {
    if base_id.len() > 1 && base_id.starts_with('b') {
        return Some(VersionShape::OldBeta {
            raw: base_id.to_string(),
        });
    }
    if base_id.len() > 1 && base_id.starts_with('a') {
        return Some(VersionShape::OldAlpha {
            raw: base_id.to_string(),
        });
    }
    None
}

fn parse_release_stage(base_id: &str) -> Option<VersionShape> {
    let tokens = tokenize_version_id(base_id);
    let dash_index = tokens
        .iter()
        .position(|token| matches!(token.kind, TokenKind::Separator('-')))?;
    let release = reconstruct_release(&tokens[..dash_index])?;
    let stage = parse_stage_marker(&tokens[dash_index + 1..])?;
    match stage.kind {
        StageKind::PreRelease => Some(VersionShape::PreRelease {
            release,
            label: stage.label,
        }),
        StageKind::ReleaseCandidate => Some(VersionShape::ReleaseCandidate {
            release,
            label: stage.label,
        }),
    }
}

fn parse_special_snapshot_family(base_id: &str) -> Option<VersionShape> {
    parse_combat_test(base_id)
        .or_else(|| parse_deep_dark_snapshot(base_id))
        .or_else(|| parse_experimental_snapshot(base_id))
}

fn parse_combat_test(base_id: &str) -> Option<VersionShape> {
    let tokens = tokenize_version_id(base_id);
    let underscore_index = tokens
        .iter()
        .position(|token| matches!(token.kind, TokenKind::Separator('_')))?;
    let release = reconstruct_release(&tokens[..underscore_index])?;
    let rest = &tokens[underscore_index + 1..];
    if rest.len() < 3 {
        return None;
    }
    if !matches_word(rest.first()?, "combat") || !matches!(rest[1].kind, TokenKind::Separator('-'))
    {
        return None;
    }
    let label = collect_compact_label(&rest[2..]);
    if label.is_empty() {
        return None;
    }
    Some(VersionShape::CombatTest { release, label })
}

fn parse_deep_dark_snapshot(base_id: &str) -> Option<VersionShape> {
    let tokens = tokenize_version_id(base_id);
    let underscore_index = tokens
        .iter()
        .position(|token| matches!(token.kind, TokenKind::Separator('_')))?;
    let release = reconstruct_release(&tokens[..underscore_index])?;
    let rest = &tokens[underscore_index + 1..];
    let expected = [
        ExpectedPart::Word("deep"),
        ExpectedPart::Separator('_'),
        ExpectedPart::Word("dark"),
        ExpectedPart::Separator('_'),
        ExpectedPart::Word("experimental"),
        ExpectedPart::Separator('_'),
        ExpectedPart::Word("snapshot"),
    ];
    let consumed = consume_prefix(rest, &expected)?;
    let label = if consumed < rest.len() && matches!(rest[consumed].kind, TokenKind::Separator('-'))
    {
        collect_compact_label(&rest[consumed + 1..])
    } else {
        String::new()
    };

    Some(VersionShape::DeepDarkExperimentalSnapshot { release, label })
}

fn parse_experimental_snapshot(base_id: &str) -> Option<VersionShape> {
    let tokens = tokenize_version_id(base_id);
    let underscore_index = tokens
        .iter()
        .position(|token| matches!(token.kind, TokenKind::Separator('_')))?;
    let release = reconstruct_release(&tokens[..underscore_index])?;
    let rest = &tokens[underscore_index + 1..];
    if rest.len() < 4 {
        return None;
    }
    if !matches_experimental_word(rest.first()?)
        || !matches!(rest[1].kind, TokenKind::Separator('-'))
        || !matches_word(rest.get(2)?, "snapshot")
        || !matches!(rest[3].kind, TokenKind::Separator('-'))
    {
        return None;
    }
    let label = collect_compact_label(&rest[4..]);
    if label.is_empty() {
        return None;
    }
    Some(VersionShape::ExperimentalSnapshot { release, label })
}

fn parse_weekly_snapshot(base_id: &str) -> Option<VersionShape> {
    let tokens = tokenize_version_id(base_id);
    if tokens.len() < 4 {
        return None;
    }
    let year = parse_fixed_number(tokens.first()?, 2)?;
    if !matches_word(tokens.get(1)?, "w") {
        return None;
    }
    let week = parse_fixed_number(tokens.get(2)?, 2)?;
    let channel = collect_compact_label(&tokens[3..]);
    if channel.is_empty() {
        return None;
    }
    let is_potato = channel.to_ascii_lowercase().contains("potato");
    Some(VersionShape::WeeklySnapshot {
        year,
        week,
        channel,
        is_potato,
    })
}

fn parse_release(base_id: &str) -> Option<Vec<u32>> {
    let tokens = tokenize_version_id(base_id);
    reconstruct_release(&tokens).and_then(|release| {
        release
            .split('.')
            .map(|part| part.parse::<u32>().ok())
            .collect::<Option<Vec<_>>>()
    })
}

fn strip_variant_suffix(raw_id: &str) -> (String, String) {
    for suffix in ["_unobfuscated", "_original"] {
        if let Some(stripped) = raw_id.strip_suffix(suffix) {
            return (
                stripped.to_string(),
                suffix.trim_start_matches('_').to_string(),
            );
        }
    }
    (raw_id.to_string(), String::new())
}

fn reconstruct_release(tokens: &[super::tokenize::VersionToken]) -> Option<String> {
    if tokens.is_empty() {
        return None;
    }
    let mut release = String::new();
    let mut expect_number = true;
    for token in tokens {
        match (&token.kind, expect_number) {
            (TokenKind::Number, true) => {
                release.push_str(&token.raw);
                expect_number = false;
            }
            (TokenKind::Separator('.'), false) => {
                release.push('.');
                expect_number = true;
            }
            _ => return None,
        }
    }
    if expect_number { None } else { Some(release) }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StageKind {
    PreRelease,
    ReleaseCandidate,
}

struct ParsedStage {
    kind: StageKind,
    label: String,
}

fn parse_stage_marker(tokens: &[super::tokenize::VersionToken]) -> Option<ParsedStage> {
    let first = tokens.first()?;
    let word = match &first.kind {
        TokenKind::Word => first.normalized.as_str(),
        _ => return None,
    };

    if let Some(label) = word.strip_prefix("pre") {
        let suffix = if label.is_empty() {
            collect_compact_label(&tokens[1..])
        } else {
            label.to_string()
        };
        return (!suffix.is_empty()).then_some(ParsedStage {
            kind: StageKind::PreRelease,
            label: suffix,
        });
    }

    if let Some(label) = word.strip_prefix("rc") {
        let suffix = if label.is_empty() {
            collect_compact_label(&tokens[1..])
        } else {
            label.to_string()
        };
        return (!suffix.is_empty()).then_some(ParsedStage {
            kind: StageKind::ReleaseCandidate,
            label: suffix,
        });
    }

    None
}

fn collect_compact_label(tokens: &[super::tokenize::VersionToken]) -> String {
    tokens
        .iter()
        .filter(|token| !matches!(token.kind, TokenKind::Separator(_)))
        .map(|token| token.raw.as_str())
        .collect::<String>()
}

fn parse_fixed_number(token: &super::tokenize::VersionToken, width: usize) -> Option<u32> {
    if !matches!(token.kind, TokenKind::Number) || token.raw.len() != width {
        return None;
    }
    token.raw.parse::<u32>().ok()
}

fn matches_word(token: &super::tokenize::VersionToken, expected: &str) -> bool {
    matches!(token.kind, TokenKind::Word) && token.normalized == expected
}

fn matches_experimental_word(token: &super::tokenize::VersionToken) -> bool {
    if !matches!(token.kind, TokenKind::Word) {
        return false;
    }
    matches!(
        token.normalized.as_str(),
        "experimental" | "experimenta1" | "experimentai"
    )
}

enum ExpectedPart<'a> {
    Word(&'a str),
    Separator(char),
}

fn consume_prefix(
    tokens: &[super::tokenize::VersionToken],
    expected: &[ExpectedPart<'_>],
) -> Option<usize> {
    if tokens.len() < expected.len() {
        return None;
    }
    for (index, part) in expected.iter().enumerate() {
        let token = tokens.get(index)?;
        match part {
            ExpectedPart::Word(value) if !matches_word(token, value) => return None,
            ExpectedPart::Separator(value) if !matches!(token.kind, TokenKind::Separator(current) if current == *value) =>
            {
                return None;
            }
            _ => {}
        }
    }
    Some(expected.len())
}
