use super::AppState;
use crate::observability::{RedactionAudience, sanitize_public_diagnostic_text};
use axial_config::{AppConfig, Instance};
use axial_launcher::{LaunchSessionRecord, LaunchState};
use axial_minecraft::VersionEntry;

const PRESENCE_TEXT_MAX_CHARS: usize = 128;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PresenceSnapshot {
    pub enabled: bool,
    pub activity: PresenceActivity,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PresenceActivity {
    pub kind: PresenceActivityKind,
    pub details: String,
    pub state: String,
    pub active_count: usize,
    pub started_at_unix_seconds: Option<i64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PresenceActivityKind {
    Idle,
    Launching,
    Playing,
    Multi,
}

pub async fn build_presence_snapshot(state: &AppState) -> PresenceSnapshot {
    let config = state.config().current();
    if !config.discord_rpc_enabled {
        return PresenceSnapshot {
            enabled: false,
            activity: idle_presence_activity(),
        };
    }

    let active = state.sessions().active_records().await;
    let installed_versions = if active.len() == 1 {
        match state.try_claim_producer() {
            Ok(producer) => state.installed_versions_snapshot(&producer).await,
            Err(_) => None,
        }
    } else {
        None
    };
    let versions = installed_versions
        .as_ref()
        .map(|lookup| lookup.snapshot.report().versions.as_slice())
        .unwrap_or_default();

    PresenceSnapshot {
        enabled: true,
        activity: presence_activity(&config, &active, versions, |instance_id| {
            state.instances().get(instance_id)
        }),
    }
}

fn presence_activity(
    config: &AppConfig,
    active: &[LaunchSessionRecord],
    versions: &[VersionEntry],
    instance_by_id: impl Fn(&str) -> Option<Instance>,
) -> PresenceActivity {
    if active.is_empty() {
        return idle_presence_activity();
    }

    if active.len() > 1 {
        let running = active
            .iter()
            .filter(|record| is_playing(record.state))
            .count();
        let state = if running > 0 {
            format!("{} instances active", active.len())
        } else {
            format!("{} instances launching", active.len())
        };
        return PresenceActivity {
            kind: PresenceActivityKind::Multi,
            details: if running > 0 {
                "Multiple Minecraft sessions".to_string()
            } else {
                "Starting Minecraft sessions".to_string()
            },
            state,
            active_count: active.len(),
            started_at_unix_seconds: active.iter().filter_map(started_at_seconds).min(),
        };
    }

    let record = &active[0];
    let instance = instance_by_id(&record.instance_id);
    let version = versions
        .iter()
        .find(|version| version.id == record.version_id);
    let summary = version_summary(version, instance.as_ref(), config);
    let playing = is_playing(record.state);
    PresenceActivity {
        kind: if playing {
            PresenceActivityKind::Playing
        } else {
            PresenceActivityKind::Launching
        },
        details: if playing {
            "Minecraft is running".to_string()
        } else {
            "Starting Minecraft".to_string()
        },
        state: summary,
        active_count: 1,
        started_at_unix_seconds: started_at_seconds(record),
    }
}

fn idle_presence_activity() -> PresenceActivity {
    PresenceActivity {
        kind: PresenceActivityKind::Idle,
        details: "Minecraft launcher".to_string(),
        state: "Organizing instances".to_string(),
        active_count: 0,
        started_at_unix_seconds: None,
    }
}

fn started_at_seconds(record: &LaunchSessionRecord) -> Option<i64> {
    record
        .process_started_at_ms
        .or_else(|| launched_at_ms(record))
        .map(|value| (value / 1000) as i64)
}

fn launched_at_ms(record: &LaunchSessionRecord) -> Option<u64> {
    let raw = record.launched_at.as_deref()?;
    chrono::DateTime::parse_from_rfc3339(raw)
        .ok()
        .and_then(|value| u64::try_from(value.timestamp_millis()).ok())
}

fn is_playing(state: LaunchState) -> bool {
    matches!(state, LaunchState::Running | LaunchState::Degraded)
}

fn version_summary(
    version: Option<&VersionEntry>,
    instance: Option<&Instance>,
    config: &AppConfig,
) -> String {
    let version = match version {
        Some(version) => version,
        None => return with_performance_mode("Custom version".to_string(), instance, config),
    };
    let display_version = public_version_label(version);
    if version.loader.is_none() && display_version.is_none() {
        return with_performance_mode("Custom version".to_string(), instance, config);
    }
    let display_version = display_version.unwrap_or_else(|| "Minecraft".to_string());
    let launcher_kind = version
        .loader
        .as_ref()
        .map(|loader| sanitize_presence_text(&loader.component_name, "Modded"))
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "Vanilla".to_string());
    with_performance_mode(
        format!("{launcher_kind} {display_version}"),
        instance,
        config,
    )
}

fn public_version_label(version: &VersionEntry) -> Option<String> {
    let raw = if !version.minecraft_meta.display_name.trim().is_empty() {
        version.minecraft_meta.display_name.as_str()
    } else if !version.minecraft_meta.effective_version.trim().is_empty() {
        version.minecraft_meta.effective_version.as_str()
    } else {
        version.id.as_str()
    };
    let label = sanitize_presence_text(raw, "Minecraft");
    if looks_like_public_minecraft_label(&label) {
        Some(label)
    } else {
        None
    }
}

fn looks_like_public_minecraft_label(label: &str) -> bool {
    let value = label.trim().to_ascii_lowercase();
    if value.is_empty() {
        return false;
    }

    looks_like_release_label(&value)
        || looks_like_weekly_snapshot(&value)
        || value.starts_with("rd ")
        || value.starts_with("rd-")
        || value.starts_with("a1.")
        || value.starts_with("b1.")
        || value.starts_with("c0.")
}

fn looks_like_release_label(value: &str) -> bool {
    let mut chars = value.char_indices().peekable();
    let mut saw_dot = false;
    let mut end = 0;

    while let Some((index, ch)) = chars.peek().copied() {
        if ch.is_ascii_digit() || ch == '.' {
            saw_dot |= ch == '.';
            end = index + ch.len_utf8();
            let _ = chars.next();
        } else {
            break;
        }
    }

    if end == 0 || !saw_dot {
        return false;
    }

    let suffix = value[end..].trim();
    suffix.is_empty()
        || suffix.starts_with("pre-release")
        || suffix.starts_with("release candidate")
        || suffix.starts_with("snapshot")
        || suffix.starts_with("combat test")
        || suffix.starts_with("experimental")
        || suffix.starts_with("deep dark experimental")
}

fn looks_like_weekly_snapshot(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 6
        && bytes[0].is_ascii_digit()
        && bytes[1].is_ascii_digit()
        && bytes[2] == b'w'
        && bytes[3].is_ascii_digit()
        && bytes[4].is_ascii_digit()
        && bytes[5].is_ascii_lowercase()
}

fn with_performance_mode(base: String, instance: Option<&Instance>, config: &AppConfig) -> String {
    let mode = instance
        .map(|instance| instance.performance_mode.trim())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| config.performance_mode.trim());
    let mode = match mode {
        "managed" => Some("Managed"),
        "vanilla" => Some("Vanilla"),
        "custom" => Some("Custom"),
        _ => None,
    };
    let value = match mode {
        Some(mode) => format!("{base} - {mode}"),
        None => base,
    };
    sanitize_presence_text(&value, "Minecraft")
}

