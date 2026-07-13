use super::*;
use crate::guardian::{
    DiagnosisId, GuardianActionKind, GuardianDirective, GuardianDomain, GuardianFactId,
    GuardianLaunchRecoveryCurrentIntent, GuardianMode as ApiGuardianMode,
    launch_recovery_user_intent_fingerprint,
};
use crate::state::contracts::{OwnershipClass, StabilizationSystem, TargetDescriptor, TargetKind};
use crate::state::failure_memory::{FailureMemoryActionOutcome, GuardianFailureMemoryEntry};
use axial_launcher::LaunchFailureClass;
use chrono::{Duration, SecondsFormat, Utc};
use sha1::Sha1;

#[tokio::test]
async fn launch_preflight_ready_payload_for_managed_instance_does_not_create_session() {
    let fixture = TestFixture::new("preflight-managed-ready");
    let instance_id = fixture.add_instance("Survival", "1.21.1");

    let preflight = prepare_launch_preflight(&fixture.state, instance_id.clone())
        .await
        .expect("prepare preflight");

    assert_eq!(preflight.status, "ready");
    assert_eq!(preflight.mode, GuardianMode::Managed);
    assert_eq!(preflight.guardian.mode(), GuardianMode::Managed);
    assert!(!preflight.overrides.java.present);
    assert_eq!(preflight.overrides.java.origin, None);
    assert!(!preflight.overrides.preset.present);
    assert_eq!(preflight.overrides.preset.origin, None);
    assert!(!preflight.overrides.raw_jvm_args.present);
    assert_eq!(preflight.overrides.raw_jvm_args.origin, None);
    assert!(preflight.memory.max_memory_mb > 0);
    assert!(preflight.memory.min_memory_mb >= 0);
    assert!(!preflight.memory.min_clamped);
    assert!(!preflight.readiness.launchable);
    assert_readiness_reason(&preflight, LaunchReadinessReasonId::VersionJsonMissing);
    assert_eq!(fixture.state.sessions().active_session_count().await, 0);
    assert!(
        !fixture
            .state
            .sessions()
            .has_active_instance(&instance_id)
            .await
    );
}

#[tokio::test]
async fn launch_preflight_surfaces_current_instance_crash_memory_without_creating_sessions() {
    for (name, failure_class, expected_guidance) in [
        ("oom", LaunchFailureClass::OutOfMemory, "memory"),
        (
            "mod",
            LaunchFailureClass::ModAttributedCrash,
            "disable the suspected mod",
        ),
    ] {
        let fixture = TestFixture::new(&format!("preflight-failure-memory-{name}"));
        fixture.write_ready_install("1.21.1");
        let instance_id = fixture.add_instance("Survival", "1.21.1");
        fixture.update_instance(&instance_id, |instance| instance.max_memory_mb = 1024);
        fixture
            .state
            .failure_memory()
            .record(startup_failure_memory_entry(
                &instance_id,
                ApiGuardianMode::Managed,
                failure_class,
                &relative_timestamp(Duration::minutes(-5)),
            ))
            .expect("record current launch failure memory");

        let preflight = prepare_launch_preflight(&fixture.state, instance_id.clone())
            .await
            .expect("prepare preflight");

        assert_eq!(preflight.status, "ready");
        let fact = guardian_fact(&preflight, "recent_startup_failure");
        assert_eq!(
            fact_field(fact, "failure_class"),
            Some(failure_class.as_str())
        );
        assert!(preflight.guardian.details().iter().any(|line| {
            line.contains(if failure_class == LaunchFailureClass::OutOfMemory {
                "out-of-memory crash"
            } else {
                "mod-attributed crash"
            })
        }));
        assert!(
            preflight
                .guardian
                .guidance()
                .iter()
                .any(|line| line.contains(expected_guidance))
        );
        assert_eq!(fixture.state.sessions().active_session_count().await, 0);
        assert!(
            !fixture
                .state
                .sessions()
                .has_active_instance(&instance_id)
                .await
        );
    }
}

#[tokio::test]
async fn launch_preflight_ignores_unrelated_mode_instance_and_stale_crash_memory() {
    let fixture = TestFixture::new("preflight-failure-memory-filtering");
    fixture.write_ready_install("1.21.1");
    let instance_id = fixture.add_instance("Survival", "1.21.1");
    for entry in [
        startup_failure_memory_entry(
            "another-instance",
            ApiGuardianMode::Managed,
            LaunchFailureClass::OutOfMemory,
            &relative_timestamp(Duration::minutes(-5)),
        ),
        startup_failure_memory_entry(
            &instance_id,
            ApiGuardianMode::Custom,
            LaunchFailureClass::ModAttributedCrash,
            &relative_timestamp(Duration::minutes(-4)),
        ),
        startup_failure_memory_entry(
            &instance_id,
            ApiGuardianMode::Managed,
            LaunchFailureClass::MissingDependency,
            &relative_timestamp(Duration::hours(-25)),
        ),
    ] {
        fixture
            .state
            .failure_memory()
            .record(entry)
            .expect("record filtered launch failure memory");
    }

    let preflight = prepare_launch_preflight(&fixture.state, instance_id.clone())
        .await
        .expect("prepare preflight");

    assert_eq!(preflight.status, "ready");
    assert!(
        preflight
            .guardian_facts
            .iter()
            .all(|fact| fact.id.as_str() != "recent_startup_failure")
    );
    assert_eq!(fixture.state.sessions().active_session_count().await, 0);
    assert!(
        !fixture
            .state
            .sessions()
            .has_active_instance(&instance_id)
            .await
    );
}

