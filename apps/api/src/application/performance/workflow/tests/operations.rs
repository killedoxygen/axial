use super::*;
use crate::state::OperationJournalStoreError;

#[tokio::test]
async fn install_target_staging_refreshes_once_cold_and_zero_times_warm() {
    let fixture = TestFixture::new("performance-target-staging-version-index");
    let version_id = fabric_version_id("1.20.4");
    let instance_id = fixture.add_instance("Managed", &version_id);
    fixture.write_fabric_version(&version_id, "1.20.4");
    let instance = fixture
        .state
        .instances()
        .get(&instance_id)
        .expect("instance exists");
    let operation = PerformanceOperation {
        instance_id,
        game_version: None,
        loader: None,
        mode: None,
        action: PerformanceInstallAction::Install,
        rollback_id: None,
        status_operation_id: None,
        persistence_failure: None,
        installed_versions: None,
    };
    let request = fixture
        .state
        .try_admit_request()
        .expect("admit staging request");
    let producer = request
        .producer_handoff()
        .try_claim()
        .expect("claim staging producer");
    let foreground = fixture
        .state
        .register_integrity_foreground()
        .expect("register staging foreground")
        .wait_for_settlement()
        .await;

    let cold =
        stage_performance_installed_versions(&fixture.state, &operation, &producer, &foreground)
            .await
            .expect("configured library snapshot");
    assert_eq!(fixture.state.installed_versions_walk_count(), 1);
    assert_eq!(
        resolve_instance_version_target(Some(&cold), &instance, None, None)
            .expect("resolve target from staged snapshot"),
        ("1.20.4".to_string(), "fabric".to_string())
    );

    let warm =
        stage_performance_installed_versions(&fixture.state, &operation, &producer, &foreground)
            .await
            .expect("warm configured library snapshot");
    assert_eq!(fixture.state.installed_versions_walk_count(), 1);
    assert_eq!(
        resolve_instance_version_target(Some(&warm), &instance, None, None)
            .expect("resolve target from warm staged snapshot"),
        ("1.20.4".to_string(), "fabric".to_string())
    );
}

#[tokio::test]
async fn degraded_install_target_snapshot_preserves_metadata_unavailable_error() {
    let fixture = TestFixture::new("performance-target-staging-degraded");
    let instance_id = fixture.add_instance("Managed", "broken-version");
    let version_dir = fixture
        .root
        .join("library")
        .join("versions")
        .join("broken-version");
    fs::create_dir_all(&version_dir).expect("create malformed version directory");
    fs::write(version_dir.join("broken-version.json"), "{not json")
        .expect("write malformed version metadata");
    let instance = fixture
        .state
        .instances()
        .get(&instance_id)
        .expect("instance exists");
    let operation = PerformanceOperation {
        instance_id,
        game_version: None,
        loader: None,
        mode: None,
        action: PerformanceInstallAction::Install,
        rollback_id: None,
        status_operation_id: None,
        persistence_failure: None,
        installed_versions: None,
    };
    let request = fixture
        .state
        .try_admit_request()
        .expect("admit degraded staging request");
    let producer = request
        .producer_handoff()
        .try_claim()
        .expect("claim degraded staging producer");
    let foreground = fixture
        .state
        .register_integrity_foreground()
        .expect("register degraded staging foreground")
        .wait_for_settlement()
        .await;
    let snapshot =
        stage_performance_installed_versions(&fixture.state, &operation, &producer, &foreground)
            .await
            .expect("configured degraded snapshot");

    assert_eq!(
        snapshot.report().state,
        axial_minecraft::VersionScanState::Degraded
    );
    let (status, Json(body)) =
        resolve_instance_version_target(Some(&snapshot), &instance, None, None)
            .expect_err("malformed installed version metadata remains unavailable");
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        body,
        serde_json::json!({
            "error": "instance version metadata is unavailable; install the version before resolving performance files"
        })
    );
}

#[tokio::test]
async fn performance_mutation_rejects_foreign_state_foreground_before_effect() {
    let fixture = TestFixture::new("performance-foreign-foreground-owner");
    let foreign = TestFixture::new("performance-foreign-foreground-source");
    let instance_id = fixture.add_instance("Managed", "1.20.4-fabric");
    let lock_path = seed_managed_lock(&fixture.state, &instance_id, "foreign-owner-preserved");
    let foreground = foreign
        .state
        .register_integrity_foreground()
        .expect("register foreign performance foreground")
        .wait_for_settlement()
        .await;
    let operation = PerformanceOperation {
        instance_id,
        game_version: None,
        loader: None,
        mode: None,
        action: PerformanceInstallAction::Remove,
        rollback_id: None,
        status_operation_id: None,
        persistence_failure: None,
        installed_versions: None,
    };

    let error = match execute_performance_operation(&fixture.state, &operation, &foreground).await {
        Ok(_) => panic!("foreign foreground authority must be rejected"),
        Err(error) => error,
    };
    let (status, _) = error.into_application_error();
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(lock_path.is_file(), "foreign authority cannot run effects");
    assert!(fixture.state.journals().list().is_empty());
}

#[tokio::test]
async fn queued_remove_returns_install_id_and_complete_progress() {
    let fixture = TestFixture::new("queued-remove");
    let instance_id = fixture.add_instance("Managed", "1.20.4-fabric");

    let Json(response) = handle_install(
        State(fixture.state.clone()),
        Json(InstallRequest {
            instance_id: Some(instance_id.clone()),
            game_version: None,
            loader: None,
            mode: None,
            action: Some("remove".to_string()),
            rollback_id: None,
            queued: Some(true),
        }),
    )
    .await
    .expect("queued remove should be accepted");

    assert_eq!(response.status, "queued");
    let install_id = response.install_id.expect("queued response has install id");
    let events = collect_install_events(&fixture.state, &install_id).await;
    let terminal = events.last().expect("terminal event");
    assert_eq!(terminal.phase, "complete");
    assert!(terminal.done);
    assert!(terminal.error.is_none());
    let status = fixture
        .state
        .performance_operations()
        .get(&install_id)
        .await
        .expect("durable operation status");
    assert_eq!(status.instance_id, instance_id);
    assert_eq!(status.action, "remove");
    assert_eq!(status.state, "complete");
    assert_eq!(status.error, None);
}

