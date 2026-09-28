use super::*;
use serde_json::json;

fn store() -> (Arc<MetadataStore>, SettingsStore) {
    let metadata = Arc::new(MetadataStore::in_memory().unwrap());
    metadata.migrate(&[SETTINGS_MIGRATION]).unwrap();
    let settings = SettingsStore::new_with_telemetry_identity(metadata.clone(), true).unwrap();
    (metadata, settings)
}

fn patch(value: serde_json::Value) -> ConfigPatch {
    serde_json::from_value(value).unwrap()
}

#[test]
fn retained_defaults_and_public_shape_are_exact() {
    let (_, settings) = store();
    assert_eq!(
        serde_json::to_value(settings.current().unwrap()).unwrap(),
        json!({
        "revision":0,"account_selection_revision":0,"username":"Player","launch_auth_mode":"offline","max_memory_mb":4096,
            "min_memory_mb":512,"java_path_override":"","window_width":0,"window_height":0,
            "onboarding_done":false,"jvm_preset":"","performance_mode":"managed","theme":"",
            "custom_hue":null,"custom_vibrancy":null,"lightness":null,"telemetry_enabled":false,
            "discord_rpc_enabled":true,"discord_rpc_onboarding_seen":false,"music_enabled":null,
            "music_volume":null,"music_track":0
        })
    );
}

#[test]
fn authenticated_profile_names_do_not_weaken_offline_name_validation() {
    let (_, settings) = store();
    for name in ["A", "A1"] {
        let online = ConfigView {
            username: name.into(),
            launch_auth_mode: ConfigLaunchAuthMode::Online,
            ..ConfigView::default()
        };
        assert!(online.validate().is_ok());
        assert!(InstanceSettings::default().effective(&online).is_ok());
        let offline = ConfigView {
            launch_auth_mode: ConfigLaunchAuthMode::Offline,
            ..online
        };
        assert!(matches!(
            offline.validate(),
            Err(SettingsError::Validation(_))
        ));
        assert!(matches!(
            settings.update(patch(json!({
                "expected_revision":0,"username":name,"launch_auth_mode":"online"
            }))),
            Err(SettingsError::Validation(_))
        ));
    }
    for name in ["", " A", "A B", "A/B", "é", "01234567890123456"] {
        let online = ConfigView {
            username: name.into(),
            launch_auth_mode: ConfigLaunchAuthMode::Online,
            ..ConfigView::default()
        };
        assert!(matches!(
            online.validate(),
            Err(SettingsError::Validation(_))
        ));
    }
    assert_eq!(settings.current().unwrap(), ConfigView::default());
}

#[test]
fn preferences_survive_real_database_reopen_without_reinitializing() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory
        .path()
        .canonicalize()
        .unwrap()
        .join("settings.sqlite");
    {
        let metadata = Arc::new(MetadataStore::open(&database).unwrap());
        metadata.migrate(&[SETTINGS_MIGRATION]).unwrap();
        let settings = SettingsStore::new(metadata).unwrap();
        settings
            .update(patch(
                json!({"expected_revision":0,"username":" Alex_1 ","theme":"nether",
            "min_memory_mb":1024,"max_memory_mb":6144,"window_width":1280,"window_height":720,
            "onboarding_done":true,"jvm_preset":"smooth","performance_mode":"vanilla",
            "custom_hue":123,"custom_vibrancy":77,"lightness":55,"music_enabled":true,
            "music_volume":31,"music_track":7,"discord_rpc_enabled":false,
            "discord_rpc_onboarding_seen":true}),
            ))
            .unwrap();
        settings
            .update_flag(
                STATE_INSPECTOR_FLAG,
                FlagOverridePatch {
                    expected_revision: 1,
                    enabled: Some(true),
                },
            )
            .unwrap();
    }
    let metadata = Arc::new(MetadataStore::open(&database).unwrap());
    metadata.migrate(&[SETTINGS_MIGRATION]).unwrap();
    let settings = SettingsStore::new(metadata).unwrap();
    let restored = settings.current().unwrap();
    assert_eq!(restored.username, "Alex_1");
    assert_eq!(restored.revision, 2);
    assert_eq!(restored.theme, ConfigTheme::Nether);
    assert_eq!(
        (restored.min_memory_mb, restored.max_memory_mb),
        (1024, 6144)
    );
    assert_eq!((restored.window_width, restored.window_height), (1280, 720));
    assert!(restored.onboarding_done);
    assert_eq!(
        (
            restored.custom_hue,
            restored.custom_vibrancy,
            restored.lightness
        ),
        (Some(123), Some(77), Some(55))
    );
    assert_eq!(
        (
            restored.music_enabled,
            restored.music_volume,
            restored.music_track
        ),
        (Some(true), Some(31), 7)
    );
    assert!(!restored.discord_rpc_enabled);
    assert!(restored.discord_rpc_onboarding_seen);
    assert_eq!(restored.jvm_preset, ConfigJvmPreset::Smooth);
    assert_eq!(restored.performance_mode, ConfigPerformanceMode::Vanilla);
    assert!(settings.list_flags().unwrap().flags[0].enabled);
}

