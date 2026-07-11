use super::*;
use sha2::{Digest, Sha512};

fn persisted_state_bytes(state: &impl serde::Serialize) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "schema_version": 1,
        "state": state,
    }))
    .expect("serialize persisted state")
}

fn test_installed_mod_for_bytes(project_id: &str, filename: &str, bytes: &[u8]) -> InstalledMod {
    let mut installed = test_installed_mod(project_id, filename);
    installed.integrity.sha512 = hex::encode(Sha512::digest(bytes));
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
    fs::write(
        mods_dir.join(".axial-lock.json"),
        persisted_state_bytes(&axial_performance::CompositionState {
            composition_id: "core".to_string(),
            tier: CompositionTier::Core,
            installed_mods: vec![axial_performance::InstalledMod {
                project_id: "sodium".to_string(),
                version_id: "version".to_string(),
                filename: "managed.jar".to_string(),
                ownership_class: axial_performance::OwnershipClass::CompositionManaged,
                source: test_modrinth_source(),
                integrity: axial_performance::ManagedArtifactIntegrity {
                    sha512: hex::encode(Sha512::digest(b"managed")),
                    sha512_verified: false,
                },
            }],
            installed_at: "2026-05-30T00:00:00Z".to_string(),
            failure_count: 0,
            last_failure: String::new(),
        }),
    )
    .expect("write state");

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
    fs::write(
        mods_dir.join(".axial-lock.json"),
        persisted_state_bytes(&serde_json::json!({
            "composition_id": "core",
            "tier": "core",
            "installed_mods": [{
                "project_id": "sodium",
                "version_id": "version",
                "filename": "user.jar",
                "ownership_class": "user_managed",
                "source": { "provider": "modrinth" },
                "integrity": { "sha512": "", "sha512_verified": false }
            }],
            "installed_at": "2026-05-30T00:00:00Z",
            "failure_count": 0,
            "last_failure": ""
        })),
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
    fs::write(
        mods_dir.join(".axial-lock.json"),
        persisted_state_bytes(&serde_json::json!({
            "composition_id": "core",
            "tier": "core",
            "installed_mods": [{
                "project_id": "sodium",
                "version_id": "version",
                "filename": "managed.jar",
                "ownership_class": "composition_managed",
                "source": { "provider": "modrinth" },
                "integrity": { "sha512": "abc123", "sha512_verified": true }
            }],
            "installed_at": "2026-05-30T00:00:00Z",
            "failure_count": 0,
            "last_failure": ""
        })),
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
            "error": "invalid performance artifact integrity metadata"
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
    let first = axial_performance::state::save_rollback_snapshot(
        &mods_dir,
        &test_composition_state(
            "core-a",
            vec![test_installed_mod_for_bytes(
                "sodium",
                "managed-a.jar",
                b"managed-a",
            )],
        ),
    )
    .expect("save first snapshot");
    let second = axial_performance::state::save_rollback_snapshot(
        &mods_dir,
        &test_composition_state(
            "core-b",
            vec![test_installed_mod_for_bytes(
                "lithium",
                "managed-b.jar",
                b"managed-b",
            )],
        ),
    )
    .expect("save second snapshot");

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
    axial_performance::state::save_rollback_snapshot(
        &mods_dir,
        &test_composition_state(
            raw_composition_id,
            vec![test_installed_mod_for_bytes(
                "sodium",
                "managed.jar",
                b"managed",
            )],
        ),
    )
    .expect("save snapshot");

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
    let older = axial_performance::state::save_rollback_snapshot(
        &mods_dir,
        &test_composition_state(
            "core-a",
            vec![test_installed_mod_for_bytes(
                "sodium",
                "managed-a.jar",
                b"managed-a",
            )],
        ),
    )
    .expect("save older snapshot");
    fs::remove_file(mods_dir.join("managed-a.jar")).expect("remove superseded managed a");
    fs::write(mods_dir.join("managed-b.jar"), b"managed-b").expect("write managed b");
    axial_performance::state::save_state(
        &mods_dir,
        &test_composition_state(
            "core-b",
            vec![test_installed_mod_for_bytes(
                "lithium",
                "managed-b.jar",
                b"managed-b",
            )],
        ),
    )
    .expect("save current state");
    axial_performance::state::save_rollback_snapshot(
        &mods_dir,
        &test_composition_state(
            "core-b",
            vec![test_installed_mod_for_bytes(
                "lithium",
                "managed-b.jar",
                b"managed-b",
            )],
        ),
    )
    .expect("save newer snapshot");

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
            project_id: "sodium".to_string(),
            version_id: "version".to_string(),
            filename: "managed-a.jar".to_string(),
            ownership_class: axial_performance::OwnershipClass::CompositionManaged,
            source_provider: axial_performance::ManagedArtifactProvider::Modrinth,
            sha512_present: true,
            sha512_verified: false,
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
    axial_performance::state::save_rollback_snapshot(
        &mods_dir,
        &test_composition_state(
            "core-a",
            vec![test_installed_mod_for_bytes(
                "sodium",
                "managed-a.jar",
                b"snapshot-managed",
            )],
        ),
    )
    .expect("save snapshot");
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