#[tokio::test]
async fn launch_preflight_surfaces_only_active_suppression_for_the_exact_current_intent() {
    for (name, preset, suppression_offset, expected_visible) in [
        ("active", "performance", Duration::minutes(30), true),
        ("expired", "performance", Duration::minutes(-1), false),
        ("wrong-intent", "", Duration::minutes(30), false),
    ] {
        let fixture = TestFixture::new(&format!("preflight-repair-suppression-{name}"));
        fixture.write_ready_install("1.21.1");
        let instance_id = fixture.add_instance("Survival", "1.21.1");
        fixture.update_instance(&instance_id, |instance| {
            instance.jvm_preset = preset.to_string()
        });
        let stored_intent = GuardianLaunchRecoveryCurrentIntent {
            target_version_id: "1.21.1",
            requested_java: "",
            explicit_jvm_args: &[],
            requested_preset: "performance",
        };
        let intent_hash = launch_recovery_user_intent_fingerprint(
            stored_intent,
            &GuardianDirective::startup_preset_downgrade("performance"),
        )
        .expect("valid stored recovery intent");
        fixture
            .state
            .failure_memory()
            .record(
                GuardianFailureMemoryEntry::observed(
                    DiagnosisId::JvmPresetRecovery,
                    GuardianDomain::Launch,
                    instance_target(&instance_id, OwnershipClass::LauncherManaged),
                    ApiGuardianMode::Managed,
                    Some(&intent_hash),
                    relative_timestamp(Duration::minutes(-5)),
                )
                .with_action(
                    GuardianActionKind::Downgrade,
                    FailureMemoryActionOutcome::Failed,
                )
                .with_repair_attempt()
                .with_suppression_until(relative_timestamp(suppression_offset)),
            )
            .expect("record launch repair suppression");

        let preflight = prepare_launch_preflight(&fixture.state, instance_id.clone())
            .await
            .expect("prepare preflight");
        assert_eq!(preflight.status, "ready");
        let suppression_fact = preflight
            .guardian_facts
            .iter()
            .find(|fact| fact.id.as_str() == "repair_suppressed_until");

        assert_eq!(suppression_fact.is_some(), expected_visible);
        assert_eq!(
            preflight.guardian.details().iter().any(|line| {
                line.contains("Guardian will not auto-repair this launch again until")
                    && line.ends_with(" UTC.")
            }),
            expected_visible
        );
        assert_eq!(fixture.state.sessions().active_session_count().await, 0);
        assert!(
            !fixture
                .state
                .sessions()
                .has_active_instance(&instance_id)
                .await
        );
    }
}

#[tokio::test]
async fn launch_preflight_readiness_reports_missing_version_json() {
    let fixture = TestFixture::new("preflight-readiness-missing-version-json");
    let instance_id = fixture.add_instance("Survival", "1.21.1");

    let preflight = prepare_launch_preflight(&fixture.state, instance_id)
        .await
        .expect("prepare preflight");

    assert!(!preflight.readiness.launchable);
    assert_eq!(preflight.readiness.reasons.len(), 1);
    assert_readiness_reason(&preflight, LaunchReadinessReasonId::VersionJsonMissing);
    assert_eq!(
        preflight.guardian.decision(),
        GuardianSummaryDecision::Blocked
    );
    assert_guardian_fact(&preflight, "version_json_missing");
    assert!(preflight.guardian.details().iter().any(|detail| {
        detail == "Guardian blocked launch because installed version metadata is missing."
    }));
}

#[tokio::test]
async fn launch_preflight_rejects_installed_report_from_changed_library_root() {
    let fixture = TestFixture::new("preflight-library-root-switch");
    fixture.write_ready_install("1.21.1");
    let instance_id = fixture.add_instance("Survival", "1.21.1");
    let instance = fixture
        .state
        .instances()
        .get(&instance_id)
        .expect("instance");
    let config = fixture.state.config().current();
    let game_dir = fixture.state.instances().game_dir(&instance.id);
    let changed_library = fixture.root.join("changed-library");
    fs::create_dir_all(changed_library.join("versions")).expect("create changed library");
    fixture
        .state
        .set_library_dir_for_test(changed_library.to_string_lossy().into_owned());
    let producer = fixture
        .state
        .try_claim_producer()
        .expect("claim preflight producer");
    let integrity_foreground = fixture
        .state
        .register_integrity_foreground()
        .expect("register preflight foreground")
        .wait_for_settlement()
        .await;

    let preflight = build_launch_preflight_facts(
        &fixture.state,
        &producer,
        LaunchPreflightBuild {
            integrity_foreground: &integrity_foreground,
            instance_lifecycle: &fixture.state.acquire_instance_lifecycle(&instance.id).await,
            instance: &instance,
            config: &config,
            library_dir: &fixture.paths.library_dir,
            game_dir: &game_dir,
            requested_max_memory_mb: None,
            requested_min_memory_mb: None,
        },
        None,
    )
    .await;

    assert!(!preflight.readiness.launchable);
    assert!(preflight.readiness.reasons.iter().any(|reason| {
        reason.id == LaunchReadinessReasonId::InstalledVersionsDegraded
            && reason.message == VERSION_SCAN_DEGRADED_MESSAGE
    }));
    assert_eq!(fixture.state.installed_versions_walk_count(), 1);
}