#[test]
fn stale_config_and_flag_writers_never_overwrite_committed_data() {
    let (metadata, first) = store();
    let second = SettingsStore::new(metadata).unwrap();
    first
        .update(patch(json!({"expected_revision":0,"theme":"birch"})))
        .unwrap();
    assert!(matches!(
        second.update(patch(json!({"expected_revision":0,"username":"Other"}))),
        Err(SettingsError::Conflict)
    ));
    assert!(matches!(
        second.update_flag(
            STATE_INSPECTOR_FLAG,
            FlagOverridePatch {
                expected_revision: 0,
                enabled: Some(true)
            }
        ),
        Err(SettingsError::Conflict)
    ));
    assert_eq!(second.current().unwrap().theme, ConfigTheme::Birch);
    assert_eq!(second.current().unwrap().username, "Player");
    assert_eq!(second.current().unwrap().revision, 1);
}

#[test]
fn simultaneous_writers_accept_exactly_one_revision() {
    let (metadata, _) = store();
    let barrier = Arc::new(std::sync::Barrier::new(3));
    let writers = ["obsidian", "birch"].map(|theme| {
        let writer = SettingsStore::new(metadata.clone()).unwrap();
        let barrier = barrier.clone();
        std::thread::spawn(move || {
            barrier.wait();
            writer.update(patch(json!({"expected_revision":0,"theme":theme})))
        })
    });
    barrier.wait();
    let results = writers.map(|writer| writer.join().unwrap());
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, Err(SettingsError::Conflict)))
            .count(),
        1
    );
}

#[test]
fn invalid_patches_cannot_change_settings_or_revision() {
    let (_, settings) = store();
    for invalid in [
        json!({"expected_revision":0,"max_memory_mb":32769}),
        json!({"expected_revision":0,"min_memory_mb":8192}),
        json!({"expected_revision":0,"window_width":1280}),
        json!({"expected_revision":0,"username":"../../private"}),
        json!({"expected_revision":0,"java_path_override":"/java\nsecret"}),
        json!({"expected_revision":0,"custom_hue":361}),
        json!({"expected_revision":0,"music_volume":101}),
    ] {
        assert!(matches!(
            settings.update(patch(invalid)),
            Err(SettingsError::Validation(_))
        ));
        assert_eq!(settings.current().unwrap(), ConfigView::default());
    }
    for invalid in [
        json!({"theme":"nether"}),
        json!({"expected_revision":0,"username":null}),
        json!({"expected_revision":0,"performance_mode":"automatic"}),
        json!({"expected_revision":0,"guardian_mode":"managed"}),
        json!({"expected_revision":0,"library_dir":"/private"}),
        json!({"expected_revision":0,"feature_overrides":{}}),
    ] {
        assert!(serde_json::from_value::<ConfigPatch>(invalid).is_err());
    }
    assert!(serde_json::from_value::<FlagOverridePatch>(json!({"expected_revision":0})).is_err());
    assert!(
        serde_json::from_value::<FlagOverridePatch>(
            json!({"expected_revision":0,"enabled":true,"other":1})
        )
        .is_err()
    );
}

#[test]
fn nullable_preferences_distinguish_missing_and_explicit_reset() {
    let (_, settings) = store();
    settings
        .update(patch(
            json!({"expected_revision":0,"custom_hue":12,"music_enabled":false}),
        ))
        .unwrap();
    let untouched = settings
        .update(patch(json!({"expected_revision":1,"theme":"custom"})))
        .unwrap();
    assert_eq!(untouched.custom_hue, Some(12));
    assert_eq!(untouched.music_enabled, Some(false));
    let reset = settings
        .update(patch(
            json!({"expected_revision":2,"custom_hue":null,"music_enabled":null}),
        ))
        .unwrap();
    assert_eq!(reset.custom_hue, None);
    assert_eq!(reset.music_enabled, None);
}

