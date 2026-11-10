//! The only Discord-visible projection. It accepts no account, instance name,
//! server, path or command fields; callers cannot inject activity text.
use serde_json::{Value, json};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Launching,
    Playing,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Loader {
    Vanilla,
    Fabric,
    Quilt,
    Forge,
    NeoForge,
    Modded,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Performance {
    Managed,
    Vanilla,
    Custom,
    Unknown,
}

/// Supply only active sessions. Metadata must come from the registered instance.
#[derive(Clone, Debug)]
pub struct Session {
    pub phase: Phase,
    pub loader: Loader,
    pub minecraft_version: Option<String>,
    pub performance: Performance,
    pub started_at_ms: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Idle,
    Launching,
    Playing,
    Multi,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub enabled: bool,
    kind: Kind,
    details: &'static str,
    state: String,
    active_count: usize,
    started_at: Option<u64>,
}

impl Snapshot {
    pub fn from_sessions(enabled: bool, active: &[Session]) -> Self {
        if !enabled || active.is_empty() {
            return Self {
                enabled,
                kind: Kind::Idle,
                details: "Minecraft launcher",
                state: "Organizing instances".into(),
                active_count: 0,
                started_at: None,
            };
        }
        let playing = active.iter().any(|session| session.phase == Phase::Playing);
        let (kind, details, state) = match (active, playing) {
            ([single], true) => (Kind::Playing, "Minecraft is running", summary(single)),
            ([single], false) => (Kind::Launching, "Starting Minecraft", summary(single)),
            (_, true) => (
                Kind::Multi,
                "Multiple Minecraft sessions",
                format!("{} instances active", active.len()),
            ),
            (_, false) => (
                Kind::Multi,
                "Starting Minecraft sessions",
                format!("{} instances launching", active.len()),
            ),
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

    pub fn activity(&self) -> Value {
        let (kind, image, text) = match self.kind {
            Kind::Idle => (3, "axial_idle", "In the launcher"),
            Kind::Launching => (0, "axial_launching", "Launching Minecraft"),
            Kind::Playing => (0, "axial_minecraft", "Minecraft running"),
            Kind::Multi => (0, "axial_multi", "Multiple sessions"),
        };
        let mut activity = json!({
            "type": kind,
            "details": self.details,
            "state": self.state,
            "assets": {
                "large_image": "axial", "large_text": "Axial Launcher",
                "small_image": image, "small_text": text,
            },
        });
        if self.kind != Kind::Idle
            && let Some(start) = self.started_at
        {
            activity["timestamps"] = json!({ "start": start });
        }
        if self.kind == Kind::Multi {
            activity["party"] = json!({ "id": "axial-active-sessions", "size": [self.active_count, self.active_count] });
        }
        activity
    }
}

fn summary(session: &Session) -> String {
    let loader = match session.loader {
        Loader::Vanilla => "Vanilla",
        Loader::Fabric => "Fabric",
        Loader::Quilt => "Quilt",
        Loader::Forge => "Forge",
        Loader::NeoForge => "NeoForge",
        Loader::Modded => "Modded",
    };
    let version = session
        .minecraft_version
        .as_deref()
        .filter(|value| public_version_label(value));
    let base = match (version, session.loader) {
        (Some(version), _) => format!("{loader} {version}"),
        (None, Loader::Vanilla) => "Custom version".into(),
        (None, _) => format!("{loader} Minecraft"),
    };
    match session.performance {
        Performance::Managed => format!("{base} - Managed"),
        Performance::Vanilla => format!("{base} - Vanilla"),
        Performance::Custom => format!("{base} - Custom"),
        Performance::Unknown => base,
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
    let version = value.strip_prefix(['a', 'b', 'c']).unwrap_or(&value);
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

    fn playing() -> Session {
        Session {
            phase: Phase::Playing,
            loader: Loader::Fabric,
            minecraft_version: Some("1.21.1".into()),
            performance: Performance::Managed,
            started_at_ms: Some(1_781_350_000_000),
        }
    }

    #[test]
    fn idle_and_disabled_activity_carries_no_session_details() {
        let expected = json!({
            "type": 3,
            "details": "Minecraft launcher",
            "state": "Organizing instances",
            "assets": { "large_image": "axial", "large_text": "Axial Launcher", "small_image": "axial_idle", "small_text": "In the launcher" },
        });
        assert_eq!(Snapshot::from_sessions(true, &[]).activity(), expected);
        let disabled = Snapshot::from_sessions(false, &[playing()]);
        assert!(!disabled.enabled);
        assert_eq!(disabled.activity(), expected);
    }

    #[test]
    fn single_session_retains_loader_mode_and_start_time() {
        let mut session = playing();
        let activity = Snapshot::from_sessions(true, &[session.clone()]).activity();
        assert_eq!(activity["details"], "Minecraft is running");
        assert_eq!(activity["state"], "Fabric 1.21.1 - Managed");
        assert_eq!(activity["timestamps"], json!({ "start": 1_781_350_000 }));
        session.phase = Phase::Launching;
        assert_eq!(
            Snapshot::from_sessions(true, &[session]).activity()["details"],
            "Starting Minecraft"
        );
    }

    #[test]
    fn multiple_sessions_publish_only_count_and_earliest_start() {
        let first = playing();
        let mut second = first.clone();
        second.started_at_ms = Some(1_781_349_000_000);
        second.phase = Phase::Launching;
        let snapshot = Snapshot::from_sessions(true, &[first.clone(), second.clone()]);
        assert_eq!(snapshot, Snapshot::from_sessions(true, &[second, first]));
        let activity = snapshot.activity();
        assert_eq!(activity["state"], "2 instances active");
        assert_eq!(activity["timestamps"], json!({ "start": 1_781_349_000 }));
        assert_eq!(
            activity["party"],
            json!({ "id": "axial-active-sessions", "size": [2, 2] })
        );
        for forbidden in ["secrets", "buttons", "url", "instance", "account", "server"] {
            assert!(activity.get(forbidden).is_none());
        }
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
                Snapshot::from_sessions(true, &[session]).state,
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
