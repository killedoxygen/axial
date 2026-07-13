use super::*;

#[tokio::test]
async fn prepare_launch_session_rejects_active_content_mutation() {
    let fixture = TestFixture::new("prepare-content-mutation");
    fixture.write_ready_install("1.21.1");
    let instance_id = fixture.add_instance("Busy", "1.21.1");
    let _mutation = fixture
        .state
        .sessions()
        .try_lock_instance_lifecycle(&instance_id)
        .expect("content mutation lock");

    let error = match prepare_launch_session(
        &fixture.state,
        LaunchRequest {
            instance_id,
            username: None,
            max_memory_mb: None,
            min_memory_mb: None,
            client_started_at_ms: None,
        },
    )
    .await
    {
        Ok(_) => panic!("launch must wait for content mutation"),
        Err(error) => error,
    };

    assert_eq!(error.0, StatusCode::CONFLICT);
    assert_eq!(
        error.1.0["error"],
        "instance is busy with another launch or content operation"
    );
}

#[tokio::test]
async fn prepare_launch_session_ensures_instance_layout_before_building_intent() {
    let fixture = TestFixture::new("prepare-ensures-layout");
    fixture.write_ready_install("1.21.1");
    fs::write(
        fixture.paths.library_dir.join("options.txt"),
        "shared options",
    )
    .expect("write options");
    fs::write(
        fixture.paths.library_dir.join("servers.dat"),
        "shared servers",
    )
    .expect("write servers");
    let instance_id = fixture.add_instance("Survival", "1.21.1");
    let game_dir = fixture.state.instances().game_dir(&instance_id);
    let _ = fs::remove_dir_all(game_dir.join("screenshots"));
    let _ = fs::remove_dir_all(game_dir.join("logs"));

    let prepared = prepare_launch_session(
        &fixture.state,
        LaunchRequest {
            instance_id: instance_id.clone(),
            username: None,
            max_memory_mb: None,
            min_memory_mb: None,
            client_started_at_ms: None,
        },
    )
    .await
    .expect("prepare launch session");

    assert_eq!(
        prepared.task.application.command.kind,
        crate::state::contracts::CommandKind::LaunchInstance
    );
    assert_eq!(
        prepared
            .task
            .application
            .result
            .payload
            .session_id
            .as_deref(),
        Some(prepared.task.intent.session_id.as_str())
    );
    assert_eq!(
        prepared
            .task
            .application
            .result
            .carriers
            .session
            .as_ref()
            .and_then(|session| session.state.as_deref()),
        Some("queued")
    );
    assert_eq!(
        prepared.task.boundary.guardian_decision.mode,
        crate::guardian::GuardianMode::Managed
    );
    assert_eq!(prepared.task.boundary.performance_mode, "managed");
    assert_eq!(prepared.task.intent.game_dir, Some(game_dir.clone()));
    assert_eq!(prepared.task.intent.auth.player_name, "Player");
    assert_eq!(
        prepared.task.intent.auth.uuid,
        axial_minecraft::offline_uuid("Player")
    );
    assert_eq!(prepared.task.intent.auth.access_token, "0");
    assert_eq!(prepared.task.intent.auth.user_type, "msa");
    assert!(game_dir.join("screenshots").is_dir());
    assert!(game_dir.join("logs").is_dir());
    assert_eq!(
        fs::read_to_string(game_dir.join("options.txt")).expect("read options"),
        "shared options"
    );
    assert_eq!(
        fs::read_to_string(game_dir.join("servers.dat")).expect("read servers"),
        "shared servers"
    );
}

