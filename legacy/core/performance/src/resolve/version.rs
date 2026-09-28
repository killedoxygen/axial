use regex::Regex;
use std::sync::OnceLock;

pub(super) struct MCVersion {
    pub(super) major: i32,
    pub(super) minor: i32,
    pub(super) patch: i32,
    pub(super) is_snapshot: bool,
    pub(super) raw: String,
}

pub(super) fn parse_version(value: &str) -> Result<MCVersion, ()> {
    static RELEASE_PATTERN: OnceLock<Result<Regex, regex::Error>> = OnceLock::new();
    static SNAPSHOT_PATTERN: OnceLock<Result<Regex, regex::Error>> = OnceLock::new();

    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(());
    }

    let snapshot_pattern = SNAPSHOT_PATTERN
        .get_or_init(|| Regex::new(r"^\d+w\d+[a-z]$"))
        .as_ref()
        .map_err(|_| ())?;
    let snapshot = snapshot_pattern.is_match(&trimmed.to_lowercase());
    if snapshot {
        return Ok(MCVersion {
            major: 0,
            minor: 0,
            patch: 0,
            is_snapshot: true,
            raw: trimmed.to_string(),
        });
    }

    let release_pattern = RELEASE_PATTERN
        .get_or_init(|| Regex::new(r"^(\d+)\.(\d+)(?:\.(\d+))?$"))
        .as_ref()
        .map_err(|_| ())?;
    let captures = release_pattern.captures(trimmed).ok_or(())?;

    Ok(MCVersion {
        major: captures
            .get(1)
            .and_then(|value| value.as_str().parse::<i32>().ok())
            .ok_or(())?,
        minor: captures
            .get(2)
            .and_then(|value| value.as_str().parse::<i32>().ok())
            .ok_or(())?,
        patch: captures
            .get(3)
            .and_then(|value| value.as_str().parse::<i32>().ok())
            .unwrap_or(0),
        is_snapshot: false,
        raw: trimmed.to_string(),
    })
}

pub(super) fn compare_release_version(
    version: &MCVersion,
    major: i32,
    minor: i32,
    patch: i32,
) -> i32 {
    compare_versions(
        version,
        &MCVersion {
            major,
            minor,
            patch,
            is_snapshot: false,
            raw: String::new(),
        },
    )
}

pub(super) fn compare_versions(left: &MCVersion, right: &MCVersion) -> i32 {
    if left.is_snapshot && !right.is_snapshot {
        return 1;
    }
    if !left.is_snapshot && right.is_snapshot {
        return -1;
    }
    if left.is_snapshot && right.is_snapshot {
        return match left.raw.to_lowercase().cmp(&right.raw.to_lowercase()) {
            std::cmp::Ordering::Less => -1,
            std::cmp::Ordering::Equal => 0,
            std::cmp::Ordering::Greater => 1,
        };
    }
    for ordering in [
        left.major.cmp(&right.major),
        left.minor.cmp(&right.minor),
        left.patch.cmp(&right.patch),
    ] {
        if ordering.is_lt() {
            return -1;
        }
        if ordering.is_gt() {
            return 1;
        }
    }
    0
}

pub(super) fn version_in_range(version: &MCVersion, range: &str) -> bool {
    let trimmed = range.trim();
    if trimmed.is_empty() {
        return true;
    }
    for condition in trimmed.split_whitespace() {
        let (operator, raw_target) = split_range_condition(condition);
        let Ok(target) = parse_version(raw_target) else {
            return false;
        };
        let compare = compare_versions(version, &target);
        let matches = match operator {
            ">" => compare > 0,
            ">=" => compare >= 0,
            "<" => compare < 0,
            "<=" => compare <= 0,
            "=" => compare == 0,
            _ => false,
        };
        if !matches {
            return false;
        }
    }
    true
}

pub(super) fn split_range_condition(condition: &str) -> (&str, &str) {
    for operator in [">=", "<=", ">", "<", "="] {
        if let Some(rest) = condition.strip_prefix(operator) {
            return (operator, rest.trim());
        }
    }
    ("=", condition)
}