#[tokio::test]
async fn queued_remove_cancels_idle_sweep_before_shared_root_effect() {
    let fixture = TestFixture::new("queued-remove-cancels-idle-sweep");
    let instance_id = fixture.add_instance("Managed", "1.20.4-fabric");
    let lock_path = seed_managed_lock(&fixture.state, &instance_id, "sweep-blocked");
    let idle = fixture.state.subscribe_integrity_idle();
    let sweep_producer = fixture
        .state
        .try_claim_producer()
        .expect("claim idle sweep producer");
    let idle_epoch = idle.borrow().epoch();
    let reservation = fixture
        .state
        .try_reserve_idle_sweep(idle_epoch, sweep_producer)
        .expect("reserve idle sweep");
    let cancellation = reservation.cancellation();
    let request_state = fixture.state.clone();
    let request_instance = instance_id.clone();
    let request = tokio::spawn(async move {
        handle_install(
            State(request_state),
            Json(InstallRequest {
                instance_id: Some(request_instance),
                game_version: None,
                loader: None,
                mode: None,
                action: Some("remove".to_string()),
                rollback_id: None,
                queued: Some(true),
            }),
        )
        .await
    });

    tokio::time::timeout(Duration::from_secs(2), async {
        while !cancellation.is_cancelled() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("performance registration cancels idle sweep");
    assert!(lock_path.is_file(), "effect waits for sweep settlement");
    assert!(
        !request.is_finished(),
        "queue ownership waits for settlement"
    );
    reservation.settle(IdleSweepTerminal::Cancelled);

    let Json(response) = tokio::time::timeout(Duration::from_secs(2), request)
        .await
        .expect("queued ownership settles after sweep cancellation")
        .expect("queued request task")
        .expect("queued remove accepted after sweep settlement");
    let install_id = response.install_id.expect("queued response has install id");
    let events = tokio::time::timeout(
        Duration::from_secs(2),
        collect_install_events(&fixture.state, &install_id),
    )
    .await
    .expect("queued terminal publication settles after sweep cancellation");
    assert!(events.last().is_some_and(|event| event.done));
    assert!(!lock_path.exists(), "remove effect runs after settlement");
    wait_for_integrity_idle(&fixture.state, true).await;
}

#[tokio::test]
async fn queued_rollback_without_snapshot_emits_terminal_error() {
    let fixture = TestFixture::new("queued-rollback-missing");
    let instance_id = fixture.add_instance("Managed", "1.20.4-fabric");

    let Json(response) = handle_install(
        State(fixture.state.clone()),
        Json(InstallRequest {
            instance_id: Some(instance_id),
            game_version: None,
            loader: None,
            mode: None,
            action: Some("rollback".to_string()),
            rollback_id: None,
            queued: Some(true),
        }),
    )
    .await
    .expect("queued rollback should be accepted");

    assert_eq!(response.status, "queued");
    let install_id = response.install_id.expect("queued response has install id");
    let events = collect_install_events(&fixture.state, &install_id).await;
    let terminal = events.last().expect("terminal event");
    assert_eq!(terminal.phase, "error");
    assert!(terminal.done);
    assert_eq!(
        terminal.error.as_deref(),
        Some("no performance rollback snapshot available")
    );
    let status = fixture
        .state
        .performance_operations()
        .get(&install_id)
        .await
        .expect("durable operation status");
    assert_eq!(status.action, "rollback");
    assert_eq!(status.state, "failed");
    assert_eq!(
        status.error.as_deref(),
        Some("no performance rollback snapshot available")
    );
    let journal = fixture
        .state
        .journals()
        .get(&crate::state::contracts::OperationId::new(
            install_id.clone(),
        ))
        .expect("queued operation journal");
    assert_eq!(
        journal.status,
        crate::state::contracts::OperationStatus::Failed
    );

    let response = router()
        .with_state(fixture.state.clone())
        .oneshot(
            Request::builder()
                .uri(format!("/api/v1/performance/operations/{install_id}"))
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("operation status response");
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read operation body");
    let body = String::from_utf8(body.to_vec()).expect("operation body utf8");
    let value: serde_json::Value = serde_json::from_str(&body).expect("operation json");
    assert_eq!(value["id"], install_id);
    assert_eq!(value["state"], "failed");
    assert_eq!(
        value["view_model"],
        serde_json::json!({
            "state_label": "Failed",
            "tone": "err",
            "title": "Bundle update failed",
            "detail": "no performance rollback snapshot available",
            "progress": {
                "phase": "error",
                "current": 4,
                "total": 4,
                "done": true,
            },
            "is_terminal": true,
            "is_complete": false,
        })
    );
    assert_eq!(value["proof"]["operation_id"], install_id);
    assert_eq!(value["proof"]["command"], "ApplyPerformancePlan");
    assert_eq!(value["proof"]["status"], "Failed");
    assert_eq!(value["proof"]["outcome"], "Failed");
    assert_eq!(value["proof"]["failure_point"], "rollback_performance_plan");
    assert_eq!(value["proof"]["rollback"], "Unavailable");
    assert!(
        value["proof"]["fields"]
            .as_array()
            .expect("proof fields")
            .iter()
            .any(|field| field["key"] == "generated_fact"
                && field["value"] == "performance_operation_evidence")
    );
    assert_omits_raw_fragments(
        &body,
        &[
            "/Users/alice",
            "C:\\Users\\Alice",
            "provider_payload",
            "secret-token",
            "-Xmx8192M",
            "java_path",
        ],
    );

    let response = router()
        .with_state(fixture.state.clone())
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/api/v1/performance/instances/{}/operation",
                    status.instance_id
                ))
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("instance operation response");
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read instance operation body");
    let value: serde_json::Value = serde_json::from_slice(&body).expect("instance operation json");
    assert_eq!(value["operation"]["id"], install_id);
    assert_eq!(value["operation"]["view_model"]["is_terminal"], true);
    assert_eq!(value["operation"]["view_model"]["tone"], "err");
    assert_eq!(value["operation"]["proof"]["operation_id"], install_id);
}

#[tokio::test]
async fn terminal_journal_failure_retries_before_status_and_stream_release() {
    let root = test_root("queued-terminal-journal-retry");
    let journal_backend = Arc::new(ScriptedOperationBackend::default());
    journal_backend.fail_attempt(5);
    journal_backend.gate_attempt(6);
    let status_backend = Arc::new(ScriptedOperationBackend::default());
    let state =
        build_test_state_with_operation_backends(&root, journal_backend.clone(), status_backend);
    let instance_id = insert_persisted_test_instance(&state, "Managed", "1.20.4-fabric")
        .await
        .id;
    assert!(state.subscribe_integrity_idle().borrow().is_stably_idle());

    let Json(response) = handle_install(
        State(state.clone()),
        Json(InstallRequest {
            instance_id: Some(instance_id),
            game_version: None,
            loader: None,
            mode: None,
            action: Some("remove".to_string()),
            rollback_id: None,
            queued: Some(true),
        }),
    )
    .await
    .expect("queue accepted");
    let install_id = response.install_id.expect("install id");
    journal_backend.wait_for_attempt(6).await;
    assert!(
        !state.subscribe_integrity_idle().borrow().is_stably_idle(),
        "terminal journal persistence retains performance foreground"
    );

    let (events, _, done) = state
        .installs()
        .subscribe(&install_id)
        .await
        .expect("progress session");
    assert!(!done);
    assert!(events.iter().all(|event| !event.done));
    let status = state
        .performance_operations()
        .get(&install_id)
        .await
        .expect("status remains owned");
    assert_eq!(status.state, PERFORMANCE_COMMITTING_COMPLETE_STATE);
    assert_eq!(
        state
            .performance_operations()
            .current_or_latest_for_instance(&status.instance_id)
            .await
            .expect("active status")
            .id,
        install_id
    );
    let journal = state
        .journals()
        .get(&crate::state::contracts::OperationId::new(
            install_id.clone(),
        ))
        .expect("nonterminal terminal-intent journal");
    assert!(!performance_journal_is_terminal(journal.status));
    assert!(journal.completed_steps.iter().any(|step| {
        step.generated_facts
            .iter()
            .any(|fact| fact == "performance_terminal_success_v1")
    }));

    journal_backend.release();
    let events = collect_install_events(&state, &install_id).await;
    assert!(events.last().is_some_and(|event| event.done));
    wait_for_integrity_idle(&state, true).await;
    assert_eq!(
        state
            .performance_operations()
            .get(&install_id)
            .await
            .expect("terminal status")
            .state,
        "complete"
    );
    assert!(performance_journal_is_terminal(
        state
            .journals()
            .get(&crate::state::contracts::OperationId::new(
                install_id.clone()
            ))
            .expect("terminal journal")
            .status
    ));
    let terminal = state
        .journals()
        .get(&crate::state::contracts::OperationId::new(install_id))
        .expect("terminal journal checkpoints");
    assert_eq!(
        terminal
            .completed_steps
            .iter()
            .filter(|step| step.step_id == "performance_effect_started")
            .count(),
        1
    );
    assert_eq!(
        terminal
            .completed_steps
            .iter()
            .filter(|step| step.step_id == "performance_terminal_intent")
            .count(),
        1
    );
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn pre_effect_status_acceptance_failure_does_not_run_filesystem_effect() {
    let root = test_root("pre-effect-status-acceptance");
    let journal_backend = Arc::new(ScriptedOperationBackend::default());
    let status_backend = Arc::new(ScriptedOperationBackend::default());
    let state = build_test_state_with_operation_backends(
        &root,
        journal_backend.clone(),
        status_backend.clone(),
    );
    let instance_id = insert_persisted_test_instance(&state, "Managed", "1.20.4-fabric")
        .await
        .id;
    let lock_path = seed_managed_lock(&state, &instance_id, "pre-effect-preserved");
    let status = state
        .performance_operations()
        .start_with_identity(
            instance_id.clone(),
            "remove".to_string(),
            test_operation_payload(),
            crate::state::performance_operations::PerformanceOperationJournalIdentity::new(
                "remove",
                "pre-effect-preserved",
                RollbackState::Available,
            ),
        )
        .await
        .expect("status starts");
    state
        .performance_operations()
        .close()
        .await
        .expect("close status writer before effect checkpoint");
    state.installs().insert(status.id.clone()).await;
    let foreground = state
        .register_integrity_foreground()
        .expect("register pre-effect status foreground")
        .wait_for_settlement()
        .await;

    tokio::time::timeout(
        Duration::from_secs(2),
        run_queued_performance_operation(
            state.clone(),
            PerformanceOperation {
                instance_id,
                game_version: None,
                loader: None,
                mode: None,
                action: PerformanceInstallAction::Remove,
                rollback_id: None,
                status_operation_id: Some(status.id.clone()),
                persistence_failure: None,
                installed_versions: None,
            },
            state.installs().clone(),
            status.id.clone(),
            foreground,
        ),
    )
    .await
    .expect("acceptance rejection returns to reconciliation");

    assert!(lock_path.is_file(), "filesystem effect must not run");
    assert!(
        !state
            .performance_operations()
            .has_retry_candidate(&status.id)
    );
    assert_eq!(
        state
            .performance_operations()
            .get(&status.id)
            .await
            .expect("unpromoted status")
            .state,
        "queued"
    );
    let journal = state
        .journals()
        .get(&crate::state::contracts::OperationId::new(status.id))
        .expect("uncertain effect journal is terminalized first");
    assert_eq!(
        journal.status,
        crate::state::contracts::OperationStatus::Failed
    );
    assert_eq!(
        journal
            .completed_steps
            .iter()
            .filter(|step| step.step_id == "performance_effect_started")
            .count(),
        1
    );
    assert_eq!(status_backend.attempts.load(Ordering::SeqCst), 1);
    assert!(journal_backend.attempts.load(Ordering::SeqCst) >= 5);
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn competing_status_retrier_committing_own_candidate_is_accepted_exactly() {
    let root = test_root("status-competing-retrier");
    let journal_backend = Arc::new(ScriptedOperationBackend::default());
    let status_backend = Arc::new(ScriptedOperationBackend::default());
    status_backend.fail_attempt(2);
    status_backend.gate_attempt(3);
    let state =
        build_test_state_with_operation_backends(&root, journal_backend, status_backend.clone());
    let status = state
        .performance_operations()
        .start(
            "instance-a".to_string(),
            "install".to_string(),
            test_operation_payload(),
        )
        .await
        .expect("status starts");
    let failed = state
        .performance_operations()
        .record_effect_started(&status.id)
        .await;
    assert!(matches!(
        failed,
        Err(crate::state::performance_operations::PerformanceOperationStoreError::Persistence(_))
    ));

    let competing_state = state.clone();
    let competing_id = status.id.clone();
    let competing = tokio::spawn(async move {
        competing_state
            .performance_operations()
            .retry_critical(&competing_id)
            .await
    });
    status_backend.wait_for_attempt(3).await;
    let retry_state = state.clone();
    let retry_id = crate::state::contracts::OperationId::new(status.id.clone());
    let requested = tokio::spawn(async move {
        retry_performance_status_transition(
            &retry_state,
            &retry_id,
            "effect_started",
            None,
            failed,
            None,
        )
        .await
    });
    tokio::time::sleep(Duration::from_millis(60)).await;
    status_backend.release();

    competing
        .await
        .expect("competing retry task")
        .expect("competing retry commits candidate");
    requested
        .await
        .expect("requested retry task")
        .expect("exact committed status is accepted");
    assert_eq!(
        state
            .performance_operations()
            .get(&status.id)
            .await
            .expect("effect status")
            .state,
        "effect_started"
    );
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn cleared_foreign_status_candidate_reapplies_requested_transition() {
    for (name, fail_foreign_retry) in [
        ("status-foreign-cleared", false),
        ("status-foreign-retry-fails", true),
    ] {
        let root = test_root(name);
        let journal_backend = Arc::new(ScriptedOperationBackend::default());
        let status_backend = Arc::new(ScriptedOperationBackend::default());
        status_backend.fail_attempt(2);
        if fail_foreign_retry {
            status_backend.fail_attempt(3);
        }
        let state =
            build_test_state_with_operation_backends(&root, journal_backend, status_backend);
        let status = state
            .performance_operations()
            .start(
                "instance-a".to_string(),
                "install".to_string(),
                test_operation_payload(),
            )
            .await
            .expect("status starts");
        state
            .performance_operations()
            .record_effect_started(&status.id)
            .await
            .expect_err("older transition fails persistence");
        let requested = state
            .performance_operations()
            .record_committing_complete(&status.id)
            .await;
        assert!(matches!(
            requested,
            Err(
                crate::state::performance_operations::PerformanceOperationStoreError::RetryRequired
            )
        ));
        if !fail_foreign_retry {
            state
                .performance_operations()
                .retry_critical(&status.id)
                .await
                .expect("foreign candidate clears before classification");
        }

        retry_performance_status_transition(
            &state,
            &crate::state::contracts::OperationId::new(status.id.clone()),
            PERFORMANCE_COMMITTING_COMPLETE_STATE,
            None,
            requested,
            None,
        )
        .await
        .expect("requested transition is applied after foreign candidate");
        assert_eq!(
            state
                .performance_operations()
                .get(&status.id)
                .await
                .expect("committing status")
                .state,
            PERFORMANCE_COMMITTING_COMPLETE_STATE
        );
        let _ = fs::remove_dir_all(root);
    }
}

#[tokio::test]
async fn terminal_status_correction_preserves_authority_after_older_candidate() {
    let root = test_root("status-correction-foreign-candidate");
    let journal_backend = Arc::new(ScriptedOperationBackend::default());
    let status_backend = Arc::new(ScriptedOperationBackend::default());
    status_backend.fail_attempt(3);
    let state = build_test_state_with_operation_backends(&root, journal_backend, status_backend);
    let status = state
        .performance_operations()
        .start(
            "instance-a".to_string(),
            "install".to_string(),
            test_operation_payload(),
        )
        .await
        .expect("status starts");
    state
        .performance_operations()
        .record_complete(&status.id)
        .await
        .expect("status completes");
    state
        .performance_operations()
        .record_reconciliation_failed(&status.id, "older correction", "install")
        .await
        .expect_err("older correction fails persistence");
    let requested = state
        .performance_operations()
        .record_reconciliation_failed(&status.id, "requested correction", "install")
        .await;
    assert!(matches!(
        requested,
        Err(crate::state::performance_operations::PerformanceOperationStoreError::RetryRequired)
    ));

    retry_performance_status_correction(
        &state,
        &crate::state::contracts::OperationId::new(status.id.clone()),
        PerformanceInstallAction::Install,
        "requested correction",
        requested,
    )
    .await
    .expect("requested correction applies after older candidate");
    let corrected = state
        .performance_operations()
        .get(&status.id)
        .await
        .expect("corrected status");
    assert_eq!(corrected.state, "failed");
    assert_eq!(corrected.error.as_deref(), Some("requested correction"));
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn synchronous_effect_status_commit_failure_terminalizes_without_running_effect() {
    let root = test_root("sync-effect-status-failure");
    let journal_backend = Arc::new(ScriptedOperationBackend::default());
    let status_backend = Arc::new(ScriptedOperationBackend::default());
    journal_backend.gate_attempt(3);
    let state = build_test_state_with_operation_backends(
        &root,
        journal_backend.clone(),
        status_backend.clone(),
    );
    let instance_id = insert_persisted_test_instance(&state, "Managed", "1.20.4-fabric")
        .await
        .id;
    let lock_path = seed_managed_lock(&state, &instance_id, "status-failure-preserved");
    let request_state = state.clone();
    let request_instance = instance_id.clone();
    let request = tokio::spawn(async move {
        handle_install(
            State(request_state),
            Json(InstallRequest {
                instance_id: Some(request_instance),
                game_version: None,
                loader: None,
                mode: None,
                action: Some("remove".to_string()),
                rollback_id: None,
                queued: Some(false),
            }),
        )
        .await
    });

    journal_backend.wait_for_attempt(3).await;
    let effect_status_attempt = status_backend.attempts.load(Ordering::SeqCst) + 1;
    status_backend.fail_attempt(effect_status_attempt);
    status_backend.gate_attempt(effect_status_attempt + 1);
    journal_backend.release();
    status_backend
        .wait_for_attempt(effect_status_attempt + 1)
        .await;
    let error = tokio::time::timeout(Duration::from_millis(500), request)
        .await
        .expect("sync status failure response is bounded")
        .expect("sync request task")
        .expect_err("status persistence failure is public failure");
    assert_eq!(error.0, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(json_error_message(&error), PERFORMANCE_JOURNAL_ERROR);
    assert!(lock_path.is_file(), "effect waits behind durable status");

    status_backend.release();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if state
                .performance_operations()
                .current_or_latest_for_instance(&instance_id)
                .await
                .is_some_and(|status| status.state == "failed")
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("detached owner publishes failed status");
    let status = state
        .performance_operations()
        .current_or_latest_for_instance(&instance_id)
        .await
        .expect("terminal status");
    assert_eq!(status.state, "failed");
    assert!(
        !state
            .performance_operations()
            .has_retry_candidate(&status.id)
    );
    assert!(performance_journal_is_terminal(
        state
            .journals()
            .get(&crate::state::contracts::OperationId::new(status.id))
            .expect("terminal journal")
            .status
    ));
    assert!(lock_path.is_file(), "status failure never runs effect");
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn pre_effect_journal_acceptance_failure_exits_without_retry_or_filesystem_effect() {
    let root = test_root("pre-effect-journal-acceptance");
    let journal_backend = Arc::new(ScriptedOperationBackend::default());
    let status_backend = Arc::new(ScriptedOperationBackend::default());
    let state =
        build_test_state_with_operation_backends(&root, journal_backend.clone(), status_backend);
    let instance_id = insert_persisted_test_instance(&state, "Managed", "1.20.4-fabric")
        .await
        .id;
    let lock_path = seed_managed_lock(&state, &instance_id, "journal-acceptance-preserved");
    let status = state
        .performance_operations()
        .start(
            instance_id.clone(),
            "remove".to_string(),
            test_operation_payload(),
        )
        .await
        .expect("status starts");
    state
        .journals()
        .close()
        .await
        .expect("close journal owner before initial checkpoint");
    state.installs().insert(status.id.clone()).await;
    let foreground = state
        .register_integrity_foreground()
        .expect("register pre-effect journal foreground")
        .wait_for_settlement()
        .await;

    tokio::time::timeout(
        Duration::from_secs(2),
        run_queued_performance_operation(
            state.clone(),
            PerformanceOperation {
                instance_id,
                game_version: None,
                loader: None,
                mode: None,
                action: PerformanceInstallAction::Remove,
                rollback_id: None,
                status_operation_id: Some(status.id.clone()),
                persistence_failure: None,
                installed_versions: None,
            },
            state.installs().clone(),
            status.id.clone(),
            foreground,
        ),
    )
    .await
    .expect("non-retryable journal acceptance failure exits worker");

    assert!(lock_path.is_file(), "filesystem effect must not run");
    assert_eq!(journal_backend.attempts.load(Ordering::SeqCst), 0);
    assert!(!state.journals().has_retry_candidate());
    assert!(
        state
            .journals()
            .get(&crate::state::contracts::OperationId::new(
                status.id.clone()
            ))
            .is_none()
    );
    assert_eq!(
        state
            .performance_operations()
            .get(&status.id)
            .await
            .expect("status stays resumable")
            .state,
        "removing"
    );
    let (events, _, done) = state
        .installs()
        .subscribe(&status.id)
        .await
        .expect("progress session");
    assert!(!done);
    assert!(events.iter().all(|event| !event.done));
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn committed_guardian_evidence_is_reused_after_retry_without_rerun_loop() {
    let root = test_root("evidence-retry-reuse");
    let journal_backend = Arc::new(ScriptedOperationBackend::default());
    journal_backend.fail_attempt(2);
    let status_backend = Arc::new(ScriptedOperationBackend::default());
    let state = build_test_state_with_operation_backends(&root, journal_backend, status_backend);
    let instance_id = insert_persisted_test_instance(&state, "Managed", "1.20.4-fabric")
        .await
        .id;
    let lock_path = seed_managed_lock(&state, &instance_id, "evidence-retry");

    let Json(response) = handle_install(
        State(state.clone()),
        Json(InstallRequest {
            instance_id: Some(instance_id),
            game_version: None,
            loader: None,
            mode: None,
            action: Some("remove".to_string()),
            rollback_id: None,
            queued: Some(true),
        }),
    )
    .await
    .expect("queue accepted");
    let install_id = response.install_id.expect("install id");
    let events = collect_install_events(&state, &install_id).await;
    assert!(events.last().is_some_and(|event| event.done));
    assert!(!lock_path.exists(), "remove effect runs once after retry");
    let journal = state
        .journals()
        .get(&crate::state::contracts::OperationId::new(install_id))
        .expect("terminal journal");
    assert_eq!(
        journal.status,
        crate::state::contracts::OperationStatus::Succeeded
    );
    assert_eq!(
        journal
            .completed_steps
            .iter()
            .filter(|step| step.step_id == "performance_effect_started")
            .count(),
        1
    );
    assert_eq!(
        journal
            .completed_steps
            .iter()
            .filter(|step| step.step_id == "performance_terminal_intent")
            .count(),
        1
    );
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn failed_start_returns_bounded_error_then_detached_owner_terminalizes_without_effect() {
    let root = test_root("failed-start-detached-reconcile");
    let journal_backend = Arc::new(ScriptedOperationBackend::default());
    let status_backend = Arc::new(ScriptedOperationBackend::default());
    status_backend.set_fail_all(true);
    let state =
        build_test_state_with_operation_backends(&root, journal_backend, status_backend.clone());
    let instance_id = insert_persisted_test_instance(&state, "Managed", "1.20.4-fabric")
        .await
        .id;
    let lock_path = seed_managed_lock(&state, &instance_id, "failed-start-preserved");
    let payload = || InstallRequest {
        instance_id: Some(instance_id.clone()),
        game_version: None,
        loader: None,
        mode: None,
        action: Some("remove".to_string()),
        rollback_id: None,
        queued: Some(true),
    };

    let error = tokio::time::timeout(
        Duration::from_secs(2),
        handle_install(State(state.clone()), Json(payload())),
    )
    .await
    .expect("bounded start response")
    .expect_err("failed physical start returns 500");
    assert_eq!(error.0, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(json_error_message(&error), PERFORMANCE_JOURNAL_ERROR);
    let retry_ids = state
        .performance_operations()
        .retry_candidate_ids_for_test();
    assert_eq!(retry_ids.len(), 1);
    let install_id = retry_ids[0].clone();
    let conflict = handle_install(State(state.clone()), Json(payload()))
        .await
        .expect_err("hidden failed start reserves instance");
    assert_eq!(conflict.0, StatusCode::CONFLICT);
    assert!(lock_path.is_file());

    status_backend.set_fail_all(false);
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if state
                .performance_operations()
                .get(&install_id)
                .await
                .is_some_and(|status| status.state == "failed")
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("detached failed start terminalizes");
    assert!(lock_path.is_file(), "failed start must never run effect");
    assert!(performance_journal_is_terminal(
        state
            .journals()
            .get(&crate::state::contracts::OperationId::new(install_id))
            .expect("failed-start journal")
            .status
    ));
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn panicked_performance_worker_is_supervised_to_terminal_authority() {
    let fixture = TestFixture::new("panicked-performance-worker-supervision");
    let instance_id = fixture.add_instance("Managed", "1.20.4-fabric");
    let reservation = fixture
        .state
        .performance_operations()
        .reserve_operation_id();
    let operation_id = reservation.operation_id().to_string();
    let identity = PerformanceWorkerIdentity::default();
    identity.set(&operation_id);
    let producer = fixture
        .state
        .try_claim_producer()
        .expect("claim supervised worker producer");
    let foreground = fixture
        .state
        .register_integrity_foreground()
        .expect("register supervised worker foreground")
        .wait_for_settlement()
        .await;

    supervise_performance_worker(
        fixture.state.clone(),
        PerformanceInstallAction::Remove,
        identity,
        producer,
        foreground,
        {
            let worker_state = fixture.state.clone();
            move |_runtime_owner, _worker_foreground| async move {
                worker_state
                    .performance_operations()
                    .start_reserved_with_identity(
                        reservation,
                        instance_id,
                        "remove".to_string(),
                        test_operation_payload(),
                        crate::state::performance_operations::PerformanceOperationJournalIdentity::new(
                            "remove",
                            "panic-supervision",
                            RollbackState::Unavailable,
                        ),
                    )
                    .await
                    .expect("durable performance start succeeds before panic");
                panic!("injected performance worker panic");
            }
        },
    )
    .await;

    let terminal = fixture
        .state
        .performance_operations()
        .get(&operation_id)
        .await
        .expect("supervised terminal status");
    assert_eq!(terminal.state, "failed");
    assert_eq!(
        terminal.error.as_deref(),
        Some("performance operation stopped before its result could be confirmed")
    );
    let events = collect_install_events(&fixture.state, &operation_id).await;
    assert!(events.last().is_some_and(|event| event.done));
    assert!(performance_journal_is_terminal(
        fixture
            .state
            .journals()
            .get(&crate::state::contracts::OperationId::new(operation_id))
            .expect("supervised terminal journal")
            .status
    ));
    wait_for_integrity_idle(&fixture.state, true).await;
}

#[tokio::test]
async fn interrupted_worker_retains_foreground_through_terminal_persistence_retry() {
    let root = test_root("performance-supervisor-terminal-persistence");
    let journal_backend = Arc::new(ScriptedOperationBackend::default());
    journal_backend.fail_attempt(1);
    journal_backend.gate_attempt(2);
    let status_backend = Arc::new(ScriptedOperationBackend::default());
    let state =
        build_test_state_with_operation_backends(&root, journal_backend.clone(), status_backend);
    let instance_id = insert_persisted_test_instance(&state, "Managed", "1.20.4-fabric")
        .await
        .id;
    let status = state
        .performance_operations()
        .start_with_identity(
            instance_id,
            "remove".to_string(),
            test_operation_payload(),
            crate::state::performance_operations::PerformanceOperationJournalIdentity::new(
                "remove",
                "terminal-persistence-retry",
                RollbackState::Unavailable,
            ),
        )
        .await
        .expect("start persistence-gated status");
    let identity = PerformanceWorkerIdentity::default();
    identity.set(&status.id);
    let producer = state
        .try_claim_producer()
        .expect("claim persistence-gated supervisor");
    let foreground = state
        .register_integrity_foreground()
        .expect("register persistence-gated foreground")
        .wait_for_settlement()
        .await;
    let supervisor_state = state.clone();
    let supervisor = tokio::spawn(supervise_performance_worker(
        supervisor_state,
        PerformanceInstallAction::Remove,
        identity,
        producer,
        foreground,
        |_runtime_owner, _worker_foreground| async move {
            panic!("injected persistence-gated worker panic");
        },
    ));

    journal_backend.wait_for_attempt(2).await;
    assert!(!supervisor.is_finished());
    assert!(
        !state.subscribe_integrity_idle().borrow().is_stably_idle(),
        "terminal persistence retry retains foreground authority"
    );
    journal_backend.release();
    tokio::time::timeout(Duration::from_secs(3), supervisor)
        .await
        .expect("supervision settles after persistence retry")
        .expect("supervision task");
    assert_eq!(
        state
            .performance_operations()
            .get(&status.id)
            .await
            .expect("terminal persistence status")
            .state,
        "failed"
    );
    wait_for_integrity_idle(&state, true).await;
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn terminal_supervisor_releases_unsettled_authority_only_after_integrity_shutdown() {
    let fixture = TestFixture::new("performance-supervisor-shutdown-escape");
    let instance_id = fixture.add_instance("Managed", "1.20.4-fabric");
    let status = fixture
        .state
        .performance_operations()
        .start_with_identity(
            instance_id,
            "remove".to_string(),
            test_operation_payload(),
            crate::state::performance_operations::PerformanceOperationJournalIdentity::new(
                "remove",
                "shutdown-escape",
                RollbackState::Unavailable,
            ),
        )
        .await
        .expect("start shutdown escape status");
    fixture
        .state
        .journals()
        .close()
        .await
        .expect("close journal authority before supervision");
    let identity = PerformanceWorkerIdentity::default();
    identity.set(&status.id);
    let producer = fixture
        .state
        .try_claim_producer()
        .expect("claim shutdown escape supervisor");
    let foreground = fixture
        .state
        .register_integrity_foreground()
        .expect("register shutdown escape foreground")
        .wait_for_settlement()
        .await;
    let supervisor_state = fixture.state.clone();
    let supervisor = tokio::spawn(supervise_performance_worker(
        supervisor_state,
        PerformanceInstallAction::Remove,
        identity,
        producer,
        foreground,
        |_runtime_owner, _worker_foreground| async move {
            panic!("injected shutdown escape worker panic");
        },
    ));
    tokio::task::yield_now().await;
    assert!(!supervisor.is_finished());

    let shutdown_state = fixture.state.clone();
    let shutdown = tokio::spawn(async move { shutdown_state.quiesce().await });
    tokio::time::timeout(Duration::from_secs(3), supervisor)
        .await
        .expect("supervisor exits after integrity shutdown")
        .expect("shutdown supervisor task");
    tokio::time::timeout(Duration::from_secs(3), shutdown)
        .await
        .expect("shutdown joins escaped supervisor")
        .expect("shutdown task")
        .expect("state quiesces");
    assert_eq!(
        fixture
            .state
            .performance_operations()
            .get(&status.id)
            .await
            .expect("unsettled shutdown status remains durable")
            .state,
        "queued"
    );
}

#[tokio::test]
async fn aborted_queued_request_does_not_cancel_owned_start_or_worker() {
    let root = test_root("queued-request-abort");
    let journal_backend = Arc::new(ScriptedOperationBackend::default());
    let status_backend = Arc::new(ScriptedOperationBackend::default());
    status_backend.gate_attempt(1);
    let state =
        build_test_state_with_operation_backends(&root, journal_backend, status_backend.clone());
    let instance_id = insert_persisted_test_instance(&state, "Managed", "1.20.4-fabric")
        .await
        .id;
    let lock_path = seed_managed_lock(&state, &instance_id, "abort-owned");
    let request_state = state.clone();
    let request_instance = instance_id.clone();
    let request = tokio::spawn(async move {
        handle_install(
            State(request_state),
            Json(InstallRequest {
                instance_id: Some(request_instance),
                game_version: None,
                loader: None,
                mode: None,
                action: Some("remove".to_string()),
                rollback_id: None,
                queued: Some(true),
            }),
        )
        .await
    });
    status_backend.wait_for_attempt(1).await;
    assert!(
        !state.subscribe_integrity_idle().borrow().is_stably_idle(),
        "queued start persistence retains performance foreground"
    );
    request.abort();
    assert!(
        request
            .await
            .expect_err("request task is cancelled at start gate")
            .is_cancelled()
    );
    assert!(
        !state.subscribe_integrity_idle().borrow().is_stably_idle(),
        "request cancellation cannot release detached performance foreground"
    );
    let shutdown_state = state.clone();
    let quiesce = tokio::spawn(async move { shutdown_state.quiesce().await });
    tokio::time::timeout(Duration::from_secs(1), async {
        while state.lifecycle_phase() != crate::state::AppLifecyclePhase::QuiescingProducers {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("performance producer quiescence begins");
    assert!(!quiesce.is_finished());
    status_backend.release();

    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if state
                .performance_operations()
                .current_or_latest_for_instance(&instance_id)
                .await
                .is_some_and(|status| status.state == "complete")
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("detached queue owner completes after request abort");
    assert!(!lock_path.exists(), "owned worker still applies effect");
    let status = state
        .performance_operations()
        .current_or_latest_for_instance(&instance_id)
        .await
        .expect("terminal operation");
    assert!(performance_journal_is_terminal(
        state
            .journals()
            .get(&crate::state::contracts::OperationId::new(status.id))
            .expect("terminal journal")
            .status
    ));
    quiesce
        .await
        .expect("quiesce task")
        .expect("owned performance worker settles before quiescence");
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn synchronous_planned_commit_failure_is_bounded_and_never_runs_effect() {
    let root = test_root("sync-planned-failure");
    let journal_backend = Arc::new(ScriptedOperationBackend::default());
    journal_backend.fail_attempt(1);
    journal_backend.gate_attempt(2);
    let status_backend = Arc::new(ScriptedOperationBackend::default());
    let state =
        build_test_state_with_operation_backends(&root, journal_backend.clone(), status_backend);
    let instance_id = insert_persisted_test_instance(&state, "Managed", "1.20.4-fabric")
        .await
        .id;
    let lock_path = seed_managed_lock(&state, &instance_id, "sync-planned-preserved");
    let request_state = state.clone();
    let request_instance = instance_id.clone();
    let request = tokio::spawn(async move {
        handle_install(
            State(request_state),
            Json(InstallRequest {
                instance_id: Some(request_instance),
                game_version: None,
                loader: None,
                mode: None,
                action: Some("remove".to_string()),
                rollback_id: None,
                queued: Some(false),
            }),
        )
        .await
    });

    journal_backend.wait_for_attempt(2).await;
    let error = tokio::time::timeout(Duration::from_millis(500), request)
        .await
        .expect("sync failure response is bounded")
        .expect("sync request task")
        .expect_err("planned persistence failure is public failure");
    assert_eq!(error.0, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(json_error_message(&error), PERFORMANCE_JOURNAL_ERROR);
    assert!(
        lock_path.is_file(),
        "effect is gated while retry is pending"
    );

    journal_backend.release();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if state
                .performance_operations()
                .current_or_latest_for_instance(&instance_id)
                .await
                .is_some_and(|status| status.state == "failed")
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("detached owner terminalizes planned failure");
    assert!(
        lock_path.is_file(),
        "failed planned commit never runs effect"
    );
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn synchronous_terminal_intent_failure_reconciles_without_restart_replay() {
    let root = test_root("sync-terminal-intent-restart");
    let journal_backend = Arc::new(ScriptedOperationBackend::default());
    journal_backend.fail_attempt(4);
    journal_backend.gate_attempt(5);
    let status_backend = Arc::new(ScriptedOperationBackend::default());
    let state =
        build_test_state_with_operation_backends(&root, journal_backend.clone(), status_backend);
    let instance_id = insert_persisted_test_instance(&state, "Managed", "1.20.4-fabric")
        .await
        .id;
    let lock_path = seed_managed_lock(&state, &instance_id, "sync-effect-once");
    let request_state = state.clone();
    let request_instance = instance_id.clone();
    let request = tokio::spawn(async move {
        handle_install(
            State(request_state),
            Json(InstallRequest {
                instance_id: Some(request_instance),
                game_version: None,
                loader: None,
                mode: None,
                action: Some("remove".to_string()),
                rollback_id: None,
                queued: Some(false),
            }),
        )
        .await
    });

    journal_backend.wait_for_attempt(5).await;
    let error = tokio::time::timeout(Duration::from_millis(500), request)
        .await
        .expect("terminal persistence failure response is bounded")
        .expect("sync request task")
        .expect_err("terminal persistence failure returns bounded error");
    assert_eq!(json_error_message(&error), PERFORMANCE_JOURNAL_ERROR);
    assert!(
        !lock_path.exists(),
        "effect completed before terminal failure"
    );
    journal_backend.release();

    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if state
                .performance_operations()
                .current_or_latest_for_instance(&instance_id)
                .await
                .is_some_and(|status| status.state == "complete")
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("detached owner publishes terminal success");
    let sentinel = seed_managed_lock(&state, &instance_id, "restart-sentinel");
    state
        .performance_operations()
        .close()
        .await
        .expect("close status store");
    state.journals().close().await.expect("close journal store");
    drop(state);

    let reloaded = build_test_state(&root, None, None);
    assert_eq!(resume_pending_performance_operations(reloaded).await, 0);
    assert!(
        sentinel.is_file(),
        "terminal status prevents restart replay"
    );
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn restart_terminalizes_mismatched_gated_journal_without_spinning() {
    let root = test_root("restart-mismatched-gate");
    let state = build_test_state(&root, None, None);
    let instance_id = insert_persisted_test_instance(&state, "Managed", "1.20.4-fabric")
        .await
        .id;
    let lock_path = seed_managed_lock(&state, &instance_id, "expected-target");
    let status = state
        .performance_operations()
        .start_with_identity(
            instance_id,
            "remove".to_string(),
            test_operation_payload(),
            crate::state::performance_operations::PerformanceOperationJournalIdentity::new(
                "remove",
                "expected-target",
                RollbackState::Available,
            ),
        )
        .await
        .expect("persist status");
    begin_performance_operation_journal(
        &state,
        PerformanceInstallAction::Remove,
        "expected-target",
        RollbackState::Available,
        Some(&status.id),
    )
    .await
    .expect("persist gated journal");
    state
        .performance_operations()
        .close()
        .await
        .expect("close status store");
    state.journals().close().await.expect("close journal store");
    drop(state);
    rewrite_performance_journal_targets(&root, &status.id, "wrong-target");

    let reloaded = build_test_state(&root, None, None);
    tokio::time::timeout(
        Duration::from_secs(2),
        resume_pending_performance_operations(reloaded.clone()),
    )
    .await
    .expect("mismatched resume is bounded");
    assert_eq!(
        reloaded
            .performance_operations()
            .get(&status.id)
            .await
            .expect("mismatched status retained")
            .state,
        "failed"
    );
    assert!(
        lock_path.is_file(),
        "mismatched gate cannot authorize effect"
    );
    assert!(
        reloaded
            .journals()
            .get(&crate::state::contracts::OperationId::new(format!(
                "{}-reconciliation",
                status.id
            )))
            .is_some_and(|journal| performance_journal_is_terminal(journal.status))
    );
    assert!(
        reloaded
            .journals()
            .get(&crate::state::contracts::OperationId::new(
                status.id.clone()
            ))
            .is_some_and(|journal| performance_journal_is_terminal(journal.status)),
        "same-ID active journal is terminal before reconciliation evidence"
    );
    assert!(
        performance_operation_status(&reloaded, &status.id)
            .await
            .expect("mismatched public status")
            .proof
            .is_none()
    );
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn repeated_mismatches_terminalize_at_full_journal_capacity() {
    let root = test_root("mismatch-capacity");
    let state = build_test_state(&root, None, None).with_operation_stores(
        Arc::new(OperationJournalStore::with_max_entries(4)),
        Arc::new(PerformanceOperationStore::new()),
    );
    let mut statuses = Vec::new();
    for index in 0..4 {
        let status = state
            .performance_operations()
            .start_with_identity(
                format!("instance-{index}"),
                "remove".to_string(),
                test_operation_payload(),
                crate::state::performance_operations::PerformanceOperationJournalIdentity::new(
                    "remove",
                    format!("expected-{index}"),
                    RollbackState::Unavailable,
                ),
            )
            .await
            .expect("status starts");
        state
            .journals()
            .create(test_mismatched_performance_journal(
                &status.id,
                &format!("actual-{index}"),
                false,
            ))
            .await
            .expect("active mismatched journal starts");
        statuses.push(status);
    }

    for status in &statuses {
        assert!(
            terminalize_mismatched_performance_operation(
                &state,
                state.installs(),
                status,
                PerformanceInstallAction::Remove,
                "restart reconciliation failed",
            )
            .await,
            "full active capacity must not block mismatch reconciliation"
        );
        assert_eq!(
            state
                .performance_operations()
                .get(&status.id)
                .await
                .expect("status retained")
                .state,
            "failed"
        );
        assert!(
            performance_operation_status(&state, &status.id)
                .await
                .expect("public mismatch status")
                .proof
                .is_none()
        );
        assert!(
            state
                .journals()
                .get(&crate::state::contracts::OperationId::new(format!(
                    "{}-reconciliation",
                    status.id
                )))
                .is_some_and(|journal| performance_journal_is_terminal(journal.status))
        );
    }
    assert_eq!(state.journals().list().len(), 4);
    assert!(
        state
            .journals()
            .list()
            .iter()
            .all(|journal| performance_journal_is_terminal(journal.status))
    );
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn malformed_and_identityless_mismatches_fail_closed_without_proof() {
    for (name, identity, malformed) in [
        ("mismatch-malformed-journal", true, true),
        ("mismatch-identityless-status", false, false),
    ] {
        let root = test_root(name);
        let state = build_test_state(&root, None, None).with_operation_stores(
            Arc::new(OperationJournalStore::with_max_entries(8)),
            Arc::new(PerformanceOperationStore::new()),
        );
        let status = if identity {
            state
                .performance_operations()
                .start_with_identity(
                    "instance-a".to_string(),
                    "remove".to_string(),
                    test_operation_payload(),
                    crate::state::performance_operations::PerformanceOperationJournalIdentity::new(
                        "remove",
                        "expected-target",
                        RollbackState::Unavailable,
                    ),
                )
                .await
                .expect("identified status starts")
        } else {
            state
                .performance_operations()
                .start(
                    "instance-a".to_string(),
                    "remove".to_string(),
                    test_operation_payload(),
                )
                .await
                .expect("identityless status starts")
        };
        state
            .journals()
            .create(test_mismatched_performance_journal(
                &status.id,
                "actual-target",
                malformed,
            ))
            .await
            .expect("mismatched journal starts");

        assert!(
            terminalize_mismatched_performance_operation(
                &state,
                state.installs(),
                &status,
                PerformanceInstallAction::Remove,
                "restart reconciliation failed",
            )
            .await
        );
        let original = state
            .journals()
            .get(&crate::state::contracts::OperationId::new(
                status.id.clone(),
            ))
            .expect("same-ID journal retained");
        assert_eq!(
            original.status,
            crate::state::contracts::OperationStatus::Failed
        );
        if malformed {
            assert_eq!(
                original.failure_point.as_deref(),
                Some("performance_journal_invalid")
            );
        }
        assert!(
            performance_operation_status(&state, &status.id)
                .await
                .expect("failed public status")
                .proof
                .is_none()
        );
        assert!(
            state
                .journals()
                .get(&crate::state::contracts::OperationId::new(format!(
                    "{}-reconciliation",
                    status.id
                )))
                .is_some_and(|journal| performance_journal_is_terminal(journal.status))
        );
        let _ = fs::remove_dir_all(root);
    }
}

#[tokio::test]
async fn mismatched_journal_failure_intent_retries_before_terminal_status() {
    let root = test_root("mismatch-failure-intent-retry");
    let journal_backend = Arc::new(ScriptedOperationBackend::default());
    let status_backend = Arc::new(ScriptedOperationBackend::default());
    let state =
        build_test_state_with_operation_backends(&root, journal_backend.clone(), status_backend);
    let status = state
        .performance_operations()
        .start_with_identity(
            "instance-a".to_string(),
            "remove".to_string(),
            test_operation_payload(),
            crate::state::performance_operations::PerformanceOperationJournalIdentity::new(
                "remove",
                "expected-target",
                RollbackState::Unavailable,
            ),
        )
        .await
        .expect("status starts");
    state
        .journals()
        .create(test_mismatched_performance_journal(
            &status.id,
            "actual-target",
            false,
        ))
        .await
        .expect("mismatched journal starts");
    journal_backend.fail_attempt(2);

    assert!(
        terminalize_mismatched_performance_operation(
            &state,
            state.installs(),
            &status,
            PerformanceInstallAction::Remove,
            "restart reconciliation failed",
        )
        .await
    );
    let original = state
        .journals()
        .get(&crate::state::contracts::OperationId::new(
            status.id.clone(),
        ))
        .expect("same-ID journal retained");
    assert!(performance_journal_is_terminal(original.status));
    assert!(original.completed_steps.iter().any(|step| {
        step.step_id == "performance_terminal_intent"
            && step
                .generated_facts
                .iter()
                .any(|fact| fact == "performance_terminal_failure_v1")
    }));
    assert_eq!(
        state
            .performance_operations()
            .get(&status.id)
            .await
            .expect("failed status")
            .state,
        "failed"
    );
    assert!(
        state
            .journals()
            .get(&crate::state::contracts::OperationId::new(format!(
                "{}-reconciliation",
                status.id
            )))
            .is_some_and(|journal| performance_journal_is_terminal(journal.status))
    );
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn foreign_retry_candidate_is_cleared_before_requested_journal_is_applied() {
    let root = test_root("foreign-journal-candidate");
    let journal_backend = Arc::new(ScriptedOperationBackend::default());
    journal_backend.fail_attempt(1);
    let status_backend = Arc::new(ScriptedOperationBackend::default());
    let state = build_test_state_with_operation_backends(&root, journal_backend, status_backend);
    let foreign_id = crate::state::contracts::OperationId::new("foreign-operation");
    let foreign = crate::state::contracts::OperationJournalEntry::new(
        crate::state::contracts::JournalId::new("journal-foreign-operation"),
        foreign_id.clone(),
        crate::state::contracts::CommandKind::RefreshPerformanceRules,
        crate::state::contracts::StabilizationSystem::Application,
        crate::state::contracts::OwnershipClass::LauncherManaged,
        RollbackState::NotApplicable,
    );
    state
        .journals()
        .create(foreign)
        .await
        .expect_err("seed foreign failed candidate");
    assert!(state.journals().has_retry_candidate());
    let instance_id = insert_persisted_test_instance(&state, "Managed", "1.20.4-fabric")
        .await
        .id;
    let lock_path = seed_managed_lock(&state, &instance_id, "foreign-candidate");

    let response = handle_install(
        State(state.clone()),
        Json(InstallRequest {
            instance_id: Some(instance_id.clone()),
            game_version: None,
            loader: None,
            mode: None,
            action: Some("remove".to_string()),
            rollback_id: None,
            queued: Some(false),
        }),
    )
    .await
    .expect("foreign candidate is reconciled before requested operation");

    assert_eq!(response.0.status, "removed");
    assert!(state.journals().get(&foreign_id).is_some());
    let status = state
        .performance_operations()
        .current_or_latest_for_instance(&instance_id)
        .await
        .expect("requested status");
    assert_eq!(status.state, "complete");
    assert!(
        performance_journal_is_terminal(
            state
                .journals()
                .get(&crate::state::contracts::OperationId::new(status.id))
                .expect("requested journal")
                .status
        ),
        "requested mutation is independently committed"
    );
    assert!(!lock_path.exists(), "requested effect runs after own gate");
    let _ = fs::remove_dir_all(root);
}

#[test]
fn journal_errors_use_bounded_public_and_log_classifications() {
    let error = OperationJournalStoreError::Persistence(std::io::Error::other(
        "/Users/alice/private/operation-journals.json",
    ));
    assert_eq!(error.class(), "persistence");

    let response = PerformanceOperationExecutionError::Journal {
        error,
        operation_id: None,
        expected: None,
    }
    .into_application_error();
    assert_eq!(response.0, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        response.1.0,
        serde_json::json!({
            "error": "Could not save performance operation safety state. Check app data permissions and try again."
        })
    );
    assert_omits_raw_fragments(
        &response.1.0.to_string(),
        &["/Users/alice", "operation-journals.json"],
    );
}

#[tokio::test]
async fn queued_operation_rejects_same_instance_overlap() {
    let fixture = TestFixture::new("queued-overlap");
    let instance_id = fixture.add_instance("Managed", "1.20.4-fabric");
    fixture
        .state
        .performance_operations()
        .start(
            instance_id.clone(),
            "remove".to_string(),
            test_operation_payload(),
        )
        .await
        .expect("prelock instance");

    let error = handle_install(
        State(fixture.state.clone()),
        Json(InstallRequest {
            instance_id: Some(instance_id),
            game_version: None,
            loader: None,
            mode: None,
            action: Some("remove".to_string()),
            rollback_id: None,
            queued: Some(true),
        }),
    )
    .await
    .expect_err("overlapping queued operation should fail");

    assert_eq!(error.0, StatusCode::CONFLICT);
    assert_eq!(
        error.1.0,
        serde_json::json!({
            "error": "a performance operation is already queued for this instance"
        })
    );
}

#[tokio::test]
async fn operation_status_route_returns_persisted_status() {
    let fixture = TestFixture::new("operation-status-route");
    let started = fixture
        .state
        .performance_operations()
        .start(
            "instance-a".to_string(),
            "install".to_string(),
            test_operation_payload(),
        )
        .await
        .expect("operation starts");
    fixture
        .state
        .performance_operations()
        .record_progress(&started.id, "applying")
        .await
        .expect("progress accepted");

    let response = router()
        .with_state(fixture.state.clone())
        .oneshot(
            Request::builder()
                .uri(format!("/api/v1/performance/operations/{}", started.id))
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("route response");
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    let response: serde_json::Value = serde_json::from_slice(&body).expect("operation status json");

    assert_eq!(response["id"], started.id);
    assert_eq!(response["instance_id"], "instance-a");
    assert_eq!(response["action"], "install");
    assert_eq!(response["state"], "applying");
    assert_eq!(
        response["view_model"],
        serde_json::json!({
            "state_label": "Applying",
            "tone": "mute",
            "title": "Applying bundle",
            "detail": "Applying managed performance files.",
            "progress": {
                "phase": "applying",
                "current": 2,
                "total": 4,
                "done": false,
            },
            "is_terminal": false,
            "is_complete": false,
        })
    );
}

#[tokio::test]
async fn operation_status_routes_redact_payload_and_error_details() {
    let fixture = TestFixture::new("operation-status-redaction");
    let instance_id = fixture.add_instance("Managed", "1.20.4-fabric");
    let started = fixture
        .state
        .performance_operations()
        .start(
            instance_id.clone(),
            "install/provider_payload=secret-token".to_string(),
            PerformanceOperationPayload {
                game_version: Some("/Users/alice/.minecraft/private-version".to_string()),
                loader: Some("fabric".to_string()),
                mode: Some("managed --accessToken secret-token".to_string()),
                rollback_id: Some("rb-old\\secret".to_string()),
            },
        )
        .await
        .expect("operation starts");
    fixture
        .state
        .performance_operations()
        .record_failed(
            &started.id,
            "provider_payload={\"url\":\"https://cdn.example.test/private-provider/sodium-secret.jar?token=secret-token\"}; java_path=C:\\Users\\Alice\\Java\\bin\\java.exe; -Xmx8192M",
        )
        .await
        .expect("failure accepted");

    let response = router()
        .with_state(fixture.state.clone())
        .oneshot(
            Request::builder()
                .uri(format!("/api/v1/performance/operations/{}", started.id))
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("route response");

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    let body = String::from_utf8(body.to_vec()).expect("utf8 body");
    let value: serde_json::Value = serde_json::from_str(&body).expect("operation json");

    assert_eq!(value["state"], "failed");
    assert_eq!(value["action"], "unknown");
    assert_eq!(value["error"], "performance operation failed");
    assert_eq!(value["view_model"]["tone"], "err");
    assert_eq!(
        value["view_model"]["detail"],
        "performance operation failed"
    );
    assert_eq!(value["view_model"]["progress"]["phase"], "error");
    assert_eq!(value["payload"]["game_version"], "redacted");
    assert_eq!(value["payload"]["loader"], "fabric");
    assert_eq!(value["payload"]["mode"], "redacted");
    assert_eq!(value["payload"]["rollback_id"], "redacted");
    assert_omits_raw_fragments(
        &body,
        &[
            "/Users/alice",
            "C:\\Users\\Alice",
            "provider_payload",
            "private-provider",
            "sodium-secret.jar",
            "secret-token",
            "-Xmx8192M",
        ],
    );

    let response = router()
        .with_state(fixture.state.clone())
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/api/v1/performance/instances/{instance_id}/operation"
                ))
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("instance route response");

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read instance body");
    let body = String::from_utf8(body.to_vec()).expect("utf8 instance body");
    let value: serde_json::Value = serde_json::from_str(&body).expect("instance operation json");

    assert_eq!(value["operation"]["id"], started.id);
    assert_eq!(value["operation"]["action"], "unknown");
    assert_eq!(value["operation"]["error"], "performance operation failed");
    assert_eq!(value["operation"]["view_model"]["tone"], "err");
    assert_eq!(
        value["operation"]["view_model"]["detail"],
        "performance operation failed"
    );
    assert_eq!(value["operation"]["payload"]["game_version"], "redacted");
    assert_eq!(value["operation"]["payload"]["loader"], "fabric");
    assert_eq!(value["operation"]["payload"]["mode"], "redacted");
    assert_eq!(value["operation"]["payload"]["rollback_id"], "redacted");
    assert_omits_raw_fragments(
        &body,
        &[
            "/Users/alice",
            "C:\\Users\\Alice",
            "provider_payload",
            "private-provider",
            "sodium-secret.jar",
            "secret-token",
            "-Xmx8192M",
        ],
    );
}

#[tokio::test]
async fn instance_operation_route_returns_null_when_none_exists() {
    let fixture = TestFixture::new("instance-operation-empty");
    let instance_id = fixture.add_instance("Managed", "1.20.4-fabric");

    let response = router()
        .with_state(fixture.state.clone())
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/api/v1/performance/instances/{instance_id}/operation"
                ))
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("route response");

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    let value: serde_json::Value = serde_json::from_slice(&body).expect("operation response json");
    assert_eq!(value, serde_json::json!({ "operation": null }));
}

#[tokio::test]
async fn instance_operation_route_discovers_reloaded_pending_operation() {
    let root = test_root("instance-operation-reloaded");
    let state = build_test_state(&root, None, None);
    let instance_id = insert_persisted_test_instance(&state, "Managed", "1.20.4-fabric")
        .await
        .id;
    let started = state
        .performance_operations()
        .start_with_identity(
            instance_id.clone(),
            "remove".to_string(),
            test_operation_payload(),
            crate::state::performance_operations::PerformanceOperationJournalIdentity::new(
                "remove",
                "performance_composition_lock",
                RollbackState::Unavailable,
            ),
        )
        .await
        .expect("persist pending operation");
    state
        .performance_operations()
        .record_progress(&started.id, "removing")
        .await
        .expect("progress accepted");
    state
        .performance_operations()
        .close()
        .await
        .expect("status store closes before reload");
    state
        .journals()
        .close()
        .await
        .expect("journal store closes before reload");
    drop(state);

    let reloaded = build_test_state(&root, None, None);
    let response = router()
        .with_state(reloaded)
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/api/v1/performance/instances/{instance_id}/operation"
                ))
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("route response");

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    let value: serde_json::Value = serde_json::from_slice(&body).expect("operation response json");
    assert_eq!(value["operation"]["id"], started.id);
    assert_eq!(value["operation"]["instance_id"], instance_id);
    assert_eq!(value["operation"]["state"], "removing");
    assert_eq!(value["operation"]["view_model"]["state_label"], "Removing");
    assert_eq!(value["operation"]["view_model"]["is_terminal"], false);
    assert_eq!(
        value["operation"]["view_model"]["progress"]["phase"],
        "removing"
    );

    let _ = fs::remove_dir_all(&root);
}

#[tokio::test]
async fn startup_resume_runs_persisted_pending_remove_operation() {
    let root = test_root("startup-resume-remove");
    let state = build_test_state(&root, None, None);
    let instance_id = insert_persisted_test_instance(&state, "Managed", "1.20.4-fabric")
        .await
        .id;
    let started = state
        .performance_operations()
        .start_with_identity(
            instance_id.clone(),
            "remove".to_string(),
            test_operation_payload(),
            crate::state::performance_operations::PerformanceOperationJournalIdentity::new(
                "remove",
                "performance_composition_lock",
                RollbackState::Unavailable,
            ),
        )
        .await
        .expect("persist pending operation");
    state
        .performance_operations()
        .record_progress(&started.id, "removing")
        .await
        .expect("progress accepted");
    state
        .performance_operations()
        .close()
        .await
        .expect("status store closes before reload");
    state
        .journals()
        .close()
        .await
        .expect("journal store closes before reload");
    drop(state);

    let reloaded = build_test_state(&root, None, None);
    let loaded = reloaded
        .performance_operations()
        .get(&started.id)
        .await
        .expect("pending operation should reload");
    assert_eq!(loaded.state, "removing");
    let effect_blocker = reloaded
        .admit_managed_instance(&instance_id, true)
        .await
        .expect("hold resumed instance lifecycle");

    let resumed = resume_pending_performance_operations(reloaded.clone()).await;
    assert_eq!(resumed, 1);
    assert!(
        !reloaded
            .subscribe_integrity_idle()
            .borrow()
            .is_stably_idle(),
        "resumed child retains foreground after startup owner returns"
    );
    drop(effect_blocker);
    let events = collect_install_events(&reloaded, &started.id).await;
    let phases = events
        .iter()
        .map(|event| event.phase.as_str())
        .collect::<Vec<_>>();
    assert_eq!(phases, vec!["queued", "planning", "removing", "complete"]);
    let completed = reloaded
        .performance_operations()
        .get(&started.id)
        .await
        .expect("completed operation status");
    assert_eq!(completed.instance_id, instance_id);
    assert_eq!(completed.state, "complete");
    assert_eq!(completed.error, None);
    wait_for_integrity_idle(&reloaded, true).await;

    let _ = fs::remove_dir_all(&root);
}

#[tokio::test]
async fn startup_reconciles_terminal_journal_without_replaying_effect() {
    let (root, install_id, instance_id, lock_path) =
        seed_restart_checkpoint("restart-terminal", RestartCheckpoint::Terminal).await;
    let state = build_test_state(&root, None, None);

    assert_eq!(
        resume_pending_performance_operations(state.clone()).await,
        1
    );
    let events = collect_install_events(&state, &install_id).await;
    assert!(events.last().is_some_and(|event| event.done));
    assert!(
        lock_path.is_file(),
        "terminal journal prevents effect replay"
    );
    let status = state
        .performance_operations()
        .get(&install_id)
        .await
        .expect("terminal status reconciled");
    assert_eq!(status.instance_id, instance_id);
    assert_eq!(status.state, "complete");
    assert_eq!(
        state
            .journals()
            .get(&crate::state::contracts::OperationId::new(install_id))
            .expect("terminal journal retained")
            .status,
        crate::state::contracts::OperationStatus::Succeeded
    );
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn startup_finishes_terminal_intent_without_replaying_effect() {
    let (root, install_id, _, lock_path) =
        seed_restart_checkpoint("restart-terminal-intent", RestartCheckpoint::TerminalIntent).await;
    let state = build_test_state(&root, None, None);

    assert_eq!(
        resume_pending_performance_operations(state.clone()).await,
        1
    );
    let events = collect_install_events(&state, &install_id).await;
    assert!(events.last().is_some_and(|event| event.done));
    assert!(
        lock_path.is_file(),
        "terminal intent prevents effect replay"
    );
    assert_eq!(
        state
            .performance_operations()
            .get(&install_id)
            .await
            .expect("terminal status")
            .state,
        "complete"
    );
    let journal = state
        .journals()
        .get(&crate::state::contracts::OperationId::new(install_id))
        .expect("terminalized journal");
    assert_eq!(
        journal.status,
        crate::state::contracts::OperationStatus::Succeeded
    );
    assert_eq!(
        journal
            .completed_steps
            .iter()
            .filter(|step| step.step_id == "performance_terminal_intent")
            .count(),
        1
    );
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn startup_finishes_rollback_intent_with_distinct_result_target_and_proof() {
    let root = test_root("restart-rollback-result-target");
    let state = build_test_state(&root, None, None);
    let instance_id = insert_persisted_test_instance(&state, "Rollback", "1.20.4-fabric")
        .await
        .id;
    let status = state
        .performance_operations()
        .start_with_identity(
            instance_id,
            "rollback".to_string(),
            PerformanceOperationPayload {
                game_version: None,
                loader: None,
                mode: None,
                rollback_id: Some("rollback-snapshot".to_string()),
            },
            crate::state::performance_operations::PerformanceOperationJournalIdentity::new(
                "rollback",
                "rollback-snapshot",
                RollbackState::Available,
            ),
        )
        .await
        .expect("persist rollback status");
    let operation_id = begin_performance_operation_journal(
        &state,
        PerformanceInstallAction::Rollback,
        "rollback-snapshot",
        RollbackState::Available,
        Some(&status.id),
    )
    .await
    .expect("persist rollback journal");
    record_performance_terminal_intent(
        &state,
        &operation_id,
        PerformanceInstallAction::Rollback,
        "restored-composition",
        RollbackState::Available,
        true,
    )
    .await
    .expect("persist rollback terminal intent");
    state
        .performance_operations()
        .record_committing_complete(&status.id)
        .await
        .expect("persist committing status");
    state
        .performance_operations()
        .close()
        .await
        .expect("status store closes");
    state
        .journals()
        .close()
        .await
        .expect("journal store closes");
    drop(state);

    let reloaded = build_test_state(&root, None, None);
    assert_eq!(
        resume_pending_performance_operations(reloaded.clone()).await,
        1
    );
    assert_eq!(
        reloaded
            .performance_operations()
            .get(&status.id)
            .await
            .expect("rollback status completes")
            .state,
        "complete"
    );
    let terminal = reloaded
        .journals()
        .get(&operation_id)
        .expect("rollback journal terminalizes");
    assert_eq!(
        terminal.status,
        crate::state::contracts::OperationStatus::Succeeded
    );
    assert!(terminal.completed_steps.iter().any(|step| {
        step.step_id == "rollback_performance_plan"
            && step.rollback == RollbackState::Applied
            && step
                .changed_target
                .as_ref()
                .is_some_and(|target| target.id == "restored-composition")
    }));
    assert!(
        performance_operation_status(&reloaded, &status.id)
            .await
            .expect("public rollback status")
            .proof
            .is_some()
    );
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn startup_fails_effect_started_checkpoint_without_replaying_effect() {
    let (root, install_id, _, lock_path) =
        seed_restart_checkpoint("restart-effect-started", RestartCheckpoint::EffectStarted).await;
    let state = build_test_state(&root, None, None);

    assert_eq!(
        resume_pending_performance_operations(state.clone()).await,
        1
    );
    let events = collect_install_events(&state, &install_id).await;
    let terminal = events.last().expect("terminal progress");
    assert!(terminal.done);
    assert_eq!(terminal.phase, "error");
    assert!(
        lock_path.is_file(),
        "uncertain effect must never be replayed"
    );
    let status = state
        .performance_operations()
        .get(&install_id)
        .await
        .expect("failed status");
    assert_eq!(status.state, "failed");
    assert_eq!(
        status.error.as_deref(),
        Some("performance operation outcome could not be confirmed after restart")
    );
    let journal = state
        .journals()
        .get(&crate::state::contracts::OperationId::new(install_id))
        .expect("failed journal");
    assert_eq!(
        journal.status,
        crate::state::contracts::OperationStatus::Failed
    );
    assert_eq!(
        journal
            .completed_steps
            .iter()
            .filter(|step| step.step_id == "performance_effect_started")
            .count(),
        1
    );
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn startup_fails_critical_status_when_journal_is_missing_or_corrupt() {
    for (name, corrupt) in [
        ("restart-missing-critical-journal", false),
        ("restart-corrupt-critical-journal", true),
    ] {
        let (root, install_id, _, lock_path) =
            seed_restart_checkpoint(name, RestartCheckpoint::EffectStarted).await;
        let journal_path = operation_journal_snapshot_path(&root);
        if corrupt {
            fs::write(&journal_path, b"{not-valid-json").expect("corrupt journal snapshot");
        } else {
            fs::remove_file(&journal_path).expect("remove journal snapshot");
        }

        let state = build_test_state(&root, None, None);
        assert_eq!(
            resume_pending_performance_operations(state.clone()).await,
            1
        );
        assert!(lock_path.is_file(), "critical status cannot replay effect");
        assert_eq!(
            state
                .performance_operations()
                .get(&install_id)
                .await
                .expect("critical status retained")
                .state,
            "failed"
        );
        assert!(
            state
                .journals()
                .get(&crate::state::contracts::OperationId::new(install_id))
                .is_some_and(|journal| {
                    journal.status == crate::state::contracts::OperationStatus::Failed
                })
        );
        let _ = fs::remove_dir_all(root);
    }
}

#[tokio::test]
async fn startup_rejects_mismatched_terminal_and_intent_without_proof() {
    for (name, checkpoint) in [
        ("restart-mismatched-terminal", RestartCheckpoint::Terminal),
        (
            "restart-mismatched-terminal-intent",
            RestartCheckpoint::TerminalIntent,
        ),
    ] {
        let (root, install_id, _, lock_path) = seed_restart_checkpoint(name, checkpoint).await;
        rewrite_performance_journal_targets(&root, &install_id, "wrong-target");
        let state = build_test_state(&root, None, None);

        assert_eq!(
            resume_pending_performance_operations(state.clone()).await,
            1
        );
        assert!(
            lock_path.is_file(),
            "mismatched terminal cannot replay effect"
        );
        assert_eq!(
            state
                .performance_operations()
                .get(&install_id)
                .await
                .expect("mismatched status retained")
                .state,
            "failed"
        );
        let public = performance_operation_status(&state, &install_id)
            .await
            .expect("public mismatched status");
        assert!(public.proof.is_none());
        let reconciliation_id =
            crate::state::contracts::OperationId::new(format!("{install_id}-reconciliation"));
        assert!(
            state
                .journals()
                .get(&reconciliation_id)
                .is_some_and(|journal| {
                    journal.status == crate::state::contracts::OperationStatus::Failed
                })
        );
        let _ = fs::remove_dir_all(root);
    }
}

#[tokio::test]
async fn startup_rejects_ambiguous_or_malformed_terminal_intent_without_replay() {
    for (name, corruption) in [
        (
            "restart-terminal-intent-wrong-step",
            TerminalIntentCorruption::WrongStepAndInjectedAction,
        ),
        (
            "restart-terminal-intent-both-facts",
            TerminalIntentCorruption::BothFacts,
        ),
    ] {
        let (root, install_id, _, lock_path) =
            seed_restart_checkpoint(name, RestartCheckpoint::TerminalIntent).await;
        corrupt_performance_terminal_intent(&root, &install_id, corruption);
        let state = build_test_state(&root, None, None);

        assert_eq!(
            resume_pending_performance_operations(state.clone()).await,
            1
        );
        assert!(lock_path.is_file(), "untrusted intent cannot replay effect");
        assert_eq!(
            state
                .performance_operations()
                .get(&install_id)
                .await
                .expect("corrupt intent status retained")
                .state,
            "failed"
        );
        assert!(
            performance_operation_status(&state, &install_id)
                .await
                .expect("corrupt intent public status")
                .proof
                .is_none()
        );
        assert!(
            state
                .journals()
                .get(&crate::state::contracts::OperationId::new(format!(
                    "{install_id}-reconciliation"
                )))
                .is_some_and(|journal| performance_journal_is_terminal(journal.status))
        );
        let _ = fs::remove_dir_all(root);
    }
}

#[tokio::test]
async fn startup_rejects_matching_plan_with_malformed_terminal_transition() {
    let (root, install_id, _, lock_path) =
        seed_restart_checkpoint("restart-malformed-terminal", RestartCheckpoint::Terminal).await;
    corrupt_performance_terminal_step(&root, &install_id);
    let state = build_test_state(&root, None, None);

    assert_eq!(
        resume_pending_performance_operations(state.clone()).await,
        1
    );
    assert!(
        lock_path.is_file(),
        "malformed terminal cannot replay effect"
    );
    assert_eq!(
        state
            .performance_operations()
            .get(&install_id)
            .await
            .expect("malformed terminal status retained")
            .state,
        "failed"
    );
    assert!(
        performance_operation_status(&state, &install_id)
            .await
            .expect("malformed terminal public status")
            .proof
            .is_none()
    );
    assert!(
        state
            .journals()
            .get(&crate::state::contracts::OperationId::new(format!(
                "{install_id}-reconciliation"
            )))
            .is_some_and(|journal| performance_journal_is_terminal(journal.status))
    );
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn startup_invalid_terminal_status_cannot_republish_matching_success_proof() {
    let (root, install_id, _, lock_path) = seed_restart_checkpoint(
        "restart-terminal-missing-identity",
        RestartCheckpoint::Terminal,
    )
    .await;
    remove_persisted_performance_journal_identity(&root, &install_id);
    let state = build_test_state(&root, None, None);

    assert_eq!(
        resume_pending_performance_operations(state.clone()).await,
        1
    );
    assert!(
        lock_path.is_file(),
        "invalid terminal status cannot replay effect"
    );
    let public = performance_operation_status(&state, &install_id)
        .await
        .expect("invalid terminal public status");
    assert_eq!(public.status.state, "failed");
    assert!(public.proof.is_none());
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn startup_unknown_status_state_fails_without_effect() {
    let root = test_root("restart-unknown-state");
    let state = build_test_state(&root, None, None);
    let instance_id = insert_persisted_test_instance(&state, "Managed", "1.20.4-fabric")
        .await
        .id;
    let lock_path = seed_managed_lock(&state, &instance_id, "unknown-state-lock");
    state
        .performance_operations()
        .close()
        .await
        .expect("close unused status store");
    state.journals().close().await.expect("close journal store");
    drop(state);
    let install_id = test_performance_operation_id(77);
    write_persisted_performance_status(
        &root,
        &install_id,
        &instance_id,
        "remove",
        "unexpected_provider_state",
        test_operation_payload(),
    );

    let reloaded = build_test_state(&root, None, None);
    assert_eq!(
        resume_pending_performance_operations(reloaded.clone()).await,
        1
    );
    assert!(lock_path.is_file(), "unknown state cannot authorize effect");
    assert_eq!(
        reloaded
            .performance_operations()
            .get(&install_id)
            .await
            .expect("unknown status retained")
            .state,
        "failed"
    );
    assert!(
        performance_operation_status(&reloaded, &install_id)
            .await
            .expect("unknown public status")
            .proof
            .is_none()
    );
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn startup_corrects_terminal_status_with_active_same_id_journal() {
    let root = test_root("restart-terminal-status-active-journal");
    let state = build_test_state(&root, None, None);
    let status = state
        .performance_operations()
        .start_with_identity(
            "instance-a".to_string(),
            "remove".to_string(),
            test_operation_payload(),
            crate::state::performance_operations::PerformanceOperationJournalIdentity::new(
                "remove",
                "active-target",
                RollbackState::Unavailable,
            ),
        )
        .await
        .expect("status starts");
    begin_performance_operation_journal(
        &state,
        PerformanceInstallAction::Remove,
        "active-target",
        RollbackState::Unavailable,
        Some(&status.id),
    )
    .await
    .expect("active journal starts");
    state
        .performance_operations()
        .record_complete(&status.id)
        .await
        .expect("corrupt terminal status persists");
    state
        .performance_operations()
        .close()
        .await
        .expect("status store closes");
    state
        .journals()
        .close()
        .await
        .expect("journal store closes");
    drop(state);

    let reloaded = build_test_state(&root, None, None);
    assert_eq!(
        resume_pending_performance_operations(reloaded.clone()).await,
        0
    );
    assert_eq!(
        reloaded
            .performance_operations()
            .get(&status.id)
            .await
            .expect("corrected terminal status")
            .state,
        "failed"
    );
    assert!(
        reloaded
            .journals()
            .get(&crate::state::contracts::OperationId::new(
                status.id.clone()
            ))
            .is_some_and(|journal| performance_journal_is_terminal(journal.status))
    );
    assert!(
        performance_operation_status(&reloaded, &status.id)
            .await
            .expect("corrected public status")
            .proof
            .is_none()
    );
    assert_eq!(
        reloaded.installs().active_install_count().await,
        0,
        "background reconciliation cannot create an active install session"
    );
    let journals_after_first_reconciliation = reloaded.journals().list();
    reloaded
        .performance_operations()
        .close()
        .await
        .expect("corrected status store closes");
    reloaded
        .journals()
        .close()
        .await
        .expect("corrected journal store closes");
    drop(reloaded);

    let second_restart = build_test_state(&root, None, None);
    assert_eq!(
        resume_pending_performance_operations(second_restart.clone()).await,
        0,
        "stable reconciliation marker needs no repeated cleanup"
    );
    assert_eq!(
        second_restart.journals().list(),
        journals_after_first_reconciliation,
        "second restart cannot author another reconciliation journal"
    );
    assert_eq!(
        second_restart
            .performance_operations()
            .get(&status.id)
            .await
            .expect("stable corrected status")
            .state,
        "failed"
    );
    assert!(
        performance_operation_status(&second_restart, &status.id)
            .await
            .expect("stable corrected public status")
            .proof
            .is_none()
    );
    assert_eq!(second_restart.installs().active_install_count().await, 0);
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn startup_terminalizes_journal_only_performance_operation_without_status() {
    let root = test_root("restart-journal-only");
    let state = build_test_state(&root, None, None);
    let operation_id = begin_performance_operation_journal(
        &state,
        PerformanceInstallAction::Remove,
        "journal-only-target",
        RollbackState::Unavailable,
        None,
    )
    .await
    .expect("orphan journal starts");
    state
        .journals()
        .close()
        .await
        .expect("journal store closes");
    state
        .performance_operations()
        .close()
        .await
        .expect("status store closes");
    drop(state);

    let reloaded = build_test_state(&root, None, None);
    assert_eq!(
        resume_pending_performance_operations(reloaded.clone()).await,
        0
    );
    assert!(
        reloaded
            .performance_operations()
            .get(operation_id.as_str())
            .await
            .is_none(),
        "orphan reconciliation must not invent public status"
    );
    assert!(
        reloaded
            .journals()
            .get(&operation_id)
            .is_some_and(|journal| {
                journal.status == crate::state::contracts::OperationStatus::Failed
            })
    );
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn custom_install_resume_preserves_effective_remove_identity() {
    let root = test_root("restart-custom-effective-remove");
    let state = build_test_state(&root, None, None);
    let instance_id = insert_persisted_test_instance(&state, "Custom", "1.20.4-fabric")
        .await
        .id;
    let status = state
        .performance_operations()
        .start_with_identity(
            instance_id,
            "install".to_string(),
            PerformanceOperationPayload {
                game_version: None,
                loader: None,
                mode: Some("custom".to_string()),
                rollback_id: None,
            },
            crate::state::performance_operations::PerformanceOperationJournalIdentity::new(
                "remove",
                "performance_composition_lock",
                RollbackState::Unavailable,
            ),
        )
        .await
        .expect("persist custom install request");
    state
        .performance_operations()
        .close()
        .await
        .expect("status store closes");
    state
        .journals()
        .close()
        .await
        .expect("journal store closes");
    drop(state);

    let reloaded = build_test_state(&root, None, None);
    assert_eq!(
        resume_pending_performance_operations(reloaded.clone()).await,
        1
    );
    let events = collect_install_events(&reloaded, &status.id).await;
    assert!(events.last().is_some_and(|event| event.done));
    assert_eq!(
        reloaded
            .performance_operations()
            .get(&status.id)
            .await
            .expect("custom operation completes")
            .state,
        "complete"
    );
    let journal = reloaded
        .journals()
        .get(&crate::state::contracts::OperationId::new(status.id))
        .expect("effective remove journal");
    assert!(
        journal
            .planned_steps
            .iter()
            .any(|step| { step.step_id == "remove_performance_plan" })
    );
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn persisted_identity_rejects_preflight_target_drift_on_resume() {
    let root = test_root("restart-preflight-drift");
    let state = build_test_state(&root, None, None);
    let instance_id = insert_persisted_test_instance(&state, "Managed", "1.20.4-fabric")
        .await
        .id;
    seed_managed_lock(&state, &instance_id, "original-target");
    let status = state
        .performance_operations()
        .start_with_identity(
            instance_id.clone(),
            "remove".to_string(),
            test_operation_payload(),
            crate::state::performance_operations::PerformanceOperationJournalIdentity::new(
                "remove",
                "original-target",
                RollbackState::Available,
            ),
        )
        .await
        .expect("persist original identity");
    let sentinel = seed_managed_lock(&state, &instance_id, "drifted-target");
    state
        .performance_operations()
        .close()
        .await
        .expect("status store closes");
    state
        .journals()
        .close()
        .await
        .expect("journal store closes");
    drop(state);

    let reloaded = build_test_state(&root, None, None);
    assert_eq!(
        resume_pending_performance_operations(reloaded.clone()).await,
        1
    );
    assert!(sentinel.is_file(), "drift cannot authorize a new target");
    wait_for_journal_first_failed_status(&reloaded, &status.id).await;
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn invalid_timestamps_are_unknown_on_both_operation_endpoints() {
    let root = test_root("public-operation-timestamps");
    let operation_id = test_performance_operation_id(1);
    let seed = build_test_state(&root, None, None);
    let instance_id = insert_persisted_test_instance(&seed, "Timestamp", "1.20.4-fabric")
        .await
        .id;
    seed.performance_operations()
        .close()
        .await
        .expect("seed status store closes");
    seed.journals()
        .close()
        .await
        .expect("seed journal store closes");
    drop(seed);
    write_persisted_performance_status(
        &root,
        &operation_id,
        &instance_id,
        "remove",
        "removing",
        test_operation_payload(),
    );
    let path = crate::state::performance_operations::operation_path(
        &crate::state::performance_operations::operation_dir(&test_paths(&root)),
        &operation_id,
    );
    let mut persisted: serde_json::Value =
        serde_json::from_slice(&fs::read(&path).expect("status bytes")).expect("status json");
    persisted["created_at"] = serde_json::Value::String("/Users/alice/token=secret".to_string());
    persisted["updated_at"] = serde_json::Value::String("not-rfc3339".to_string());
    fs::write(
        &path,
        serde_json::to_vec_pretty(&persisted).expect("serialize invalid timestamps"),
    )
    .expect("rewrite timestamps");
    let state = build_test_state(&root, None, None);

    let direct = performance_operation_status(&state, &operation_id)
        .await
        .expect("direct operation status");
    assert_eq!(direct.status.created_at, "unknown");
    assert_eq!(direct.status.updated_at, "unknown");
    let by_instance = performance_instance_operation(&state, &instance_id)
        .await
        .expect("instance operation response");
    let by_instance = by_instance.operation.expect("instance operation status");
    assert_eq!(by_instance.status.created_at, "unknown");
    assert_eq!(by_instance.status.updated_at, "unknown");
    let encoded = serde_json::to_string(&(direct, by_instance)).expect("serialize public statuses");
    assert_omits_raw_fragments(&encoded, &["/Users/alice", "token=", "not-rfc3339"]);
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn startup_terminalizes_malformed_status_journal_first_with_bounded_public_fields() {
    let root = test_root("restart-malformed-status");
    let operation_id = test_performance_operation_id(1);
    write_persisted_performance_status(
        &root,
        &operation_id,
        "",
        "install",
        "applying",
        PerformanceOperationPayload {
            game_version: Some("/Users/alice/private-version".to_string()),
            loader: Some("fabric?token=secret-token".to_string()),
            mode: None,
            rollback_id: None,
        },
    );
    let state = build_test_state(&root, None, None);
    assert_eq!(
        state
            .performance_operations()
            .get(&operation_id)
            .await
            .expect("malformed record retained")
            .state,
        crate::state::performance_operations::PERFORMANCE_RESUME_INVALID_STATE
    );

    assert_eq!(
        resume_pending_performance_operations(state.clone()).await,
        1
    );
    assert_journal_first_failed_status(&state, &operation_id).await;
    let public = performance_operation_status(&state, &operation_id)
        .await
        .expect("public failed status");
    let encoded = serde_json::to_string(&public).expect("serialize public status");
    assert_omits_raw_fragments(
        &encoded,
        &["/Users/alice", "private-version", "secret-token", "token="],
    );
    assert!(state.performance_operations().load_issue_count() >= 1);
    let issues = format!("{:?}", state.performance_operations().load_issues());
    assert_omits_raw_fragments(
        &issues,
        &["/Users/alice", "private-version", "secret-token"],
    );
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn startup_terminalizes_duplicate_instance_records_journal_first() {
    let root = test_root("restart-duplicate-status");
    let first = test_performance_operation_id(1);
    let second = test_performance_operation_id(2);
    for (id, action) in [(&first, "install"), (&second, "remove")] {
        write_persisted_performance_status(
            &root,
            id,
            "missing-instance",
            action,
            "applying",
            PerformanceOperationPayload {
                game_version: Some("C:\\Users\\Alice\\private-version".to_string()),
                loader: None,
                mode: None,
                rollback_id: None,
            },
        );
    }
    let state = build_test_state(&root, None, None);
    let mut blocked_before_resume = 0;
    for id in [&first, &second] {
        if state
            .performance_operations()
            .get(id)
            .await
            .is_some_and(|status| {
                status.state
                    == crate::state::performance_operations::PERFORMANCE_RESUME_BLOCKED_STATE
            })
        {
            blocked_before_resume += 1;
        }
    }
    assert_eq!(blocked_before_resume, 1);

    assert_eq!(
        resume_pending_performance_operations(state.clone()).await,
        2
    );
    for id in [&first, &second] {
        wait_for_journal_first_failed_status(&state, id).await;
        let public = performance_operation_status(&state, id)
            .await
            .expect("public failed duplicate status");
        let encoded = serde_json::to_string(&public).expect("serialize public status");
        assert_omits_raw_fragments(&encoded, &["C:\\Users\\Alice", "private-version"]);
    }
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn startup_drains_over_limit_records_into_journal_first_terminal_failures() {
    let root = test_root("restart-over-limit-status");
    let ids = (0..17)
        .map(|index| test_performance_operation_id(index + 1))
        .collect::<Vec<_>>();
    for (index, id) in ids.iter().enumerate() {
        write_persisted_performance_status(
            &root,
            id,
            &format!("missing-instance-{index}"),
            "remove",
            "removing",
            PerformanceOperationPayload {
                game_version: None,
                loader: Some("provider_payload=secret-token".to_string()),
                mode: None,
                rollback_id: None,
            },
        );
    }
    let state = build_test_state(&root, None, None);
    let mut blocked_before_resume = 0;
    for id in &ids {
        if state
            .performance_operations()
            .get(id)
            .await
            .is_some_and(|status| {
                status.state
                    == crate::state::performance_operations::PERFORMANCE_RESUME_BLOCKED_STATE
            })
        {
            blocked_before_resume += 1;
        }
    }
    assert!(blocked_before_resume >= 1);

    assert_eq!(
        resume_pending_performance_operations(state.clone()).await,
        17
    );
    for id in &ids {
        wait_for_journal_first_failed_status(&state, id).await;
    }
    let terminal_count = ids
        .iter()
        .filter(|id| {
            let original = state
                .journals()
                .get(&crate::state::contracts::OperationId::new((*id).clone()))
                .is_some_and(|journal| performance_journal_is_terminal(journal.status));
            let reconciliation = state
                .journals()
                .get(&crate::state::contracts::OperationId::new(format!(
                    "{}-reconciliation",
                    id
                )))
                .is_some_and(|journal| performance_journal_is_terminal(journal.status));
            original || reconciliation
        })
        .count();
    assert_eq!(terminal_count, 17);
    let public = performance_operation_status(&state, ids.last().expect("last id"))
        .await
        .expect("public over-limit status");
    let encoded = serde_json::to_string(&public).expect("serialize public status");
    assert_omits_raw_fragments(&encoded, &["provider_payload", "secret-token"]);
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn missing_operation_status_route_returns_json_error() {
    let fixture = TestFixture::new("operation-status-missing");

    let error = handle_operation_status(
        State(fixture.state.clone()),
        Path("performance-install-00000000000000000000000000000000".to_string()),
    )
    .await
    .expect_err("missing operation should fail");

    assert_eq!(error.0, StatusCode::NOT_FOUND);
    assert_eq!(
        error.1.0,
        serde_json::json!({ "error": "performance operation not found" })
    );
}

fn seed_managed_lock(state: &AppState, instance_id: &str, composition_id: &str) -> PathBuf {
    let mods_dir = state.instances().game_dir(instance_id).join("mods");
    let managed = CompositionState {
        composition_id: composition_id.to_string(),
        tier: CompositionTier::Core,
        installed_mods: Vec::new(),
        installed_at: "2026-07-10T00:00:00Z".to_string(),
        failure_count: 0,
        last_failure: String::new(),
    };
    write_managed_state_fixture(&mods_dir, &managed)
}

fn test_mismatched_performance_journal(
    operation_id: &str,
    target_id: &str,
    malformed: bool,
) -> crate::state::contracts::OperationJournalEntry {
    use crate::state::contracts::{
        CommandKind, JournalId, OperationId, OperationJournalEntry, OperationJournalStep,
        OperationPhase, OwnershipClass, StabilizationSystem, TargetKind,
    };

    let mut journal = OperationJournalEntry::new(
        JournalId::new(format!("journal-{operation_id}")),
        OperationId::new(operation_id.to_string()),
        CommandKind::ApplyPerformancePlan,
        StabilizationSystem::Application,
        OwnershipClass::CompositionManaged,
        RollbackState::Unavailable,
    );
    journal.targets = vec![
        TargetDescriptor::new(
            StabilizationSystem::Performance,
            TargetKind::PerformanceComposition,
            target_id,
            OwnershipClass::CompositionManaged,
        ),
        TargetDescriptor::new(
            StabilizationSystem::Performance,
            TargetKind::Artifact,
            format!("{target_id}_managed_artifacts"),
            OwnershipClass::CompositionManaged,
        ),
    ];
    let mut planned =
        OperationJournalStep::new("remove_performance_plan", OperationPhase::Installing);
    planned.rollback = RollbackState::Unavailable;
    planned.generated_facts.extend([
        "performance_operation_evidence".to_string(),
        "performance_rollback_evidence".to_string(),
    ]);
    planned
        .generated_facts
        .push("performance_effect_gate_v1".to_string());
    journal.planned_steps.push(planned.clone());
    if malformed {
        planned.step_id = "apply_performance_plan".to_string();
        journal.planned_steps.push(planned);
    }
    journal
}

#[derive(Clone, Copy)]
enum RestartCheckpoint {
    Terminal,
    TerminalIntent,
    EffectStarted,
}

async fn insert_persisted_test_instance(
    state: &AppState,
    name: &str,
    version_id: &str,
) -> axial_config::Instance {
    let instance = crate::state::new_instance(
        axial_config::generate_instance_id(),
        name.to_string(),
        version_id.to_string(),
        String::new(),
        String::new(),
    );
    let foreground = state
        .register_integrity_foreground()
        .expect("register persisted fixture foreground")
        .wait_for_settlement()
        .await;
    state
        .create_instance(&foreground, instance, None)
        .await
        .expect("persist restart instance fixture")
}

async fn seed_restart_checkpoint(
    name: &str,
    checkpoint: RestartCheckpoint,
) -> (PathBuf, String, String, PathBuf) {
    let root = test_root(name);
    let state = build_test_state(&root, None, None);
    let instance_id = insert_persisted_test_instance(&state, "Managed", "1.20.4-fabric")
        .await
        .id;
    let lock_path = seed_managed_lock(&state, &instance_id, name);
    let status = state
        .performance_operations()
        .start_with_identity(
            instance_id.clone(),
            "remove".to_string(),
            test_operation_payload(),
            crate::state::performance_operations::PerformanceOperationJournalIdentity::new(
                "remove",
                name,
                RollbackState::Unavailable,
            ),
        )
        .await
        .expect("start persisted operation");
    let operation_id = begin_performance_operation_journal(
        &state,
        PerformanceInstallAction::Remove,
        name,
        RollbackState::Unavailable,
        Some(&status.id),
    )
    .await
    .expect("initial journal");
    match checkpoint {
        RestartCheckpoint::Terminal | RestartCheckpoint::TerminalIntent => {
            record_performance_terminal_intent(
                &state,
                &operation_id,
                PerformanceInstallAction::Remove,
                name,
                RollbackState::Unavailable,
                true,
            )
            .await
            .expect("terminal intent checkpoint");
            state
                .performance_operations()
                .record_committing_complete(&status.id)
                .await
                .expect("committing status");
            if matches!(checkpoint, RestartCheckpoint::Terminal) {
                let mut step = crate::state::contracts::OperationJournalStep::new(
                    "remove_performance_plan",
                    crate::state::contracts::OperationPhase::Installing,
                );
                step.result = crate::state::contracts::OperationStepResult::Completed;
                step.rollback = RollbackState::Unavailable;
                step.changed_target = Some(TargetDescriptor::new(
                    crate::state::contracts::StabilizationSystem::Performance,
                    crate::state::contracts::TargetKind::PerformanceComposition,
                    name,
                    crate::state::contracts::OwnershipClass::CompositionManaged,
                ));
                step.generated_facts.extend([
                    "performance_operation_evidence".to_string(),
                    "performance_rollback_evidence".to_string(),
                ]);
                state
                    .journals()
                    .record_success(
                        &operation_id,
                        step,
                        crate::state::contracts::OperationOutcome::Succeeded,
                    )
                    .await
                    .expect("terminal journal");
            }
        }
        RestartCheckpoint::EffectStarted => {
            record_performance_effect_started(
                &state,
                &operation_id,
                PerformanceInstallAction::Remove,
                name,
                RollbackState::Unavailable,
            )
            .await
            .expect("effect checkpoint");
            state
                .performance_operations()
                .record_effect_started(&status.id)
                .await
                .expect("effect status");
        }
    }
    state
        .performance_operations()
        .close()
        .await
        .expect("status store closes for restart");
    state
        .journals()
        .close()
        .await
        .expect("journal store closes for restart");
    drop(state);
    (root, status.id, instance_id, lock_path)
}

fn test_performance_operation_id(index: usize) -> String {
    format!("performance-install-{index:032x}")
}

fn write_persisted_performance_status(
    root: &FsPath,
    id: &str,
    instance_id: &str,
    action: &str,
    state: &str,
    payload: PerformanceOperationPayload,
) {
    let paths = test_paths(root);
    let dir = crate::state::performance_operations::operation_dir(&paths);
    fs::create_dir_all(&dir).expect("create performance status directory");
    let status = crate::state::performance_operations::PerformanceOperationStatus {
        id: id.to_string(),
        instance_id: instance_id.to_string(),
        action: action.to_string(),
        payload,
        state: state.to_string(),
        error: None,
        created_at: "2026-07-10T00:00:00Z".to_string(),
        updated_at: "2026-07-10T00:00:00Z".to_string(),
        journal_identity: Some(
            crate::state::performance_operations::PerformanceOperationJournalIdentity::new(
                action,
                "performance_reconciliation",
                RollbackState::Unavailable,
            ),
        ),
    };
    let mut persisted = serde_json::to_value(&status).expect("serialize performance status");
    persisted
        .as_object_mut()
        .expect("performance status object")
        .insert(
            "journal_identity".to_string(),
            serde_json::json!({
                "action": action,
                "target_id": "performance_reconciliation",
                "rollback": RollbackState::Unavailable,
            }),
        );
    fs::write(
        crate::state::performance_operations::operation_path(&dir, id),
        serde_json::to_vec_pretty(&persisted).expect("serialize persisted performance status"),
    )
    .expect("write performance status");
}

fn operation_journal_snapshot_path(root: &FsPath) -> PathBuf {
    test_paths(root)
        .config_dir
        .join("state")
        .join("operation-journals.json")
}

fn rewrite_performance_journal_targets(root: &FsPath, operation_id: &str, target_id: &str) {
    let path = operation_journal_snapshot_path(root);
    let mut snapshot: serde_json::Value =
        serde_json::from_slice(&fs::read(&path).expect("journal snapshot bytes"))
            .expect("journal snapshot json");
    let entry = snapshot["entries"]
        .as_array_mut()
        .expect("journal entries")
        .iter_mut()
        .find(|entry| entry["operation_id"] == operation_id)
        .expect("performance journal entry");
    for target in entry["targets"].as_array_mut().expect("journal targets") {
        target["id"] = serde_json::Value::String(target_id.to_string());
    }
    fs::write(
        path,
        serde_json::to_vec_pretty(&snapshot).expect("serialize journal snapshot"),
    )
    .expect("rewrite journal targets");
}

#[derive(Clone, Copy)]
enum TerminalIntentCorruption {
    WrongStepAndInjectedAction,
    BothFacts,
}

fn corrupt_performance_terminal_intent(
    root: &FsPath,
    operation_id: &str,
    corruption: TerminalIntentCorruption,
) {
    let path = operation_journal_snapshot_path(root);
    let mut snapshot: serde_json::Value =
        serde_json::from_slice(&fs::read(&path).expect("journal snapshot bytes"))
            .expect("journal snapshot json");
    let entry = snapshot["entries"]
        .as_array_mut()
        .expect("journal entries")
        .iter_mut()
        .find(|entry| entry["operation_id"] == operation_id)
        .expect("performance journal entry");
    let steps = entry["completed_steps"]
        .as_array_mut()
        .expect("completed steps");
    let intent_index = steps
        .iter()
        .position(|step| step["step_id"] == "performance_terminal_intent")
        .expect("terminal intent step");
    match corruption {
        TerminalIntentCorruption::WrongStepAndInjectedAction => {
            steps[intent_index]["step_id"] =
                serde_json::Value::String("untrusted_terminal_intent".to_string());
            let mut injected = steps[intent_index].clone();
            injected["step_id"] = serde_json::Value::String("apply_performance_plan".to_string());
            injected["generated_facts"] = serde_json::json!([
                "performance_operation_evidence",
                "performance_rollback_evidence"
            ]);
            steps.push(injected);
        }
        TerminalIntentCorruption::BothFacts => {
            steps[intent_index]["generated_facts"]
                .as_array_mut()
                .expect("terminal facts")
                .push(serde_json::Value::String(
                    "performance_terminal_failure_v1".to_string(),
                ));
        }
    }
    fs::write(
        path,
        serde_json::to_vec_pretty(&snapshot).expect("serialize corrupt terminal intent"),
    )
    .expect("rewrite terminal intent");
}

fn corrupt_performance_terminal_step(root: &FsPath, operation_id: &str) {
    let path = operation_journal_snapshot_path(root);
    let mut snapshot: serde_json::Value =
        serde_json::from_slice(&fs::read(&path).expect("journal snapshot bytes"))
            .expect("journal snapshot json");
    let entry = snapshot["entries"]
        .as_array_mut()
        .expect("journal entries")
        .iter_mut()
        .find(|entry| entry["operation_id"] == operation_id)
        .expect("performance journal entry");
    let terminal = entry["completed_steps"]
        .as_array_mut()
        .expect("completed steps")
        .iter_mut()
        .find(|step| step["step_id"] == "remove_performance_plan")
        .expect("terminal result step");
    terminal["generated_facts"]
        .as_array_mut()
        .expect("terminal generated facts")
        .push(serde_json::Value::String(
            "unexpected_terminal_fact".to_string(),
        ));
    fs::write(
        path,
        serde_json::to_vec_pretty(&snapshot).expect("serialize malformed terminal"),
    )
    .expect("rewrite terminal step");
}

fn remove_persisted_performance_journal_identity(root: &FsPath, operation_id: &str) {
    let path = crate::state::performance_operations::operation_path(
        &crate::state::performance_operations::operation_dir(&test_paths(root)),
        operation_id,
    );
    let mut status: serde_json::Value =
        serde_json::from_slice(&fs::read(&path).expect("performance status bytes"))
            .expect("performance status json");
    status
        .as_object_mut()
        .expect("performance status object")
        .remove("journal_identity");
    fs::write(
        path,
        serde_json::to_vec_pretty(&status).expect("serialize identityless status"),
    )
    .expect("rewrite identityless status");
}

async fn wait_for_journal_first_failed_status(state: &AppState, operation_id: &str) {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if state
                .performance_operations()
                .get(operation_id)
                .await
                .is_some_and(|status| status.state == "failed")
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("operation terminalizes");
    assert_journal_first_failed_status(state, operation_id).await;
}

async fn assert_journal_first_failed_status(state: &AppState, operation_id: &str) {
    assert_eq!(
        state
            .performance_operations()
            .get(operation_id)
            .await
            .expect("failed status")
            .state,
        "failed"
    );
    let original = state
        .journals()
        .get(&crate::state::contracts::OperationId::new(
            operation_id.to_string(),
        ));
    let reconciliation = state
        .journals()
        .get(&crate::state::contracts::OperationId::new(format!(
            "{operation_id}-reconciliation"
        )));
    let journal = original
        .as_ref()
        .filter(|journal| performance_journal_is_terminal(journal.status))
        .or_else(|| {
            reconciliation
                .as_ref()
                .filter(|journal| performance_journal_is_terminal(journal.status))
        })
        .expect("terminal journal evidence exists before failed status publication");
    assert!(performance_journal_is_terminal(journal.status));
    if original.is_none() {
        assert_eq!(
            journal.failure_point.as_deref(),
            Some("performance_journal_identity_mismatch")
        );
    }
}