fn sanitize_presence_text(raw: &str, fallback: &str) -> String {
    sanitize_public_diagnostic_text(
        raw,
        RedactionAudience::UserVisible,
        PRESENCE_TEXT_MAX_CHARS,
        fallback,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{AppStateInit, InstallStore, SessionStore};
    use axial_config::{
        AppConfig, AppPaths, ConfigStore, Instance, InstanceRegistrySnapshot, InstanceStore,
    };
    use axial_launcher::{LaunchSessionRecord, SessionId};
    use axial_minecraft::{
        MinecraftVersionMeta, VersionEntry, VersionLoaderAttachment, VersionSubjectKind,
        build_id_for,
    };
    use axial_performance::PerformanceManager;
    use std::{fs, path::PathBuf, sync::Arc};

    fn config() -> AppConfig {
        AppConfig::default()
    }

    fn instance(id: &str, version_id: &str) -> Instance {
        Instance {
            id: id.to_string(),
            name: "Private Instance".to_string(),
            version_id: version_id.to_string(),
            created_at: String::new(),
            last_played_at: String::new(),
            art_seed: 0,
            max_memory_mb: 0,
            min_memory_mb: 0,
            java_path: String::new(),
            window_width: 0,
            window_height: 0,
            jvm_preset: String::new(),
            performance_mode: String::new(),
            extra_jvm_args: String::new(),
            auto_optimize: false,
            icon: String::new(),
            accent: String::new(),
            loader_key: String::new(),
            minecraft_version: String::new(),
        }
    }

    fn record(
        session_id: &str,
        instance_id: &str,
        version_id: &str,
        state: LaunchState,
    ) -> LaunchSessionRecord {
        LaunchSessionRecord {
            session_id: SessionId(session_id.to_string()),
            instance_id: instance_id.to_string(),
            version_id: version_id.to_string(),
            launched_at: Some("2026-06-13T12:00:00Z".to_string()),
            benchmark: None,
            state,
            pid: Some(123),
            process_started_at_ms: Some(1_781_350_000_000),
            boot_completed_at_ms: None,
            boot_duration_ms: None,
            priority: None,
            exit_code: None,
            command: Vec::new(),
            java_path: None,
            natives_dir: None,
            failure: None,
            crash_evidence: None,
            healing: None,
            guardian: None,
            outcome: None,
            stages: Vec::new(),
        }
    }

    fn version(id: &str, loader_name: Option<&str>) -> VersionEntry {
        VersionEntry {
            subject_kind: VersionSubjectKind::InstalledVersion,
            id: id.to_string(),
            raw_kind: "release".to_string(),
            release_time: String::new(),
            minecraft_meta: MinecraftVersionMeta {
                family: "release".to_string(),
                base_id: "1.21.1".to_string(),
                effective_version: "1.21.1".to_string(),
                variant_of: String::new(),
                variant_kind: String::new(),
                display_name: "1.21.1".to_string(),
                display_hint: String::new(),
            },
            lifecycle: Default::default(),
            inherits_from: String::new(),
            launchable: true,
            installed: true,
            status: "ready".to_string(),
            status_detail: String::new(),
            needs_install: String::new(),
            java_component: String::new(),
            java_major: 21,
            loader: loader_name.map(|name| VersionLoaderAttachment {
                component_id: axial_minecraft::LoaderComponentId::Fabric,
                component_name: name.to_string(),
                build_id: build_id_for(
                    axial_minecraft::LoaderComponentId::Fabric,
                    "1.21.1",
                    "0.16.10",
                ),
                loader_version: "0.16.10".to_string(),
                build_meta: Default::default(),
            }),
        }
    }

    fn private_version(id: &str) -> VersionEntry {
        let mut version = version(id, None);
        version.minecraft_meta.display_name = "Private Modpack 1.21.1".to_string();
        version.minecraft_meta.effective_version = "Private Modpack 1.21.1".to_string();
        version
    }

    #[test]
    fn p00_b08_contract_presence_empty_is_public_and_generic() {
        let activity = presence_activity(&config(), &[], &[], |_| None);

        assert_eq!(activity.kind, PresenceActivityKind::Idle);
        assert_eq!(activity.details, "Minecraft launcher");
        assert_eq!(activity.state, "Organizing instances");
    }

    #[test]
    fn p00_b08_contract_presence_single_playing_session_is_exact() {
        let versions = vec![version("fabric-loader-0.16.10-1.21.1", Some("Fabric"))];
        let active = vec![record(
            "session",
            "instance",
            "fabric-loader-0.16.10-1.21.1",
            LaunchState::Running,
        )];
        let instance = instance("instance", "fabric-loader-0.16.10-1.21.1");

        let activity = presence_activity(&config(), &active, &versions, |_| Some(instance.clone()));

        assert_eq!(activity.kind, PresenceActivityKind::Playing);
        assert_eq!(activity.details, "Minecraft is running");
        assert_eq!(activity.state, "Fabric 1.21.1 - Managed");
    }

    #[test]
    fn p00_b08_contract_presence_multiple_active_sessions_are_exact() {
        let active = vec![
            record("first", "a", "1.21.1", LaunchState::Running),
            record("second", "b", "1.20.1", LaunchState::Starting),
        ];

        let activity = presence_activity(&config(), &active, &[], |_| None);

        assert_eq!(activity.kind, PresenceActivityKind::Multi);
        assert_eq!(activity.details, "Multiple Minecraft sessions");
        assert_eq!(activity.state, "2 instances active");
        assert!(!activity.state.contains("Private"));
    }

    #[test]
    fn p00_b08_contract_presence_single_launching_session_is_exact() {
        let active = vec![record(
            "session",
            "instance",
            "1.21.1",
            LaunchState::Starting,
        )];

        let activity = presence_activity(&config(), &active, &[], |_| None);

        assert_eq!(activity.kind, PresenceActivityKind::Launching);
        assert_eq!(activity.details, "Starting Minecraft");
        assert_eq!(activity.state, "Custom version - Managed");
        assert_eq!(activity.active_count, 1);
        assert_eq!(activity.started_at_unix_seconds, Some(1_781_350_000));
    }

    #[test]
    fn p00_b08_contract_presence_multiple_launching_sessions_are_exact() {
        let active = vec![
            record("first", "a", "1.21.1", LaunchState::Starting),
            record("second", "b", "1.20.1", LaunchState::Preparing),
        ];

        let activity = presence_activity(&config(), &active, &[], |_| None);

        assert_eq!(activity.kind, PresenceActivityKind::Multi);
        assert_eq!(activity.details, "Starting Minecraft sessions");
        assert_eq!(activity.state, "2 instances launching");
        assert_eq!(activity.active_count, 2);
    }

    #[test]
    fn p00_b08_contract_presence_is_identical_for_all_three_session_permutations() {
        let mut first = record("first", "a", "1.21.1", LaunchState::Starting);
        first.process_started_at_ms = Some(1_781_350_003_000);
        let mut second = record("second", "b", "1.20.1", LaunchState::Running);
        second.process_started_at_ms = Some(1_781_350_001_000);
        let mut third = record("third", "c", "1.19.4", LaunchState::Preparing);
        third.process_started_at_ms = Some(1_781_350_002_000);
        let records = [first, second, third];
        let orders = [
            [0, 1, 2],
            [0, 2, 1],
            [1, 0, 2],
            [1, 2, 0],
            [2, 0, 1],
            [2, 1, 0],
        ];
        let expected = presence_activity(&config(), &records, &[], |_| None);

        for order in orders {
            let active = order.map(|index| records[index].clone());
            let actual = presence_activity(&config(), &active, &[], |_| None);
            assert_eq!(actual, expected);
        }
        assert_eq!(expected.kind, PresenceActivityKind::Multi);
        assert_eq!(expected.details, "Multiple Minecraft sessions");
        assert_eq!(expected.state, "3 instances active");
        assert_eq!(expected.started_at_unix_seconds, Some(1_781_350_001));
    }

    #[tokio::test]
    async fn p00_b08_contract_presence_disabled_snapshot_is_idle() {
        let root = test_root("disabled");
        let paths = test_paths(&root);
        let config = Arc::new(
            ConfigStore::from_config(
                paths.clone(),
                AppConfig {
                    discord_rpc_enabled: false,
                    ..AppConfig::default()
                },
            )
            .expect("create config"),
        );
        let instances = Arc::new(
            InstanceStore::from_snapshot(paths.clone(), InstanceRegistrySnapshot::default())
                .expect("create instances"),
        );
        let state = AppState::new(AppStateInit {
            app_name: "Axial".to_string(),
            version: "test".to_string(),
            config,
            instances,
            installs: Arc::new(InstallStore::new()),
            sessions: Arc::new(SessionStore::new()),
            performance: Arc::new(
                PerformanceManager::load_for_startup(paths.performance_dir())
                    .expect("create performance manager"),
            ),
            startup_warnings: Vec::new(),
        });

        let snapshot = build_presence_snapshot(&state).await;

        assert!(!snapshot.enabled);
        assert_eq!(snapshot.activity, idle_presence_activity());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn custom_local_version_names_fall_back_to_generic_summary() {
        let versions = vec![private_version("private-pack")];
        let active = vec![record(
            "session",
            "instance",
            "private-pack",
            LaunchState::Running,
        )];

        let activity = presence_activity(&config(), &active, &versions, |_| None);

        assert_eq!(activity.state, "Custom version - Managed");
        assert!(!activity.state.contains("Private"));
    }

    #[tokio::test]
    async fn repeated_single_session_snapshots_reuse_redacted_installed_version_report() {
        let root = test_root("shared-installed-versions");
        let paths = test_paths(&root);
        let private_version_id = "private-access-token-pack";
        let version_dir = paths.library_dir().join("versions").join(private_version_id);
        fs::create_dir_all(&version_dir).expect("create installed version directory");
        fs::write(
            version_dir.join(format!("{private_version_id}.json")),
            serde_json::json!({
                "id": private_version_id,
                "type": "release",
                "mainClass": "net.minecraft.client.main.Main",
                "libraries": []
            })
            .to_string(),
        )
        .expect("write installed version metadata");
        fs::write(
            version_dir.join(format!("{private_version_id}.jar")),
            b"client",
        )
        .expect("write installed version jar");
        let config = Arc::new(
            ConfigStore::from_config(
                paths.clone(),
                AppConfig {
                    library_dir: paths.library_dir().to_string_lossy().into_owned(),
                    discord_rpc_enabled: true,
                    ..AppConfig::default()
                },
            )
            .expect("create config"),
        );
        let instances = Arc::new(
            InstanceStore::from_snapshot(paths.clone(), InstanceRegistrySnapshot::default())
                .expect("create instances"),
        );
        let state = AppState::new(AppStateInit {
            app_name: "Axial".to_string(),
            version: "test".to_string(),
            config,
            instances,
            installs: Arc::new(InstallStore::new()),
            sessions: Arc::new(SessionStore::new()),
            performance: Arc::new(
                PerformanceManager::load_for_startup(paths.performance_dir())
                    .expect("create performance manager"),
            ),
            startup_warnings: Vec::new(),
        });
        let instance = state
            .instances()
            .insert_for_test("Alice access_token profile", private_version_id)
            .expect("insert private instance");
        state
            .sessions()
            .insert(record(
                "presence-session",
                &instance.id,
                private_version_id,
                LaunchState::Running,
            ))
            .await
            .expect("insert active session");

        let first = build_presence_snapshot(&state).await;
        let second = build_presence_snapshot(&state).await;

        assert_eq!(first, second);
        assert_eq!(state.installed_versions_walk_count(), 1);
        assert_eq!(second.activity.state, "Custom version - Managed");
        let public_text = format!("{} {}", second.activity.details, second.activity.state);
        let root_text = root.to_string_lossy();
        for private_fragment in [
            private_version_id,
            "Alice",
            "access_token",
            root_text.as_ref(),
        ] {
            assert!(
                !public_text.contains(private_fragment),
                "private presence fragment was exposed"
            );
        }

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn suspicious_presence_text_falls_back() {
        assert_eq!(
            sanitize_presence_text(r"C:\Users\Alice\AppData\Roaming\.minecraft", "Minecraft"),
            "Minecraft"
        );
        assert_eq!(
            sanitize_presence_text("/home/alice/.minecraft/access_token", "Minecraft"),
            "Minecraft"
        );
    }

    fn test_root(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "axial-presence-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or_default()
        ))
    }

    fn test_paths(root: &std::path::Path) -> AppPaths {
        AppPaths::from_root(root.to_path_buf()).expect("absolute test app root")
    }
}