#[test]
fn corruption_is_preserved_on_read_open_and_mutation() {
    let (metadata, settings) = store();
    for corrupt in [
        "{private-malformed",
        "{}",
        &"x".repeat(MAX_DOCUMENT_BYTES + 1),
    ] {
        metadata
            .transaction(|tx| -> Result<(), StorageError> {
                tx.execute(
                    "UPDATE settings_config SET document=?1 WHERE singleton=1",
                    [corrupt],
                )?;
                Ok(())
            })
            .unwrap();
        assert!(matches!(settings.current(), Err(SettingsError::Corrupt)));
        assert!(matches!(
            SettingsStore::new(metadata.clone()),
            Err(SettingsError::Corrupt)
        ));
        assert!(matches!(
            settings.update(patch(json!({"expected_revision":0,"theme":"nether"}))),
            Err(SettingsError::Corrupt)
        ));
        let retained: String = metadata
            .read(|connection| -> Result<_, StorageError> {
                Ok(connection.query_row(
                    "SELECT document FROM settings_config WHERE singleton=1",
                    [],
                    |row| row.get(0),
                )?)
            })
            .unwrap();
        assert_eq!(retained, corrupt);
    }
}

#[test]
fn failed_sql_commit_does_not_publish_a_revision_or_replace_preferences() {
    let (metadata, settings) = store();
    let changes = settings.subscribe().unwrap();
    metadata.transaction(|tx| -> Result<(),StorageError> {
        tx.execute_batch("CREATE TRIGGER reject_settings BEFORE UPDATE ON settings_config BEGIN SELECT RAISE(ABORT,'private-path'); END;")?;
        Ok(())
    }).unwrap();
    let error = settings
        .update(patch(json!({"expected_revision":0,"theme":"nether"})))
        .unwrap_err();
    assert!(!error.to_string().contains("private-path"));
    assert_eq!(settings.current().unwrap(), ConfigView::default());
    assert!(!changes.has_changed().unwrap());
}

#[test]
fn subscribers_receive_only_committed_latest_snapshots() {
    let (_, settings) = store();
    let mut changes = settings.subscribe().unwrap();
    assert_eq!(changes.borrow_and_update().revision, 0);
    settings
        .update(patch(
            json!({"expected_revision":0,"discord_rpc_enabled":false}),
        ))
        .unwrap();
    assert!(changes.has_changed().unwrap());
    assert_eq!(changes.borrow_and_update().revision, 1);
    assert!(!changes.borrow().discord_rpc_enabled);
    let late = settings.subscribe().unwrap();
    assert_eq!(late.borrow().revision, 1);
}

#[test]
fn telemetry_identity_is_private_and_rotates_after_consent_revocation() {
    let (_, settings) = store();
    assert_eq!(settings.telemetry_identity().unwrap(), None);
    settings
        .update(patch(
            json!({"expected_revision":0,"telemetry_enabled":true}),
        ))
        .unwrap();
    let old_id = settings.telemetry_identity().unwrap().unwrap();
    assert!(uuid::Uuid::parse_str(&old_id).is_ok());
    settings
        .update(patch(
            json!({"expected_revision":1,"telemetry_enabled":false}),
        ))
        .unwrap();
    assert_eq!(settings.telemetry_identity().unwrap(), None);
    settings
        .update(patch(
            json!({"expected_revision":2,"telemetry_enabled":true}),
        ))
        .unwrap();
    assert_ne!(settings.telemetry_identity().unwrap().unwrap(), old_id);
    let public = serde_json::to_string(&settings.current().unwrap()).unwrap();
    assert!(!public.contains("telemetry_install_id"));
    assert!(!public.contains(&old_id));
}

#[test]
fn keyless_consent_persists_without_identity_and_configured_restart_creates_it_once() {
    let metadata = Arc::new(MetadataStore::in_memory().unwrap());
    let keyless = SettingsStore::new(metadata.clone()).unwrap();
    let config = keyless
        .update(patch(
            json!({"expected_revision":0,"telemetry_enabled":true}),
        ))
        .unwrap();
    assert!(config.telemetry_enabled);
    assert_eq!(keyless.telemetry_identity().unwrap(), None);
    let configured = SettingsStore::new_with_telemetry_identity(metadata.clone(), true).unwrap();
    let identity = configured.telemetry_identity().unwrap().unwrap();
    assert_eq!(configured.current().unwrap().revision, 2);
    let reopened = SettingsStore::new_with_telemetry_identity(metadata, true).unwrap();
    assert_eq!(reopened.telemetry_identity().unwrap(), Some(identity));
    assert_eq!(reopened.current().unwrap().revision, 2);
}

