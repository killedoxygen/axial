use super::*;
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
    assert_eq!(preflight.guardian.mode, GuardianMode::Managed);
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
async fn launch_preflight_readiness_reports_missing_version_json() {
    let fixture = TestFixture::new("preflight-readiness-missing-version-json");
    let instance_id = fixture.add_instance("Survival", "1.21.1");

    let preflight = prepare_launch_preflight(&fixture.state, instance_id)
        .await
        .expect("prepare preflight");

    assert!(!preflight.readiness.launchable);
    assert_eq!(preflight.readiness.reasons.len(), 1);
    assert_readiness_reason(&preflight, LaunchReadinessReasonId::VersionJsonMissing);
    assert_eq!(preflight.guardian.decision, GuardianDecision::Blocked);
    assert_guardian_fact(&preflight, "version_json_missing");
    assert!(preflight.guardian.details.iter().any(|detail| {
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

    let preflight = build_launch_preflight_facts(
        &fixture.state,
        &producer,
        LaunchPreflightBuild {
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
    let component = "axial-test-runtime-missing-client";
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
    let instance_id = fixture.add_instance("Survival", "1.21.1");

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
    assert_eq!(preflight.guardian.decision, GuardianDecision::Blocked);
    assert_guardian_fact(&preflight, "client_jar_missing");
    assert_guardian_fact(&preflight, "managed_runtime_missing");
    assert!(preflight.guardian.details.iter().any(|detail| {
        detail == "Guardian blocked launch because client game files are missing."
    }));
}

#[tokio::test]
async fn launch_preflight_readiness_reports_missing_library_metadata_as_corrupt_guardian_fact() {
    let fixture = TestFixture::new("preflight-readiness-missing-libraries");
    fixture.write_version_json(
        "1.21.1",
        serde_json::json!({
            "id": "1.21.1",
            "type": "release",
            "mainClass": "net.minecraft.client.main.Main",
            "assetIndex": {},
            "javaVersion": { "component": "java-runtime-delta", "majorVersion": 21 },
            "libraries": [{
                "name": "com.example:demo:1.0.0",
                "downloads": {
                    "artifact": {
                        "path": "com/example/demo/1.0.0/demo-1.0.0.jar",
                        "url": "https://example.invalid/demo-1.0.0.jar"
                    }
                }
            }]
        }),
    );
    let version_dir = fixture.paths.library_dir.join("versions").join("1.21.1");
    fs::write(version_dir.join("1.21.1.jar"), b"client jar").expect("write client jar");
    fixture.write_ready_runtime("java-runtime-delta");
    let instance_id = fixture.add_instance("Survival", "1.21.1");

    let preflight = prepare_launch_preflight(&fixture.state, instance_id)
        .await
        .expect("prepare preflight");

    assert!(!preflight.readiness.launchable);
    assert_readiness_reason(&preflight, LaunchReadinessReasonId::LibrariesCorrupt);
    assert_eq!(preflight.guardian.decision, GuardianDecision::Blocked);
    assert!(preflight.guardian_facts.iter().any(|fact| {
        fact.id.as_str() == "artifact_checksum_mismatch"
            && fact
                .target
                .as_ref()
                .is_some_and(|target| target.id == "libraries")
    }));
    assert!(preflight.guardian.details.iter().any(|detail| {
        detail == "Guardian blocked launch because launcher-managed game files are corrupt."
    }));
}

#[tokio::test]
async fn launch_preflight_readiness_reports_missing_asset_index_as_guardian_fact() {
    let fixture = TestFixture::new("preflight-readiness-missing-asset-index");
    fixture.write_version_json(
        "1.21.1",
        serde_json::json!({
            "id": "1.21.1",
            "type": "release",
            "mainClass": "net.minecraft.client.main.Main",
            "assetIndex": { "id": "test-assets" },
            "javaVersion": { "component": "java-runtime-delta", "majorVersion": 21 },
            "libraries": []
        }),
    );
    let version_dir = fixture.paths.library_dir.join("versions").join("1.21.1");
    fs::write(version_dir.join("1.21.1.jar"), b"client jar").expect("write client jar");
    fixture.write_ready_runtime("java-runtime-delta");
    let instance_id = fixture.add_instance("Survival", "1.21.1");

    let preflight = prepare_launch_preflight(&fixture.state, instance_id)
        .await
        .expect("prepare preflight");

    assert!(!preflight.readiness.launchable);
    assert_readiness_reason(&preflight, LaunchReadinessReasonId::AssetIndexMissing);
    assert_eq!(preflight.guardian.decision, GuardianDecision::Blocked);
    assert_guardian_fact(&preflight, "asset_index_missing");
    assert!(
        preflight.guardian.details.iter().any(|detail| {
            detail == "Guardian blocked launch because the asset index is missing."
        })
    );
}

#[tokio::test]
async fn launch_preflight_readiness_reports_corrupt_launcher_artifacts_as_guardian_fact() {
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

    let preflight = prepare_launch_preflight(&fixture.state, instance_id)
        .await
        .expect("prepare preflight");

    assert!(!preflight.readiness.launchable);
    assert_readiness_reason(&preflight, LaunchReadinessReasonId::ClientJarCorrupt);
    assert_readiness_reason(&preflight, LaunchReadinessReasonId::LibrariesCorrupt);
    assert_readiness_reason(&preflight, LaunchReadinessReasonId::AssetIndexCorrupt);
    assert_eq!(preflight.guardian.decision, GuardianDecision::Blocked);
    for target_id in ["client_jar", "libraries", "asset_index"] {
        assert!(preflight.guardian_facts.iter().any(|fact| {
            fact.id.as_str() == "artifact_checksum_mismatch"
                && fact
                    .target
                    .as_ref()
                    .is_some_and(|target| target.id == target_id)
        }));
    }
    assert!(preflight.guardian.details.iter().any(|detail| {
        detail == "Guardian blocked launch because launcher-managed game files are corrupt."
    }));
    assert!(preflight.guardian.guidance.iter().any(|detail| {
        detail == "Install or repair the affected version before launching again."
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
            "javaVersion": { "component": "axial-test-runtime-missing", "majorVersion": 21 },
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
        preflight.guardian.decision,
        GuardianDecision::Allowed | GuardianDecision::Warned
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
    let component = "axial-test-runtime-repair-marker";
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

    let preflight = build_launch_preflight_facts(
        &fixture.state,
        &producer,
        LaunchPreflightBuild {
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
        preflight,
        ManagedRuntimeRepairLaunch {
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
        repaired.guardian_summary.decision,
        GuardianDecision::Intervened
    );
    assert!(
        repaired.guardian_summary.details.iter().any(|detail| {
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
    let component = "axial-test-runtime-corrupt-marker";
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

    let preflight = build_launch_preflight_facts(
        &fixture.state,
        &fixture
            .state
            .try_claim_producer()
            .expect("claim preflight producer"),
        LaunchPreflightBuild {
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
        preflight,
        ManagedRuntimeRepairLaunch {
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
        repaired.guardian_summary.decision,
        GuardianDecision::Intervened
    );
    assert!(
        repaired.guardian_summary.details.iter().any(|detail| {
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
async fn prepare_launch_session_blocks_present_managed_runtime_missing_java_without_session() {
    let fixture = TestFixture::new("prepare-blocks-runtime-missing-java");
    let component = "axial-test-runtime-missing-java";
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
    let runtime_root = fixture.paths.config_dir.join("runtimes").join(component);
    fs::create_dir_all(&runtime_root).expect("runtime root");
    fs::write(runtime_root.join(".axial-ready"), b"ready").expect("ready marker");
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
        Ok(_) => panic!("corrupt managed runtime should not queue"),
        Err(error) => error,
    };

    assert_eq!(error.0, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(error.1.0["guardian"]["decision"], "blocked");
    assert_eq!(fixture.state.sessions().active_session_count().await, 0);
    assert!(
        !fixture
            .state
            .sessions()
            .has_active_instance(&instance_id)
            .await
    );
    let memory = fixture.state.failure_memory().list();
    assert_eq!(memory.len(), 1);
    assert_eq!(
        memory[0].last_action_outcome,
        Some(FailureMemoryActionOutcome::Failed)
    );
    assert_eq!(memory[0].repair_attempt_count, 1);
    let journal = fixture
        .state
        .journals()
        .latest_for_command(CommandKind::RepairInstance)
        .expect("repair journal");
    assert_eq!(journal.status, OperationStatus::Failed);
    let payload = error.1.0.to_string();
    assert!(!payload.contains(&fixture.root.to_string_lossy().to_string()));
}

#[cfg(unix)]
#[tokio::test]
async fn prepare_launch_session_blocks_present_managed_runtime_non_executable_java_without_session()
{
    let fixture = TestFixture::new("prepare-blocks-runtime-non-executable-java");
    let component = "axial-test-runtime-non-executable-java";
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
    let runtime_root = fixture.paths.config_dir.join("runtimes").join(component);
    let runtime_bin = runtime_root.join("bin");
    fs::create_dir_all(&runtime_bin).expect("runtime bin");
    fs::write(runtime_bin.join("java"), b"java").expect("non executable java");
    fs::write(runtime_root.join(".axial-ready"), b"ready").expect("ready marker");
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
        Ok(_) => panic!("non-executable managed runtime should not queue"),
        Err(error) => error,
    };

    assert_eq!(error.0, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(error.1.0["guardian"]["decision"], "blocked");
    assert_eq!(fixture.state.sessions().active_session_count().await, 0);
    assert!(
        !fixture
            .state
            .sessions()
            .has_active_instance(&instance_id)
            .await
    );
    let memory = fixture.state.failure_memory().list();
    assert_eq!(memory.len(), 1);
    assert_eq!(
        memory[0].last_action_outcome,
        Some(FailureMemoryActionOutcome::Failed)
    );
    let journal = fixture
        .state
        .journals()
        .latest_for_command(CommandKind::RepairInstance)
        .expect("repair journal");
    assert_eq!(journal.status, OperationStatus::Failed);
    let payload = error.1.0.to_string();
    assert!(!payload.contains(&fixture.root.to_string_lossy().to_string()));
}

#[tokio::test]
async fn launch_preparation_blocks_when_managed_runtime_repair_is_suppressed() {
    let fixture = TestFixture::new("prepare-blocks-suppressed-runtime-repair");
    let component = "axial-test-runtime-suppressed";
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
    let preflight = build_launch_preflight_facts(
        &fixture.state,
        &fixture
            .state
            .try_claim_producer()
            .expect("claim preflight producer"),
        LaunchPreflightBuild {
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
        preflight,
        ManagedRuntimeRepairLaunch {
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
        repaired.guardian_summary.decision,
        GuardianDecision::Intervened
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
        Some(FailureMemoryActionOutcome::Suppressed)
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
    assert_eq!(preflight.guardian.decision, GuardianDecision::Blocked);
    assert_guardian_fact(&preflight, "incomplete_install");
    assert!(
        preflight.guardian.details.iter().any(|detail| {
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
