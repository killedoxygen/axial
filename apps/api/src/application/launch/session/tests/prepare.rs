use super::*;

#[tokio::test]
async fn prepare_launch_session_rejects_active_content_mutation() {
    let fixture = TestFixture::new("prepare-content-mutation");
    fixture.write_ready_install("1.21.1");
    let instance_id = fixture.add_instance("Busy", "1.21.1");
    let _mutation = fixture.state.acquire_instance_lifecycle(&instance_id).await;

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
        Ok(_) => panic!("launch must reject an active content mutation"),
        Err(error) => error,
    };

    assert_eq!(error.0, StatusCode::CONFLICT);
    assert_eq!(
        error.1.0["error"],
        "instance is busy with another launch or content operation"
    );
}

#[tokio::test]
async fn launch_foreground_cancels_sweep_before_lifecycle_or_launch_effects() {
    let fixture = TestFixture::new("launch-foreground-settlement-barrier");
    fixture.write_ready_install("1.21.1");
    let instance_id = fixture.add_instance("Foreground barrier", "1.21.1");
    let idle_epoch = fixture.state.subscribe_integrity_idle().borrow().epoch();
    let sweep_producer = fixture
        .state
        .try_claim_producer()
        .expect("claim sweep producer");
    let reservation = fixture
        .state
        .try_reserve_idle_sweep(idle_epoch, sweep_producer)
        .expect("reserve idle sweep");
    let cancellation = reservation.cancellation();
    let state = fixture.state.clone();
    let launch_instance_id = instance_id.clone();
    let launch_producer = state.try_claim_producer().expect("claim launch producer");
    let preparation = tokio::spawn(async move {
        let result = prepare_launch_session_owned(
            &state,
            LaunchRequest {
                instance_id: launch_instance_id,
                username: None,
                max_memory_mb: None,
                min_memory_mb: None,
                client_started_at_ms: None,
            },
            &launch_producer,
        )
        .await;
        (launch_producer, result)
    });

    tokio::time::timeout(std::time::Duration::from_millis(100), async {
        while !cancellation.is_cancelled() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("launch cancels active sweep");
    assert!(!preparation.is_finished());
    assert_eq!(fixture.state.sessions().active_session_count().await, 0);
    let lifecycle = tokio::time::timeout(
        std::time::Duration::from_millis(100),
        fixture.state.acquire_instance_lifecycle(&instance_id),
    )
    .await
    .expect("launch must not wait on lifecycle before sweep settlement");
    drop(lifecycle);

    drop(reservation);
    let (launch_producer, prepared) =
        tokio::time::timeout(std::time::Duration::from_secs(5), preparation)
            .await
            .expect("launch preparation after sweep settlement")
            .expect("launch preparation owner");
    let prepared = prepared.expect("launch preparation succeeds");
    assert_eq!(fixture.state.sessions().active_session_count().await, 1);
    assert!(
        !fixture
            .state
            .subscribe_integrity_idle()
            .borrow()
            .is_stably_idle()
    );
    drop(prepared);
    drop(launch_producer);
    assert!(
        fixture
            .state
            .subscribe_integrity_idle()
            .borrow()
            .is_stably_idle()
    );
}

#[tokio::test]
async fn prepare_launch_session_rejects_shutdown_without_returning_a_task() {
    let fixture = TestFixture::new("prepare-rejects-shutdown-admission");
    fixture.write_ready_install("1.21.1");
    let instance_id = fixture.add_instance("Shutdown admission", "1.21.1");
    fixture
        .state
        .sessions()
        .terminate_all()
        .await
        .expect("latch session shutdown");

    let error = match fixture.prepare(instance_id, None).await {
        Ok(_) => panic!("shutdown must reject launch preparation"),
        Err(error) => error,
    };

    assert_eq!(error.0, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        error.1.0,
        serde_json::json!({
            "error": "Launches are unavailable while the application is shutting down."
        })
    );
    assert_eq!(fixture.state.sessions().active_session_count().await, 0);
}

#[tokio::test]
async fn prepare_launch_session_creates_no_user_owned_paths() {
    let fixture = TestFixture::new("prepare-creates-no-user-owned-paths");
    fixture.write_ready_install("1.21.1");
    let instance_id = fixture.add_instance("Survival", "1.21.1");
    let game_dir = fixture.state.instances().game_dir(&instance_id);
    let user_owned_paths = [
        "mods",
        "saves",
        "resourcepacks",
        "shaderpacks",
        "config",
        "screenshots",
        "logs",
        "options.txt",
        "servers.dat",
    ];
    for relative in user_owned_paths {
        let path = game_dir.join(relative);
        if path.is_dir() {
            fs::remove_dir_all(path).expect("remove prepared user-owned directory");
        } else if path.exists() {
            fs::remove_file(path).expect("remove prepared user-owned file");
        }
    }
    assert!(game_dir.is_dir());

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

    assert_eq!(prepared.task.preflight_stage_evidence.len(), 2);
    assert_eq!(
        prepared.task.preflight_stage_evidence[0].id,
        "guardian_launch_safety_decision"
    );
    assert_eq!(
        prepared.task.preflight_stage_evidence[0].details[0],
        "mode:Managed"
    );
    assert_eq!(
        prepared.task.preflight_stage_evidence[1].details,
        ["mode:managed"]
    );
    assert_eq!(prepared.task.intent.game_dir, Some(game_dir.clone()));
    assert_eq!(prepared.task.intent.auth.player_name, "Player");
    assert_eq!(
        prepared.task.intent.auth.uuid,
        axial_minecraft::offline_uuid("Player")
    );
    assert_eq!(prepared.task.intent.auth.access_token, "0");
    assert_eq!(prepared.task.intent.auth.user_type, "msa");
    for relative in user_owned_paths {
        assert!(
            !game_dir.join(relative).exists(),
            "launch preparation created user-owned path {relative}"
        );
    }
}

#[tokio::test]
async fn p00_b11_contract_preparation_uses_authenticated_player_and_application_identity() {
    let fixture = TestFixture::new("prepare-syncs-offline-account-name");
    fixture.write_ready_install("1.21.1");
    fixture
        .state
        .accounts()
        .create_offline_account("OldName")
        .await
        .expect("create offline account");
    let mut config = fixture.state.config().current();
    config.username = "NewName".to_string();
    fixture
        .state
        .config()
        .replace_for_test(config)
        .expect("set config username");
    let instance_id = fixture.add_instance("Survival", "1.21.1");

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

    assert_eq!(prepared.task.intent.auth.player_name, "NewName");
    assert_eq!(prepared.task.instance.id, instance_id);
    assert_eq!(prepared.task.performance_mode, "managed");
    assert!(prepared.task.intent.low_impact_startup);
    assert!(!prepared.task.session_id.0.is_empty());
    let record = fixture
        .state
        .sessions()
        .get(&prepared.task.session_id.0)
        .await
        .expect("prepared session record");
    assert_eq!(record.session_id, prepared.task.session_id);
    assert_eq!(record.instance_id, prepared.task.instance.id);
    let active = fixture
        .state
        .accounts()
        .active_account()
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
        .await
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
            crash_evidence: None,
            healing: None,
            guardian: None,
            outcome: None,
            stages: Vec::new(),
        })
        .await
        .expect("insert session");

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