#[tokio::test]
async fn launch_preflight_readiness_reports_missing_client_jar() {
    let fixture = TestFixture::new("preflight-readiness-missing-client-jar");
    let component = "java-runtime-delta";
    let expected_client = b"client jar";
    fixture.write_version_json(
        "1.21.1",
        serde_json::json!({
            "id": "1.21.1",
            "type": "release",
            "mainClass": "net.minecraft.client.main.Main",
            "assetIndex": {},
            "downloads": { "client": {
                "sha1": sha1_hex(expected_client),
                "size": expected_client.len()
            }},
            "javaVersion": { "component": component, "majorVersion": 21 },
            "libraries": []
        }),
    );
    let instance_id = fixture.add_instance("Survival", "1.21.1");
    fixture.activate_expected_version_inventory(
        &instance_id,
        "1.21.1",
        Some(expected_client.len() as u64),
        [],
    );

    let preflight = prepare_launch_preflight(&fixture.state, instance_id)
        .await
        .expect("prepare preflight");

    assert!(!preflight.readiness.launchable);
    assert_readiness_reason(&preflight, LaunchReadinessReasonId::ClientJarMissing);
    let runtime_reason =
        readiness_reason(&preflight, LaunchReadinessReasonId::ManagedRuntimeMissing);
    assert_eq!(
        runtime_reason.severity,
        LaunchReadinessSeverity::Recoverable
    );
    assert_eq!(
        preflight.guardian.decision(),
        GuardianSummaryDecision::Blocked
    );
    assert_guardian_fact(&preflight, "artifact_missing");
    assert_guardian_fact(&preflight, "managed_runtime_missing");
    assert!(preflight.guardian.details().iter().any(|detail| {
        detail == "Guardian blocked launch because launcher-managed game files are corrupt."
    }));
}

#[tokio::test]
async fn launch_preflight_preserves_tier_zero_size_drift_fact_semantics() {
    let fixture = TestFixture::new("preflight-readiness-client-size-drift");
    let expected_client = b"client jar";
    fixture.write_version_json(
        "1.21.1",
        serde_json::json!({
            "id": "1.21.1",
            "type": "release",
            "mainClass": "net.minecraft.client.main.Main",
            "assetIndex": {},
            "downloads": { "client": {
                "sha1": sha1_hex(expected_client),
                "size": expected_client.len()
            }},
            "javaVersion": { "component": "java-runtime-delta", "majorVersion": 21 },
            "libraries": []
        }),
    );
    let version_dir = fixture.paths.library_dir.join("versions").join("1.21.1");
    fs::write(version_dir.join("1.21.1.jar"), b"oversized client jar")
        .expect("write oversized client jar");
    fixture.write_ready_runtime("java-runtime-delta");
    let instance_id = fixture.add_instance("Survival", "1.21.1");
    fixture.activate_expected_version_inventory(
        &instance_id,
        "1.21.1",
        Some(expected_client.len() as u64),
        [],
    );

    let preflight = prepare_launch_preflight(&fixture.state, instance_id)
        .await
        .expect("prepare preflight");

    assert!(!preflight.readiness.launchable);
    assert_readiness_reason(&preflight, LaunchReadinessReasonId::ClientJarCorrupt);
    assert_guardian_fact(&preflight, "artifact_size_drift");
    assert!(
        !preflight
            .guardian_facts
            .iter()
            .any(|fact| fact.id == GuardianFactId::ArtifactChecksumMismatch)
    );
    assert_eq!(
        preflight.guardian.decision(),
        GuardianSummaryDecision::Blocked
    );
}

#[tokio::test]
async fn launch_preflight_readiness_reports_missing_library_as_guardian_fact() {
    let fixture = TestFixture::new("preflight-readiness-missing-libraries");
    let client = b"client jar";
    let library = b"library";
    fixture.write_version_json(
        "1.21.1",
        serde_json::json!({
            "id": "1.21.1",
            "type": "release",
            "mainClass": "net.minecraft.client.main.Main",
            "assetIndex": {},
            "downloads": { "client": {
                "sha1": sha1_hex(client),
                "size": client.len()
            }},
            "javaVersion": { "component": "java-runtime-delta", "majorVersion": 21 },
            "libraries": [{
                "name": "com.example:demo:1.0.0",
                "downloads": {
                    "artifact": {
                        "path": "com/example/demo/1.0.0/demo-1.0.0.jar",
                        "url": "https://example.invalid/demo-1.0.0.jar",
                        "sha1": sha1_hex(library),
                        "size": library.len()
                    }
                }
            }]
        }),
    );
    let version_dir = fixture.paths.library_dir.join("versions").join("1.21.1");
    fs::write(version_dir.join("1.21.1.jar"), client).expect("write client jar");
    fixture.write_ready_runtime("java-runtime-delta");
    let instance_id = fixture.add_instance("Survival", "1.21.1");
    fixture.activate_expected_version_inventory(
        &instance_id,
        "1.21.1",
        Some(client.len() as u64),
        [TestFixture::expected_file(
            TestKnownGoodRoot::Libraries,
            "com/example/demo/1.0.0/demo-1.0.0.jar",
            KnownGoodArtifactKind::Library,
            library.len(),
        )],
    );

    let preflight = prepare_launch_preflight(&fixture.state, instance_id)
        .await
        .expect("prepare preflight");

    assert!(!preflight.readiness.launchable);
    assert_readiness_reason(&preflight, LaunchReadinessReasonId::LibrariesMissing);
    assert_eq!(
        preflight.guardian.decision(),
        GuardianSummaryDecision::Blocked
    );
    assert!(preflight.guardian_facts.iter().any(|fact| {
        fact.id.as_str() == "artifact_missing"
            && fact
                .fields
                .iter()
                .any(|field| field.key == "artifact_kind" && field.value == "library")
    }));
    assert!(preflight.guardian.details().iter().any(|detail| {
        detail == "Guardian blocked launch because launcher-managed game files are corrupt."
    }));
}