#[tokio::test]
async fn prepare_launch_session_syncs_active_offline_account_from_config_username() {
    let fixture = TestFixture::new("prepare-syncs-offline-account-name");
    fixture.write_ready_install("1.21.1");
    fixture
        .state
        .accounts()
        .create_offline_account("OldName")
        .expect("create offline account");
    let mut config = fixture.state.config().current();
    config.username = "NewName".to_string();
    fixture
        .state
        .config()
        .replace_in_memory(config)
        .expect("set config username");
    let instance_id = fixture.add_instance("Survival", "1.21.1");

    let prepared = prepare_launch_session(
        &fixture.state,
        LaunchRequest {
            instance_id,
            username: None,
            max_memory_mb: None,
            min_memory_mb: None,
            client_started_at_ms: None,
        },
    )
    .await
    .expect("prepare launch session");

    assert_eq!(prepared.task.intent.auth.player_name, "NewName");
    assert_eq!(prepared.task.intent.username, "NewName");
    let active = fixture
        .state
        .accounts()
        .active_account()
        .expect("active account")
        .expect("active account");
    assert_eq!(active.display_name, "NewName");
}

#[tokio::test]
async fn prepare_launch_session_rejects_invalid_offline_request_username_as_bad_request() {
    let fixture = TestFixture::new("prepare-invalid-offline-name");
    fixture
        .state
        .accounts()
        .create_offline_account("LocalUser")
        .expect("create offline account");
    let instance_id = fixture.add_instance("Survival", "1.21.1");

    let error = match prepare_launch_session(
        &fixture.state,
        LaunchRequest {
            instance_id,
            username: Some("Bad Name!".to_string()),
            max_memory_mb: None,
            min_memory_mb: None,
            client_started_at_ms: None,
        },
    )
    .await
    {
        Ok(_) => panic!("invalid username should fail"),
        Err(error) => error,
    };

    assert_eq!(error.0, StatusCode::BAD_REQUEST);
    assert_eq!(
        error.1.0["error"],
        "Letters, numbers, and underscores only."
    );
}

#[tokio::test]
async fn prepare_launch_session_uses_online_auth_context_from_active_minecraft_account() {
    let fixture = TestFixture::new("prepare-online-auth");
    fixture.set_launch_auth_mode("online");
    let instance_id = fixture.add_instance("Survival", "1.21.1");
    fixture.add_active_minecraft_account(true).await;
    let prepared = fixture
        .prepare(instance_id, None)
        .await
        .expect("prepare launch session");

    assert_eq!(prepared.task.config.username, "Player");
    assert_eq!(prepared.task.intent.username, "Player");
    assert_eq!(prepared.task.intent.auth.player_name, "ProfileName");
    assert_eq!(
        prepared.task.intent.auth.uuid,
        "4f9c7f7d0b1245d9a5c2f03a8c120001"
    );
    assert_eq!(
        prepared.task.intent.auth.access_token,
        "minecraft-access-token"
    );
    assert_eq!(prepared.task.intent.auth.user_type, "msa");
    assert_eq!(prepared.task.intent.auth.client_id, "");
    assert_eq!(prepared.task.intent.auth.xuid, "");
}

#[tokio::test]
async fn prepare_launch_session_rejects_online_auth_missing_refresh_token_boundedly() {
    let fixture = TestFixture::new("prepare-online-auth-no-refresh");
    fixture.set_launch_auth_mode("online");
    let instance_id = fixture.add_instance("Survival", "1.21.1");
    let error = match fixture.prepare(instance_id, None).await {
        Ok(_) => panic!("online auth without refresh token should fail"),
        Err(error) => error,
    };

    assert_eq!(error.0, StatusCode::PRECONDITION_FAILED);
    assert_eq!(error.1.0["failure_class"], "auth_mode_incompatible");
    assert_eq!(error.1.0["launch_auth_mode"], "online");
    assert_eq!(error.1.0["online_mode_ready"], false);
    assert_eq!(error.1.0["auth_refresh_status"], "sign_in_required");
    assert_eq!(error.1.0["auth_refresh_reason"], "refresh_token_missing");
    assert_launch_error_is_token_safe(&error.1.0);
}

