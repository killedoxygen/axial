use super::*;
use sha2::{Digest, Sha512};

fn sha512(bytes: &[u8]) -> String {
    hex::encode(Sha512::digest(bytes))
}

#[tokio::test]
async fn plan_missing_game_version_returns_json_error() {
    let fixture = TestFixture::new("plan-missing-game-version");

    let error = handle_plan(
        State(fixture.state.clone()),
        Query(PlanQuery {
            game_version: None,
            loader: None,
            mode: None,
            instance_id: None,
        }),
    )
    .await
    .expect_err("missing game_version should fail");

    assert_eq!(error.0, StatusCode::BAD_REQUEST);
    assert_eq!(
        error.1.0,
        serde_json::json!({ "error": "game_version query parameter is required" })
    );
}

#[tokio::test]
async fn plan_invalid_mode_returns_json_error() {
    let fixture = TestFixture::new("plan-invalid-mode");

    let error = handle_plan(
        State(fixture.state.clone()),
        Query(PlanQuery {
            game_version: Some("1.20.4".to_string()),
            loader: Some("fabric".to_string()),
            mode: Some(r"C:\Users\Alice\.minecraft --accessToken raw-secret".to_string()),
            instance_id: None,
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
async fn plan_custom_mode_serializes_as_inactive() {
    let fixture = TestFixture::new("plan-custom-mode");

    let Json(response) = handle_plan(
        State(fixture.state.clone()),
        Query(PlanQuery {
            game_version: Some(" 1.20.4 ".to_string()),
            loader: Some(" fabric ".to_string()),
            mode: Some("custom".to_string()),
            instance_id: None,
        }),
    )
    .await
    .expect("custom plan should serialize");

    assert!(!response.active);
    assert_eq!(response.effective.selected_mode, PerformanceMode::Custom);
    assert_eq!(response.effective.loader, "fabric");
    assert!(response.effective.managed_artifacts.is_empty());
}

#[tokio::test]
async fn plan_effective_contract_covers_managed_vanilla_and_custom_modes() {
    let fixture = TestFixture::new("plan-effective-contract-modes");

    for (raw_mode, expected_mode, expected_active) in [
        ("managed", PerformanceMode::Managed, true),
        ("vanilla", PerformanceMode::Vanilla, false),
        ("custom", PerformanceMode::Custom, false),
    ] {
        let Json(response) = handle_plan(
            State(fixture.state.clone()),
            Query(PlanQuery {
                game_version: Some("1.20.4".to_string()),
                loader: Some("fabric".to_string()),
                mode: Some(raw_mode.to_string()),
                instance_id: None,
            }),
        )
        .await
        .expect("effective plan should serialize");

        assert_eq!(response.active, expected_active);
        assert_eq!(response.effective.active, expected_active);
        assert_eq!(response.effective.selected_mode, expected_mode);
        assert_eq!(
            response.effective.version_family,
            axial_performance::types::VersionFamily::F
        );
        assert_eq!(response.effective.loader, "fabric");
        assert!(!response.effective.explanation.summary.trim().is_empty());
        assert!(
            response.effective.explanation.summary.len() <= 180,
            "{}",
            response.effective.explanation.summary
        );
    }
}

#[tokio::test]
async fn plan_route_exposes_only_the_effective_contract() {
    let fixture = TestFixture::new("plan-effective-route-shape");

    let response = router()
        .with_state(fixture.state.clone())
        .oneshot(
            Request::builder()
                .uri("/api/v1/performance/plan?game_version=1.20.4&loader=fabric&mode=managed")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("route response");

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    let value: serde_json::Value = serde_json::from_slice(&body).expect("plan json");

    assert_eq!(value["active"], true);
    assert!(value["guardian_facts"].is_array());
    assert_eq!(value["effective"]["active"], true);
    assert_eq!(value["effective"]["selected_mode"], "managed");
    assert!(value["effective"]["composition"]["tier"].is_string());
    assert!(value["effective"]["managed_artifacts"].is_array());
    for removed in [
        "composition_id",
        "family",
        "loader",
        "mode",
        "tier",
        "mods",
        "jvm_preset",
        "warnings",
        "fallback_reason",
    ] {
        assert!(
            value.get(removed).is_none(),
            "removed flat plan field {removed} must stay absent"
        );
    }
}

#[tokio::test]
async fn plan_effective_contract_preserves_hyphenated_family_d_composition_id() {
    let fixture = TestFixture::new("plan-effective-family-d-identity");

    let Json(response) = handle_plan(
        State(fixture.state.clone()),
        Query(PlanQuery {
            game_version: Some("1.15.2".to_string()),
            loader: Some("fabric".to_string()),
            mode: Some("managed".to_string()),
            instance_id: None,
        }),
    )
    .await
    .expect("family d plan should serialize");

    assert!(response.effective.composition.selected);
    assert_eq!(
        response.effective.composition.id.as_deref(),
        Some("family-d-vanilla-enhanced")
    );
}

#[tokio::test]
async fn plan_missing_instance_returns_json_error() {
    let fixture = TestFixture::new("plan-missing-instance");

    let error = handle_plan(
        State(fixture.state.clone()),
        Query(PlanQuery {
            game_version: Some("1.20.4".to_string()),
            loader: Some("fabric".to_string()),
            mode: None,
            instance_id: Some("0000000000000000".to_string()),
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
async fn plan_invalid_instance_id_returns_json_error() {
    let fixture = TestFixture::new("plan-invalid-instance-id");

    let error = handle_plan(
        State(fixture.state.clone()),
        Query(PlanQuery {
            game_version: Some("1.20.4".to_string()),
            loader: Some("fabric".to_string()),
            mode: None,
            instance_id: Some("invalid".to_string()),
        }),
    )
    .await
    .expect_err("invalid instance identity should fail");

    assert_eq!(error.0, StatusCode::BAD_REQUEST);
    assert_eq!(
        error.1.0,
        serde_json::json!({ "error": "instance identity is invalid" })
    );
}

#[tokio::test]
async fn plan_without_instance_id_stays_request_only() {
    let manifest = nvidium_always_manifest("2026-05-30T14:10:00Z");
    let signed = signed_rules_response(&manifest);
    let remote_url = spawn_rules_server(
        serde_json::to_vec(&manifest).expect("serialize remote manifest"),
        Some(signed.signature),
    )
    .await;
    let fixture = TestFixture::new_with_remote_url_and_public_key(
        "plan-request-only-iris",
        Some(remote_url),
        Some(signed.public_key),
    );
    let Json(status) = handle_rules_refresh(State(fixture.state.clone()))
        .await
        .expect("remote manifest should refresh");
    assert_eq!(
        status.status.rule_source,
        axial_performance::RuleSource::Remote
    );
    assert!(status.guardian_facts.is_empty());
    let instance_id = fixture.add_instance("Managed", "1.20.4-fabric");
    let mods_dir = fixture
        .state
        .instances()
        .game_dir(&instance_id)
        .join("mods");
    fs::create_dir_all(&mods_dir).expect("create mods dir");
    fs::write(mods_dir.join("iris-mc1.20.1-1.7.0.jar"), b"iris").expect("write iris jar");

    let Json(response) = handle_plan(
        State(fixture.state.clone()),
        Query(PlanQuery {
            game_version: Some("1.20.4".to_string()),
            loader: Some("fabric".to_string()),
            mode: Some("managed".to_string()),
            instance_id: None,
        }),
    )
    .await
    .expect("request-only plan should serialize");

    assert!(
        response
            .effective
            .managed_artifacts
            .iter()
            .any(|managed_mod| managed_mod.slug == "nvidium")
    );
    assert!(!response.effective.fallback.selected);
}

#[tokio::test]
async fn plan_with_instance_id_uses_user_installed_iris_file_for_nvidium_exclusion() {
    let manifest = nvidium_always_manifest("2026-05-30T14:20:00Z");
    let signed = signed_rules_response(&manifest);
    let remote_url = spawn_rules_server(
        serde_json::to_vec(&manifest).expect("serialize remote manifest"),
        Some(signed.signature),
    )
    .await;
    let fixture = TestFixture::new_with_remote_url_and_public_key(
        "plan-iris-nvidium-exclusion",
        Some(remote_url),
        Some(signed.public_key),
    );
    let Json(status) = handle_rules_refresh(State(fixture.state.clone()))
        .await
        .expect("remote manifest should refresh");
    assert_eq!(
        status.status.rule_source,
        axial_performance::RuleSource::Remote
    );
    assert!(status.guardian_facts.is_empty());
    let instance_id = fixture.add_instance("Managed", "1.20.4-fabric");
    let mods_dir = fixture
        .state
        .instances()
        .game_dir(&instance_id)
        .join("mods");
    fs::create_dir_all(&mods_dir).expect("create mods dir");
    fs::write(mods_dir.join("iris-mc1.20.1-1.7.0.jar"), b"iris").expect("write iris jar");

    let Json(response) = handle_plan(
        State(fixture.state.clone()),
        Query(PlanQuery {
            game_version: Some("1.20.4".to_string()),
            loader: Some("fabric".to_string()),
            mode: Some("managed".to_string()),
            instance_id: Some(instance_id),
        }),
    )
    .await
    .expect("instance-scoped plan should serialize");

    assert!(
        response
            .effective
            .managed_artifacts
            .iter()
            .all(|managed_mod| managed_mod.slug != "nvidium")
    );
    assert!(
        response
            .effective
            .explanation
            .details
            .iter()
            .any(|warning| { warning == "nvidium skipped: incompatible with managed mod iris" })
    );
}

#[tokio::test]
async fn health_custom_mode_ignores_corrupt_state_and_has_one_warnings_field() {
    let fixture = TestFixture::new("health-custom-corrupt-state");
    let instance_id = fixture.add_instance("Custom", "1.20.4-fabric");
    let mut instance = fixture
        .state
        .instances()
        .get(&instance_id)
        .expect("instance should exist");
    instance.performance_mode = "custom".to_string();
    fixture
        .state
        .instances()
        .replace_for_test(instance)
        .expect("update instance");
    let mods_dir = fixture
        .state
        .instances()
        .game_dir(&instance_id)
        .join("mods");
    fs::write(mods_dir.join(".axial-lock.json"), "{not json").expect("write corrupt state");

    let Json(response) = handle_health(
        State(fixture.state.clone()),
        Query(HealthQuery {
            instance_id: Some(instance_id),
        }),
    )
    .await
    .expect("custom health should not read state");

    assert!(!response.active);
    assert_eq!(response.health, BundleHealth::Disabled);
    assert!(response.warnings.is_empty());
    let value = serde_json::to_value(&response).expect("serialize response");
    let object = value.as_object().expect("response object");
    assert_eq!(
        object
            .keys()
            .filter(|key| key.as_str() == "warnings")
            .count(),
        1
    );
}

#[tokio::test]
async fn health_response_includes_bounded_managed_artifact_summary() {
    let fixture = TestFixture::new("health-managed-artifacts");
    let version_id = fabric_version_id("1.20.4");
    let instance_id = fixture.add_instance("Managed", &version_id);
    fixture.write_fabric_version(&version_id, "1.20.4");
    let mods_dir = fixture
        .state
        .instances()
        .game_dir(&instance_id)
        .join("mods");
    fs::create_dir_all(&mods_dir).expect("create mods dir");
    fs::write(mods_dir.join("managed.jar"), b"managed").expect("write managed file");
    write_managed_state_fixture(
        &mods_dir,
        &test_composition_state(
            "core",
            vec![InstalledMod {
                project_id: "AANobbMI".to_string(),
                version_id: "NFkjnzWE".to_string(),
                filename: "managed.jar".to_string(),
                role: axial_performance::ManagedArtifactRole::Root,
                size: 7,
                ownership_class: axial_performance::OwnershipClass::CompositionManaged,
                source: test_modrinth_source(),
                integrity: axial_performance::ManagedArtifactIntegrity {
                    sha512: sha512(b"managed"),
                },
            }],
        ),
    );

    let Json(response) = handle_health(
        State(fixture.state.clone()),
        Query(HealthQuery {
            instance_id: Some(instance_id.clone()),
        }),
    )
    .await
    .expect("managed health should serialize");

    assert_eq!(fixture.state.installed_versions_walk_count(), 1);

    assert!(response.active);
    assert_eq!(response.health, BundleHealth::Invalid);
    assert_eq!(response.installed_count, 1);
    assert_eq!(
        response.managed_artifacts,
        vec![PerformanceManagedArtifactSummary {
            project_id: "AANobbMI".to_string(),
            version_id: "NFkjnzWE".to_string(),
            filename: "managed.jar".to_string(),
            ownership_class: axial_performance::OwnershipClass::CompositionManaged,
            source_provider: axial_performance::ManagedArtifactProvider::Modrinth,
            role: axial_performance::ManagedArtifactRole::Root,
            size: 7,
        }]
    );
    let value = serde_json::to_value(&response).expect("serialize response");
    assert!(value.get("managed_artifacts").is_some());
    assert!(value.to_string().contains("managed.jar"));
    assert!(!value.to_string().contains(&mods_dir.display().to_string()));
    assert!(!value.to_string().contains(&sha512(b"managed")));
    assert_eq!(response.proof.health, bundle_health_token(response.health));
    assert_eq!(
        response.proof.target.ownership,
        crate::state::contracts::OwnershipClass::CompositionManaged
    );
    assert!(
        response
            .proof
            .fields
            .iter()
            .any(|field| { field.key == "managed_artifact_count" && field.value == "1" })
    );
    assert!(
        !serde_json::to_string(&response.proof)
            .expect("serialize proof")
            .contains("managed.jar")
    );
    assert!(!response.view_model.title.trim().is_empty());
    assert!(!response.view_model.detail.trim().is_empty());
    assert_eq!(response.view_model.managed_artifact_count, 1);
    assert_eq!(
        response.view_model.health.as_deref(),
        Some(bundle_health_token(response.health))
    );
    assert!(response.view_model.actions.iter().any(|action| {
        action.action.as_deref() == Some("install")
            && !action.label.trim().is_empty()
            && action.enabled
            && action.disabled_reason.is_none()
    }));
    assert!(response.view_model.actions.iter().any(|action| {
        action.action.as_deref() == Some("rollback")
            && !action.enabled
            && action.disabled_reason.as_deref() == Some("No rollback snapshot is available.")
    }));
    assert!(
        !serde_json::to_string(&response.view_model)
            .expect("serialize view model")
            .contains("managed.jar")
    );
    assert_eq!(response.display.memory.label, "0.5 to 4 GB");
    assert_eq!(response.display.runtime.label, "Java 21");
    assert!(response.display.runtime.detected);
    assert_eq!(response.display.mode.mode, "managed");
    assert_eq!(response.display.mode.source, "global");

    let Json(warm_response) = handle_health(
        State(fixture.state.clone()),
        Query(HealthQuery {
            instance_id: Some(instance_id),
        }),
    )
    .await
    .expect("warm managed health should serialize");
    assert_eq!(warm_response.display.runtime.label, "Java 21");
    assert_eq!(fixture.state.installed_versions_walk_count(), 1);
}

#[tokio::test]
async fn health_response_bounds_public_composition_identifiers() {
    let fixture = TestFixture::new("health-public-composition-redaction");
    let version_id = fabric_version_id("1.20.4");
    let instance_id = fixture.add_instance("Managed", &version_id);
    fixture.write_fabric_version(&version_id, "1.20.4");
    let mods_dir = fixture
        .state
        .instances()
        .game_dir(&instance_id)
        .join("mods");
    fs::create_dir_all(&mods_dir).expect("create mods dir");
    fs::write(mods_dir.join("managed.jar"), b"managed").expect("write managed file");
    let raw_composition_id = r"C:\Users\Alice\.minecraft\mods\secret.jar";
    write_managed_state_fixture(
        &mods_dir,
        &test_composition_state(
            raw_composition_id,
            vec![InstalledMod {
                project_id: "AANobbMI".to_string(),
                version_id: "NFkjnzWE".to_string(),
                filename: "managed.jar".to_string(),
                role: axial_performance::ManagedArtifactRole::Root,
                size: 7,
                ownership_class: axial_performance::OwnershipClass::CompositionManaged,
                source: test_modrinth_source(),
                integrity: axial_performance::ManagedArtifactIntegrity {
                    sha512: sha512(b"managed"),
                },
            }],
        ),
    );

    let Json(response) = handle_health(
        State(fixture.state.clone()),
        Query(HealthQuery {
            instance_id: Some(instance_id),
        }),
    )
    .await
    .expect("managed health should serialize");
    let encoded = serde_json::to_string(&response).expect("serialize response");
    let proof = serde_json::to_string(&response.proof).expect("serialize proof");
    let view_model = serde_json::to_string(&response.view_model).expect("serialize view model");

    assert_ne!(response.composition_id, raw_composition_id);
    assert!(response.composition_id.starts_with("composition-"));
    assert!(
        response
            .proof
            .target
            .id
            .starts_with("performance_composition-")
    );
    assert!(
        response
            .proof
            .fields
            .iter()
            .any(|field| { field.key == "composition_id" && field.value.starts_with("none-") })
    );
    for forbidden in ["Alice", ".minecraft", "secret.jar", raw_composition_id] {
        assert!(!encoded.contains(forbidden), "{forbidden}");
        assert!(!proof.contains(forbidden), "{forbidden}");
        assert!(!view_model.contains(forbidden), "{forbidden}");
    }
}

#[tokio::test]
async fn health_plan_uses_user_installed_iris_file_for_nvidium_exclusion() {
    let mut manifest = axial_performance::builtin_manifest().expect("builtin manifest");
    manifest.generated_at = "2026-05-30T14:00:00Z".to_string();
    for composition in &mut manifest.compositions {
        for managed_mod in &mut composition.mods {
            if managed_mod.slug == "nvidium" {
                managed_mod.condition = axial_performance::types::ModCondition::Always;
                managed_mod.hardware_req = None;
            }
        }
    }
    let signed = signed_rules_response(&manifest);
    let remote_url = spawn_rules_server(
        serde_json::to_vec(&manifest).expect("serialize remote manifest"),
        Some(signed.signature),
    )
    .await;
    let fixture = TestFixture::new_with_remote_url_and_public_key(
        "health-iris-nvidium-exclusion",
        Some(remote_url),
        Some(signed.public_key),
    );
    let Json(status) = handle_rules_refresh(State(fixture.state.clone()))
        .await
        .expect("remote manifest should refresh");
    assert_eq!(
        status.status.rule_source,
        axial_performance::RuleSource::Remote
    );
    assert!(status.guardian_facts.is_empty());
    let version_id = fabric_version_id("1.20.4");
    let instance_id = fixture.add_instance("Managed", &version_id);
    fixture.write_fabric_version(&version_id, "1.20.4");
    let mods_dir = fixture
        .state
        .instances()
        .game_dir(&instance_id)
        .join("mods");
    fs::create_dir_all(&mods_dir).expect("create mods dir");
    fs::write(mods_dir.join("iris-mc1.20.1-1.7.0.jar"), b"iris").expect("write iris jar");

    let Json(response) = handle_health(
        State(fixture.state.clone()),
        Query(HealthQuery {
            instance_id: Some(instance_id),
        }),
    )
    .await
    .expect("managed health should serialize");

    assert!(response.active);
    assert!(
        response
            .warnings
            .iter()
            .any(|warning| { warning == "nvidium skipped: incompatible with managed mod iris" })
    );
}

#[tokio::test]
async fn health_invalidates_user_managed_artifact_in_tracked_state() {
    let fixture = TestFixture::new("health-user-managed-state");
    let instance_id = fixture.add_instance("Managed", "1.20.4-fabric");
    let mods_dir = fixture
        .state
        .instances()
        .game_dir(&instance_id)
        .join("mods");
    fs::create_dir_all(&mods_dir).expect("create mods dir");
    let mut invalid_state = serde_json::to_value(test_composition_state(
        "core",
        vec![InstalledMod {
            project_id: "AANobbMI".to_string(),
            version_id: "NFkjnzWE".to_string(),
            filename: "user.jar".to_string(),
            role: axial_performance::ManagedArtifactRole::Root,
            size: 4,
            ownership_class: axial_performance::OwnershipClass::CompositionManaged,
            source: test_modrinth_source(),
            integrity: axial_performance::ManagedArtifactIntegrity {
                sha512: sha512(b"user"),
            },
        }],
    ))
    .expect("serialize invalid ownership state");
    invalid_state["installed_mods"][0]["ownership_class"] = serde_json::json!("user_managed");
    fs::write(
        mods_dir.join(".axial-lock.json"),
        managed_state_fixture_bytes(&invalid_state),
    )
    .expect("write state");

    let Json(response) = handle_health(
        State(fixture.state.clone()),
        Query(HealthQuery {
            instance_id: Some(instance_id),
        }),
    )
    .await
    .expect("invalid ownership should become health response");

    assert_eq!(response.health, BundleHealth::Invalid);
    assert!(response.managed_artifacts.is_empty());
    assert_eq!(
        response.warnings,
        vec!["invalid performance artifact ownership metadata".to_string()]
    );
    assert_eq!(response.guardian_facts.len(), 1);
    let fact = &response.guardian_facts[0];
    assert_eq!(fact.id.as_str(), "performance_user_owned_conflict");
    assert_eq!(fact.domain, crate::guardian::GuardianDomain::Performance);
    assert_eq!(
        fact.severity,
        Some(crate::guardian::GuardianSeverity::Blocking)
    );
    assert_eq!(
        fact.confidence,
        Some(crate::guardian::GuardianConfidence::Confirmed)
    );
}