#[tokio::test]
async fn launch_preflight_readiness_reports_missing_asset_index_as_guardian_fact() {
    let fixture = TestFixture::new("preflight-readiness-missing-asset-index");
    let client = b"client jar";
    let asset_index = b"asset index";
    fixture.write_version_json(
        "1.21.1",
        serde_json::json!({
            "id": "1.21.1",
            "type": "release",
            "mainClass": "net.minecraft.client.main.Main",
            "assetIndex": {
                "id": "test-assets",
                "sha1": sha1_hex(asset_index),
                "size": asset_index.len()
            },
            "downloads": { "client": {
                "sha1": sha1_hex(client),
                "size": client.len()
            }},
            "javaVersion": { "component": "java-runtime-delta", "majorVersion": 21 },
            "libraries": []
        }),
    );
    let version_dir = fixture.paths.library_dir.join("versions").join("1.21.1");
    fs::write(version_dir.join("1.21.1.jar"), client).expect("write client jar");
    fixture.write_ready_runtime("java-runtime-delta");
    let instance_id = fixture.add_instance("Survival", "1.21.1");
    fixture.activate_expected_version_inventory(
        &instance_id,
        "1.21.1",
        Some(client.len() as u64),
        [TestFixture::expected_file(
            TestKnownGoodRoot::Assets,
            "indexes/test-assets.json",
            KnownGoodArtifactKind::AssetIndex,
            asset_index.len(),
        )],
    );

    let preflight = prepare_launch_preflight(&fixture.state, instance_id)
        .await
        .expect("prepare preflight");

    assert!(!preflight.readiness.launchable);
    assert_readiness_reason(&preflight, LaunchReadinessReasonId::AssetIndexMissing);
    assert_eq!(
        preflight.guardian.decision(),
        GuardianSummaryDecision::Blocked
    );
    assert_guardian_fact(&preflight, "artifact_missing");
    assert!(preflight.guardian.details().iter().any(|detail| {
        detail == "Guardian blocked launch because launcher-managed game files are corrupt."
    }));
}

#[tokio::test]
async fn launch_preflight_tier_zero_does_not_hash_same_size_artifact_drift() {
    let fixture = TestFixture::new("preflight-readiness-corrupt-artifacts");
    let expected_client = b"fresh-client";
    let expected_library = b"fresh-library";
    let expected_asset_index = b"fresh-assets";
    fixture.write_version_json(
        "1.21.1",
        serde_json::json!({
            "id": "1.21.1",
            "type": "release",
            "mainClass": "net.minecraft.client.main.Main",
            "assetIndex": {
                "id": "test-assets",
                "sha1": sha1_hex(expected_asset_index),
                "size": expected_asset_index.len()
            },
            "downloads": {
                "client": {
                    "sha1": sha1_hex(expected_client),
                    "size": expected_client.len()
                }
            },
            "javaVersion": { "component": "java-runtime-delta", "majorVersion": 21 },
            "libraries": [{
                "name": "com.example:demo:1.0.0",
                "downloads": {
                    "artifact": {
                        "path": "com/example/demo/1.0.0/demo-1.0.0.jar",
                        "url": "https://example.invalid/demo-1.0.0.jar",
                        "sha1": sha1_hex(expected_library),
                        "size": expected_library.len()
                    }
                }
            }]
        }),
    );
    let version_dir = fixture.paths.library_dir.join("versions").join("1.21.1");
    fs::write(version_dir.join("1.21.1.jar"), b"wrong-client").expect("write corrupt client jar");
    let library_path = fixture
        .paths
        .library_dir
        .join("libraries")
        .join("com/example/demo/1.0.0/demo-1.0.0.jar");
    fs::create_dir_all(library_path.parent().expect("library parent")).expect("library dir");
    fs::write(&library_path, b"wrong-library").expect("write corrupt library");
    let asset_index_path = fixture
        .paths
        .library_dir
        .join("assets")
        .join("indexes")
        .join("test-assets.json");
    fs::create_dir_all(asset_index_path.parent().expect("asset index parent"))
        .expect("asset index dir");
    fs::write(&asset_index_path, b"wrong-assets").expect("write corrupt asset index");
    fixture.write_ready_runtime("java-runtime-delta");
    let instance_id = fixture.add_instance("Survival", "1.21.1");
    fixture.activate_expected_version_inventory(
        &instance_id,
        "1.21.1",
        Some(expected_client.len() as u64),
        [
            TestFixture::expected_file(
                TestKnownGoodRoot::Libraries,
                "com/example/demo/1.0.0/demo-1.0.0.jar",
                KnownGoodArtifactKind::Library,
                expected_library.len(),
            ),
            TestFixture::expected_file(
                TestKnownGoodRoot::Assets,
                "indexes/test-assets.json",
                KnownGoodArtifactKind::AssetIndex,
                expected_asset_index.len(),
            ),
        ],
    );

    let preflight = prepare_launch_preflight(&fixture.state, instance_id)
        .await
        .expect("prepare preflight");

    assert!(preflight.readiness.launchable);
    assert_ne!(
        preflight.guardian.decision(),
        GuardianSummaryDecision::Blocked
    );
    assert!(!preflight.guardian_facts.iter().any(|fact| {
        matches!(
            fact.id,
            GuardianFactId::ArtifactChecksumMismatch | GuardianFactId::ArtifactSizeDrift
        )
    }));

    let payload = serde_json::to_string(&preflight).expect("preflight json");
    assert!(!payload.contains(&fixture.root.to_string_lossy().to_string()));
    assert!(!payload.contains("wrong-client"));
    assert!(!payload.contains("wrong-library"));
    assert!(!payload.contains("wrong-assets"));
}

