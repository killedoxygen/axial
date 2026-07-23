use super::*;
use sha2::{Digest, Sha512};

fn test_installed_mod_for_bytes(project_id: &str, filename: &str, bytes: &[u8]) -> InstalledMod {
    let mut installed = test_installed_mod(project_id, filename);
    installed.integrity.sha512 = hex::encode(Sha512::digest(bytes));
    installed.size = bytes.len() as u64;
    installed
}

#[tokio::test]
async fn install_missing_instance_id_returns_json_error() {
    let fixture = TestFixture::new("install-missing-instance-id");

    let error = handle_install(
        State(fixture.state.clone()),
        Json(InstallRequest {
            instance_id: None,
            game_version: None,
            loader: None,
            mode: None,
            action: None,
            rollback_id: None,
            queued: None,
        }),
    )
    .await
    .expect_err("missing instance_id should fail");

    assert_eq!(error.0, StatusCode::BAD_REQUEST);
    assert_eq!(
        error.1.0,
        serde_json::json!({ "error": "instance_id is required" })
    );
}

#[tokio::test]
async fn install_missing_instance_returns_json_error() {
    let fixture = TestFixture::new("install-missing-instance");

    let error = handle_install(
        State(fixture.state.clone()),
        Json(InstallRequest {
            instance_id: Some("missing".to_string()),
            game_version: None,
            loader: None,
            mode: None,
            action: None,
            rollback_id: None,
            queued: None,
        }),
    )
    .await
    .expect_err("missing instance should fail");

    assert_eq!(error.0, StatusCode::NOT_FOUND);
    assert_eq!(
        error.1.0,
        serde_json::json!({ "error": "instance not found" })
    );
}

#[tokio::test]
async fn install_invalid_action_returns_redacted_json_error() {
    let fixture = TestFixture::new("install-invalid-action");
    let instance_id = fixture.add_instance("Managed", "1.20.4-fabric");

    let error = handle_install(
        State(fixture.state.clone()),
        Json(InstallRequest {
            instance_id: Some(instance_id),
            game_version: None,
            loader: None,
            mode: None,
            action: Some("/Users/alice/.minecraft --accessToken raw-secret".to_string()),
            rollback_id: None,
            queued: None,
        }),
    )
    .await
    .expect_err("invalid action should fail");

    assert_eq!(error.0, StatusCode::BAD_REQUEST);
    let body = serde_json::to_string(&error.1.0).expect("error json");
    assert_eq!(
        error.1.0,
        serde_json::json!({ "error": "invalid performance action" })
    );
    assert_omits_raw_fragments(
        &body,
        &["/Users/alice", ".minecraft", "--accessToken", "raw-secret"],
    );
}

#[tokio::test]
async fn install_invalid_mode_returns_redacted_json_error() {
    let fixture = TestFixture::new("install-invalid-mode");
    let instance_id = fixture.add_instance("Managed", "1.20.4-fabric");

    let error = handle_install(
        State(fixture.state.clone()),
        Json(InstallRequest {
            instance_id: Some(instance_id),
            game_version: None,
            loader: None,
            mode: Some(r"C:\Users\Alice\.minecraft --accessToken raw-secret".to_string()),
            action: None,
            rollback_id: None,
            queued: None,
        }),
    )
    .await
    .expect_err("invalid mode should fail");

    assert_eq!(error.0, StatusCode::BAD_REQUEST);
    let body = serde_json::to_string(&error.1.0).expect("error json");
    assert_eq!(
        error.1.0,
        serde_json::json!({ "error": "invalid performance mode" })
    );
    assert_omits_raw_fragments(
        &body,
        &[
            "C:\\Users\\Alice",
            ".minecraft",
            "--accessToken",
            "raw-secret",
        ],
    );
}

