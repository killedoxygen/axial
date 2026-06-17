use super::*;

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
    let phases = events
        .iter()
        .map(|event| event.phase.as_str())
        .collect::<Vec<_>>();
    assert_eq!(phases, vec!["queued", "planning", "removing", "complete"]);
    let terminal = events.last().expect("terminal event");
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
        .await;

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
        .await;

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
    let instance_id = state
        .instances()
        .add(
            "Managed".to_string(),
            "1.20.4-fabric".to_string(),
            String::new(),
            String::new(),
            None,
        )
        .expect("add instance")
        .id;
    let started = state
        .performance_operations()
        .start(
            instance_id.clone(),
            "remove".to_string(),
            test_operation_payload(),
        )
        .await
        .expect("persist pending operation");
    state
        .performance_operations()
        .record_progress(&started.id, "removing")
        .await;
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
    let instance_id = state
        .instances()
        .add(
            "Managed".to_string(),
            "1.20.4-fabric".to_string(),
            String::new(),
            String::new(),
            None,
        )
        .expect("add instance")
        .id;
    let started = state
        .performance_operations()
        .start(
            instance_id.clone(),
            "remove".to_string(),
            test_operation_payload(),
        )
        .await
        .expect("persist pending operation");
    state
        .performance_operations()
        .record_progress(&started.id, "removing")
        .await;
    drop(state);

    let reloaded = build_test_state(&root, None, None);
    let loaded = reloaded
        .performance_operations()
        .get(&started.id)
        .await
        .expect("pending operation should reload");
    assert_eq!(loaded.state, "removing");

    let resumed = resume_pending_performance_operations(reloaded.clone()).await;
    assert_eq!(resumed, 1);
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

    let _ = fs::remove_dir_all(&root);
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