#[tokio::test]
async fn prepare_launch_session_rejects_online_auth_without_verified_account_boundedly() {
    let fixture = TestFixture::new("prepare-online-auth-missing");
    fixture.set_launch_auth_mode("online");
    let instance_id = fixture.add_instance("Survival", "1.21.1");

    let error = match fixture.prepare(instance_id.clone(), None).await {
        Ok(_) => panic!("online auth without account should fail"),
        Err(error) => error,
    };

    assert_eq!(error.0, StatusCode::PRECONDITION_FAILED);
    assert_eq!(error.1.0["failure_class"], "auth_mode_incompatible");
    assert_eq!(error.1.0["launch_auth_mode"], "online");
    assert_eq!(error.1.0["online_mode_ready"], false);
    let text = error.1.0.to_string();
    for material in [
        "minecraft-access-token",
        "msa-access-token",
        "provider-secret-payload",
    ] {
        assert!(
            !text.contains(material),
            "public launch error exposed sensitive material {material}"
        );
    }
    assert!(
        !fixture
            .state
            .sessions()
            .has_active_instance(&instance_id)
            .await
    );
}

#[tokio::test]
async fn prepare_launch_session_rejects_online_auth_without_java_ownership() {
    let fixture = TestFixture::new("prepare-online-auth-unowned");
    fixture.set_launch_auth_mode("online");
    let instance_id = fixture.add_instance("Survival", "1.21.1");
    fixture.add_active_minecraft_account(false).await;

    let error = match fixture.prepare(instance_id, None).await {
        Ok(_) => panic!("online auth without ownership should fail"),
        Err(error) => error,
    };

    assert_eq!(error.0, StatusCode::PRECONDITION_FAILED);
    assert_eq!(error.1.0["failure_class"], "auth_mode_incompatible");
    assert_eq!(error.1.0["online_mode_ready"], false);
    let text = error.1.0.to_string();
    assert!(!text.contains("minecraft-access-token"));
}

#[tokio::test]
async fn prepare_launch_session_rejects_same_instance_active_launch() {
    let fixture = TestFixture::new("prepare-active-conflict");
    let instance_id = fixture.add_instance("Survival", "1.21.1");
    fixture
        .state
        .sessions()
        .insert(LaunchSessionRecord {
            session_id: SessionId("active-session".to_string()),
            instance_id: instance_id.clone(),
            version_id: "1.21.1".to_string(),
            launched_at: Some(timestamp_utc()),
            benchmark: None,
            state: LaunchState::Queued,
            pid: None,
            process_started_at_ms: None,
            boot_completed_at_ms: None,
            boot_duration_ms: None,
            priority: None,
            exit_code: None,
            command: Vec::new(),
            java_path: None,
            natives_dir: None,
            failure: None,
            healing: None,
            guardian: None,
            outcome: None,
            stages: Vec::new(),
        })
        .await;

    let error = match prepare_launch_session(
        &fixture.state,
        LaunchRequest {
            instance_id,
            username: None,
            max_memory_mb: None,
            min_memory_mb: None,
            client_started_at_ms: None,
        },
    )
    .await
    {
        Ok(_) => panic!("active instance should conflict"),
        Err(error) => error,
    };

    assert_eq!(error.0, StatusCode::CONFLICT);
    assert_eq!(
        error.1.0,
        serde_json::json!({ "error": "instance already has an active session" })
    );
}

#[tokio::test]
async fn omitted_request_memory_uses_backend_derived_defaults_for_fresh_builtin_global() {
    let fixture = TestFixture::new("prepare-derived-memory-defaults");
    let instance_id = fixture.add_instance("Survival", "1.21.1");
    let config = fixture.state.config().current();
    let expected_defaults = policy::derived_launch_memory_defaults(
        &fixture
            .state
            .instances()
            .get(&instance_id)
            .expect("instance"),
        &config,
        None,
        None,
        None,
        capture_launch_memory_evidence().host_total_memory_mb,
    );

    let prepared = fixture
        .prepare_with_memory(instance_id, None, None)
        .await
        .expect("prepare launch session");

    if let Some(defaults) = expected_defaults {
        assert_eq!(prepared.task.intent.max_memory_mb, defaults.max_memory_mb);
        assert_eq!(prepared.task.intent.min_memory_mb, defaults.min_memory_mb);
        assert_ne!(
            prepared.task.intent.min_memory_mb,
            AppConfig::default().min_memory_mb
        );
    } else {
        assert_eq!(prepared.task.intent.max_memory_mb, config.max_memory_mb);
        assert_eq!(prepared.task.intent.min_memory_mb, config.min_memory_mb);
    }
}