#[tokio::test]
async fn launch_preflight_readiness_reports_missing_managed_runtime_as_recoverable_fact() {
    let fixture = TestFixture::new("preflight-readiness-missing-managed-runtime");
    fixture.write_version_json(
        "1.21.1",
        serde_json::json!({
            "id": "1.21.1",
            "type": "release",
            "mainClass": "net.minecraft.client.main.Main",
            "assetIndex": {},
            "javaVersion": { "component": "java-runtime-epsilon", "majorVersion": 21 },
            "libraries": []
        }),
    );
    let version_dir = fixture.paths.library_dir.join("versions").join("1.21.1");
    fs::write(version_dir.join("1.21.1.jar"), b"client jar").expect("write client jar");
    let instance_id = fixture.add_instance("Survival", "1.21.1");

    let preflight = prepare_launch_preflight(&fixture.state, instance_id)
        .await
        .expect("prepare preflight");

    assert!(preflight.readiness.launchable);
    assert!(matches!(
        preflight.guardian.decision(),
        GuardianSummaryDecision::Allowed | GuardianSummaryDecision::Warned
    ));
    assert_eq!(
        readiness_reason(&preflight, LaunchReadinessReasonId::ManagedRuntimeMissing).severity,
        LaunchReadinessSeverity::Recoverable
    );
    let fact = guardian_fact(&preflight, "managed_runtime_missing");
    assert_eq!(fact.domain, crate::guardian::GuardianDomain::Runtime);
    assert_eq!(fact.severity, Some(GuardianSeverity::Recoverable));
}

fn sha1_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha1::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

#[tokio::test]
async fn launch_preparation_repairs_managed_runtime_ready_marker_before_blocking_readiness() {
    let fixture = TestFixture::new("prepare-repairs-runtime-ready-marker");
    let component = "java-runtime-delta";
    fixture.write_version_json(
        "1.21.1",
        serde_json::json!({
            "id": "1.21.1",
            "type": "release",
            "mainClass": "net.minecraft.client.main.Main",
            "assetIndex": {},
            "javaVersion": { "component": component, "majorVersion": 21 },
            "libraries": []
        }),
    );
    let version_dir = fixture.paths.library_dir.join("versions").join("1.21.1");
    fs::write(version_dir.join("1.21.1.jar"), b"client jar").expect("client jar");
    let runtime_root = fixture.write_global_runtime_without_ready_marker(component);
    let instance_id = fixture.add_instance("Survival", "1.21.1");
    let instance = fixture
        .state
        .instances()
        .get(&instance_id)
        .expect("instance");
    let config = fixture.state.config().current();
    let game_dir = fixture.state.instances().game_dir(&instance.id);
    let producer = fixture
        .state
        .try_claim_producer()
        .expect("claim runtime repair producer");
    let integrity_foreground = fixture
        .state
        .register_integrity_foreground()
        .expect("register runtime repair foreground")
        .wait_for_settlement()
        .await;

    let preflight = build_launch_preflight_facts(
        &fixture.state,
        &producer,
        LaunchPreflightBuild {
            integrity_foreground: &integrity_foreground,
            instance_lifecycle: &fixture.state.acquire_instance_lifecycle(&instance.id).await,
            instance: &instance,
            config: &config,
            library_dir: &fixture.paths.library_dir,
            game_dir: &game_dir,
            requested_max_memory_mb: None,
            requested_min_memory_mb: None,
        },
        None,
    )
    .await;
    assert!(
        readiness_has_managed_runtime_missing(&preflight.readiness),
        "missing managed runtime readiness reason: {:?}",
        preflight.readiness.reasons
    );
    assert_eq!(fixture.state.installed_versions_walk_count(), 1);

    let repaired = maybe_repair_managed_runtime_before_launch_owned(
        &fixture.state,
        &producer,
        &integrity_foreground,
        preflight,
        ManagedRuntimeRepairLaunch {
            instance_lifecycle: &fixture.state.acquire_instance_lifecycle(&instance.id).await,
            instance: &instance,
            library_dir: &fixture.paths.library_dir,
            game_dir: &game_dir,
            requested_max_memory_mb: None,
            requested_min_memory_mb: None,
        },
    )
    .await
    .expect("persist managed-runtime repair journal");

    assert_eq!(fixture.state.installed_versions_walk_count(), 1);
    assert!(runtime_root.join(".axial-ready").is_file());
    assert_eq!(
        repaired.guardian_summary.decision(),
        GuardianSummaryDecision::Intervened
    );
    assert!(
        repaired.guardian_summary.details().iter().any(|detail| {
            detail == "Guardian repaired the managed Java runtime before launch."
        })
    );
    let memory = fixture.state.failure_memory().list();
    assert_eq!(memory.len(), 1);
    assert_eq!(
        memory[0].last_action_outcome,
        Some(FailureMemoryActionOutcome::Repaired)
    );
    assert_eq!(memory[0].repair_attempt_count, 1);
}