#[tokio::test]
async fn install_custom_mode_removes_only_managed_artifacts() {
    let fixture = TestFixture::new("install-custom-remove");
    let instance_id = fixture.add_instance("Custom", "1.20.4-fabric");
    let mods_dir = fixture
        .state
        .instances()
        .game_dir(&instance_id)
        .join("mods");
    fs::create_dir_all(&mods_dir).expect("create mods dir");
    fs::write(mods_dir.join("managed.jar"), b"managed").expect("write managed mod");
    fs::write(mods_dir.join("user.jar"), b"user").expect("write user mod");
    let managed_state = test_composition_state(
        "core",
        vec![test_installed_mod_for_bytes(
            "AANobbMI",
            "managed.jar",
            b"managed",
        )],
    );
    write_managed_state_fixture(&mods_dir, &managed_state);

    let Json(response) = handle_install(
        State(fixture.state.clone()),
        Json(InstallRequest {
            instance_id: Some(instance_id),
            game_version: None,
            loader: None,
            mode: Some("custom".to_string()),
            action: None,
            rollback_id: None,
            queued: None,
        }),
    )
    .await
    .expect("custom mode should remove managed bundle");

    assert!(!response.active);
    assert_eq!(response.status, "removed");
    assert_eq!(response.health, BundleHealth::Disabled);
    assert_eq!(response.installed_count, 0);
    assert!(response.warnings.is_empty());
    assert!(!mods_dir.join("managed.jar").exists());
    assert!(!mods_dir.join(".axial-lock.json").exists());
    assert!(mods_dir.join("user.jar").is_file());
    let journal = fixture
        .state
        .journals()
        .latest_for_command(crate::state::contracts::CommandKind::ApplyPerformancePlan)
        .expect("remove journal");
    assert_eq!(
        journal.status,
        crate::state::contracts::OperationStatus::Succeeded
    );
    assert_eq!(
        journal.rollback,
        crate::state::contracts::RollbackState::Available
    );
    assert!(journal.targets.iter().any(|target| {
        target.id == "core"
            && target.ownership == crate::state::contracts::OwnershipClass::CompositionManaged
    }));
    let completed = journal
        .completed_steps
        .iter()
        .find(|step| step.step_id == "remove_performance_plan")
        .expect("completed remove step");
    assert_eq!(
        completed
            .changed_target
            .as_ref()
            .map(|target| (target.id.as_str(), target.ownership)),
        Some((
            "core",
            crate::state::contracts::OwnershipClass::CompositionManaged
        ))
    );
    assert!(
        completed
            .generated_facts
            .contains(&"performance_rollback_evidence".to_string())
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
}

#[tokio::test]
async fn managed_remove_rejects_active_session_then_succeeds_after_settlement() {
    let fixture = TestFixture::new("remove-active-session");
    let instance_id = fixture.add_instance("Running managed", "1.20.4-fabric");
    let mods_dir = fixture
        .state
        .instances()
        .game_dir(&instance_id)
        .join("mods");
    fs::create_dir_all(&mods_dir).expect("create mods dir");
    fs::write(mods_dir.join("managed.jar"), b"managed").expect("write managed mod");
    let managed_state = test_composition_state(
        "core",
        vec![test_installed_mod_for_bytes(
            "AANobbMI",
            "managed.jar",
            b"managed",
        )],
    );
    write_managed_state_fixture(&mods_dir, &managed_state);
    fixture
        .state
        .sessions()
        .insert(test_launch_record("running-managed-session", &instance_id))
        .await
        .expect("insert active session");

    let request = || InstallRequest {
        instance_id: Some(instance_id.clone()),
        game_version: None,
        loader: None,
        mode: None,
        action: Some("remove".to_string()),
        rollback_id: None,
        queued: None,
    };
    let (status, Json(body)) = handle_install(State(fixture.state.clone()), Json(request()))
        .await
        .expect_err("active session must reject managed mutation");

    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(
        body,
        serde_json::json!({
            "error": "managed composition mutation is blocked while the instance is running"
        })
    );
    assert!(mods_dir.join("managed.jar").is_file());
    assert!(mods_dir.join(".axial-lock.json").is_file());

    fixture
        .state
        .sessions()
        .terminate_all()
        .await
        .expect("settle active session");
    let Json(response) = handle_install(State(fixture.state.clone()), Json(request()))
        .await
        .expect("managed mutation after session settlement");

    assert_eq!(response.status, "removed");
    assert!(!mods_dir.join("managed.jar").exists());
    assert!(!mods_dir.join(".axial-lock.json").exists());
}

#[tokio::test]
async fn install_remove_rejects_invalid_ownership_without_deleting_files() {
    let fixture = TestFixture::new("install-invalid-ownership-remove");
    let instance_id = fixture.add_instance("Custom", "1.20.4-fabric");
    let mods_dir = fixture
        .state
        .instances()
        .game_dir(&instance_id)
        .join("mods");
    fs::create_dir_all(&mods_dir).expect("create mods dir");
    fs::write(mods_dir.join("user.jar"), b"user").expect("write user file");
    let mut invalid_state = serde_json::to_value(test_composition_state(
        "core",
        vec![test_installed_mod_for_bytes(
            "AANobbMI", "user.jar", b"user",
        )],
    ))
    .expect("serialize invalid ownership fixture");
    invalid_state["installed_mods"][0]["ownership_class"] = serde_json::json!("user_managed");
    fs::write(
        mods_dir.join(".axial-lock.json"),
        managed_state_fixture_bytes(&invalid_state),
    )
    .expect("write invalid state");

    let error = handle_install(
        State(fixture.state.clone()),
        Json(InstallRequest {
            instance_id: Some(instance_id),
            game_version: None,
            loader: None,
            mode: Some("custom".to_string()),
            action: None,
            rollback_id: None,
            queued: None,
        }),
    )
    .await
    .expect_err("invalid ownership should fail");

    assert_eq!(error.0, StatusCode::BAD_REQUEST);
    assert_eq!(
        error.1.0,
        serde_json::json!({
            "error": "invalid performance artifact ownership metadata"
        })
    );
    assert_eq!(
        fs::read(mods_dir.join("user.jar")).expect("read user"),
        b"user"
    );
    assert!(mods_dir.join(".axial-lock.json").is_file());
}

#[tokio::test]
async fn install_remove_rejects_invalid_integrity_without_deleting_files() {
    let fixture = TestFixture::new("install-invalid-integrity-remove");
    let instance_id = fixture.add_instance("Custom", "1.20.4-fabric");
    let mods_dir = fixture
        .state
        .instances()
        .game_dir(&instance_id)
        .join("mods");
    fs::create_dir_all(&mods_dir).expect("create mods dir");
    fs::write(mods_dir.join("managed.jar"), b"managed").expect("write managed file");
    let mut invalid_state = serde_json::to_value(test_composition_state(
        "core",
        vec![test_installed_mod_for_bytes(
            "AANobbMI",
            "managed.jar",
            b"managed",
        )],
    ))
    .expect("serialize invalid integrity fixture");
    invalid_state["installed_mods"][0]["integrity"]["sha512"] = serde_json::json!("abc123");
    fs::write(
        mods_dir.join(".axial-lock.json"),
        managed_state_fixture_bytes(&invalid_state),
    )
    .expect("write invalid state");

    let error = handle_install(
        State(fixture.state.clone()),
        Json(InstallRequest {
            instance_id: Some(instance_id),
            game_version: None,
            loader: None,
            mode: Some("custom".to_string()),
            action: None,
            rollback_id: None,
            queued: None,
        }),
    )
    .await
    .expect_err("invalid integrity should fail");

    assert_eq!(error.0, StatusCode::BAD_REQUEST);
    assert_eq!(
        error.1.0,
        serde_json::json!({
            "error": "invalid performance state metadata"
        })
    );
    assert_eq!(
        fs::read(mods_dir.join("managed.jar")).expect("read managed"),
        b"managed"
    );
    assert!(mods_dir.join(".axial-lock.json").is_file());
}

#[tokio::test]
async fn rollback_without_snapshot_returns_json_error() {
    let fixture = TestFixture::new("rollback-missing");
    let instance_id = fixture.add_instance("Managed", "1.20.4-fabric");

    let error = handle_install(
        State(fixture.state.clone()),
        Json(InstallRequest {
            instance_id: Some(instance_id),
            game_version: None,
            loader: None,
            mode: None,
            action: Some("rollback".to_string()),
            rollback_id: None,
            queued: None,
        }),
    )
    .await
    .expect_err("missing rollback snapshot should fail");

    assert_eq!(error.0, StatusCode::BAD_REQUEST);
    assert_eq!(
        error.1.0,
        serde_json::json!({ "error": "no performance rollback snapshot available" })
    );
}

#[tokio::test]
async fn rollback_list_route_returns_snapshot_metadata() {
    let fixture = TestFixture::new("rollback-list");
    let instance_id = fixture.add_instance("Managed", "1.20.4-fabric");
    let mods_dir = fixture
        .state
        .instances()
        .game_dir(&instance_id)
        .join("mods");
    fs::create_dir_all(&mods_dir).expect("create mods dir");
    fs::write(mods_dir.join("managed-a.jar"), b"managed-a").expect("write managed a");
    fs::write(mods_dir.join("managed-b.jar"), b"managed-b").expect("write managed b");
    let first_state = test_composition_state(
        "core-a",
        vec![test_installed_mod_for_bytes(
            "AANobbMI",
            "managed-a.jar",
            b"managed-a",
        )],
    );
    let first = write_rollback_fixture(
        &mods_dir,
        "rb-rollback-list-first",
        "2026-07-10T00:00:00Z",
        &first_state,
        false,
    );
    let second_state = test_composition_state(
        "core-b",
        vec![test_installed_mod_for_bytes(
            "gvQqBUqZ",
            "managed-b.jar",
            b"managed-b",
        )],
    );
    let second = write_rollback_fixture(
        &mods_dir,
        "rb-rollback-list-second",
        "2026-07-10T00:00:01Z",
        &second_state,
        true,
    );

    let response = router()
        .with_state(fixture.state.clone())
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/api/v1/performance/rollback?instance_id={instance_id}"
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
    let value: serde_json::Value = serde_json::from_slice(&body).expect("rollback list json");
    let snapshots = value["snapshots"].as_array().expect("snapshots array");

    assert_eq!(snapshots.len(), 2);
    assert!(snapshots.iter().any(|snapshot| {
        snapshot["id"] == first.id
            && snapshot["composition_id"] == "core-a"
            && snapshot["artifact_count"] == 1
            && snapshot["ownership_class"] == "composition_managed"
            && snapshot["rollback_available"] == true
            && snapshot["latest"] == false
    }));
    assert!(snapshots.iter().any(|snapshot| {
        snapshot["id"] == second.id
            && snapshot["composition_id"] == "core-b"
            && snapshot["artifact_count"] == 1
            && snapshot["ownership_class"] == "composition_managed"
            && snapshot["rollback_available"] == true
            && snapshot["latest"] == true
    }));
}

#[tokio::test]
async fn queued_first_install_and_exact_reapply_report_factual_effect_proof() {
    let mut fixture = TestFixture::new("first-install-available-rollback-proof");
    let version_id = "1.5.2";
    let instance_id = fixture
        .add_persisted_instance("First managed install proof", version_id)
        .await;
    fixture.write_vanilla_version(version_id);

    let Json(queued) = handle_install(
        State(fixture.state.clone()),
        Json(InstallRequest {
            instance_id: Some(instance_id.clone()),
            game_version: Some(version_id.to_string()),
            loader: Some("vanilla".to_string()),
            mode: Some("managed".to_string()),
            action: None,
            rollback_id: None,
            queued: Some(true),
        }),
    )
    .await
    .expect("queue first managed install");
    let operation_id = queued.install_id.expect("queued operation id");
    let events = collect_install_events(&fixture.state, &operation_id).await;
    assert_eq!(events.last().expect("terminal progress").phase, "complete");

    let status = fixture
        .state
        .performance_operations()
        .get(
            &crate::state::contracts::OperationId::try_from(operation_id.as_str())
                .expect("strict operation id"),
        )
        .await
        .expect("durable first-install status");
    assert_eq!(status.state, "complete");
    assert_eq!(
        status
            .journal_identity
            .as_ref()
            .expect("journal identity")
            .rollback,
        RollbackState::Unavailable
    );
    let journal = fixture
        .state
        .journals()
        .get(
            &crate::state::contracts::OperationId::try_from(operation_id.as_str())
                .expect("strict operation id"),
        )
        .expect("first-install journal");
    assert_eq!(journal.rollback, RollbackState::Unavailable);
    assert!(
        journal
            .planned_steps
            .iter()
            .all(|step| step.rollback == RollbackState::Unavailable)
    );

    let public = performance_operation_status(&fixture.state, &operation_id)
        .await
        .expect("public first-install status");
    assert_eq!(public.status.state, "complete");
    assert_eq!(
        public.proof.expect("terminal operation proof").rollback,
        RollbackState::Available
    );
    let snapshots = performance_rollback_list(
        &fixture.state,
        RollbackQuery {
            instance_id: Some(instance_id.clone()),
        },
    )
    .await
    .expect("first-install rollback list")
    .snapshots;
    assert_eq!(snapshots.len(), 1);
    assert_eq!(
        snapshots[0].target,
        axial_performance::RollbackSnapshotTarget::ManagedStateAbsent
    );
    assert!(snapshots[0].rollback_available);

    let Json(reapplied) = handle_install(
        State(fixture.state.clone()),
        Json(InstallRequest {
            instance_id: Some(instance_id),
            game_version: Some(version_id.to_string()),
            loader: Some("vanilla".to_string()),
            mode: Some("managed".to_string()),
            action: None,
            rollback_id: None,
            queued: Some(true),
        }),
    )
    .await
    .expect("queue exact managed reapply");
    let reapply_id = reapplied.install_id.expect("exact reapply operation id");
    let events = collect_install_events(&fixture.state, &reapply_id).await;
    assert_eq!(
        events.last().expect("reapply terminal progress").phase,
        "complete"
    );

    let operation_id = crate::state::contracts::OperationId::try_from(reapply_id.as_str())
        .expect("strict operation id");
    let journal = fixture
        .state
        .journals()
        .get(&operation_id)
        .expect("exact reapply journal");
    assert!(!journal.completed_steps.iter().any(|step| {
        step.step_id == "performance_effect_started" || step.changed_target.is_some()
    }));
    assert!(journal.completed_steps.iter().any(|step| {
        step.step_id == "performance_terminal_intent"
            && step
                .generated_facts
                .iter()
                .any(|fact| fact == "performance_terminal_target_unchanged_v1")
    }));
    let public = performance_operation_status(&fixture.state, &reapply_id)
        .await
        .expect("exact reapply public status");
    assert_eq!(public.status.state, "complete");
    assert!(
        public
            .proof
            .expect("exact reapply proof")
            .fields
            .iter()
            .all(|field| field.key != "latest_changed_target")
    );

    let root = fixture.preserve_root_for_restart();
    fixture
        .state
        .performance_operations()
        .close()
        .await
        .expect("close performance operations before exact reapply restart");
    fixture
        .state
        .journals()
        .close()
        .await
        .expect("close journals before exact reapply restart");
    drop(fixture);

    let restarted = build_test_state(&root, None, None);
    let public = performance_operation_status(&restarted, &reapply_id)
        .await
        .expect("exact reapply status after restart");
    assert_eq!(public.status.state, "complete");
    assert!(
        public
            .proof
            .expect("exact reapply proof after restart")
            .fields
            .iter()
            .all(|field| field.key != "latest_changed_target")
    );
    let journal = restarted
        .journals()
        .get(&operation_id)
        .expect("exact reapply journal after restart");
    assert!(!journal.completed_steps.iter().any(|step| {
        step.step_id == "performance_effect_started" || step.changed_target.is_some()
    }));
    drop(restarted);
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn first_install_absence_rollback_lists_after_restart_and_preserves_user_files() {
    let mut fixture = TestFixture::new("first-install-absence-rollback");
    let version_id = "1.5.2";
    let instance_id = fixture
        .add_persisted_instance("First managed install", version_id)
        .await;
    fixture.write_vanilla_version(version_id);
    let mods_dir = fixture
        .state
        .instances()
        .game_dir(&instance_id)
        .join("mods");
    fs::create_dir_all(&mods_dir).expect("create mods directory");
    fs::write(mods_dir.join("user.jar"), b"user-v1").expect("write user file");

    let Json(installed) = handle_install(
        State(fixture.state.clone()),
        Json(InstallRequest {
            instance_id: Some(instance_id.clone()),
            game_version: Some(version_id.to_string()),
            loader: Some("vanilla".to_string()),
            mode: Some("managed".to_string()),
            action: None,
            rollback_id: None,
            queued: None,
        }),
    )
    .await
    .expect("first managed install");

    assert!(installed.active);
    assert_eq!(installed.status, "complete");
    assert_eq!(installed.composition_id, "family-a-vanilla-enhanced");
    assert!(mods_dir.join(".axial-lock.json").is_file());

    let root = fixture.preserve_root_for_restart();
    fixture
        .state
        .performance_operations()
        .close()
        .await
        .expect("close performance operation store before restart");
    fixture
        .state
        .journals()
        .close()
        .await
        .expect("close operation journal store before restart");
    drop(fixture);

    let restarted = build_test_state(&root, None, None);
    let response = router()
        .with_state(restarted.clone())
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/api/v1/performance/rollback?instance_id={instance_id}"
                ))
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("rollback list after restart");
    let status = response.status();
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read rollback list");
    assert_eq!(
        status,
        StatusCode::OK,
        "rollback list after restart failed: {}",
        String::from_utf8_lossy(&body)
    );
    let value: serde_json::Value = serde_json::from_slice(&body).expect("rollback list json");
    let snapshots = value["snapshots"].as_array().expect("snapshots array");
    assert_eq!(snapshots.len(), 1);
    let snapshot = &snapshots[0];
    assert_eq!(snapshot["target"], "managed_state_absent");
    assert!(snapshot["composition_id"].is_null());
    assert!(snapshot["tier"].is_null());
    assert_eq!(snapshot["installed_count"], 0);
    assert_eq!(snapshot["artifact_count"], 0);
    assert_eq!(snapshot["rollback_available"], true);
    assert_eq!(snapshot["latest"], true);

    let Json(rolled_back) = handle_install(
        State(restarted.clone()),
        Json(InstallRequest {
            instance_id: Some(instance_id.clone()),
            game_version: None,
            loader: None,
            mode: None,
            action: Some("rollback".to_string()),
            rollback_id: snapshot["id"].as_str().map(str::to_string),
            queued: None,
        }),
    )
    .await
    .expect("rollback first install to absence after restart");

    assert!(!rolled_back.active);
    assert_eq!(rolled_back.status, "rolled_back");
    assert_eq!(rolled_back.health, BundleHealth::Disabled);
    assert!(rolled_back.composition_id.is_empty());
    assert!(rolled_back.tier.is_empty());
    assert_eq!(rolled_back.installed_count, 0);
    assert!(rolled_back.managed_artifacts.is_empty());
    assert!(!mods_dir.join(".axial-lock.json").exists());
    assert_eq!(
        fs::read(mods_dir.join("user.jar")).expect("read user file"),
        b"user-v1"
    );

    let retained = performance_rollback_list(
        &restarted,
        RollbackQuery {
            instance_id: Some(instance_id),
        },
    )
    .await
    .expect("list retained absence rollback");
    assert_eq!(retained.snapshots.len(), 1);
    assert_eq!(
        retained.snapshots[0].target,
        axial_performance::RollbackSnapshotTarget::ManagedStateAbsent
    );
    assert!(retained.snapshots[0].rollback_available);

    restarted
        .performance_operations()
        .close()
        .await
        .expect("close restarted performance operation store");
    restarted
        .journals()
        .close()
        .await
        .expect("close restarted operation journal store");
    drop(restarted);
    fs::remove_dir_all(root).expect("remove preserved restart root");
}