#[test]
fn legacy_settings_preview_preserves_preferences_and_excludes_authority_and_identity() {
    let prepared = prepare_legacy_import(&json!({
        "username":"Alex_1","max_memory_mb":6144,"min_memory_mb":1024,
        "theme":"custom","custom_hue":123,"custom_vibrancy":77,"lightness":55,
        "music_enabled":false,"music_volume":0,"music_track":7,"onboarding_done":true,
        "launch_auth_mode":"online","telemetry_enabled":true,"telemetry_install_id":"old-profile-id",
        "feature_overrides":{"dev.state-inspector":true},"library_dir":"/baseline/private",
        "library_mode":"existing","guardian_mode":"managed","guardian_idle_integrity_enabled":true
    })).unwrap();
    assert_eq!(prepared.config.revision, 0);
    assert_eq!(prepared.config.username, "Alex_1");
    assert_eq!(
        prepared.config.launch_auth_mode,
        ConfigLaunchAuthMode::Online
    );
    assert_eq!(prepared.config.custom_hue, Some(123));
    assert_eq!(
        (prepared.config.custom_vibrancy, prepared.config.lightness),
        (Some(77), Some(55))
    );
    assert_eq!(
        (
            prepared.config.music_enabled,
            prepared.config.music_volume,
            prepared.config.music_track
        ),
        (Some(false), Some(0), 7)
    );
    assert!(prepared.config.onboarding_done && prepared.config.telemetry_enabled);
    assert_eq!(
        prepared.feature_overrides.get(STATE_INSPECTOR_FLAG),
        Some(&true)
    );
    assert!(prepared.excluded_library_metadata);
    let rendered = serde_json::to_string(&prepared.config).unwrap();
    assert!(
        !rendered.contains("old-profile-id")
            && !rendered.contains("baseline")
            && !rendered.contains("guardian")
    );
    for field in ["unsupported_preference", "revision", "expected_revision"] {
        let mut source = json!({"username":"Player","max_memory_mb":4096,"min_memory_mb":512});
        source[field] = json!(1);
        assert!(prepare_legacy_import(&source).is_err());
    }
    assert!(prepare_legacy_import(&json!({"username":"Player"})).is_err());
    assert!(prepare_legacy_import(&json!({"username":"Player","max_memory_mb":4096,"min_memory_mb":512,"feature_overrides":{"unknown.flag":true}})).is_err());
}

#[test]
fn flags_reset_to_default_and_release_never_exposes_developer_capabilities() {
    let (_, settings) = store();
    assert_eq!(
        settings.list_flags().unwrap().flags[0].source,
        FlagSource::Default
    );
    let set = settings
        .update_flag(
            STATE_INSPECTOR_FLAG,
            FlagOverridePatch {
                expected_revision: 0,
                enabled: Some(true),
            },
        )
        .unwrap();
    assert_eq!(set.flags[0].source, FlagSource::Override);
    assert!(set.flags[0].enabled);
    assert!(
        settings
            .list_flags_for_build(false)
            .unwrap()
            .flags
            .is_empty()
    );
    assert!(matches!(
        settings.update_flag_for_build(
            STATE_INSPECTOR_FLAG,
            FlagOverridePatch {
                expected_revision: 1,
                enabled: None
            },
            false
        ),
        Err(SettingsError::UnknownFlag)
    ));
    let reset = settings
        .update_flag(
            STATE_INSPECTOR_FLAG,
            FlagOverridePatch {
                expected_revision: 1,
                enabled: None,
            },
        )
        .unwrap();
    assert_eq!(reset.flags[0].source, FlagSource::Default);
    assert!(!reset.flags[0].enabled);
}

#[test]
fn instance_settings_inherit_and_reset_with_retained_memory_clamping() {
    let global = ConfigView {
        revision: 7,
        java_path_override: "/custom/java".into(),
        window_width: 1280,
        window_height: 720,
        jvm_preset: ConfigJvmPreset::Smooth,
        ..ConfigView::default()
    };
    let mut instance = InstanceSettings::default();
    let inherited = instance.effective(&global).unwrap();
    assert_eq!(
        (inherited.max_memory_mb, inherited.min_memory_mb),
        (4096, 512)
    );
    assert_eq!(inherited.java_path, "/custom/java");
    assert_eq!(inherited.jvm_preset, ConfigJvmPreset::Smooth);
    assert_eq!(inherited.global_config_revision, 7);
    instance.max_memory_mb = 1024;
    instance.min_memory_mb = 2048;
    instance.java_path = "/instance/java".into();
    instance.performance_mode = "vanilla".into();
    instance.window_width = 1920;
    let overridden = instance.effective(&global).unwrap();
    assert_eq!(
        (overridden.max_memory_mb, overridden.min_memory_mb),
        (1024, 1024)
    );
    assert_eq!(overridden.java_path, "/instance/java");
    assert_eq!(overridden.performance_mode, ConfigPerformanceMode::Vanilla);
    assert_eq!(
        (overridden.window_width, overridden.window_height),
        (1920, 720)
    );
    assert_eq!(
        InstanceSettings::default().effective(&global).unwrap(),
        inherited
    );
}