#[tokio::test]
async fn launch_preparation_repairs_corrupt_managed_runtime_ready_marker_before_launch() {
    let fixture = TestFixture::new("prepare-repairs-runtime-corrupt-ready-marker");
    let component = "java-runtime-delta";
    fixture.write_version_json(
        "1.21.1",
        serde_json::json!({
            "id": "1.21.1",
            "type": "release",
            "mainClass": "net.minecraft.client.main.Main",
            "assetIndex": {},
            "javaVersion": { "component": component, "majorVersion": 21 },
            "libraries": []
        }),
    );
    let version_dir = fixture.paths.library_dir.join("versions").join("1.21.1");
    fs::write(version_dir.join("1.21.1.jar"), b"client jar").expect("client jar");
    let runtime_root = fixture.write_global_runtime_without_ready_marker(component);
    fs::create_dir(runtime_root.join(".axial-ready")).expect("corrupt ready marker directory");
    let instance_id = fixture.add_instance("Survival", "1.21.1");
    let instance = fixture
        .state
        .instances()
        .get(&instance_id)
        .expect("instance");
    let config = fixture.state.config().current();
    let game_dir = fixture.state.instances().game_dir(&instance.id);
    let integrity_foreground = fixture
        .state
        .register_integrity_foreground()
        .expect("register runtime repair foreground")
        .wait_for_settlement()
        .await;

    let preflight = build_launch_preflight_facts(
        &fixture.state,
        &fixture
            .state
            .try_claim_producer()
            .expect("claim preflight producer"),
        LaunchPreflightBuild {
            integrity_foreground: &integrity_foreground,
            instance_lifecycle: &fixture.state.acquire_instance_lifecycle(&instance.id).await,
            instance: &instance,
            config: &config,
            library_dir: &fixture.paths.library_dir,
            game_dir: &game_dir,
            requested_max_memory_mb: None,
            requested_min_memory_mb: None,
        },
        None,
    )
    .await;
    assert!(
        readiness_has_managed_runtime_missing(&preflight.readiness),
        "corrupt managed runtime should be readiness-visible before repair: {:?}",
        preflight.readiness.reasons
    );

    let producer = fixture
        .state
        .try_claim_producer()
        .expect("claim runtime repair producer");
    let repaired = maybe_repair_managed_runtime_before_launch_owned(
        &fixture.state,
        &producer,
        &integrity_foreground,
        preflight,
        ManagedRuntimeRepairLaunch {
            instance_lifecycle: &fixture.state.acquire_instance_lifecycle(&instance.id).await,
            instance: &instance,
            library_dir: &fixture.paths.library_dir,
            game_dir: &game_dir,
            requested_max_memory_mb: None,
            requested_min_memory_mb: None,
        },
    )
    .await
    .expect("persist managed-runtime repair journal");

    assert!(runtime_root.join(".axial-ready").is_file());
    assert_eq!(
        repaired.guardian_summary.decision(),
        GuardianSummaryDecision::Intervened
    );
    assert!(
        repaired.guardian_summary.details().iter().any(|detail| {
            detail == "Guardian repaired the managed Java runtime before launch."
        })
    );
    let memory = fixture.state.failure_memory().list();
    assert_eq!(memory.len(), 1);
    assert_eq!(
        memory[0].last_action_outcome,
        Some(FailureMemoryActionOutcome::Repaired)
    );
    assert_eq!(memory[0].repair_attempt_count, 1);
    let journal = fixture
        .state
        .journals()
        .latest_for_command(CommandKind::RepairInstance)
        .expect("repair journal");
    assert_eq!(journal.status, OperationStatus::Succeeded);
    assert_eq!(journal.outcome, Some(OperationOutcome::Succeeded));
    assert!(journal.completed_steps.iter().any(|step| {
        step.generated_facts
            .iter()
            .any(|fact| fact == "RuntimeRepairApplied")
    }));
    let payload = serde_json::to_string(&repaired.guardian_summary).expect("guardian summary json");
    assert!(!payload.contains(&fixture.root.to_string_lossy().to_string()));
}

