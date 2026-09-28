//! This projection accepts no account, instance name, server, path, or command fields.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PresencePhase {
    Launching,
    Playing,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PresenceLoader {
    Vanilla,
    Fabric,
    Quilt,
    Forge,
    NeoForge,
    Modded,
}

impl PresenceLoader {
    fn label(self) -> &'static str {
        match self {
            Self::Vanilla => "Vanilla",
            Self::Fabric => "Fabric",
            Self::Quilt => "Quilt",
            Self::Forge => "Forge",
            Self::NeoForge => "NeoForge",
            Self::Modded => "Modded",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PresencePerformance {
    Managed,
    Vanilla,
    Custom,
    Unknown,
}

impl PresencePerformance {
    fn label(self) -> Option<&'static str> {
        match self {
            Self::Managed => Some("Managed"),
            Self::Vanilla => Some("Vanilla"),
            Self::Custom => Some("Custom"),
            Self::Unknown => None,
        }
    }
}

/// Supply only active sessions. Metadata must come from the registered instance.
#[derive(Clone, Debug)]
pub struct PresenceSession {
    pub phase: PresencePhase,
    pub loader: PresenceLoader,
    pub minecraft_version: Option<String>,
    pub performance: PresencePerformance,
    pub started_at_ms: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ActivityKind {
    Idle,
    Launching,
    Playing,
    Multi,
}

/// Fields are private so callers cannot inject arbitrary Discord activity text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PresenceSnapshot {
    pub(super) enabled: bool,
    pub(super) kind: ActivityKind,
    pub(super) details: &'static str,
    pub(super) state: String,
    pub(super) active_count: usize,
    pub(super) started_at: Option<u64>,
}

impl PresenceSnapshot {
    pub fn from_sessions(enabled: bool, active: &[PresenceSession]) -> Self {
        if !enabled || active.is_empty() {
            return Self {
                enabled,
                kind: ActivityKind::Idle,
                details: "Minecraft launcher",
                state: "Organizing instances".into(),
                active_count: 0,
                started_at: None,
            };
        }
        let playing = active
            .iter()
            .any(|session| session.phase == PresencePhase::Playing);
        let (kind, details, state) = if active.len() > 1 {
            (
                ActivityKind::Multi,
                if playing {
                    "Multiple Minecraft sessions"
                } else {
                    "Starting Minecraft sessions"
                },
                format!(
                    "{} instances {}",
                    active.len(),
                    if playing { "active" } else { "launching" }
                ),
            )
        } else {
            (
                if playing {
                    ActivityKind::Playing
                } else {
                    ActivityKind::Launching
                },
                if playing {
                    "Minecraft is running"
                } else {
                    "Starting Minecraft"
                },
                session_summary(&active[0]),
            )
        };
        Self {
            enabled,
            kind,
            details,
            state,
            active_count: active.len(),
            started_at: active
                .iter()
                .filter_map(|session| session.started_at_ms)
                .min()
                .map(|ms| ms / 1000),
        }
    }
}

fn session_summary(session: &PresenceSession) -> String {
    let version = session
        .minecraft_version
        .as_deref()
        .filter(|value| public_version_label(value));
    let base = match (version, session.loader) {
        (Some(version), loader) => format!("{} {version}", loader.label()),
        (None, PresenceLoader::Vanilla) => "Custom version".into(),
        (None, loader) => format!("{} Minecraft", loader.label()),
    };
    match session.performance.label() {
        Some(mode) => format!("{base} - {mode}"),
        None => base,
    }
}

/// Match complete public version grammars; accepting a prefix can leak private
/// names such as `1.21 experimental alice@private-server.example`.
fn public_version_label(raw: &str) -> bool {
    if raw.len() > 80 || raw.trim() != raw || !raw.is_ascii() {
        return false;
    }
    let value = raw.to_ascii_lowercase();
    let bytes = value.as_bytes();
    if bytes.len() == 6
        && bytes[..2].iter().all(u8::is_ascii_digit)
        && bytes[2] == b'w'
        && bytes[3..5].iter().all(u8::is_ascii_digit)
        && bytes[5].is_ascii_lowercase()
    {
        return true;
    }
    if let Some(rest) = value
        .strip_prefix("rd-")
        .or_else(|| value.strip_prefix("rd "))
    {
        return numeric(rest);
    }
    let version = value
        .strip_prefix('a')
        .or_else(|| value.strip_prefix('b'))
        .or_else(|| value.strip_prefix('c'))
        .unwrap_or(&value);
    let split = version
        .find(|ch: char| !ch.is_ascii_digit() && ch != '.')
        .unwrap_or(version.len());
    let (base, suffix) = version.split_at(split);
    let mut parts = base.split('.');
    if !parts.next().is_some_and(numeric)
        || !parts.next().is_some_and(numeric)
        || !parts.all(numeric)
    {
        return false;
    }
    if suffix.is_empty() {
        return true;
    }
    // Old Alpha/Beta identifiers may carry a revision letter or numeric patch.
    if version != value
        && (suffix.len() == 1 && suffix.as_bytes()[0].is_ascii_lowercase()
            || suffix.strip_prefix('_').is_some_and(numeric))
    {
        return true;
    }
    [
        "-pre",
        "-rc",
        " pre-release ",
        " release candidate ",
        " snapshot ",
        " combat test ",
        " experimental snapshot ",
        " deep dark experimental snapshot ",
    ]
    .iter()
    .any(|prefix| suffix.strip_prefix(prefix).is_some_and(numeric))
}

fn numeric(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn playing() -> PresenceSession {
        PresenceSession {
            phase: PresencePhase::Playing,
            loader: PresenceLoader::Fabric,
            minecraft_version: Some("1.21.1".into()),
            performance: PresencePerformance::Managed,
            started_at_ms: Some(1_781_350_000_000),
        }
    }

    #[test]
    fn disabled_presence_contains_no_active_session_details() {
        let snapshot = PresenceSnapshot::from_sessions(false, &[playing()]);
        assert!(!snapshot.enabled);
        assert_eq!(snapshot.kind, ActivityKind::Idle);
        assert_eq!(snapshot.state, "Organizing instances");
        assert_eq!(snapshot.started_at, None);
    }

    #[test]
    fn single_session_retains_loader_mode_and_start_time() {
        let mut session = playing();
        let snapshot = PresenceSnapshot::from_sessions(true, &[session.clone()]);
        assert_eq!(snapshot.kind, ActivityKind::Playing);
        assert_eq!(snapshot.details, "Minecraft is running");
        assert_eq!(snapshot.state, "Fabric 1.21.1 - Managed");
        assert_eq!(snapshot.started_at, Some(1_781_350_000));
        session.phase = PresencePhase::Launching;
        assert_eq!(
            PresenceSnapshot::from_sessions(true, &[session]).details,
            "Starting Minecraft"
        );
    }

    #[test]
    fn multiple_sessions_publish_only_count_and_earliest_start() {
        let first = playing();
        let mut second = first.clone();
        second.started_at_ms = Some(1_781_349_000_000);
        second.phase = PresencePhase::Launching;
        let snapshot = PresenceSnapshot::from_sessions(true, &[first.clone(), second.clone()]);
        assert_eq!(
            snapshot,
            PresenceSnapshot::from_sessions(true, &[second, first])
        );
        assert_eq!(snapshot.kind, ActivityKind::Multi);
        assert_eq!(snapshot.state, "2 instances active");
        assert_eq!(snapshot.started_at, Some(1_781_349_000));
    }

    #[test]
    fn private_versions_and_public_looking_prefixes_never_escape() {
        for private in [
            "Private Modpack 1.21.1",
            "1.21 experimental alice@example.com",
            "a1.secret",
            "rd-private-server",
            "1.21\nsecret",
            "/Users/alice/1.21",
            "1.21 - bearer abc",
            "b1.8_alice",
        ] {
            let mut session = playing();
            session.minecraft_version = Some(private.into());
            assert_eq!(
                PresenceSnapshot::from_sessions(true, &[session]).state,
                "Fabric Minecraft - Managed"
            );
        }
    }

    #[test]
    fn public_release_snapshot_and_historic_labels_remain_visible() {
        for label in [
            "1.21.1",
            "1.21-pre1",
            "1.21-rc1",
            "1.21 Pre-Release 1",
            "1.21 Release Candidate 1",
            "24w14a",
            "a1.2.6",
            "b1.7.3",
            "c0.30",
            "rd-132211",
            "a1.0.4_01",
            "b1.8.1",
            "1.19 Deep Dark Experimental Snapshot 1",
        ] {
            assert!(public_version_label(label), "{label}");
        }
    }
}