#[tokio::test]
async fn rollback_list_route_bounds_public_snapshot_descriptors() {
    let fixture = TestFixture::new("rollback-list-redaction");
    let instance_id = fixture.add_instance("Managed", "1.20.4-fabric");
    let mods_dir = fixture
        .state
        .instances()
        .game_dir(&instance_id)
        .join("mods");
    fs::create_dir_all(&mods_dir).expect("create mods dir");
    fs::write(mods_dir.join("managed.jar"), b"managed").expect("write managed");
    let raw_composition_id = r"C:\Users\Alice\.minecraft\mods\secret.jar";
    let state = test_composition_state(
        raw_composition_id,
        vec![test_installed_mod_for_bytes(
            "AANobbMI",
            "managed.jar",
            b"managed",
        )],
    );
    write_rollback_fixture(
        &mods_dir,
        "rb-rollback-list-redaction",
        "2026-07-10T00:00:00Z",
        &state,
        true,
    );

    let response = router()
        .with_state(fixture.state.clone())
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/api/v1/performance/rollback?instance_id={instance_id}"
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
    let value: serde_json::Value = serde_json::from_slice(&body).expect("rollback list json");
    let snapshot = value["snapshots"][0].as_object().expect("snapshot object");
    let encoded = serde_json::to_string(&value).expect("serialize rollback response");

    assert_ne!(
        snapshot["composition_id"].as_str(),
        Some(raw_composition_id)
    );
    assert!(
        snapshot["composition_id"]
            .as_str()
            .is_some_and(|value| value.starts_with("composition-"))
    );
    assert!(
        snapshot["created_at"]
            .as_str()
            .is_some_and(|value| value.contains('T'))
    );
    for forbidden in ["Alice", ".minecraft", "secret.jar", raw_composition_id] {
        assert!(!encoded.contains(forbidden), "{forbidden}");
    }
}