#[tokio::test]
async fn prepare_launch_session_queues_recoverable_managed_runtime_missing_java() {
    let fixture = TestFixture::new("prepare-recovers-runtime-missing-java");
    let component = "java-runtime-delta";
    fixture.write_version_json(
        "1.21.1",
        serde_json::json!({
            "id": "1.21.1",
            "type": "release",
            "mainClass": "net.minecraft.client.main.Main",
            "assetIndex": {},
            "javaVersion": { "component": component, "majorVersion": 21 },
            "libraries": []
        }),
    );
    let version_dir = fixture.paths.library_dir.join("versions").join("1.21.1");
    fs::write(version_dir.join("1.21.1.jar"), b"client jar").expect("client jar");
    let runtime_root = fixture
        .state
        .managed_runtime_cache()
        .component_root(component)
        .expect("runtime root");
    fs::create_dir_all(&runtime_root).expect("runtime root");
    fs::write(runtime_root.join(".axial-ready"), b"ready").expect("ready marker");
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
    .expect("recoverable runtime should queue for ensure");

    assert!(
        !fixture
            .state
            .subscribe_integrity_idle()
            .borrow()
            .is_stably_idle(),
        "repaired re-preflight must return foreground ownership to the runner"
    );
    assert_eq!(fixture.state.sessions().active_session_count().await, 1);
    assert!(
        fixture
            .state
            .sessions()
            .has_active_instance(&instance_id)
            .await
    );
    assert!(fixture.state.failure_memory().list().is_empty());
    drop(prepared);
    assert!(
        fixture
            .state
            .subscribe_integrity_idle()
            .borrow()
            .is_stably_idle()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn prepare_launch_session_queues_recoverable_non_executable_managed_runtime() {
    let fixture = TestFixture::new("prepare-recovers-runtime-non-executable-java");
    let component = "java-runtime-delta";
    fixture.write_version_json(
        "1.21.1",
        serde_json::json!({
            "id": "1.21.1",
            "type": "release",
            "mainClass": "net.minecraft.client.main.Main",
            "assetIndex": {},
            "javaVersion": { "component": component, "majorVersion": 21 },
            "libraries": []
        }),
    );
    let version_dir = fixture.paths.library_dir.join("versions").join("1.21.1");
    fs::write(version_dir.join("1.21.1.jar"), b"client jar").expect("client jar");
    let runtime_root = fixture
        .state
        .managed_runtime_cache()
        .component_root(component)
        .expect("runtime root");
    let runtime_bin = runtime_root.join("bin");
    fs::create_dir_all(&runtime_bin).expect("runtime bin");
    fs::write(runtime_bin.join("java"), b"java").expect("non executable java");
    fs::write(runtime_root.join(".axial-ready"), b"ready").expect("ready marker");
    let instance_id = fixture.add_instance("Survival", "1.21.1");

    let _prepared = prepare_launch_session(
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
    .expect("recoverable runtime should queue for ensure");

    assert_eq!(fixture.state.sessions().active_session_count().await, 1);
    assert!(
        fixture
            .state
            .sessions()
            .has_active_instance(&instance_id)
            .await
    );
    assert!(fixture.state.failure_memory().list().is_empty());
}

#[tokio::test]
async fn launch_preparation_blocks_when_managed_runtime_repair_is_suppressed() {
    let fixture = TestFixture::new("prepare-blocks-suppressed-runtime-repair");
    let component = "java-runtime-delta";
    fixture.write_version_json(
        "1.21.1",
        serde_json::json!({
            "id": "1.21.1",
            "type": "release",
            "mainClass": "net.minecraft.client.main.Main",
            "assetIndex": {},
            "javaVersion": { "component": component, "majorVersion": 21 },
            "libraries": []
        }),
    );
    let version_dir = fixture.paths.library_dir.join("versions").join("1.21.1");
    fs::write(version_dir.join("1.21.1.jar"), b"client jar").expect("client jar");
    let runtime_root = fixture.write_global_runtime_without_ready_marker(component);
    let instance_id = fixture.add_instance("Survival", "1.21.1");
    let instance = fixture
        .state
        .instances()
        .get(&instance_id)
        .expect("instance");
    let config = fixture.state.config().current();
    let game_dir = fixture.state.instances().game_dir(&instance.id);
    let integrity_foreground = fixture
        .state
        .register_integrity_foreground()
        .expect("register runtime repair foreground")
        .wait_for_settlement()
        .await;
    let preflight = build_launch_preflight_facts(
        &fixture.state,
        &fixture
            .state
            .try_claim_producer()
            .expect("claim preflight producer"),
        LaunchPreflightBuild {
            integrity_foreground: &integrity_foreground,
            instance_lifecycle: &fixture.state.acquire_instance_lifecycle(&instance.id).await,
            instance: &instance,
            config: &config,
            library_dir: &fixture.paths.library_dir,
            game_dir: &game_dir,
            requested_max_memory_mb: None,
            requested_min_memory_mb: None,
        },
        None,
    )
    .await;

    let producer = fixture
        .state
        .try_claim_producer()
        .expect("claim runtime repair producer");
    let repaired = maybe_repair_managed_runtime_before_launch_owned(
        &fixture.state,
        &producer,
        &integrity_foreground,
        preflight,
        ManagedRuntimeRepairLaunch {
            instance_lifecycle: &fixture.state.acquire_instance_lifecycle(&instance.id).await,
            instance: &instance,
            library_dir: &fixture.paths.library_dir,
            game_dir: &game_dir,
            requested_max_memory_mb: None,
            requested_min_memory_mb: None,
        },
    )
    .await
    .expect("persist managed-runtime repair journal");
    assert_eq!(
        repaired.guardian_summary.decision(),
        GuardianSummaryDecision::Intervened
    );
    fs::remove_file(runtime_root.join(".axial-ready")).expect("remove ready marker");

    let error = match prepare_launch_session(
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
    {
        Ok(_) => panic!("suppressed repair should block launch preparation"),
        Err(error) => error,
    };

    assert_eq!(error.0, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(error.1.0["guardian"]["decision"], "blocked");
    assert_eq!(
        error.1.0["readiness"]["reasons"][0]["severity"],
        "recoverable"
    );
    assert_eq!(fixture.state.sessions().active_session_count().await, 0);
    assert!(
        !fixture
            .state
            .sessions()
            .has_active_instance(&instance_id)
            .await
    );
    let memory = fixture.state.failure_memory().list();
    assert_eq!(
        memory[0].last_action_outcome,
        Some(FailureMemoryActionOutcome::Repaired)
    );
}

#[tokio::test]
async fn launch_preflight_readiness_reports_incomplete_install_marker() {
    let fixture = TestFixture::new("preflight-readiness-incomplete-install");
    fixture.write_ready_install("1.21.1");
    fs::write(
        fixture
            .paths
            .library_dir
            .join("versions")
            .join("1.21.1")
            .join(".incomplete"),
        b"installing",
    )
    .expect("incomplete marker");
    let instance_id = fixture.add_instance("Survival", "1.21.1");

    let preflight = prepare_launch_preflight(&fixture.state, instance_id)
        .await
        .expect("prepare preflight");

    assert!(!preflight.readiness.launchable);
    assert_eq!(preflight.readiness.reasons.len(), 1);
    assert_readiness_reason(&preflight, LaunchReadinessReasonId::IncompleteInstall);
    assert_eq!(
        preflight.guardian.decision(),
        GuardianSummaryDecision::Blocked
    );
    assert_guardian_fact(&preflight, "incomplete_install");
    assert!(
        preflight.guardian.details().iter().any(|detail| {
            detail == "Guardian blocked launch because the install is incomplete."
        })
    );
}

#[tokio::test]
async fn prepare_launch_session_rejects_missing_version_json_with_guardian_block() {
    let fixture = TestFixture::new("prepare-rejects-missing-version-json");
    let instance_id = fixture.add_instance("Survival", "1.21.1");

    let error = match prepare_launch_session(
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
    {
        Ok(_) => panic!("missing version metadata should not queue"),
        Err(error) => error,
    };

    assert_eq!(error.0, StatusCode::PRECONDITION_FAILED);
    assert_eq!(error.1.0["error"], "Guardian blocked launch preflight.");
    assert_eq!(error.1.0["readiness"]["launchable"], false);
    assert_eq!(
        error.1.0["readiness"]["reasons"][0]["id"],
        "version_json_missing"
    );
    assert_eq!(error.1.0["guardian"]["decision"], "blocked");
    assert!(
        error.1.0["guardian"]["details"]
            .as_array()
            .is_some_and(|details| details.iter().any(|detail| detail.as_str()
                == Some("Guardian blocked launch because installed version metadata is missing.")))
    );
    assert_eq!(fixture.state.sessions().active_session_count().await, 0);
    assert!(
        !fixture
            .state
            .sessions()
            .has_active_instance(&instance_id)
            .await
    );
    let payload = error.1.0.to_string();
    assert!(!payload.contains(&fixture.root.to_string_lossy().to_string()));
}

#[tokio::test]
async fn prepare_launch_session_rejects_incomplete_install_without_session() {
    let fixture = TestFixture::new("prepare-rejects-incomplete-install");
    fixture.write_ready_install("1.21.1");
    fs::write(
        fixture
            .paths
            .library_dir
            .join("versions")
            .join("1.21.1")
            .join(".incomplete"),
        b"installing",
    )
    .expect("incomplete marker");
    let instance_id = fixture.add_instance("Survival", "1.21.1");

    let error = match prepare_launch_session(
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
    {
        Ok(_) => panic!("incomplete install should not queue"),
        Err(error) => error,
    };

    assert_eq!(error.0, StatusCode::PRECONDITION_FAILED);
    assert_eq!(error.1.0["error"], "Guardian blocked launch preflight.");
    assert_eq!(error.1.0["readiness"]["launchable"], false);
    assert_eq!(
        error.1.0["readiness"]["reasons"][0]["id"],
        "incomplete_install"
    );
    assert_eq!(error.1.0["guardian"]["decision"], "blocked");
    assert!(
        error.1.0["guardian"]["details"]
            .as_array()
            .is_some_and(|details| details.iter().any(|detail| detail.as_str()
                == Some("Guardian blocked launch because the install is incomplete.")))
    );
    assert_eq!(
        error.1.0["notice"]["message"],
        "Guardian blocked launch preflight."
    );
    assert!(
        error.1.0["notice"]["details"]
            .as_array()
            .is_some_and(|details| details.iter().any(|detail| detail.as_str()
                == Some("Guardian blocked launch because the install is incomplete.")))
    );
    assert_eq!(fixture.state.sessions().active_session_count().await, 0);
    assert!(
        !fixture
            .state
            .sessions()
            .has_active_instance(&instance_id)
            .await
    );
    let payload = error.1.0.to_string();
    assert!(!payload.contains(&fixture.root.to_string_lossy().to_string()));
    assert!(!payload.contains(".incomplete"));
}

#[tokio::test]
async fn prepare_launch_session_rejects_incomplete_parent_without_session() {
    let fixture = TestFixture::new("prepare-rejects-incomplete-parent");
    fixture.write_ready_install("1.21.1");
    fixture.write_child_version("fabric-loader-0.16.10-1.21.1", "1.21.1");
    fs::write(
        fixture
            .paths
            .library_dir
            .join("versions")
            .join("1.21.1")
            .join(".incomplete"),
        b"installing",
    )
    .expect("incomplete marker");
    let instance_id = fixture.add_instance("Modded", "fabric-loader-0.16.10-1.21.1");

    let error = match prepare_launch_session(
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
    {
        Ok(_) => panic!("incomplete parent install should not queue"),
        Err(error) => error,
    };

    assert_eq!(error.0, StatusCode::PRECONDITION_FAILED);
    assert_eq!(
        error.1.0["readiness"]["reasons"][0]["id"],
        "incomplete_install"
    );
    assert_eq!(error.1.0["guardian"]["decision"], "blocked");
    assert!(
        error.1.0["guardian"]["details"]
            .as_array()
            .is_some_and(|details| details.iter().any(|detail| detail.as_str()
                == Some("Guardian blocked launch because the install is incomplete.")))
    );
    assert_eq!(fixture.state.sessions().active_session_count().await, 0);
    assert!(
        !fixture
            .state
            .sessions()
            .has_active_instance(&instance_id)
            .await
    );
}

fn startup_failure_memory_entry(
    instance_id: &str,
    mode: ApiGuardianMode,
    failure_class: LaunchFailureClass,
    observed_at: &str,
) -> GuardianFailureMemoryEntry {
    let diagnosis_id = serde_json::from_value::<DiagnosisId>(serde_json::Value::String(
        failure_class.as_str().to_string(),
    ))
    .expect("launch failure diagnosis");
    GuardianFailureMemoryEntry::observed(
        diagnosis_id,
        GuardianDomain::Startup,
        instance_target(instance_id, OwnershipClass::UserOwned),
        mode,
        None,
        observed_at,
    )
}

fn instance_target(instance_id: &str, ownership: OwnershipClass) -> TargetDescriptor {
    TargetDescriptor::new(
        StabilizationSystem::Guardian,
        TargetKind::Instance,
        instance_id,
        ownership,
    )
}

fn relative_timestamp(offset: Duration) -> String {
    (Utc::now() + offset).to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn fact_field<'a>(fact: &'a GuardianFact, key: &str) -> Option<&'a str> {
    fact.fields
        .iter()
        .find(|field| field.key == key)
        .map(|field| field.value.as_str())
}
