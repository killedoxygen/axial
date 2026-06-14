use croopor_api::state::presence::{PresenceActivityKind, PresenceSnapshot};
use serde_json::{Value, json};

const DISCORD_ASSET_KEY: &str = "croopor";
const DISCORD_ASSET_TEXT: &str = "Croopor Launcher";
const DISCORD_IDLE_ASSET_KEY: &str = "croopor_idle";
const DISCORD_LAUNCHING_ASSET_KEY: &str = "croopor_launching";
const DISCORD_MINECRAFT_ASSET_KEY: &str = "croopor_minecraft";
const DISCORD_MULTI_ASSET_KEY: &str = "croopor_multi";
const ACTIVITY_TYPE_PLAYING: u8 = 0;
const ACTIVITY_TYPE_WATCHING: u8 = 3;

pub(super) fn discord_activity(snapshot: &PresenceSnapshot) -> Value {
    let activity = &snapshot.activity;
    let mut value = json!({
        "type": activity_type(activity.kind),
        "details": activity.details,
        "state": activity.state,
        "assets": {
            "large_image": DISCORD_ASSET_KEY,
            "large_text": DISCORD_ASSET_TEXT,
        },
    });

    let (small_image, small_text) = small_asset(activity.kind);
    value["assets"]["small_image"] = json!(small_image);
    value["assets"]["small_text"] = json!(small_text);

    if activity.kind != PresenceActivityKind::Idle
        && let Some(start) = activity.started_at_unix_seconds
    {
        value["timestamps"] = json!({ "start": start });
    }

    if activity.kind == PresenceActivityKind::Multi {
        value["party"] = json!({
            "id": "croopor-active-sessions",
            "size": [
                activity.active_count,
                activity.active_count,
            ],
        });
    }

    value
}

fn activity_type(kind: PresenceActivityKind) -> u8 {
    match kind {
        PresenceActivityKind::Idle => ACTIVITY_TYPE_WATCHING,
        PresenceActivityKind::Launching
        | PresenceActivityKind::Playing
        | PresenceActivityKind::Multi => ACTIVITY_TYPE_PLAYING,
    }
}

fn small_asset(kind: PresenceActivityKind) -> (&'static str, &'static str) {
    match kind {
        PresenceActivityKind::Idle => (DISCORD_IDLE_ASSET_KEY, "In the launcher"),
        PresenceActivityKind::Launching => (DISCORD_LAUNCHING_ASSET_KEY, "Launching Minecraft"),
        PresenceActivityKind::Playing => (DISCORD_MINECRAFT_ASSET_KEY, "Minecraft running"),
        PresenceActivityKind::Multi => (DISCORD_MULTI_ASSET_KEY, "Multiple sessions"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use croopor_api::state::presence::{PresenceActivity, PresenceActivityKind};

    #[test]
    fn activity_payload_uses_croopor_asset_and_timestamp_for_gameplay() {
        let snapshot = PresenceSnapshot {
            enabled: true,
            activity: PresenceActivity {
                kind: PresenceActivityKind::Playing,
                details: "Minecraft is running".to_string(),
                state: "Fabric 1.21.1 - Managed".to_string(),
                active_count: 1,
                started_at_unix_seconds: Some(1_781_350_000),
            },
        };

        let activity = discord_activity(&snapshot);

        assert_eq!(activity["type"], ACTIVITY_TYPE_PLAYING);
        assert_eq!(activity["details"], "Minecraft is running");
        assert_eq!(activity["state"], "Fabric 1.21.1 - Managed");
        assert_eq!(activity["assets"]["large_image"], DISCORD_ASSET_KEY);
        assert_eq!(activity["assets"]["large_text"], DISCORD_ASSET_TEXT);
        assert_eq!(
            activity["assets"]["small_image"],
            DISCORD_MINECRAFT_ASSET_KEY
        );
        assert_eq!(activity["assets"]["small_text"], "Minecraft running");
        assert_eq!(activity["timestamps"]["start"], 1_781_350_000);
    }

    #[test]
    fn idle_activity_has_no_elapsed_timer() {
        let snapshot = PresenceSnapshot {
            enabled: true,
            activity: PresenceActivity {
                kind: PresenceActivityKind::Idle,
                details: "Minecraft launcher".to_string(),
                state: "Organizing instances".to_string(),
                active_count: 0,
                started_at_unix_seconds: Some(1_781_350_000),
            },
        };

        let activity = discord_activity(&snapshot);

        assert_eq!(activity["details"], "Minecraft launcher");
        assert_eq!(activity["type"], ACTIVITY_TYPE_WATCHING);
        assert_eq!(activity["assets"]["small_image"], DISCORD_IDLE_ASSET_KEY);
        assert_eq!(activity["assets"]["small_text"], "In the launcher");
        assert!(activity.get("timestamps").is_none());
    }

    #[test]
    fn multi_activity_uses_party_count_without_join_buttons() {
        let snapshot = PresenceSnapshot {
            enabled: true,
            activity: PresenceActivity {
                kind: PresenceActivityKind::Multi,
                details: "Multiple Minecraft sessions".to_string(),
                state: "2 instances active".to_string(),
                active_count: 2,
                started_at_unix_seconds: Some(1_781_350_000),
            },
        };

        let activity = discord_activity(&snapshot);

        assert_eq!(activity["assets"]["small_image"], DISCORD_MULTI_ASSET_KEY);
        assert_eq!(activity["party"]["id"], "croopor-active-sessions");
        assert_eq!(activity["party"]["size"], json!([2, 2]));
        assert!(activity.get("buttons").is_none());
        assert!(activity.get("secrets").is_none());
    }
}