#[tokio::test]
async fn rollback_with_specific_snapshot_id_restores_older_snapshot() {
    let fixture = TestFixture::new("rollback-specific");
    let instance_id = fixture.add_instance("Managed", "1.20.4-fabric");
    let mods_dir = fixture
        .state
        .instances()
        .game_dir(&instance_id)
        .join("mods");
    fs::create_dir_all(&mods_dir).expect("create mods dir");
    fs::write(mods_dir.join("managed-a.jar"), b"managed-a").expect("write managed a");
    let older_state = test_composition_state(
        "core-a",
        vec![test_installed_mod_for_bytes(
            "AANobbMI",
            "managed-a.jar",
            b"managed-a",
        )],
    );
    let older = write_rollback_fixture(
        &mods_dir,
        "rb-rollback-specific-older",
        "2026-07-10T00:00:00Z",
        &older_state,
        false,
    );
    fs::remove_file(mods_dir.join("managed-a.jar")).expect("remove superseded managed a");
    fs::write(mods_dir.join("managed-b.jar"), b"managed-b").expect("write managed b");
    let newer_state = test_composition_state(
        "core-b",
        vec![test_installed_mod_for_bytes(
            "gvQqBUqZ",
            "managed-b.jar",
            b"managed-b",
        )],
    );
    write_managed_state_fixture(&mods_dir, &newer_state);
    write_rollback_fixture(
        &mods_dir,
        "rb-rollback-specific-newer",
        "2026-07-10T00:00:01Z",
        &newer_state,
        true,
    );

    let Json(response) = handle_install(
        State(fixture.state.clone()),
        Json(InstallRequest {
            instance_id: Some(instance_id),
            game_version: None,
            loader: None,
            mode: None,
            action: Some("rollback".to_string()),
            rollback_id: Some(older.id.clone()),
            queued: None,
        }),
    )
    .await
    .expect("specific rollback should restore");

    assert_eq!(response.status, "rolled_back");
    assert_eq!(response.composition_id, "core-a");
    assert_eq!(
        response.managed_artifacts,
        vec![PerformanceManagedArtifactSummary {
            project_id: "AANobbMI".to_string(),
            version_id: "NFkjnzWE".to_string(),
            filename: "managed-a.jar".to_string(),
            ownership_class: axial_performance::OwnershipClass::CompositionManaged,
            source_provider: axial_performance::ManagedArtifactProvider::Modrinth,
            role: axial_performance::ManagedArtifactRole::Root,
            size: 9,
        }]
    );
    assert_eq!(
        fs::read(mods_dir.join("managed-a.jar")).expect("read managed a"),
        b"managed-a"
    );
    assert!(!mods_dir.join("managed-b.jar").exists());
    let journal = fixture
        .state
        .journals()
        .latest_for_command(crate::state::contracts::CommandKind::ApplyPerformancePlan)
        .expect("rollback journal");
    assert_eq!(
        journal.status,
        crate::state::contracts::OperationStatus::Succeeded
    );
    assert_eq!(
        journal.rollback,
        crate::state::contracts::RollbackState::Available
    );
    let completed = journal
        .completed_steps
        .iter()
        .find(|step| step.step_id == "rollback_performance_plan")
        .expect("completed rollback step");
    assert_eq!(
        completed.rollback,
        crate::state::contracts::RollbackState::Applied
    );
    assert_eq!(
        completed
            .changed_target
            .as_ref()
            .map(|target| (target.id.as_str(), target.ownership)),
        Some((
            "core-a",
            crate::state::contracts::OwnershipClass::CompositionManaged
        ))
    );
    assert!(
        completed
            .generated_facts
            .contains(&"performance_rollback_evidence".to_string())
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
}

#[tokio::test]
async fn rollback_rejects_untracked_same_name_target_without_overwriting() {
    let fixture = TestFixture::new("rollback-untracked-target");
    let instance_id = fixture.add_instance("Managed", "1.20.4-fabric");
    let mods_dir = fixture
        .state
        .instances()
        .game_dir(&instance_id)
        .join("mods");
    fs::create_dir_all(&mods_dir).expect("create mods dir");
    fs::write(mods_dir.join("managed-a.jar"), b"snapshot-managed").expect("write managed a");
    let snapshot_state = test_composition_state(
        "core-a",
        vec![test_installed_mod_for_bytes(
            "AANobbMI",
            "managed-a.jar",
            b"snapshot-managed",
        )],
    );
    write_rollback_fixture(
        &mods_dir,
        "rb-rollback-untracked-target",
        "2026-07-10T00:00:00Z",
        &snapshot_state,
        true,
    );
    fs::write(mods_dir.join("managed-a.jar"), b"user-replacement").expect("replace target");

    let error = handle_install(
        State(fixture.state.clone()),
        Json(InstallRequest {
            instance_id: Some(instance_id),
            game_version: None,
            loader: None,
            mode: None,
            action: Some("rollback".to_string()),
            rollback_id: None,
            queued: None,
        }),
    )
    .await
    .expect_err("rollback should reject untracked same-name target");

    assert_eq!(error.0, StatusCode::BAD_REQUEST);
    assert_eq!(
        error.1.0,
        serde_json::json!({ "error": "invalid performance rollback state" })
    );
    assert_eq!(
        fs::read(mods_dir.join("managed-a.jar")).expect("read target"),
        b"user-replacement"
    );
}

#[tokio::test]
async fn rollback_invalid_snapshot_id_returns_json_error() {
    let fixture = TestFixture::new("rollback-invalid-id");
    let instance_id = fixture.add_instance("Managed", "1.20.4-fabric");

    let error = handle_install(
        State(fixture.state.clone()),
        Json(InstallRequest {
            instance_id: Some(instance_id),
            game_version: None,
            loader: None,
            mode: None,
            action: Some("rollback".to_string()),
            rollback_id: Some("../latest".to_string()),
            queued: None,
        }),
    )
    .await
    .expect_err("invalid rollback id should fail");

    assert_eq!(error.0, StatusCode::BAD_REQUEST);
    assert_eq!(
        error.1.0,
        serde_json::json!({ "error": "invalid performance rollback snapshot id" })
    );
}

#[tokio::test]
async fn rollback_missing_snapshot_id_returns_json_error() {
    let fixture = TestFixture::new("rollback-missing-id");
    let instance_id = fixture.add_instance("Managed", "1.20.4-fabric");

    let error = handle_install(
        State(fixture.state.clone()),
        Json(InstallRequest {
            instance_id: Some(instance_id),
            game_version: None,
            loader: None,
            mode: None,
            action: Some("rollback".to_string()),
            rollback_id: Some("rb-missing".to_string()),
            queued: None,
        }),
    )
    .await
    .expect_err("missing rollback id should fail");

    assert_eq!(error.0, StatusCode::BAD_REQUEST);
    assert_eq!(
        error.1.0,
        serde_json::json!({ "error": "performance rollback snapshot not found" })
    );
}
