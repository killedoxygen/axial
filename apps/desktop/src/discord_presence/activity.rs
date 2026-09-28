use super::snapshot::{ActivityKind, PresenceSnapshot};
use serde_json::{Value, json};

pub(super) fn discord_activity(snapshot: &PresenceSnapshot) -> Value {
    let (kind, image, text) = match snapshot.kind {
        ActivityKind::Idle => (3, "axial_idle", "In the launcher"),
        ActivityKind::Launching => (0, "axial_launching", "Launching Minecraft"),
        ActivityKind::Playing => (0, "axial_minecraft", "Minecraft running"),
        ActivityKind::Multi => (0, "axial_multi", "Multiple sessions"),
    };
    let mut activity = json!({
        "type": kind,
        "details": snapshot.details,
        "state": snapshot.state,
        "assets": {
            "large_image": "axial", "large_text": "Axial Launcher",
            "small_image": image, "small_text": text,
        },
    });
    if snapshot.kind != ActivityKind::Idle
        && let Some(start) = snapshot.started_at
    {
        activity["timestamps"] = json!({ "start": start });
    }
    if snapshot.kind == ActivityKind::Multi {
        activity["party"] = json!({ "id": "axial-active-sessions", "size": [snapshot.active_count, snapshot.active_count] });
    }
    activity
}

#[cfg(test)]
mod tests {
    use super::super::snapshot::{
        PresenceLoader, PresencePerformance, PresencePhase, PresenceSession,
    };
    use super::*;

    #[test]
    fn idle_activity_uses_watching_without_timestamps_or_session_fields() {
        let activity = discord_activity(&PresenceSnapshot::from_sessions(true, &[]));
        assert_eq!(
            activity,
            json!({
                "type": 3,
                "details": "Minecraft launcher",
                "state": "Organizing instances",
                "assets": { "large_image": "axial", "large_text": "Axial Launcher", "small_image": "axial_idle", "small_text": "In the launcher" },
            })
        );
    }

    #[test]
    fn multiple_sessions_have_count_but_no_join_secrets_or_identifying_fields() {
        let session = PresenceSession {
            phase: PresencePhase::Playing,
            loader: PresenceLoader::Vanilla,
            minecraft_version: Some("1.21".into()),
            performance: PresencePerformance::Vanilla,
            started_at_ms: Some(1000),
        };
        let activity = discord_activity(&PresenceSnapshot::from_sessions(
            true,
            &[session.clone(), session],
        ));
        assert_eq!(
            activity["party"],
            json!({ "id": "axial-active-sessions", "size": [2, 2] })
        );
        assert_eq!(activity["timestamps"], json!({ "start": 1 }));
        for forbidden in ["secrets", "buttons", "url", "instance", "account", "server"] {
            assert!(activity.get(forbidden).is_none());
        }
    }
}
