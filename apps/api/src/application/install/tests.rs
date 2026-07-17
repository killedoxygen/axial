use super::*;
use crate::application::InstallVersionCommand;
use crate::execution::file::{FileWriteRequest, write_file_atomically};
use crate::execution::persistence::{AtomicWriteBackend, PersistenceCoordinator};
use crate::guardian::{DiagnosisId, GuardianInstallArtifactFailureKind};
use crate::state::contracts::{
    CommandKind, JournalId, OperationId, OperationJournalStep, OperationOutcome, OperationPhase,
    OperationStatus, OperationStepResult, OwnershipClass, RollbackState, StabilizationSystem,
    TargetDescriptor, TargetKind,
};
use crate::state::{
    AppState, AppStateInit, GuardianFailureMemoryStore, InstallStore, OperationJournalStore,
    SessionStore,
};
use axial_config::{AppPaths, ConfigStore, InstanceRegistrySnapshot, InstanceStore};
use axial_minecraft::download::{ExecutionDownloadFact, ExecutionDownloadFactKind};
use axial_minecraft::{
    DownloadError, DownloadProgress, LoaderComponentId, LoaderError, LoaderInstallError,
    LoaderProviderFailureKind, RuntimeId, RuntimeSourceFailure, RuntimeSourceFailureKind,
    build_id_for,
};
use axial_performance::PerformanceManager;
use axum::{body::to_bytes, response::IntoResponse};
use serde_json::json;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;
use tokio::sync::{Notify, mpsc as tokio_mpsc};
use tokio::time::timeout;

#[test]
fn install_staging_builds_command_operation_and_payload() {
    let operation_id = install_operation_id("install-1");
    let staging = stage_install_version_command(
        InstallVersionCommand {
            version_id: "1.21.5".to_string(),
        },
        "install-1".to_string(),
        operation_id.clone(),
    );

    assert_eq!(staging.command.kind, CommandKind::InstallVersion);
    assert_eq!(
        staging.command.target.as_ref().map(|target| target.kind),
        Some(TargetKind::Version)
    );
    assert_eq!(staging.result.operation_id, Some(operation_id.clone()));
    assert_eq!(
        staging
            .result
            .carriers
            .operation
            .as_ref()
            .and_then(|operation| operation.operation_id.as_ref()),
        Some(&operation_id)
    );
    assert_eq!(
        staging.result.payload.install_id.as_deref(),
        Some("install-1")
    );
}

#[test]
fn known_good_acceptance_failure_replaces_terminal_success_with_bounded_failure() {
    let error = known_good_acceptance_download_error(io::Error::other(
        "/private/library/state/known-good write failed",
    ));
    let progress = install_progress_with_terminal_error(
        terminal_failure_progress_or_default(Some(done_progress())),
        &error,
    );
    let progress = sanitize_install_progress(progress);

    assert!(progress.done);
    assert_eq!(progress.error.as_deref(), Some(INSTALL_FAILURE_MESSAGE));
    assert!(
        !serde_json::to_string(&progress)
            .expect("progress json")
            .contains("/private/library")
    );
}

#[test]
fn content_queue_request_is_strict_and_cannot_claim_setup_cleanup() {
    let request = serde_json::from_value::<InstallQueueRequest>(serde_json::json!({
        "kind": "content",
        "instance_id": "0000000000000001",
        "label": "Adding content",
        "action": {
            "kind": "install",
            "selections": [{
                "canonical_id": "modrinth:sodium",
                "kind": "mod",
                "version_id": "version-2"
            }],
            "allow_incompatible": false,
            "remove_instance_on_failure": true
        }
    }));

    assert!(request.is_err());
}

#[test]
fn content_queue_view_model_retains_semantic_intent_without_urls() {
    let spec = InstallQueueSpec::Content {
        instance_id: "0000000000000001".to_string(),
        label: "Updating Sodium".to_string(),
        prerequisite_queue_id: None,
        action: ContentQueueAction::Install {
            selections: vec![QueuedContentSelection {
                canonical_id: "modrinth:sodium".to_string(),
                kind: axial_content::ContentKind::Mod,
                version_id: Some("version-2".to_string()),
            }],
            allow_incompatible: false,
            setup_cleanup: None,
        },
    };

    let item = install_queue_install_item(&spec);
    let content = item.content.expect("content queue item");
    let encoded = serde_json::to_string(&content).expect("serialize content item");

    assert_eq!(content.instance_id, "0000000000000001");
    assert!(encoded.contains("modrinth:sodium"));
    assert!(!encoded.contains("https://"));
    assert!(!encoded.contains("remove_instance_on_failure"));
}

#[test]
fn modpack_queue_view_model_exposes_only_opaque_file_ids() {
    let selection_id = format!("mpf1-{}", "a".repeat(64));
    let spec = InstallQueueSpec::Content {
        instance_id: "0000000000000001".to_string(),
        label: "Adding selected pack files".to_string(),
        prerequisite_queue_id: None,
        action: ContentQueueAction::Modpack {
            canonical_id: "modrinth:pack".to_string(),
            version_id: "pack-version".to_string(),
            selected_file_ids: vec![selection_id.clone()],
            include_overrides: false,
            setup_cleanup: None,
        },
    };

    let item = install_queue_install_item(&spec);
    let content = item.content.expect("content queue item");
    let encoded = serde_json::to_string(&content).expect("serialize content item");

    assert!(encoded.contains(&selection_id));
    assert!(encoded.contains("selected_file_ids"));
    assert!(!encoded.contains("selected_paths"));
    assert!(!encoded.contains("mods/"));
}

#[test]
fn old_modpack_selected_paths_queue_field_is_rejected() {
    let request = serde_json::from_value::<InstallQueueRequest>(serde_json::json!({
        "kind": "content",
        "instance_id": "0000000000000001",
        "label": "Adding selected pack files",
        "action": {
            "kind": "modpack",
            "canonical_id": "modrinth:pack",
            "version_id": "pack-version",
            "selected_paths": ["mods/provider-authored.jar"],
            "include_overrides": false
        }
    }));

    assert!(request.is_err());
}

#[test]
fn retry_is_disabled_after_setup_cleanup_removes_the_instance() {
    let progress = InstallProgressViewModel {
        phase_id: CONTENT_INSTANCE_REMOVED_PHASE.to_string(),
        label: "Setup failed and the incomplete instance was removed".to_string(),
        progress_pct: 100,
        terminal: true,
        failed: true,
        active_step: None,
    };

    let failure = install_failure_view_model(&progress, None).expect("failure view model");

    assert_eq!(failure.state_id, "failed_instance_removed");
    assert!(!failure.retry_action.enabled);
}

#[test]
fn effective_install_version_id_trims_version_id() {
    let payload = InstallVersionStartRequest {
        version_id: " 1.21.5 ".to_string(),
    };

    assert_eq!(effective_install_version_id(&payload), "1.21.5");
}

#[test]
fn sanitize_install_progress_preserves_safe_non_error_progress() {
    let progress = DownloadProgress {
        phase: "libraries".to_string(),
        current: 7,
        total: 42,
        file: Some("1.20.1.json".to_string()),
        error: None,
        done: false,
        bytes_done: None,
        bytes_total: None,
    };

    assert_eq!(sanitize_install_progress(progress.clone()), progress);
}

#[test]
fn sanitize_install_progress_hides_raw_terminal_error_fragments() {
    let progress = DownloadProgress {
        phase: "error".to_string(),
        current: 0,
        total: 0,
        file: None,
        error: Some(
            "request failed: GET https://piston-meta.mojang.com/mc/game/version_manifest_v2.json \
                 parse version json: expected value at line 1 column 1 \
                 prepare java runtime: failed in /home/zero/.axial/runtime/java \
                 and C:\\Users\\zero\\AppData\\Roaming\\Axial\\runtime\\java"
                .to_string(),
        ),
        done: true,
        bytes_done: None,
        bytes_total: None,
    };

    let sanitized = sanitize_install_progress(progress);
    let message = sanitized.error.as_deref().expect("error is present");

    assert_eq!(message, INSTALL_FAILURE_MESSAGE);
    assert_no_public_raw_fragments(message);
}

#[test]
fn sanitize_install_progress_preserves_runtime_unavailable_terminal_message() {
    let error = DownloadError::RuntimeUnavailableForPlatform {
        component: "jre-legacy".to_string(),
        platform: "mac-os-arm64".to_string(),
    };
    let progress = install_progress_with_terminal_error(
        progress("error", true, Some(&error.to_string())),
        &error,
    );

    let sanitized = sanitize_install_progress(progress);
    let message = sanitized.error.as_deref().expect("error is present");

    assert_ne!(message, INSTALL_FAILURE_MESSAGE);
    assert!(message.contains("Java runtime"));
    assert!(message.contains("not available for this device"));
    assert!(message.contains("jre-legacy"));
    assert!(message.contains("mac-os-arm64"));
    assert_no_public_raw_fragments(message);
}

#[test]
fn sanitize_install_progress_preserves_rosetta_required_terminal_message() {
    let error = DownloadError::RuntimeRosettaRequired {
        component: "jre-legacy".to_string(),
    };
    let progress = install_progress_with_terminal_error(
        progress("error", true, Some(&error.to_string())),
        &error,
    );

    let sanitized = sanitize_install_progress(progress);
    let message = sanitized.error.as_deref().expect("error is present");

    assert_ne!(message, INSTALL_FAILURE_MESSAGE);
    assert!(message.contains("Rosetta 2"));
    assert!(message.contains("jre-legacy"));
    assert!(message.contains("softwareupdate --install-rosetta --agree-to-license"));
    assert_no_public_raw_fragments(message);
}

#[test]
fn sanitize_install_progress_preserves_shape_and_only_changes_error_text() {
    let progress = DownloadProgress {
        phase: "error".to_string(),
        current: 13,
        total: 21,
        file: Some("1.20.1.json".to_string()),
        error: Some(
            "request failed for https://example.invalid/manifest.json in /tmp/axial".to_string(),
        ),
        done: true,
        bytes_done: None,
        bytes_total: None,
    };

    let sanitized = sanitize_install_progress(progress.clone());

    assert_eq!(sanitized.phase, progress.phase);
    assert_eq!(sanitized.current, progress.current);
    assert_eq!(sanitized.total, progress.total);
    assert_eq!(sanitized.file, progress.file);
    assert_eq!(sanitized.done, progress.done);
    assert_eq!(sanitized.error.as_deref(), Some(INSTALL_FAILURE_MESSAGE));
}

#[test]
fn sanitize_install_progress_redacts_raw_non_terminal_progress() {
    let progress = DownloadProgress {
            phase: r"C:\Users\Alice\.minecraft --accessToken raw-secret".to_string(),
            current: 7,
            total: 42,
            file: Some("/Users/alice/.axial/libraries/secret.jar".to_string()),
            error: Some(
                "provider_payload={\"token\":\"secret\"} account_id=account-secret username=SecretPlayer"
                    .to_string(),
            ),
            done: false,
                    bytes_done: None,
            bytes_total: None,
};

    let sanitized = sanitize_install_progress(progress);

    assert_eq!(sanitized.phase, "install");
    assert_eq!(sanitized.file, None);
    assert_eq!(sanitized.error.as_deref(), Some(INSTALL_FAILURE_MESSAGE));
}

#[test]
fn observed_install_failure_progress_is_sanitized_terminal_error() {
    let progress = observed_install_failure_progress();

    assert_eq!(progress.phase, "error");
    assert_eq!(progress.current, 0);
    assert_eq!(progress.total, 0);
    assert_eq!(progress.file, None);
    assert_eq!(progress.error.as_deref(), Some(INSTALL_FAILURE_MESSAGE));
    assert!(progress.done);
}

#[test]
fn install_progress_view_model_authors_vanilla_progress_copy() {
    let progress = DownloadProgress {
        phase: "assets".to_string(),
        current: 1,
        total: 2,
        file: None,
        error: None,
        done: false,
        bytes_done: None,
        bytes_total: None,
    };

    let view_model = vanilla_install_progress_view_model(&progress);

    assert_eq!(view_model.phase_id, "assets");
    assert_eq!(view_model.label, "Assets (1/2)");
    assert_eq!(view_model.progress_pct, 57);
    assert!(!view_model.terminal);
    assert!(!view_model.failed);
}

#[test]
fn install_progress_pct_prefers_transfer_plan_bytes() {
    let mut progress = base_progress("assets");
    progress.bytes_done = Some(50);
    progress.bytes_total = Some(100);

    let vanilla = vanilla_install_progress_view_model(&progress);
    let loader = loader_install_progress_view_model(&progress);

    assert_eq!(vanilla.progress_pct, 50);
    assert_eq!(loader.progress_pct, 60);
}

#[test]
fn install_progress_pct_caps_byte_weighted_progress_below_done() {
    let mut progress = base_progress("java_runtime");
    progress.bytes_done = Some(100);
    progress.bytes_total = Some(100);

    let view_model = vanilla_install_progress_view_model(&progress);

    assert_eq!(view_model.progress_pct, 99);
}

#[test]
fn install_progress_pct_ignores_bytes_on_terminal_events() {
    let mut progress = done_progress();
    progress.bytes_done = Some(10);
    progress.bytes_total = Some(100);

    let view_model = vanilla_install_progress_view_model(&progress);

    assert_eq!(view_model.progress_pct, 100);
    assert!(view_model.terminal);
}

#[test]
fn install_progress_coalescer_compacts_high_volume_events_and_keeps_terminal() {
    let mut coalescer = InstallProgressCoalescer::default();
    let mut emitted = Vec::new();

    for current in 1..=100 {
        let mut progress = base_progress("java_runtime");
        progress.current = current;
        progress.total = 100;
        emitted.extend(coalescer.push(progress));
    }
    emitted.extend(coalescer.push(done_progress()));

    assert!(emitted.len() < 100);
    assert_eq!(emitted.first().map(|progress| progress.current), Some(1));
    assert!(emitted.iter().any(|progress| progress.current == 100));
    assert_eq!(
        emitted.last().map(|progress| progress.phase.as_str()),
        Some("done")
    );
    assert!(emitted.last().is_some_and(|progress| progress.done));
}

#[test]
fn install_progress_coalescer_emits_byte_total_changes() {
    let mut coalescer = InstallProgressCoalescer::default();
    let mut first = base_progress("libraries");
    first.bytes_done = Some(10);
    first.bytes_total = Some(100);
    let mut second = base_progress("libraries");
    second.current = 2;
    second.bytes_done = Some(10);
    second.bytes_total = Some(200);

    let first_emitted = coalescer.push(first);
    let second_emitted = coalescer.push(second);

    assert_eq!(first_emitted.len(), 1);
    assert_eq!(second_emitted.len(), 1);
    assert_eq!(second_emitted[0].bytes_total, Some(200));
}

#[test]
fn install_progress_coalescer_preserves_runtime_ready_transition() {
    let mut coalescer = InstallProgressCoalescer::default();
    let mut emitted = Vec::new();
    emitted.extend(coalescer.push(base_progress("java_runtime")));

    let mut pending = base_progress("java_runtime");
    pending.current = 2;
    emitted.extend(coalescer.push(pending));
    emitted.extend(coalescer.push(base_progress("java_runtime_ready")));

    assert_eq!(
        emitted
            .iter()
            .map(|progress| progress.phase.as_str())
            .collect::<Vec<_>>(),
        vec!["java_runtime", "java_runtime", "java_runtime_ready"]
    );
}

#[test]
fn install_progress_pct_falls_back_to_phase_table_without_bytes() {
    let mut progress = base_progress("java_runtime");
    progress.current = 1;
    progress.total = 1;
    progress.bytes_done = Some(0);
    progress.bytes_total = Some(0);

    let view_model = vanilla_install_progress_view_model(&progress);

    assert_eq!(view_model.progress_pct, 0);
}

#[test]
fn install_progress_view_model_authors_runtime_copy_from_typed_counts() {
    let mut progress = base_progress("java_runtime");
    progress.current = 2;
    progress.total = 5;
    progress.file = Some("jre.bundle/Contents/Home/bin/java".to_string());

    let view_model = vanilla_install_progress_view_model(&progress);

    assert_eq!(view_model.label, "Java runtime files (2/5)");
    assert_eq!(
        view_model.active_step.expect("runtime active step").label,
        "Java runtime files (2/5)"
    );
}

#[test]
fn install_progress_view_model_authors_runtime_ready_copy_from_phase() {
    let progress = base_progress("java_runtime_ready");

    let view_model = vanilla_install_progress_view_model(&progress);

    assert_eq!(view_model.phase_id, "java_runtime_ready");
    assert_eq!(view_model.label, "Java runtime ready");
}

#[test]
fn install_progress_view_model_authors_loader_active_step() {
    let progress = DownloadProgress {
        phase: "loader_processors".to_string(),
        current: 1,
        total: 4,
        file: None,
        error: None,
        done: false,
        bytes_done: None,
        bytes_total: None,
    };

    let view_model = loader_install_progress_view_model(&progress);
    let active_step = view_model.active_step.expect("active step");

    assert_eq!(view_model.phase_id, "loader_processors");
    assert_eq!(view_model.label, "Running processors (1/4)");
    assert_eq!(view_model.progress_pct, 13);
    assert_eq!(active_step.phase_id, "loader_processors");
    assert_eq!(active_step.label, "Running processors (1/4)");
    assert_eq!(active_step.progress_pct, 25);
}

#[test]
fn public_install_progress_json_includes_backend_view_model() {
    let payload = public_vanilla_install_progress_json(&DownloadProgress {
        phase: "libraries".to_string(),
        current: 1,
        total: 4,
        file: None,
        error: None,
        done: false,
        bytes_done: None,
        bytes_total: None,
    });

    assert_eq!(payload["phase"], "libraries");
    assert_eq!(payload["view_model"]["phase_id"], "libraries");
    assert_eq!(payload["view_model"]["label"], "Libraries (1/4)");
    assert_eq!(payload["view_model"]["progress_pct"], 10);
}

#[tokio::test]
async fn install_events_keep_terminal_installs_subscribable_after_stream_ends() {
    let root = temp_root("install-events-terminal-retention");
    let state = build_test_state(&root);
    state.installs().insert("done-install".to_string()).await;
    state.installs().emit("done-install", done_progress()).await;

    let response = install_events_stream(&state, "done-install")
        .await
        .expect("terminal install events should be served")
        .into_response();
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("sse body should complete");
    let body = String::from_utf8(body.to_vec()).expect("sse body is utf8");

    assert!(body.contains("event: progress"));
    assert!(body.contains("\"phase\":\"done\""));
    let (snapshot, _) = state
        .installs()
        .subscribe_records("done-install")
        .await
        .expect("terminal install remains subscribable after stream completion");
    assert!(snapshot.done);
    assert_eq!(
        snapshot
            .latest
            .as_ref()
            .map(|record| record.progress.phase.as_str()),
        Some("done")
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn install_events_replay_latest_snapshot_not_prior_progress_log() {
    let root = temp_root("install-events-compact-replay");
    let state = build_test_state(&root);
    state.installs().insert("active-install".to_string()).await;
    state
        .installs()
        .emit("active-install", base_progress("client_jar"))
        .await;
    state
        .installs()
        .emit("active-install", base_progress("libraries"))
        .await;

    let response = install_events_stream(&state, "active-install")
        .await
        .expect("active install events should be served")
        .into_response();
    let body_task = tokio::spawn(async move {
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("sse body should complete");
        String::from_utf8(body.to_vec()).expect("sse body is utf8")
    });

    tokio::time::sleep(Duration::from_millis(20)).await;
    state
        .installs()
        .emit("active-install", done_progress())
        .await;
    let body = timeout(Duration::from_secs(1), body_task)
        .await
        .expect("stream should finish after terminal progress")
        .expect("body task should not panic");

    assert!(body.contains("\"phase\":\"libraries\""));
    assert!(body.contains("\"phase\":\"done\""));
    assert!(!body.contains("\"phase\":\"client_jar\""));

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn install_events_return_bounded_not_found_for_unknown_install() {
    let root = temp_root("install-events-unknown");
    let state = build_test_state(&root);

    let error = install_events_stream(&state, "missing-install")
        .await
        .expect_err("missing install should be 404");

    assert_eq!(error.0, StatusCode::NOT_FOUND);
    assert_eq!(error.1.0["error"], "install session not found");

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn install_existing_active_response_includes_backend_operation_id() {
    let root = temp_root("install-existing-active-operation");
    let state = build_test_state(&root);
    configure_library_dir(&state, &root.join("library"));
    state
        .installs()
        .insert_or_existing_vanilla("existing-install".to_string(), "1.21.5".to_string())
        .await;
    let operation_id = install_operation_id("existing-install");
    begin_install_operation_journal(state.journals(), &operation_id, "1.21.5")
        .await
        .expect("create existing install journal");
    assert!(state.installs().mark_initialized("existing-install").await);

    let producer = state.try_claim_producer().expect("claim install producer");
    let response = start_install_version_with_foreground(
        &state,
        InstallVersionStartRequest {
            version_id: "1.21.5".to_string(),
        },
        &producer,
        None,
    )
    .await
    .expect("existing active install should be returned");

    assert_eq!(response.install_id, "existing-install");
    assert_eq!(response.operation_id, operation_id);
    assert!(state.journals().get(&operation_id).is_some());

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn vanilla_start_registers_before_waiting_on_the_install_store() {
    let root = temp_root("vanilla-install-foreground-order");
    let state = build_test_state(&root);
    configure_library_dir(&state, &root.join("library"));
    state
        .installs()
        .insert_or_existing_vanilla("existing-install".to_string(), "1.21.5".to_string())
        .await;
    assert!(state.installs().mark_initialized("existing-install").await);

    let epoch = state.subscribe_integrity_idle().borrow().epoch();
    let reservation = state
        .try_reserve_idle_sweep(
            epoch,
            state.try_claim_producer().expect("claim sweep producer"),
        )
        .expect("reserve sweep");
    let cancellation = reservation.cancellation();
    let start = tokio::spawn({
        let state = state.clone();
        async move {
            let producer = state.try_claim_producer().expect("claim install producer");
            start_install_version_with_foreground(
                &state,
                InstallVersionStartRequest {
                    version_id: "1.21.5".to_string(),
                },
                &producer,
                None,
            )
            .await
        }
    });

    timeout(Duration::from_secs(1), async {
        while !cancellation.is_cancelled() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("foreground registration cancels sweep");
    assert!(!start.is_finished());
    assert_eq!(state.installs().active_install_count().await, 1);

    drop(reservation);
    let response = timeout(Duration::from_secs(1), start)
        .await
        .expect("start settles")
        .expect("start owner")
        .expect("existing install response");
    assert_eq!(response.install_id, "existing-install");
    wait_for_integrity_idle(&state).await;
    state.installs().remove("existing-install").await;
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn queued_install_dispatch_uses_inherited_foreground_after_fresh_admission_closes() {
    let root = temp_root("queued-install-inherited-foreground");
    let state = build_test_state(&root);
    configure_library_dir(&state, &root.join("library"));
    state
        .installs()
        .insert_or_existing_vanilla("existing-install".to_string(), "1.21.5".to_string())
        .await;
    assert!(state.installs().mark_initialized("existing-install").await);
    let foreground = state
        .register_integrity_foreground()
        .expect("register inherited foreground")
        .wait_for_settlement()
        .await;
    let producer = state.try_claim_producer().expect("claim queue producer");
    let shutdown_state = state.clone();
    let quiesce = tokio::spawn(async move { shutdown_state.quiesce().await });
    timeout(Duration::from_secs(1), async {
        loop {
            if state.register_integrity_foreground().is_err() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("integrity admission closes after request drain");

    let inherited = start_queued_install(
        &state,
        &crate::state::InstallQueueSpec::vanilla("1.21.5".to_string()),
        &producer,
        Some(foreground.retained()),
    )
    .await
    .expect("settled inherited foreground remains valid");
    assert_eq!(inherited.install_id, "existing-install");

    let fresh = start_queued_install(
        &state,
        &crate::state::InstallQueueSpec::vanilla("1.21.5".to_string()),
        &producer,
        None,
    )
    .await
    .expect_err("closed integrity admission rejects fresh registration");
    assert_eq!(fresh.0, StatusCode::SERVICE_UNAVAILABLE);

    drop(foreground);
    drop(producer);
    timeout(Duration::from_secs(1), quiesce)
        .await
        .expect("queue producer drains")
        .expect("quiesce task")
        .expect("quiesce succeeds");
    state.installs().remove("existing-install").await;
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn install_foreground_activity_releases_and_reacquires_without_overlap() {
    let root = temp_root("install-foreground-reacquire");
    let state = build_test_state(&root);
    let foreground = register_install_foreground(&state)
        .expect("register install foreground")
        .wait_for_settlement()
        .await;
    let activity = InstallForegroundActivity::new_with_update_admission(
        foreground,
        state
            .try_admit_update_sensitive_operation()
            .expect("admit install update-sensitive operation"),
    );
    assert!(!state.subscribe_integrity_idle().borrow().is_stably_idle());

    activity.release();
    assert!(state.subscribe_integrity_idle().borrow().is_stably_idle());
    assert!(retain_install_foreground(&state, &activity).await.is_some());
    assert!(!state.subscribe_integrity_idle().borrow().is_stably_idle());

    drop(activity);
    wait_for_integrity_idle(&state).await;
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn install_foreground_retention_waits_for_store_terminal() {
    let root = temp_root("install-foreground-terminal-retention");
    let state = build_test_state(&root);
    let install_id = "foreground-retained-install";
    state.installs().insert(install_id.to_string()).await;
    let foreground = register_install_foreground(&state)
        .expect("register install foreground")
        .wait_for_settlement()
        .await;
    let activity = InstallForegroundActivity::new_with_update_admission(
        foreground,
        state
            .try_admit_update_sensitive_operation()
            .expect("admit install update-sensitive operation"),
    );
    let producer = state.try_claim_producer().expect("claim install producer");

    spawn_install_foreground_retention(
        state.clone(),
        install_id.to_string(),
        producer,
        activity.clone(),
    );
    drop(activity);
    tokio::task::yield_now().await;
    assert!(!state.subscribe_integrity_idle().borrow().is_stably_idle());

    state.installs().emit(install_id, done_progress()).await;
    wait_for_integrity_idle(&state).await;
    state.installs().remove(install_id).await;
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn failed_progress_journal_task_keeps_foreground_and_queue_active() {
    let root = temp_root("install-journal-failure-retention");
    let state = build_test_state(&root);
    let install_id = "journal-failed-install";
    let operation_id = install_operation_id(install_id);
    state
        .installs()
        .insert_or_existing_vanilla(install_id.to_string(), "1.21.5".to_string())
        .await;
    assert!(state.installs().mark_initialized(install_id).await);
    begin_install_operation_journal(state.journals(), &operation_id, "1.21.5")
        .await
        .expect("begin install operation journal");
    state
        .installs()
        .enqueue_queued_install(
            "journal-failed-queue".to_string(),
            InstallQueueSpec::vanilla("1.21.5".to_string()),
            InstallQueuePlacement::Back,
        )
        .await;
    let reserved = state
        .installs()
        .reserve_next_queued_install()
        .await
        .expect("reserve active queue entry");
    assert!(
        state
            .installs()
            .mark_queued_install_started(&reserved.queue_id, install_id.to_string())
            .await
    );
    state
        .installs()
        .enqueue_queued_install(
            "journal-failed-successor".to_string(),
            InstallQueueSpec::vanilla("1.21.6".to_string()),
            InstallQueuePlacement::Back,
        )
        .await;
    let queue_start_gate = state.installs().acquire_queue_start_gate().await;

    let foreground = register_install_foreground(&state)
        .expect("register install foreground")
        .wait_for_settlement()
        .await;
    let foreground = InstallForegroundActivity::new_with_update_admission(
        foreground,
        state
            .try_admit_update_sensitive_operation()
            .expect("admit install update-sensitive operation"),
    );
    spawn_install_foreground_retention(
        state.clone(),
        install_id.to_string(),
        state
            .try_claim_producer()
            .expect("claim foreground retention producer"),
        foreground.clone(),
    );
    spawn_install_queue_monitor(state.clone(), install_id.to_string());
    let interrupted_state = state.clone();
    let interrupted_foreground = foreground.clone();
    let interrupted_journals = state.journals().clone();
    let interrupted_operation_id = operation_id.clone();
    let (handler_started_tx, handler_started_rx) = tokio::sync::oneshot::channel();
    let (handler_release_tx, handler_release_rx) = tokio::sync::oneshot::channel();
    let supervisor = InstallStore::spawn_tracked_worker_with_interrupt_handler_owned(
        state.installs().clone(),
        state
            .try_claim_producer()
            .expect("claim tracked worker producer"),
        install_id.to_string(),
        interrupted_install_progress(),
        async move {
            let progress_task = tokio::spawn(async { false });
            assert!(!finish_install_progress_task(progress_task).await);
        },
        move |progress| async move {
            assert!(
                retain_install_foreground(&interrupted_state, &interrupted_foreground)
                    .await
                    .is_some()
            );
            let _ = handler_started_tx.send(());
            handler_release_rx
                .await
                .expect("release interrupted journal settlement");
            record_install_operation_interrupted(
                interrupted_journals.as_ref(),
                &interrupted_operation_id,
                &progress,
            )
            .await
            .is_ok()
        },
    );
    drop(foreground);

    timeout(Duration::from_secs(1), handler_started_rx)
        .await
        .expect("interruption handler should start")
        .expect("interruption handler start signal");

    let before_terminal = state
        .installs()
        .snapshot(install_id)
        .await
        .expect("install remains owned before interruption settlement");
    assert!(!before_terminal.done);
    assert!(!install_journal_is_terminal(
        state
            .journals()
            .get(&operation_id)
            .expect("live install journal")
            .status
    ));
    assert!(!state.subscribe_integrity_idle().borrow().is_stably_idle());
    let queue_before_terminal = state.installs().queue_snapshot().await;
    assert_eq!(
        queue_before_terminal
            .active
            .as_ref()
            .and_then(|active| active.install_id.as_deref()),
        Some(install_id)
    );
    assert_eq!(queue_before_terminal.pending.len(), 1);
    assert_eq!(
        queue_before_terminal.pending[0].queue_id,
        "journal-failed-successor"
    );

    handler_release_tx
        .send(())
        .expect("release interruption handler");
    timeout(Duration::from_secs(1), supervisor)
        .await
        .expect("tracked worker supervisor should settle")
        .expect("tracked worker supervisor should not panic");

    let terminal_journal = state
        .journals()
        .get(&operation_id)
        .expect("interrupted terminal journal");
    assert!(install_journal_is_terminal(terminal_journal.status));
    assert_eq!(
        terminal_journal.failure_point.as_deref(),
        Some("install_worker_interrupted")
    );
    let terminal_store = state
        .installs()
        .snapshot(install_id)
        .await
        .expect("interrupted store terminal");
    assert!(terminal_store.done);
    assert!(
        terminal_store
            .latest
            .as_ref()
            .is_some_and(|record| record.progress.done && record.progress.error.is_some())
    );

    wait_for_integrity_idle(&state).await;
    assert!(install_journal_is_terminal(
        state
            .journals()
            .get(&operation_id)
            .expect("terminal journal remains visible at idle")
            .status
    ));
    timeout(Duration::from_secs(1), async {
        loop {
            if state.installs().queue_snapshot().await.active.is_none() {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("terminal queue owner should release");
    let queue_at_terminal = state.installs().queue_snapshot().await;
    assert_eq!(queue_at_terminal.pending.len(), 1);
    assert_eq!(
        queue_at_terminal.pending[0].queue_id,
        "journal-failed-successor"
    );

    drop(queue_start_gate);
    wait_for_queue_empty(&state).await;
    state.installs().remove(install_id).await;
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn install_queue_status_authors_backend_queue_view_models() {
    let root = temp_root("install-queue-view-model");
    let state = build_test_state(&root);
    state
        .installs()
        .enqueue_queued_install(
            "queue-vanilla".to_string(),
            InstallQueueSpec::vanilla("1.21.5".to_string()),
            InstallQueuePlacement::Back,
        )
        .await;
    state
        .installs()
        .enqueue_queued_install(
            "queue-loader".to_string(),
            InstallQueueSpec::loader(
                LoaderComponentId::Fabric,
                build_id_for(LoaderComponentId::Fabric, "1.21.5", "0.16.10"),
                "fabric-loader-1.21.5".to_string(),
                "1.21.5".to_string(),
                "0.16.10".to_string(),
            ),
            InstallQueuePlacement::Front,
        )
        .await;

    let response = install_queue_state_response(&state, None, None).await;

    assert!(response.active.is_none());
    assert!(response.started_install.is_none());
    assert_eq!(response.items.len(), 2);
    assert_eq!(response.view_model.state_id, "queued");
    assert_eq!(response.view_model.status_label, "Queued");
    assert_eq!(response.view_model.queued_count, 2);
    assert_eq!(response.view_model.queued_count_label, "2 queued");
    assert_eq!(
        response.view_model.next_label.as_deref(),
        Some("Fabric 0.16.10 for Minecraft 1.21.5")
    );
    assert_eq!(response.items[0].queue_id, "queue-loader");
    assert_eq!(response.items[0].position, 1);
    assert_eq!(response.items[0].total, 2);
    assert_eq!(response.items[0].kind, "loader");
    assert_eq!(
        response.items[0].label,
        "Fabric 0.16.10 for Minecraft 1.21.5"
    );
    assert_eq!(response.items[0].title, "Install queued");
    assert!(response.items[0].detail.contains("Position 1 of 2"));
    assert_eq!(
        response.items[0]
            .install_item
            .loader
            .as_ref()
            .expect("loader item")
            .component_id,
        "net.fabricmc.fabric-loader"
    );
    assert_eq!(response.items[0].remove_action.action, "remove_from_queue");
    assert!(response.items[0].remove_action.enabled);
    assert_eq!(response.items[1].label, "Minecraft 1.21.5");
    assert_no_public_raw_fragments(&serde_json::to_string(&response).expect("queue json"));

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn enqueue_prestart_failure_does_not_insert_pending_queue_item() {
    let root = temp_root("install-queue-start-failure-retention");
    let state = build_test_state(&root);

    let admitted = state.try_admit_request().expect("admit install request");
    let error = enqueue_install_owned(
        &state,
        InstallQueueRequest::Vanilla {
            version_id: "1.21.5".to_string(),
        },
        admitted.producer_handoff(),
    )
    .await
    .expect_err("missing library should fail before queue insertion");

    assert_eq!(error.0, StatusCode::PRECONDITION_FAILED);
    let snapshot = state.installs().queue_snapshot().await;
    assert!(snapshot.active.is_none());
    assert!(snapshot.pending.is_empty());

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn install_queue_state_shows_reserved_item_while_starting() {
    let root = temp_root("install-queue-reserved-starting");
    let state = build_test_state(&root);
    state
        .installs()
        .enqueue_queued_install(
            "queue-starting".to_string(),
            InstallQueueSpec::vanilla("1.21.5".to_string()),
            InstallQueuePlacement::Back,
        )
        .await;
    let reserved = state
        .installs()
        .reserve_next_queued_install()
        .await
        .expect("active reservation");
    assert_eq!(reserved.queue_id, "queue-starting");

    let response = install_queue_state_response(&state, None, None).await;

    let active = response.active.expect("starting active view");
    assert_eq!(active.queue_id, "queue-starting");
    assert_eq!(active.install_id, None);
    assert_eq!(active.operation_id, None);
    assert_eq!(active.title, "Starting install");
    assert_eq!(active.progress.phase_id, "starting");
    assert_eq!(response.view_model.state_id, "active");
    assert_eq!(response.view_model.status_label, "Installing");

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn cancelled_queue_start_after_content_journal_commit_completes_owned_handoff() {
    let root = temp_root("install-queue-owned-content-start");
    let state = build_test_state(&root);
    let queue_id = "queue-owned-content-start";
    state
        .installs()
        .enqueue_queued_install(
            queue_id.to_string(),
            InstallQueueSpec::Content {
                instance_id: "missing-instance".to_string(),
                label: "Removing content".to_string(),
                action: ContentQueueAction::Uninstall {
                    canonical_ids: vec!["modrinth:missing".to_string()],
                },
                prerequisite_queue_id: None,
            },
            InstallQueuePlacement::Back,
        )
        .await;
    let producer = state
        .try_claim_producer()
        .expect("claim queue start producer");
    let (journal_committed_tx, journal_committed_rx) = tokio::sync::oneshot::channel();
    let (resume_after_journal_tx, resume_after_journal_rx) = tokio::sync::oneshot::channel();
    let (worker_spawned_tx, worker_spawned_rx) = tokio::sync::oneshot::channel();
    let (resume_queue_mark_tx, resume_queue_mark_rx) = tokio::sync::oneshot::channel();
    let waiter_state = state.clone();

    let waiter = tokio::spawn(async move {
        maybe_start_next_queued_install_owned_with(
            &waiter_state,
            &producer,
            move |start_state, spec, start_producer| async move {
                let InstallQueueSpec::Content {
                    instance_id,
                    label,
                    action,
                    ..
                } = spec
                else {
                    panic!("expected content queue spec");
                };
                let started = start_content_operation_with_after_journal(
                    &start_state,
                    &instance_id,
                    &label,
                    &action,
                    &start_producer,
                    move |install_id, operation_id| async move {
                        let _ = journal_committed_tx.send((install_id, operation_id));
                        let _ = resume_after_journal_rx.await;
                    },
                )
                .await?;
                let _ = worker_spawned_tx.send(started.install_id.clone());
                let _ = resume_queue_mark_rx.await;
                Ok(started)
            },
        )
        .await
    });

    let (install_id, operation_id) = timeout(Duration::from_secs(1), journal_committed_rx)
        .await
        .expect("content journal commit gap is reached")
        .expect("content journal commit is observed");
    let reserved = state
        .installs()
        .queue_snapshot()
        .await
        .active
        .expect("queue entry remains reserved");
    assert_eq!(reserved.queue_id, queue_id);
    assert!(reserved.install_id.is_none());
    assert!(state.installs().snapshot(&install_id).await.is_none());
    assert_eq!(
        state
            .journals()
            .get(&operation_id)
            .expect("planned content journal")
            .status,
        OperationStatus::Planned
    );

    waiter.abort();
    assert!(
        waiter
            .await
            .expect_err("request waiter is cancelled")
            .is_cancelled()
    );
    let _ = resume_after_journal_tx.send(());

    let spawned_install_id = timeout(Duration::from_secs(1), worker_spawned_rx)
        .await
        .expect("owned start inserts the install and spawns its worker")
        .expect("worker spawn is observed");
    assert_eq!(spawned_install_id, install_id);
    assert!(state.installs().snapshot(&install_id).await.is_some());
    let reserved = state
        .installs()
        .queue_snapshot()
        .await
        .active
        .expect("queue remains reserved until the exact start is marked");
    assert_eq!(reserved.queue_id, queue_id);
    assert!(reserved.install_id.is_none());

    let _ = resume_queue_mark_tx.send(());
    timeout(Duration::from_secs(1), async {
        loop {
            let snapshot = state.installs().queue_snapshot().await;
            if snapshot.active.is_none()
                || snapshot.active.as_ref().is_some_and(|active| {
                    active.queue_id == queue_id
                        && active.install_id.as_deref() == Some(install_id.as_str())
                })
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("owned start marks the exact reserved queue entry");
    timeout(Duration::from_secs(1), async {
        while state
            .journals()
            .get(&operation_id)
            .is_some_and(|entry| entry.status == OperationStatus::Planned)
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("content worker terminalizes the durable journal");
    wait_for_queue_empty(&state).await;

    assert_ne!(
        state
            .journals()
            .get(&operation_id)
            .expect("terminal content journal")
            .status,
        OperationStatus::Planned
    );
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn continuation_queue_skips_failed_older_head_and_starts_selected_residual() {
    let root = temp_root("install-queue-selected-residual");
    let state = build_test_state(&root);
    state
        .installs()
        .enqueue_queued_install(
            "invalid-older-head".to_string(),
            InstallQueueSpec::vanilla(String::new()),
            InstallQueuePlacement::Back,
        )
        .await;
    state
        .installs()
        .enqueue_queued_install(
            "selected-queue".to_string(),
            InstallQueueSpec::vanilla("1.21.5".to_string()),
            InstallQueuePlacement::Back,
        )
        .await;
    let attempts = Arc::new(Mutex::new(Vec::<String>::new()));
    let observed_attempts = attempts.clone();

    let started = maybe_start_selected_queued_install_owned_with(
        &state,
        "selected-queue",
        true,
        move |spec| {
            let attempts = observed_attempts.clone();
            async move {
                let version_id = spec.target_version_id().to_string();
                attempts
                    .lock()
                    .expect("record queue start attempt")
                    .push(version_id.clone());
                if version_id.is_empty() {
                    return Err((
                        StatusCode::BAD_REQUEST,
                        Json(json!({ "error": "invalid older queue head" })),
                    ));
                }
                Ok(InstallStartResponse {
                    operation_id: install_operation_id("selected-install"),
                    install_id: "selected-install".to_string(),
                    view_model: InstallProgressViewModel::starting(),
                })
            }
        },
    )
    .await
    .expect("unrelated head failure does not fail selected enqueue")
    .expect("selected install starts");

    assert_eq!(started.install_id, "selected-install");
    assert_eq!(
        *attempts.lock().expect("read queue start attempts"),
        vec![String::new(), "1.21.5".to_string()]
    );
    let snapshot = state.installs().queue_snapshot().await;
    assert!(snapshot.pending.is_empty());
    let active = snapshot.active.expect("selected queue remains active");
    assert_eq!(active.queue_id, "selected-queue");
    assert_eq!(active.install_id.as_deref(), Some("selected-install"));

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn selected_queue_skips_and_cleans_a_failed_prerequisite_dependent() {
    let root = temp_root("install-queue-selected-prerequisite");
    let state = build_test_state(&root);
    let instance = state
        .instances()
        .insert_for_test("Dependent setup", "1.21.5")
        .expect("create dependent setup instance");
    let cleanup = setup_instance_cleanup(&state, &instance, false);
    state
        .installs()
        .enqueue_queued_install(
            "failed-prerequisite".to_string(),
            InstallQueueSpec::vanilla(String::new()),
            InstallQueuePlacement::Back,
        )
        .await;
    state
        .installs()
        .enqueue_queued_install(
            "dependent-content".to_string(),
            InstallQueueSpec::Content {
                instance_id: instance.id.clone(),
                label: "Dependent content".to_string(),
                action: ContentQueueAction::Install {
                    selections: Vec::new(),
                    allow_incompatible: false,
                    setup_cleanup: Some(cleanup),
                },
                prerequisite_queue_id: Some("failed-prerequisite".to_string()),
            },
            InstallQueuePlacement::Back,
        )
        .await;
    state
        .installs()
        .enqueue_queued_install(
            "selected-queue".to_string(),
            InstallQueueSpec::vanilla("selected-version".to_string()),
            InstallQueuePlacement::Back,
        )
        .await;
    let attempts = Arc::new(Mutex::new(Vec::<String>::new()));
    let observed_attempts = attempts.clone();

    let started = maybe_start_selected_queued_install_owned_with(
        &state,
        "selected-queue",
        true,
        move |spec| {
            let attempts = observed_attempts.clone();
            async move {
                let target = spec.target_version_id().to_string();
                attempts
                    .lock()
                    .expect("record prerequisite traversal")
                    .push(target.clone());
                if target.is_empty() {
                    return Err((
                        StatusCode::BAD_REQUEST,
                        Json(json!({ "error": "failed prerequisite" })),
                    ));
                }
                Ok(InstallStartResponse {
                    operation_id: install_operation_id("selected-install"),
                    install_id: "selected-install".to_string(),
                    view_model: InstallProgressViewModel::starting(),
                })
            }
        },
    )
    .await
    .expect("failed prerequisite and dependent are settled")
    .expect("independent selection starts");

    assert_eq!(started.install_id, "selected-install");
    assert_eq!(
        *attempts.lock().expect("read prerequisite traversal"),
        vec![String::new(), "selected-version".to_string()]
    );
    assert_eq!(
        state
            .installs()
            .queued_install_succeeded("failed-prerequisite")
            .await,
        Some(false)
    );
    assert_eq!(
        state
            .installs()
            .queued_install_succeeded("dependent-content")
            .await,
        Some(false)
    );
    assert!(state.instances().get(&instance.id).is_none());
    let snapshot = state.installs().queue_snapshot().await;
    assert!(snapshot.pending.is_empty());
    assert_eq!(
        snapshot
            .active
            .as_ref()
            .map(|entry| entry.queue_id.as_str()),
        Some("selected-queue")
    );
    assert!(
        state
            .installs()
            .discard_active_queued_install("selected-queue")
            .await
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn enqueue_settles_its_selected_item_after_an_older_head_start_failure() {
    let root = temp_root("install-queue-enqueue-selected-settlement");
    let state = build_test_state(&root);
    let library_dir = root.join("library");
    fs::create_dir_all(&library_dir).expect("create library");
    state.set_library_dir_for_test(library_dir.to_string_lossy().to_string());
    state
        .installs()
        .enqueue_queued_install(
            "invalid-older-head".to_string(),
            InstallQueueSpec::vanilla(String::new()),
            InstallQueuePlacement::Back,
        )
        .await;
    let producer = state
        .try_claim_producer()
        .expect("claim selected enqueue producer");
    let update_admission = state
        .try_admit_update_sensitive_operation()
        .expect("admit selected enqueue");

    let response = enqueue_install_with_placement(
        &state,
        InstallQueueRequest::Vanilla {
            version_id: "1.21.5".to_string(),
        },
        InstallQueuePlacement::Back,
        None,
        None,
        producer,
        update_admission,
    )
    .await
    .expect("older head failure does not strand the selected enqueue");

    let install_id = response
        .started_install
        .expect("selected install starts")
        .install_id;
    let snapshot = state.installs().queue_snapshot().await;
    assert!(
        snapshot
            .active
            .as_ref()
            .is_none_or(|entry| entry.queue_id != "invalid-older-head")
    );
    assert!(
        snapshot
            .pending
            .iter()
            .all(|entry| entry.queue_id != "invalid-older-head")
    );
    state.installs().emit(&install_id, failed_progress()).await;
    wait_for_queue_empty(&state).await;

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn continuation_queue_removes_owned_selection_after_front_retry_budget() {
    let root = temp_root("install-queue-selected-front-injection");
    let state = build_test_state(&root);
    state
        .installs()
        .enqueue_queued_install(
            "older-head".to_string(),
            InstallQueueSpec::vanilla("older".to_string()),
            InstallQueuePlacement::Back,
        )
        .await;
    state
        .installs()
        .enqueue_queued_install(
            "selected-queue".to_string(),
            InstallQueueSpec::vanilla("selected".to_string()),
            InstallQueuePlacement::Back,
        )
        .await;
    let injections = Arc::new(AtomicUsize::new(0));
    let observed_injections = injections.clone();
    let injection_state = state.clone();

    let (status, Json(body)) =
        maybe_start_selected_queued_install_owned_with(&state, "selected-queue", true, move |_| {
            let attempt = observed_injections.fetch_add(1, Ordering::SeqCst);
            let state = injection_state.clone();
            async move {
                state
                    .installs()
                    .enqueue_queued_install(
                        format!("injected-front-{attempt}"),
                        InstallQueueSpec::vanilla(format!("injected-{attempt}")),
                        InstallQueuePlacement::Front,
                    )
                    .await;
                Err((
                    StatusCode::BAD_GATEWAY,
                    Json(json!({ "error": "injected start failure" })),
                ))
            }
        })
        .await
        .expect_err("front retries exhausting the budget must fail and settle the owned selection");

    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(
        body,
        json!({
            "error": "The selected install left the queue before it could start. Try again."
        })
    );
    assert_eq!(injections.load(Ordering::SeqCst), 3);
    let snapshot = state.installs().queue_snapshot().await;
    assert!(snapshot.active.is_none());
    assert!(
        snapshot
            .pending
            .iter()
            .all(|entry| entry.queue_id != "selected-queue")
    );
    assert_eq!(snapshot.pending.len(), 1);
    assert_eq!(snapshot.pending[0].queue_id, "injected-front-2");

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn continuation_queue_waits_for_selected_reservation_failure_and_errors() {
    let root = temp_root("install-queue-selected-reservation-failure");
    let state = build_test_state(&root);
    state
        .installs()
        .enqueue_queued_install(
            "selected-queue".to_string(),
            InstallQueueSpec::vanilla("1.21.5".to_string()),
            InstallQueuePlacement::Back,
        )
        .await;
    let competing_start = state.installs().acquire_queue_start_gate().await;
    let reserved = state
        .installs()
        .reserve_next_queued_install()
        .await
        .expect("competing starter reserves selected queue");
    assert_eq!(reserved.queue_id, "selected-queue");
    let continuation_starts = Arc::new(AtomicUsize::new(0));
    let observed_starts = continuation_starts.clone();

    let continuation =
        maybe_start_selected_queued_install_owned_with(&state, "selected-queue", true, move |_| {
            observed_starts.fetch_add(1, Ordering::SeqCst);
            async {
                Err((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": "unexpected continuation start" })),
                ))
            }
        });
    tokio::pin!(continuation);
    tokio::select! {
        biased;
        result = &mut continuation => panic!("continuation escaped uncommitted reservation: {result:?}"),
        _ = std::future::ready(()) => {}
    }

    assert!(
        state
            .installs()
            .discard_active_queued_install("selected-queue")
            .await
    );
    drop(competing_start);
    let (status, Json(body)) = continuation
        .await
        .expect_err("discarded selected reservation must fail continuation");
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(
        body,
        json!({
            "error": "The selected install left the queue before it could start. Try again."
        })
    );
    assert_eq!(continuation_starts.load(Ordering::SeqCst), 0);

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn continuation_queue_accepts_committed_selected_active_install() {
    let root = temp_root("install-queue-selected-active-committed");
    let state = build_test_state(&root);
    state
        .installs()
        .enqueue_queued_install(
            "selected-queue".to_string(),
            InstallQueueSpec::vanilla("1.21.5".to_string()),
            InstallQueuePlacement::Back,
        )
        .await;
    let competing_start = state.installs().acquire_queue_start_gate().await;
    let reserved = state
        .installs()
        .reserve_next_queued_install()
        .await
        .expect("competing starter reserves selected queue");
    assert_eq!(reserved.queue_id, "selected-queue");
    state
        .installs()
        .insert("selected-install".to_string())
        .await;
    assert!(
        state
            .installs()
            .mark_queued_install_started("selected-queue", "selected-install".to_string())
            .await
    );
    spawn_install_queue_monitor(state.clone(), "selected-install".to_string());
    drop(competing_start);
    let continuation_starts = Arc::new(AtomicUsize::new(0));
    let observed_starts = continuation_starts.clone();

    let started =
        maybe_start_selected_queued_install_owned_with(&state, "selected-queue", true, move |_| {
            observed_starts.fetch_add(1, Ordering::SeqCst);
            async {
                Err((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": "unexpected continuation start" })),
                ))
            }
        })
        .await
        .expect("committed active selected queue is sufficient");
    assert!(started.is_none());
    let snapshot = state.installs().queue_snapshot().await;
    let active = snapshot.active.expect("selected queue remains active");
    assert_eq!(active.queue_id, "selected-queue");
    assert_eq!(active.install_id.as_deref(), Some("selected-install"));
    assert_eq!(continuation_starts.load(Ordering::SeqCst), 0);

    state
        .installs()
        .emit("selected-install", failed_progress())
        .await;
    wait_for_queue_empty(&state).await;

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn queue_monitor_advances_only_after_terminal_progress_and_discards_start_failure() {
    let root = temp_root("install-queue-monitor-terminal");
    let state = build_test_state(&root);
    state
        .installs()
        .insert_or_existing_vanilla("active-install".to_string(), "1.21.5".to_string())
        .await;
    state
        .installs()
        .enqueue_queued_install(
            "queue-active".to_string(),
            InstallQueueSpec::vanilla("1.21.5".to_string()),
            InstallQueuePlacement::Back,
        )
        .await;
    let reserved = state
        .installs()
        .reserve_next_queued_install()
        .await
        .expect("active reservation");
    assert_eq!(reserved.queue_id, "queue-active");
    assert!(
        state
            .installs()
            .mark_queued_install_started("queue-active", "active-install".to_string())
            .await
    );
    state
        .installs()
        .enqueue_queued_install(
            "queue-pending".to_string(),
            InstallQueueSpec::vanilla("1.21.6".to_string()),
            InstallQueuePlacement::Back,
        )
        .await;

    spawn_install_queue_monitor(state.clone(), "active-install".to_string());
    state
        .installs()
        .emit("active-install", base_progress("libraries"))
        .await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let snapshot = state.installs().queue_snapshot().await;
    assert_eq!(
        snapshot
            .active
            .as_ref()
            .map(|active| active.queue_id.as_str()),
        Some("queue-active")
    );
    assert_eq!(snapshot.pending.len(), 1);
    assert_eq!(snapshot.pending[0].queue_id, "queue-pending");

    state
        .installs()
        .emit("active-install", failed_progress())
        .await;
    wait_for_queue_empty(&state).await;

    let snapshot = state.installs().queue_snapshot().await;
    assert!(snapshot.active.is_none());
    assert!(snapshot.pending.is_empty());

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn queue_monitor_does_not_start_successor_while_requests_are_draining() {
    let root = temp_root("install-queue-monitor-request-drain");
    let state = build_test_state(&root);
    state
        .installs()
        .insert_or_existing_vanilla("draining-active-install".to_string(), "1.21.5".to_string())
        .await;
    state
        .installs()
        .enqueue_queued_install(
            "draining-active-queue".to_string(),
            InstallQueueSpec::vanilla("1.21.5".to_string()),
            InstallQueuePlacement::Back,
        )
        .await;
    let reserved = state
        .installs()
        .reserve_next_queued_install()
        .await
        .expect("active queue reservation");
    assert_eq!(reserved.queue_id, "draining-active-queue");
    assert!(
        state
            .installs()
            .mark_queued_install_started(
                "draining-active-queue",
                "draining-active-install".to_string(),
            )
            .await
    );
    state
        .installs()
        .enqueue_queued_install(
            "draining-pending-queue".to_string(),
            InstallQueueSpec::vanilla("1.21.6".to_string()),
            InstallQueuePlacement::Back,
        )
        .await;

    let request = state.try_admit_request().expect("hold draining request");
    spawn_install_queue_monitor(state.clone(), "draining-active-install".to_string());
    let shutdown_state = state.clone();
    let quiesce = tokio::spawn(async move { shutdown_state.quiesce().await });
    tokio::time::timeout(Duration::from_secs(1), async {
        while state.lifecycle_phase() != crate::state::AppLifecyclePhase::DrainingRequests {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("request drain begins");

    state
        .installs()
        .emit("draining-active-install", failed_progress())
        .await;
    tokio::time::timeout(Duration::from_secs(1), async {
        while state.installs().queue_snapshot().await.active.is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("active queue clears");
    let snapshot = state.installs().queue_snapshot().await;
    assert_eq!(snapshot.pending.len(), 1);
    assert_eq!(snapshot.pending[0].queue_id, "draining-pending-queue");
    assert_eq!(state.installs().active_install_count().await, 0);

    drop(request);
    quiesce
        .await
        .expect("quiesce task")
        .expect("quiesce completes");
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn install_status_exposes_interrupted_install_as_redacted_terminal_state() {
    let root = temp_root("install-status-interrupted");
    let state = build_test_state(&root);
    let install_id = "interrupted-status-install";
    let operation_id = install_operation_id(install_id);
    state.installs().insert(install_id.to_string()).await;
    state
        .installs()
        .emit(install_id, interrupted_install_progress())
        .await;
    begin_install_operation_journal(state.journals(), &operation_id, "1.21.5")
        .await
        .expect("record install journal");
    record_install_operation_interrupted(
            state.journals(),
            &operation_id,
            &DownloadProgress {
                phase: r"C:\Users\Alice\.minecraft --accessToken provider_payload".to_string(),
                current: 0,
                total: 0,
                file: Some("/Users/alice/.axial/libraries/secret.jar".to_string()),
                error: Some(
                    "worker interrupted in /Users/alice/.axial with token secret provider_payload={\"token\":\"secret\"}"
                        .to_string(),
                ),
                done: true,
                            bytes_done: None,
                bytes_total: None,
},
        )
        .await
        .expect("record install journal");

    let response = install_status(&state, install_id)
        .await
        .expect("install status");

    assert_eq!(response.install_id, install_id);
    assert_eq!(response.operation_id, operation_id);
    assert!(response.done);
    assert_eq!(response.progress.len(), 1);
    assert_eq!(
        response.progress[0].error.as_deref(),
        Some(INSTALL_FAILURE_MESSAGE)
    );
    let guardian = response.guardian.as_ref().expect("guardian outcome");
    assert_eq!(guardian.diagnosis_id(), DiagnosisId::DownloadUnavailable);
    assert_eq!(guardian.decision(), "retry");
    assert!(
        guardian
            .label()
            .contains("install download failure as retryable")
    );
    let journal = state.journals().get(&operation_id).expect("journal");
    assert_eq!(journal.status, OperationStatus::Failed);
    assert_eq!(
        journal.failure_point.as_deref(),
        Some("install_worker_interrupted")
    );
    assert_no_public_raw_fragments(&serde_json::to_string(&response).expect("status json"));

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn restart_interrupted_install_status_preserves_stale_temp_without_promoting_partial_bytes() {
    let root = temp_root("install-restart-stale-temp-retry");
    let install_id = "restart-stale-temp-retry-install";
    let operation_id = install_operation_id(install_id);
    let destination = root
        .join("library")
        .join("versions")
        .join("1.21.5")
        .join("1.21.5.jar");
    let temp_path = launcher_managed_download_temp_path(&destination);
    fs::create_dir_all(destination.parent().expect("destination parent"))
        .expect("create destination parent");
    fs::write(&temp_path, b"partial bytes from interrupted worker")
        .expect("write restart-visible stale temp");

    {
        let state = build_test_state(&root);
        begin_install_operation_journal(state.journals(), &operation_id, "1.21.5")
            .await
            .expect("record install journal");
        let mut last_phase = None;
        record_install_operation_progress(
            state.journals(),
            &operation_id,
            &progress("client_jar", false, None),
            &mut last_phase,
        )
        .await
        .expect("record install journal");
        record_install_operation_interrupted(
            state.journals(),
            &operation_id,
            &DownloadProgress {
                phase: r"C:\Users\Alice\.minecraft --accessToken provider_payload".to_string(),
                current: 0,
                total: 0,
                file: Some("/Users/alice/.axial/versions/1.21.5/1.21.5.jar".to_string()),
                error: Some(
                    "worker interrupted in /Users/alice/.axial with token secret provider_payload={\"token\":\"secret\"}"
                        .to_string(),
                ),
                done: true,
                            bytes_done: None,
                bytes_total: None,
},
        )
        .await
        .expect("record install journal");
    }

    assert!(
        temp_path.exists(),
        "stale temp should survive the simulated restart boundary"
    );
    let reloaded = build_test_state(&root);
    let response = install_status(&reloaded, install_id)
        .await
        .expect("restart-loaded interrupted install status");

    assert!(response.done);
    assert_eq!(
        response.failure_point.as_deref(),
        Some("install_worker_interrupted")
    );
    let guardian = response.guardian.as_ref().expect("guardian outcome");
    assert_eq!(guardian.diagnosis_id(), DiagnosisId::DownloadUnavailable);
    assert_eq!(guardian.decision(), "retry");
    let failure_view_model = response
        .failure_view_model
        .as_ref()
        .expect("failure view model");
    assert!(failure_view_model.retry_action.enabled);

    let status_json = serde_json::to_string(&response).expect("status json");
    assert_no_public_raw_fragments(&status_json);
    assert!(!destination.exists());
    assert_eq!(
        fs::read(&temp_path).expect("restart-visible stale temp"),
        b"partial bytes from interrupted worker"
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn install_status_reconstructs_journal_progress_when_snapshot_is_missing() {
    let root = temp_root("install-status-journal-replay");
    let state = build_test_state(&root);
    let install_id = "journal-replay-install";
    let operation_id = install_operation_id(install_id);
    begin_install_operation_journal(state.journals(), &operation_id, "1.21.5")
        .await
        .expect("record install journal");
    let mut last_phase = None;
    record_install_operation_progress(
        state.journals(),
        &operation_id,
        &progress("libraries", false, None),
        &mut last_phase,
    )
    .await
    .expect("record install journal");
    record_install_operation_interrupted(
        state.journals(),
        &operation_id,
        &DownloadProgress {
            phase: r"C:\Users\Alice\.minecraft --accessToken provider_payload".to_string(),
            current: 0,
            total: 0,
            file: Some("/Users/alice/.axial/libraries/secret.jar".to_string()),
            error: Some(
                "worker interrupted in /Users/alice/.axial with token secret provider_payload={\"token\":\"secret\"}"
                    .to_string(),
            ),
            done: true,
                    bytes_done: None,
            bytes_total: None,
},
    )
        .await
        .expect("record install journal");

    let response = install_status(&state, install_id)
        .await
        .expect("journal-only install status");

    assert_eq!(response.install_id, install_id);
    assert_eq!(response.operation_id, operation_id);
    assert!(response.done);
    assert_eq!(
        response.failure_point.as_deref(),
        Some("install_worker_interrupted")
    );
    assert_eq!(response.progress.len(), 2);
    assert_eq!(response.progress[0].phase, "libraries");
    assert!(!response.progress[0].done);
    assert_eq!(response.progress[1].phase, "install");
    assert!(response.progress[1].done);
    assert_eq!(
        response.progress[1].error.as_deref(),
        Some(INSTALL_FAILURE_MESSAGE)
    );
    let guardian = response.guardian.as_ref().expect("guardian outcome");
    assert_eq!(guardian.diagnosis_id(), DiagnosisId::DownloadUnavailable);
    assert_no_public_raw_fragments(&serde_json::to_string(&response).expect("status json"));

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn install_events_replay_journal_terminal_progress_when_snapshot_is_missing() {
    let root = temp_root("install-events-journal-replay");
    let state = build_test_state(&root);
    let install_id = "journal-event-install";
    let operation_id = install_operation_id(install_id);
    begin_install_operation_journal(state.journals(), &operation_id, "1.21.5")
        .await
        .expect("record install journal");
    let mut last_phase = None;
    record_install_operation_progress(
        state.journals(),
        &operation_id,
        &progress("done", true, None),
        &mut last_phase,
    )
    .await
    .expect("record install journal");

    let response = install_events_stream(&state, install_id)
        .await
        .expect("journal-only install events should be served")
        .into_response();
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("sse body should complete");
    let body = String::from_utf8(body.to_vec()).expect("sse body is utf8");

    assert!(body.contains("event: progress"));
    assert!(body.contains("\"phase\":\"done\""));
    assert!(body.contains("\"done\":true"));
    assert_no_public_raw_fragments(&body);

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn install_events_replay_restart_loaded_journal_when_snapshot_is_missing() {
    let root = temp_root("install-events-restart-journal-replay");
    let install_id = "restart-journal-event-install";
    let operation_id = install_operation_id(install_id);
    {
        let state = build_test_state(&root);
        begin_install_operation_journal(state.journals(), &operation_id, "1.21.5")
            .await
            .expect("record install journal");
        let mut last_phase = None;
        record_install_operation_progress(
            state.journals(),
            &operation_id,
            &progress("done", true, None),
            &mut last_phase,
        )
        .await
        .expect("record install journal");
    }

    let reloaded = build_test_state(&root);
    let response = install_events_stream(&reloaded, install_id)
        .await
        .expect("restart-loaded journal events should be served")
        .into_response();
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("sse body should complete");
    let body = String::from_utf8(body.to_vec()).expect("sse body is utf8");

    assert!(body.contains("event: progress"));
    assert!(body.contains("\"phase\":\"done\""));
    assert!(body.contains("\"done\":true"));
    assert_no_public_raw_fragments(&body);

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn install_status_exposes_backend_authored_guardian_download_failure_outcome() {
    let root = temp_root("install-status-guardian-download-failure");
    let state = build_test_state(&root);
    let install_id = "download-failure-status-install";
    let operation_id = install_operation_id(install_id);
    state.installs().insert(install_id.to_string()).await;
    state
        .installs()
        .emit(install_id, observed_install_failure_progress())
        .await;
    begin_install_operation_journal(state.journals(), &operation_id, "1.21.5")
        .await
        .expect("record install journal");
    let mut last_phase = None;
    record_install_operation_progress(
        state.journals(),
        &operation_id,
        &observed_install_failure_progress(),
        &mut last_phase,
    )
    .await
    .expect("record install journal");
    let facts = [ExecutionDownloadFact {
        kind: ExecutionDownloadFactKind::ProviderFailure,
        target: "minecraft_client_1.21.5".to_string(),
        fields: vec![
            (
                "url".to_string(),
                "https://example.invalid/client.jar?token=secret".to_string(),
            ),
            (
                "provider_payload".to_string(),
                "{\"token\":\"secret\"}".to_string(),
            ),
        ],
    }];
    record_install_failure_outcome(
        state.journals(),
        &GuardianFailureMemoryStore::new(),
        &operation_id,
        &facts,
        "2026-07-09T10:00:00+00:00",
    )
    .await;

    let response = install_status(&state, install_id)
        .await
        .expect("install status");

    assert!(response.done);
    let guardian = response.guardian.as_ref().expect("guardian outcome");
    assert_eq!(guardian.diagnosis_id(), DiagnosisId::DownloadUnavailable);
    assert_eq!(guardian.decision(), "retry");
    assert!(
        guardian
            .label()
            .contains("install download failure as retryable")
    );
    let failure_view_model = response
        .failure_view_model
        .as_ref()
        .expect("failure view model");
    assert_eq!(failure_view_model.state_id, "failed_retryable");
    assert_eq!(failure_view_model.summary, guardian.label());
    assert!(failure_view_model.retry_action.enabled);
    assert!(
        guardian
            .detail()
            .is_some_and(|detail| detail.contains("provider or network download"))
    );
    assert!(!guardian.guidance().is_empty());
    assert_no_public_raw_fragments(&serde_json::to_string(&guardian).expect("guardian json"));
    assert_no_public_raw_fragments(
        &serde_json::to_string(&failure_view_model).expect("failure view model json"),
    );
    assert_no_public_raw_fragments(&serde_json::to_string(&response).expect("status json"));

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn install_status_exposes_runtime_unavailable_failure_without_retry() {
    let root = temp_root("install-status-runtime-unavailable");
    let state = build_test_state(&root);
    let failure_memory = GuardianFailureMemoryStore::new();
    let install_id = "runtime-unavailable-status-install";
    let operation_id = install_operation_id(install_id);
    let error = DownloadError::RuntimeUnavailableForPlatform {
        component: "jre-legacy".to_string(),
        platform: "mac-os-arm64".to_string(),
    };
    let terminal_progress = sanitize_install_progress(install_progress_with_terminal_error(
        progress("error", true, Some(&error.to_string())),
        &error,
    ));
    state.installs().insert(install_id.to_string()).await;
    state
        .installs()
        .emit(install_id, terminal_progress.clone())
        .await;
    begin_install_operation_journal(state.journals(), &operation_id, "1.11.2")
        .await
        .expect("record install journal");
    let mut last_phase = None;
    record_install_operation_progress(
        state.journals(),
        &operation_id,
        &terminal_progress,
        &mut last_phase,
    )
    .await
    .expect("record install journal");
    let facts = [ExecutionDownloadFact {
        kind: ExecutionDownloadFactKind::MetadataMissing,
        target: "minecraft_runtime_manifest".to_string(),
        fields: Vec::new(),
    }];
    record_install_failure_outcome_for_error(
        state.journals(),
        &failure_memory,
        &operation_id,
        &error,
        &facts,
        "2026-07-09T10:00:00+00:00",
    )
    .await;

    let response = install_status(&state, install_id)
        .await
        .expect("install status");

    assert!(response.done);
    assert!(
        response
            .progress
            .last()
            .and_then(|progress| progress.error.as_deref())
            .is_some_and(|message| message.contains("not available for this device")
                && message.contains("jre-legacy")
                && message.contains("mac-os-arm64"))
    );
    let guardian = response.guardian.as_ref().expect("guardian outcome");
    assert_eq!(
        guardian.diagnosis_id(),
        DiagnosisId::ManagedRuntimeUnavailableForPlatform
    );
    assert_eq!(guardian.decision(), "block");
    assert_eq!(
        guardian.label(),
        "This Minecraft version needs a Java runtime that is not available for this device."
    );
    assert!(
        guardian
            .detail()
            .is_some_and(|detail| detail.contains("jre-legacy") && detail.contains("mac-os-arm64"))
    );
    let failure_view_model = response
        .failure_view_model
        .as_ref()
        .expect("failure view model");
    assert_eq!(failure_view_model.state_id, "failed_blocked");
    assert!(!failure_view_model.retry_action.enabled);
    assert_eq!(
        failure_view_model.retry_action.disabled_reason.as_deref(),
        Some("This version cannot be installed on this device.")
    );
    assert_no_public_raw_fragments(
        &serde_json::to_string(&failure_view_model).expect("failure view model json"),
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn install_status_exposes_rosetta_required_failure_with_retry() {
    let root = temp_root("install-status-rosetta-required");
    let state = build_test_state(&root);
    let failure_memory = GuardianFailureMemoryStore::new();
    let install_id = "runtime-rosetta-status-install";
    let operation_id = install_operation_id(install_id);
    let error = DownloadError::RuntimeRosettaRequired {
        component: "jre-legacy".to_string(),
    };
    let terminal_progress = sanitize_install_progress(install_progress_with_terminal_error(
        progress("error", true, Some(&error.to_string())),
        &error,
    ));
    state.installs().insert(install_id.to_string()).await;
    state
        .installs()
        .emit(install_id, terminal_progress.clone())
        .await;
    begin_install_operation_journal(state.journals(), &operation_id, "1.11.2")
        .await
        .expect("record install journal");
    let mut last_phase = None;
    record_install_operation_progress(
        state.journals(),
        &operation_id,
        &terminal_progress,
        &mut last_phase,
    )
    .await
    .expect("record install journal");
    let facts = [ExecutionDownloadFact {
        kind: ExecutionDownloadFactKind::MetadataMissing,
        target: "minecraft_runtime_manifest".to_string(),
        fields: Vec::new(),
    }];
    record_install_failure_outcome_for_error(
        state.journals(),
        &failure_memory,
        &operation_id,
        &error,
        &facts,
        "2026-07-09T10:00:00+00:00",
    )
    .await;

    let response = install_status(&state, install_id)
        .await
        .expect("install status");

    assert!(response.done);
    assert!(
        response
            .progress
            .last()
            .and_then(|progress| progress.error.as_deref())
            .is_some_and(|message| message.contains("Rosetta 2")
                && message.contains("jre-legacy")
                && message.contains("softwareupdate --install-rosetta --agree-to-license"))
    );
    let guardian = response.guardian.as_ref().expect("guardian outcome");
    assert_eq!(
        guardian.diagnosis_id(),
        DiagnosisId::ManagedRuntimeRosettaRequired
    );
    assert_eq!(guardian.decision(), "block");
    assert_eq!(
        guardian.label(),
        "This Minecraft version needs Rosetta 2 on Apple Silicon Macs."
    );
    assert!(
        guardian
            .detail()
            .is_some_and(|detail| detail.contains("jre-legacy") && detail.contains("Rosetta 2"))
    );
    assert!(guardian.guidance().iter().any(|guidance| {
        guidance.contains("softwareupdate --install-rosetta --agree-to-license")
    }));
    let failure_view_model = response
        .failure_view_model
        .as_ref()
        .expect("failure view model");
    assert_eq!(failure_view_model.state_id, "failed_blocked");
    assert!(failure_view_model.retry_action.enabled);
    assert_eq!(failure_view_model.retry_action.disabled_reason, None);
    assert_no_public_raw_fragments(
        &serde_json::to_string(&failure_view_model).expect("failure view model json"),
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn network_install_error_wins_over_benign_accumulated_download_facts() {
    let root = temp_root("install-status-network-error-benign-facts");
    let state = build_test_state(&root);
    let failure_memory = GuardianFailureMemoryStore::new();
    let install_id = "network-error-benign-facts-install";
    let operation_id = install_operation_id(install_id);
    let request_error = reqwest::Client::builder()
        .timeout(Duration::from_millis(100))
        .build()
        .expect("client")
        .get("http://127.0.0.1:1/axial-network-failure")
        .send()
        .await
        .expect_err("closed localhost port should fail");
    let error = DownloadError::Request(request_error);
    state.installs().insert(install_id.to_string()).await;
    state
        .installs()
        .emit(install_id, observed_install_failure_progress())
        .await;
    begin_install_operation_journal(state.journals(), &operation_id, "1.21.5")
        .await
        .expect("record install journal");
    let mut last_phase = None;
    record_install_operation_progress(
        state.journals(),
        &operation_id,
        &observed_install_failure_progress(),
        &mut last_phase,
    )
    .await
    .expect("record install journal");
    let facts = [
        ExecutionDownloadFact {
            kind: ExecutionDownloadFactKind::MetadataMissing,
            target: "minecraft_client_1.21.5".to_string(),
            fields: Vec::new(),
        },
        ExecutionDownloadFact {
            kind: ExecutionDownloadFactKind::MetadataMissing,
            target: "minecraft_asset_object_checksumless".to_string(),
            fields: Vec::new(),
        },
    ];
    record_install_failure_outcome_for_error(
        state.journals(),
        &failure_memory,
        &operation_id,
        &error,
        &facts,
        "2026-07-09T10:05:00+00:00",
    )
    .await;

    let response = install_status(&state, install_id)
        .await
        .expect("install status");

    let guardian = response.guardian.as_ref().expect("guardian outcome");
    assert_eq!(guardian.diagnosis_id(), DiagnosisId::DownloadUnavailable);
    assert_eq!(guardian.decision(), "retry");
    assert!(
        guardian
            .detail()
            .is_some_and(|detail| detail.contains("provider or network download"))
    );
    let failure_view_model = response
        .failure_view_model
        .as_ref()
        .expect("failure view model");
    assert_eq!(failure_view_model.state_id, "failed_retryable");
    assert!(failure_view_model.retry_action.enabled);
    assert_eq!(failure_view_model.retry_action.disabled_reason, None);

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn request_install_error_keeps_terminal_artifact_target_for_failure_memory() {
    let journals = OperationJournalStore::new();
    let failure_memory = GuardianFailureMemoryStore::new();
    let operation_id = install_operation_id("request-terminal-target");
    let request_error = reqwest::Client::builder()
        .timeout(Duration::from_millis(100))
        .build()
        .expect("client")
        .get("http://127.0.0.1:1/axial-network-failure")
        .send()
        .await
        .expect_err("closed localhost port should fail");
    let error = DownloadError::Request(request_error);
    let terminal_target = "minecraft_client_terminal_provider_failure";
    let facts = [
        download_fact(
            ExecutionDownloadFactKind::MetadataMissing,
            "minecraft_client_stale_missing",
        ),
        ExecutionDownloadFact {
            kind: ExecutionDownloadFactKind::ProviderFailure,
            target: terminal_target.to_string(),
            fields: vec![("status".to_string(), "503".to_string())],
        },
    ];
    begin_install_operation_journal(&journals, &operation_id, "1.21.5")
        .await
        .expect("create install journal");

    record_install_failure_outcome_for_error(
        &journals,
        &failure_memory,
        &operation_id,
        &error,
        &facts,
        "2026-07-09T10:05:00+00:00",
    )
    .await;

    let entries = failure_memory.list();
    let entry = entries
        .iter()
        .find(|entry| entry.diagnosis_id.as_str() == "download_unavailable")
        .expect("provider failure memory");
    assert_eq!(entry.target.id, terminal_target);
    assert_ne!(entry.target.id, "minecraft_download");
}

#[tokio::test]
async fn local_runtime_install_failure_cannot_record_provider_failure_memory() {
    let journals = OperationJournalStore::new();
    let failure_memory = GuardianFailureMemoryStore::new();
    let operation_id = install_operation_id("local-runtime-install-failure");
    let error = DownloadError::PrepareRuntime(
        "/private/runtime/staging failed after provider download".to_string(),
    );
    let facts = [download_fact(
        ExecutionDownloadFactKind::ProviderFailure,
        "stale_runtime_provider_fact",
    )];
    begin_install_operation_journal(&journals, &operation_id, "26.2")
        .await
        .expect("create install journal");

    record_install_failure_outcome_for_error(
        &journals,
        &failure_memory,
        &operation_id,
        &error,
        &facts,
        "2026-07-17T10:05:00+00:00",
    )
    .await;

    assert!(failure_memory.list().is_empty());
    let entry = journals.get(&operation_id).expect("journal");
    let summary = install_guardian_outcome_summary_from_journal(&entry)
        .expect("local runtime Guardian outcome");
    assert_eq!(summary.diagnosis_id(), DiagnosisId::InstallExecutionFailed);
    assert_eq!(summary.decision(), "block");
    let encoded = serde_json::to_string(&entry).expect("journal json");
    assert!(!encoded.contains("/private/runtime"));
    assert!(!encoded.contains("stale_runtime_provider_fact"));
}

#[tokio::test]
async fn unavailable_runtime_source_failure_records_component_provider_memory() {
    let journals = OperationJournalStore::new();
    let failure_memory = GuardianFailureMemoryStore::new();
    let operation_id = install_operation_id("runtime-source-failure");
    let error = DownloadError::RuntimeSource(RuntimeSourceFailure::new(
        RuntimeId::from("java-runtime-delta"),
        RuntimeSourceFailureKind::Unavailable,
        "https://provider.invalid/runtime?token=private returned 503",
    ));
    let evidence = typed_runtime_failure_evidence(&operation_id, &error)
        .expect("typed runtime source evidence");
    assert_eq!(
        evidence.fields,
        vec![
            ("component".to_string(), "java-runtime-delta".to_string()),
            ("source_failure_kind".to_string(), "unavailable".to_string(),),
        ]
    );
    begin_install_operation_journal(&journals, &operation_id, "26.2")
        .await
        .expect("create install journal");

    record_install_failure_outcome_for_error(
        &journals,
        &failure_memory,
        &operation_id,
        &error,
        &[],
        "2026-07-17T10:05:00+00:00",
    )
    .await;

    let entries = failure_memory.list();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].diagnosis_id, DiagnosisId::DownloadUnavailable);
    assert_eq!(
        entries[0].target.id,
        "java_runtime_source_java-runtime-delta"
    );
    let entry = journals.get(&operation_id).expect("journal");
    let summary = install_guardian_outcome_summary_from_journal(&entry)
        .expect("runtime source Guardian outcome");
    assert_eq!(summary.diagnosis_id(), DiagnosisId::DownloadUnavailable);
    assert_eq!(summary.decision(), "retry");
    let encoded = serde_json::to_string(&entry).expect("journal json");
    assert!(!encoded.contains("provider.invalid"));
    assert!(!encoded.contains("token"));
    assert!(!encoded.contains("private"));
}

#[tokio::test]
async fn permanent_runtime_source_failures_block_without_provider_memory_or_stale_fact_override() {
    for kind in [
        RuntimeSourceFailureKind::MetadataInvalid,
        RuntimeSourceFailureKind::IntegrityMismatch,
        RuntimeSourceFailureKind::PolicyRejected,
    ] {
        let journals = OperationJournalStore::new();
        let failure_memory = GuardianFailureMemoryStore::new();
        let operation_id = install_operation_id(&format!("runtime-source-{kind:?}"));
        let error = DownloadError::RuntimeSource(RuntimeSourceFailure::new(
            RuntimeId::from("java-runtime-gamma"),
            kind,
            "/private/runtime?token=source-secret",
        ));
        let evidence = typed_runtime_failure_evidence(&operation_id, &error)
            .expect("typed runtime source evidence");
        assert_eq!(evidence.target_id, "java_runtime_source_java-runtime-gamma");
        assert_eq!(evidence.ownership, OwnershipClass::ExternalProviderDerived);
        assert_eq!(
            evidence.kind,
            GuardianInstallArtifactFailureKind::MetadataInvalid
        );
        assert_eq!(
            evidence.fields,
            vec![
                ("component".to_string(), "java-runtime-gamma".to_string()),
                ("source_failure_kind".to_string(), kind.as_str().to_string(),),
            ]
        );
        let facts = [download_fact(
            ExecutionDownloadFactKind::ProviderFailure,
            "stale_provider_fact",
        )];
        begin_install_operation_journal(&journals, &operation_id, "26.2")
            .await
            .expect("create install journal");

        record_install_failure_outcome_for_error(
            &journals,
            &failure_memory,
            &operation_id,
            &error,
            &facts,
            "2026-07-17T10:05:00+00:00",
        )
        .await;

        assert!(failure_memory.list().is_empty(), "{kind:?}");
        let entry = journals.get(&operation_id).expect("journal");
        let summary = install_guardian_outcome_summary_from_journal(&entry)
            .expect("runtime source Guardian outcome");
        assert_eq!(
            summary.diagnosis_id(),
            DiagnosisId::InstallArtifactMetadataInvalid,
            "{kind:?}"
        );
        assert_eq!(summary.decision(), "block", "{kind:?}");
        let encoded = serde_json::to_string(&entry).expect("journal json");
        assert!(!encoded.contains("/private/runtime"));
        assert!(!encoded.contains("source-secret"));
        assert!(!encoded.contains("stale_provider_fact"));
    }
}

#[tokio::test]
async fn runtime_source_failure_memory_is_isolated_by_component() {
    let journals = OperationJournalStore::new();
    let failure_memory = GuardianFailureMemoryStore::new();

    for (suffix, component) in [
        ("delta", "java-runtime-delta"),
        ("gamma", "java-runtime-gamma"),
    ] {
        let operation_id = install_operation_id(&format!("runtime-source-{suffix}"));
        let error = DownloadError::RuntimeSource(RuntimeSourceFailure::new(
            RuntimeId::from(component),
            RuntimeSourceFailureKind::Unavailable,
            "provider unavailable",
        ));
        begin_install_operation_journal(&journals, &operation_id, "26.2")
            .await
            .expect("create install journal");
        record_install_failure_outcome_for_error(
            &journals,
            &failure_memory,
            &operation_id,
            &error,
            &[],
            "2026-07-17T10:05:00+00:00",
        )
        .await;
    }

    let mut targets = failure_memory
        .list()
        .into_iter()
        .map(|entry| entry.target.id)
        .collect::<Vec<_>>();
    targets.sort();
    assert_eq!(
        targets,
        vec![
            "java_runtime_source_java-runtime-delta",
            "java_runtime_source_java-runtime-gamma",
        ]
    );
}

#[tokio::test]
async fn install_status_exposes_backend_authored_guardian_blocking_safety_outcomes() {
    let root = temp_root("install-status-guardian-blocking-failures");
    let state = build_test_state(&root);
    let cases = [
        (
            "metadata-invalid-status-install",
            ExecutionDownloadFactKind::MetadataInvalid,
            DiagnosisId::InstallArtifactMetadataInvalid,
            "block",
            "provider metadata could not be trusted",
            "invalid provider metadata",
            "Retry later",
        ),
        (
            "permission-denied-status-install",
            ExecutionDownloadFactKind::PermissionFailure,
            DiagnosisId::FilesystemPermissionDenied,
            "block",
            "could not write launcher-managed files safely",
            "filesystem refused",
            "permissions",
        ),
        (
            "temp-write-status-install",
            ExecutionDownloadFactKind::TempWriteFailed,
            DiagnosisId::TempFileWriteFailed,
            "block",
            "temporary download state could not be written safely",
            "temporary download state",
            "disk availability",
        ),
        (
            "promote-failed-status-install",
            ExecutionDownloadFactKind::PromoteFailed,
            DiagnosisId::AtomicPromotionFailed,
            "block",
            "verified download data could not be promoted safely",
            "atomic promotion failed",
            "permissions",
        ),
    ];

    for (
        install_id,
        kind,
        diagnosis_id,
        decision,
        label_fragment,
        detail_fragment,
        guidance_fragment,
    ) in cases
    {
        let operation_id = install_operation_id(install_id);
        state.installs().insert(install_id.to_string()).await;
        state
            .installs()
            .emit(install_id, observed_install_failure_progress())
            .await;
        begin_install_operation_journal(state.journals(), &operation_id, "1.21.5")
            .await
            .expect("record install journal");
        let mut last_phase = None;
        record_install_operation_progress(
            state.journals(),
            &operation_id,
            &observed_install_failure_progress(),
            &mut last_phase,
        )
        .await
        .expect("record install journal");
        let facts = [ExecutionDownloadFact {
            kind,
            target: r"C:\Users\Alice\.minecraft\libraries\secret.jar".to_string(),
            fields: vec![
                (
                    "path".to_string(),
                    "/Users/alice/.axial/libraries/secret.jar".to_string(),
                ),
                (
                    "url".to_string(),
                    "https://example.invalid/client.jar?token=secret".to_string(),
                ),
                (
                    "provider_payload".to_string(),
                    "{\"token\":\"secret\"}".to_string(),
                ),
                ("jvm_arg".to_string(), "-Xmx8192M".to_string()),
            ],
        }];
        record_install_failure_outcome(
            state.journals(),
            &GuardianFailureMemoryStore::new(),
            &operation_id,
            &facts,
            "2026-07-09T10:00:00+00:00",
        )
        .await;

        let response = install_status(&state, install_id)
            .await
            .expect("install status");

        assert!(response.done);
        let guardian = response.guardian.as_ref().expect("guardian outcome");
        assert_eq!(guardian.diagnosis_id(), diagnosis_id);
        assert_eq!(guardian.decision(), decision);
        let failure_view_model = response
            .failure_view_model
            .as_ref()
            .expect("failure view model");
        assert_eq!(failure_view_model.state_id, "failed_blocked");
        assert_eq!(failure_view_model.summary, guardian.label());
        assert!(!failure_view_model.retry_action.enabled);
        assert!(
            failure_view_model
                .retry_action
                .disabled_reason
                .as_deref()
                .is_some_and(|reason| reason.contains(guidance_fragment)
                    || reason.contains(detail_fragment)
                    || reason.contains(label_fragment)),
            "{diagnosis_id} disabled reason did not contain backend guidance: {failure_view_model:?}"
        );
        assert!(
            guardian.label().contains(label_fragment),
            "{diagnosis_id} label did not contain expected fragment: {guardian:?}"
        );
        assert!(
            guardian
                .detail()
                .is_some_and(|detail| detail.contains(detail_fragment)),
            "{diagnosis_id} detail did not contain expected fragment: {guardian:?}"
        );
        assert!(
            guardian
                .guidance()
                .iter()
                .any(|guidance| guidance.contains(guidance_fragment)),
            "{diagnosis_id} guidance did not contain expected fragment: {guardian:?}"
        );

        let journal = state.journals().get(&operation_id).expect("journal");
        assert!(journal.guardian_diagnosis_ids.contains(&diagnosis_id));
        assert_no_public_raw_fragments(&serde_json::to_string(&guardian).expect("guardian json"));
        assert_no_public_raw_fragments(
            &serde_json::to_string(&failure_view_model).expect("failure view model json"),
        );
        assert_no_public_raw_fragments(&serde_json::to_string(&response).expect("status json"));
        assert_no_sensitive_fragments(&serde_json::to_string(&journal).expect("journal json"));
    }

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn install_status_redacts_raw_progress_history_and_install_id() {
    let root = temp_root("install-status-raw-progress");
    let state = build_test_state(&root);
    let install_id = r"C:\Users\Alice\.minecraft --accessToken raw-secret";
    state.installs().insert(install_id.to_string()).await;
    state
            .installs()
            .emit(
                install_id,
                DownloadProgress {
                    phase: r"C:\Users\Alice\.minecraft --accessToken raw-secret".to_string(),
                    current: 3,
                    total: 9,
                    file: Some("/Users/alice/.axial/libraries/secret.jar".to_string()),
                    error: Some(
                        "provider_payload={\"token\":\"secret\"} account_id=account-secret username=SecretPlayer"
                            .to_string(),
                    ),
                    done: false,
                                    bytes_done: None,
                    bytes_total: None,
},
            )
            .await;

    let response = install_status(&state, install_id)
        .await
        .expect("install status");

    assert_eq!(response.install_id, "install");
    assert_eq!(response.progress.len(), 1);
    assert_eq!(response.progress[0].phase, "install");
    assert_eq!(response.progress[0].file, None);
    assert_eq!(
        response.progress[0].error.as_deref(),
        Some(INSTALL_FAILURE_MESSAGE)
    );
    assert_no_public_raw_fragments(&serde_json::to_string(&response).expect("status json"));

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn install_status_returns_not_found_for_unknown_install() {
    let root = temp_root("install-status-unknown");
    let state = build_test_state(&root);

    let error = install_status(&state, "missing-install")
        .await
        .expect_err("missing install should be 404");

    assert_eq!(error.0, StatusCode::NOT_FOUND);
    assert_eq!(error.1.0["error"], "install session not found");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn loader_pre_operation_error_response_is_bounded_and_typed() {
    let (status, Json(body)) =
        loader_pre_operation_error_response(LoaderError::CatalogUnavailable {
            message: "GET https://loader.example.invalid/catalog.json timed out".to_string(),
            provider_failure_kind: None,
            provider_status: None,
        });

    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(body["failure_kind"], json!("catalog_unavailable"));
    assert_eq!(
        body["error"],
        json!("Loader catalog is unavailable. Check your connection and try again.")
    );
    assert_no_public_raw_fragments(body["error"].as_str().expect("error is a string"));

    let (status, Json(body)) =
        loader_pre_operation_error_response(LoaderError::CatalogUnavailable {
            message: "provider_http_failure".to_string(),
            provider_failure_kind: Some(LoaderProviderFailureKind::HttpNotFound),
            provider_status: Some(404),
        });

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["failure_kind"], json!("provider_http_failure"));
    assert_eq!(
        body["error"],
        json!("Loader catalog is unavailable. Check your connection and try again.")
    );
}

#[test]
fn loader_pre_operation_error_response_preserves_safe_explicit_messages() {
    let (status, Json(body)) =
        loader_pre_operation_error_response(LoaderError::InvalidMinecraftVersion);

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["failure_kind"], json!("invalid_minecraft_version"));
    assert_eq!(body["error"], json!("Invalid Minecraft version."));
}

#[tokio::test]
async fn loader_pre_operation_failure_does_not_allocate_an_operation() {
    let root = temp_root("loader-pre-operation-boundary");
    let state = build_test_state(&root);
    configure_library_dir(&state, &root.join("library"));
    let request = state.try_admit_request().expect("admit loader request");
    let producer = request
        .producer_handoff()
        .try_claim()
        .expect("claim loader producer");

    let error = start_loader_install_with_foreground(
        &state,
        LoaderInstallStartRequest {
            component_id: LoaderComponentId::Fabric,
            build_id: "invalid-build-id".to_string(),
        },
        &producer,
        None,
    )
    .await
    .expect_err("invalid build is rejected before operation allocation");

    assert_eq!(error.0, StatusCode::BAD_REQUEST);
    assert_eq!(error.1.0["failure_kind"], json!("invalid_build_id"));
    assert!(state.journals().list().is_empty());

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn loader_start_registers_before_resolving_the_install_target() {
    let root = temp_root("loader-install-foreground-order");
    let state = build_test_state(&root);
    configure_library_dir(&state, &root.join("library"));
    let epoch = state.subscribe_integrity_idle().borrow().epoch();
    let reservation = state
        .try_reserve_idle_sweep(
            epoch,
            state.try_claim_producer().expect("claim sweep producer"),
        )
        .expect("reserve sweep");
    let cancellation = reservation.cancellation();
    let start = tokio::spawn({
        let state = state.clone();
        async move {
            let producer = state.try_claim_producer().expect("claim loader producer");
            start_loader_install_with_foreground(
                &state,
                LoaderInstallStartRequest {
                    component_id: LoaderComponentId::Fabric,
                    build_id: "invalid-build-id".to_string(),
                },
                &producer,
                None,
            )
            .await
        }
    });

    timeout(Duration::from_secs(1), async {
        while !cancellation.is_cancelled() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("foreground registration cancels sweep");
    assert!(!start.is_finished());
    assert!(state.journals().list().is_empty());

    drop(reservation);
    let error = timeout(Duration::from_secs(1), start)
        .await
        .expect("loader start settles")
        .expect("loader start owner")
        .expect_err("invalid target is rejected");
    assert_eq!(error.0, StatusCode::BAD_REQUEST);
    wait_for_integrity_idle(&state).await;
    let _ = fs::remove_dir_all(root);
}

#[test]
fn pre_operation_response_defensively_normalizes_unexpected_active_failure() {
    let error = loader_pre_operation_error_response(LoaderError::Verify(
        "private active-worker detail".to_string(),
    ));

    assert_eq!(error.0, StatusCode::BAD_GATEWAY);
    assert_eq!(error.1.0["failure_kind"], json!("catalog_unavailable"));
    assert_eq!(
        error.1.0["error"],
        json!("Loader catalog is unavailable. Check your connection and try again.")
    );
    assert!(
        !error
            .1
            .0
            .to_string()
            .contains("private active-worker detail")
    );
}

#[test]
fn typed_active_loader_progress_is_terminal_and_redacted() {
    let error = LoaderInstallError::from(LoaderError::ArtifactMissing(
        "missing https://cdn.example.invalid/path/mod-loader.jar in /tmp/axial".to_string(),
    ));
    let progress = loader_install_error_progress(&error);

    assert_eq!(progress.phase, "error");
    assert_eq!(
        progress.error.as_deref(),
        Some("Loader artifact is unavailable. Try another build or component.")
    );
    assert!(progress.done);
    assert_no_public_raw_fragments(progress.error.as_deref().expect("error is present"));
}

#[test]
fn loader_base_install_rosetta_failure_keeps_specific_terminal_message() {
    let error = LoaderInstallError::from(LoaderError::BaseInstallFailed {
        error: Box::new(DownloadError::RuntimeRosettaRequired {
            component: "jre-legacy".to_string(),
        }),
        facts: Vec::new(),
    });
    let progress = loader_install_error_progress(&error);

    let message = progress.error.clone().expect("error is present");
    assert!(message.contains("Rosetta 2"));
    assert!(message.contains("softwareupdate --install-rosetta --agree-to-license"));

    let sanitized = sanitize_install_progress(progress);
    assert_eq!(sanitized.error.as_deref(), Some(message.as_str()));
}

#[test]
fn loader_install_done_progress_marks_session_terminal() {
    let progress = loader_install_done_progress();

    assert_eq!(progress.phase, "done");
    assert_eq!(progress.current, 1);
    assert_eq!(progress.total, 1);
    assert_eq!(progress.file, None);
    assert_eq!(progress.error, None);
    assert!(progress.done);
}

#[tokio::test]
async fn vanilla_receipt_acceptance_blocks_terminal_success_and_foreground_release() {
    let root = temp_root("vanilla-receipt-acceptance-order");
    let state = build_test_state(&root);
    let install_id = "vanilla-receipt-acceptance";
    state.installs().insert(install_id.to_string()).await;
    let foreground = register_install_foreground(&state)
        .expect("register install foreground")
        .wait_for_settlement()
        .await;
    let foreground = InstallForegroundActivity::new_with_update_admission(
        foreground,
        state
            .try_admit_update_sensitive_operation()
            .expect("admit install update-sensitive operation"),
    );
    spawn_install_foreground_retention(
        state.clone(),
        install_id.to_string(),
        state
            .try_claim_producer()
            .expect("claim foreground retention producer"),
        foreground.clone(),
    );
    drop(foreground);

    let events = Arc::new(Mutex::new(Vec::new()));
    let acceptance_events = Arc::clone(&events);
    let publication_events = Arc::clone(&events);
    let (acceptance_started_tx, acceptance_started_rx) = tokio::sync::oneshot::channel();
    let (acceptance_release_tx, acceptance_release_rx) = tokio::sync::oneshot::channel();
    let (terminal_tx, mut terminal_rx) = tokio_mpsc::unbounded_channel();
    let terminal_store = state.installs().clone();
    let terminal_store_task = tokio::spawn(async move {
        let progress = terminal_rx.recv().await.expect("terminal publication");
        terminal_store.emit(install_id, progress).await;
    });
    let publication = tokio::spawn(async move {
        let acceptance = async move {
            let _ = acceptance_started_tx.send(());
            acceptance_release_rx
                .await
                .expect("release State receipt acceptance");
            acceptance_events
                .lock()
                .expect("events lock")
                .push("accepted");
            Ok::<(), io::Error>(())
        };
        acceptance.await.expect("State receipt acceptance");
        publication_events
            .lock()
            .expect("events lock")
            .push("published");
        terminal_tx
            .send(vanilla_install_done_progress())
            .expect("publish terminal success");
    });

    acceptance_started_rx
        .await
        .expect("State receipt acceptance should start");
    assert!(!publication.is_finished());
    assert!(!state.installs().snapshot(install_id).await.unwrap().done);
    assert!(!state.subscribe_integrity_idle().borrow().is_stably_idle());
    assert!(events.lock().expect("events lock").is_empty());

    acceptance_release_tx
        .send(())
        .expect("release State receipt acceptance");
    publication.await.expect("terminal publication owner");
    terminal_store_task.await.expect("terminal store owner");
    wait_for_integrity_idle(&state).await;

    assert!(state.installs().snapshot(install_id).await.unwrap().done);
    assert_eq!(
        events.lock().expect("events lock").as_slice(),
        ["accepted", "published"]
    );
    state.installs().remove(install_id).await;
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn loader_receipt_acceptance_blocks_terminal_success_and_foreground_release() {
    let root = temp_root("loader-receipt-acceptance-order");
    let state = build_test_state(&root);
    let install_id = "loader-receipt-acceptance";
    state.installs().insert(install_id.to_string()).await;
    let foreground = register_install_foreground(&state)
        .expect("register install foreground")
        .wait_for_settlement()
        .await;
    let foreground = InstallForegroundActivity::new_with_update_admission(
        foreground,
        state
            .try_admit_update_sensitive_operation()
            .expect("admit install update-sensitive operation"),
    );
    spawn_install_foreground_retention(
        state.clone(),
        install_id.to_string(),
        state
            .try_claim_producer()
            .expect("claim foreground retention producer"),
        foreground.clone(),
    );
    drop(foreground);

    let events = Arc::new(Mutex::new(Vec::new()));
    let acceptance_events = Arc::clone(&events);
    let publication_events = Arc::clone(&events);
    let (acceptance_started_tx, acceptance_started_rx) = tokio::sync::oneshot::channel();
    let (acceptance_release_tx, acceptance_release_rx) = tokio::sync::oneshot::channel();
    let (terminal_tx, mut terminal_rx) = tokio_mpsc::unbounded_channel();
    let terminal_store = state.installs().clone();
    let terminal_store_task = tokio::spawn(async move {
        let progress = terminal_rx.recv().await.expect("terminal publication");
        terminal_store.emit(install_id, progress).await;
    });

    let publication = tokio::spawn(publish_known_good_loader_terminal(
        async move {
            let _ = acceptance_started_tx.send(());
            acceptance_release_rx
                .await
                .expect("release State receipt acceptance");
            acceptance_events
                .lock()
                .expect("events lock")
                .push("accepted");
            Ok(())
        },
        Some(done_progress()),
        move |progress| {
            assert!(progress.done);
            assert!(progress.error.is_none());
            publication_events
                .lock()
                .expect("events lock")
                .push("published");
            terminal_tx
                .send(progress)
                .expect("publish terminal success");
        },
    ));

    acceptance_started_rx
        .await
        .expect("State receipt acceptance should start");
    assert!(!publication.is_finished());
    assert!(!state.installs().snapshot(install_id).await.unwrap().done);
    assert!(!state.subscribe_integrity_idle().borrow().is_stably_idle());
    assert!(events.lock().expect("events lock").is_empty());

    acceptance_release_tx
        .send(())
        .expect("release State receipt acceptance");
    let publication = publication.await.expect("terminal publication owner");
    terminal_store_task.await.expect("terminal store owner");
    wait_for_integrity_idle(&state).await;

    assert!(!publication.acceptance_failed);
    assert!(publication.failure_summary.is_none());
    assert!(state.installs().snapshot(install_id).await.unwrap().done);
    assert_eq!(
        events.lock().expect("events lock").as_slice(),
        ["accepted", "published"]
    );
    state.installs().remove(install_id).await;
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn loader_receipt_acceptance_failure_cannot_publish_success() {
    let published = Arc::new(Mutex::new(None));
    let published_progress = Arc::clone(&published);

    let publication = publish_known_good_loader_terminal(
        async {
            Err(io::Error::other(
                "/private/library/state/known-good write failed",
            ))
        },
        Some(done_progress()),
        move |progress| {
            *published_progress.lock().expect("published lock") = Some(progress);
        },
    )
    .await;
    let progress = published
        .lock()
        .expect("published lock")
        .take()
        .expect("terminal progress");
    let progress = sanitize_install_progress(progress);

    assert!(publication.acceptance_failed);
    assert_eq!(
        publication.failure_summary.as_deref(),
        Some(INSTALL_FAILURE_MESSAGE)
    );
    assert!(progress.done);
    assert_eq!(progress.error.as_deref(), Some(INSTALL_FAILURE_MESSAGE));
    assert_ne!(progress.phase, "done");
    assert!(
        !serde_json::to_string(&progress)
            .expect("progress json")
            .contains("/private/library")
    );
}

#[tokio::test]
async fn loader_receipt_identity_mismatch_cannot_publish_success() {
    let published = Arc::new(Mutex::new(None));
    let published_progress = Arc::clone(&published);

    let publication = publish_known_good_loader_terminal(
        async {
            require_exact_loader_receipt_version(
                "loader-v2-expected",
                "loader-v2-authenticated-base",
            )?;
            Ok(())
        },
        Some(done_progress()),
        move |progress| {
            *published_progress.lock().expect("published lock") = Some(progress);
        },
    )
    .await;
    let progress = published
        .lock()
        .expect("published lock")
        .take()
        .expect("terminal progress");
    let progress = sanitize_install_progress(progress);

    assert!(publication.acceptance_failed);
    assert_eq!(
        publication.failure_summary.as_deref(),
        Some(INSTALL_FAILURE_MESSAGE)
    );
    assert!(progress.done);
    assert_eq!(progress.error.as_deref(), Some(INSTALL_FAILURE_MESSAGE));
    assert_ne!(progress.phase, "done");
    assert!(
        !serde_json::to_string(&progress)
            .expect("progress json")
            .contains("authenticated-base")
    );
}

#[tokio::test]
async fn loader_install_events_keep_terminal_installs_subscribable_after_stream_ends() {
    let root = temp_root("loader-install-events-terminal-retention");
    let state = build_test_state(&root);
    state.installs().insert("done-install".to_string()).await;
    state.installs().emit("done-install", done_progress()).await;

    let response = loader_install_events_stream(&state, "done-install")
        .await
        .expect("terminal loader install events should be served")
        .into_response();
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("sse body should complete");
    let body = String::from_utf8(body.to_vec()).expect("sse body is utf8");

    assert!(body.contains("event: progress"));
    assert!(body.contains("\"phase\":\"done\""));
    let (snapshot, _) = state
        .installs()
        .subscribe_records("done-install")
        .await
        .expect("terminal loader install remains subscribable after stream completion");
    assert!(snapshot.done);
    assert_eq!(
        snapshot
            .latest
            .as_ref()
            .map(|record| record.progress.phase.as_str()),
        Some("done")
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn loader_install_events_redact_raw_terminal_progress_snapshot() {
    let root = temp_root("loader-install-events-redaction");
    let state = build_test_state(&root);
    state
        .installs()
        .insert("raw-loader-install".to_string())
        .await;
    state
            .installs()
            .emit(
                "raw-loader-install",
                DownloadProgress {
                    phase: r"C:\Users\Alice\.minecraft --accessToken raw-secret".to_string(),
                    current: 2,
                    total: 5,
                    file: Some("/Users/alice/.axial/libraries/secret.jar".to_string()),
                    error: Some(
                        "provider_payload={\"token\":\"secret\"} account_id=account-secret username=SecretPlayer -Xmx8192M"
                            .to_string(),
                    ),
                    done: true,
                    bytes_done: None,
                    bytes_total: None,
                },
            )
        .await;

    let response = loader_install_events_stream(&state, "raw-loader-install")
        .await
        .expect("loader install events should be served")
        .into_response();
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("sse body should complete");
    let body = String::from_utf8(body.to_vec()).expect("sse body is utf8");

    assert!(body.contains("\"phase\":\"install\""));
    assert!(body.contains("Install failed. Check your connection"));
    assert_no_public_raw_fragments(&body);

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn loader_install_events_return_bounded_not_found_for_unknown_install() {
    let root = temp_root("loader-install-events-unknown");
    let state = build_test_state(&root);

    let error = loader_install_events_stream(&state, "missing-install")
        .await
        .expect_err("missing loader install should be 404");

    assert_eq!(error.0, StatusCode::NOT_FOUND);
    assert_eq!(error.1.0["error"], "loader install session not found");

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn observed_vanilla_base_install_waits_and_forwards_progress() {
    let store = Arc::new(InstallStore::new());
    store
        .insert_or_existing_vanilla("vanilla-install".to_string(), "1.21.5".to_string())
        .await;
    let (progress_tx, mut progress_rx) = tokio_mpsc::unbounded_channel();

    let observed = observe_active_vanilla_base_install(&store, "1.21.5")
        .await
        .expect("observe active base")
        .expect("active base");
    let waiter = tokio::spawn(async move {
        wait_for_observed_vanilla_base_install(observed, &progress_tx).await
    });

    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(!waiter.is_finished());

    let progress = base_progress("client");
    store.emit("vanilla-install", progress.clone()).await;
    assert_eq!(
        timeout(Duration::from_secs(1), progress_rx.recv())
            .await
            .expect("progress should arrive"),
        Some(progress)
    );

    store.emit("vanilla-install", done_progress()).await;
    timeout(Duration::from_secs(1), waiter)
        .await
        .expect("waiter should finish")
        .expect("waiter should not panic")
        .expect("successful base install should not fail loader wait");
    assert_eq!(
        timeout(Duration::from_millis(50), progress_rx.recv())
            .await
            .expect("progress sender should close"),
        None
    );
}

#[tokio::test]
async fn observing_vanilla_base_install_ignores_done_removed_or_failed_sessions() {
    let store = Arc::new(InstallStore::new());

    store
        .insert_or_existing_vanilla("done-install".to_string(), "1.21.5".to_string())
        .await;
    store.emit("done-install", done_progress()).await;
    assert!(
        observe_active_vanilla_base_install(&store, "1.21.5")
            .await
            .expect("observe done session")
            .is_none()
    );

    store
        .insert_or_existing_vanilla("failed-install".to_string(), "1.21.5".to_string())
        .await;
    store.emit("failed-install", failed_progress()).await;
    assert!(
        observe_active_vanilla_base_install(&store, "1.21.5")
            .await
            .expect("observe failed session")
            .is_none()
    );

    store
        .insert_or_existing_vanilla("removed-install".to_string(), "1.21.5".to_string())
        .await;
    store.remove("removed-install").await;
    assert!(
        observe_active_vanilla_base_install(&store, "1.21.5")
            .await
            .expect("observe removed session")
            .is_none()
    );
}

#[tokio::test]
async fn observed_vanilla_base_install_fails_when_observed_channel_closes() {
    let store = Arc::new(InstallStore::new());
    let install_id = "closed-base-install";
    store
        .insert_or_existing_vanilla(install_id.to_string(), "1.21.5".to_string())
        .await;
    let (progress_tx, mut progress_rx) = tokio_mpsc::unbounded_channel();
    let observed = observe_active_vanilla_base_install(&store, "1.21.5")
        .await
        .expect("observe active base")
        .expect("active base");
    let waiter = tokio::spawn(async move {
        wait_for_observed_vanilla_base_install(observed, &progress_tx).await
    });

    let progress = base_progress("client");
    store.emit(install_id, progress.clone()).await;
    assert_eq!(
        timeout(Duration::from_secs(1), progress_rx.recv())
            .await
            .expect("forwarded progress should arrive"),
        Some(progress)
    );
    store.remove(install_id).await;

    let progress = timeout(Duration::from_secs(1), waiter)
        .await
        .expect("closed base waiter should settle")
        .expect("base waiter should not panic")
        .expect_err("a closed observed base cannot settle successfully");
    assert_eq!(progress.error.as_deref(), Some(BASE_INSTALL_FAILED_MESSAGE));
    assert!(progress.done);
}

#[tokio::test]
async fn observed_vanilla_base_install_fails_loader_when_base_fails_while_waiting() {
    let store = Arc::new(InstallStore::new());
    store
        .insert_or_existing_vanilla("vanilla-install".to_string(), "1.21.5".to_string())
        .await;
    let (progress_tx, mut progress_rx) = tokio_mpsc::unbounded_channel();

    let observed = observe_active_vanilla_base_install(&store, "1.21.5")
        .await
        .expect("observe active base")
        .expect("active base");
    let waiter = tokio::spawn(async move {
        wait_for_observed_vanilla_base_install(observed, &progress_tx).await
    });

    tokio::time::sleep(Duration::from_millis(20)).await;
    store.emit("vanilla-install", failed_progress()).await;

    let progress = timeout(Duration::from_secs(1), waiter)
        .await
        .expect("waiter should finish")
        .expect("waiter should not panic")
        .expect_err("base failure should fail loader wait");

    assert_eq!(progress.phase, "error");
    assert_eq!(progress.current, 0);
    assert_eq!(progress.total, 0);
    assert_eq!(progress.file, None);
    assert_eq!(progress.error.as_deref(), Some(BASE_INSTALL_FAILED_MESSAGE));
    assert!(progress.done);
    assert_eq!(
        timeout(Duration::from_millis(50), progress_rx.recv())
            .await
            .expect("progress sender should close"),
        None
    );
}

#[tokio::test]
async fn loader_base_failure_reacquires_after_sweep_before_failure_mutation() {
    let root = temp_root("loader-base-failure-reacquire");
    let state = build_test_state(&root);
    let base_install_id = "loader-base-failure-base";
    let loader_install_id = "loader-base-failure-loader";
    let operation_id = install_operation_id(loader_install_id);
    state
        .installs()
        .insert_or_existing_vanilla(base_install_id.to_string(), "1.21.5".to_string())
        .await;
    assert!(state.installs().mark_initialized(base_install_id).await);
    state.installs().insert(loader_install_id.to_string()).await;
    begin_install_operation_journal(state.journals(), &operation_id, "loader-test")
        .await
        .expect("begin loader install journal");

    let base_foreground = register_install_foreground(&state)
        .expect("register base foreground")
        .wait_for_settlement()
        .await;
    let base_foreground = InstallForegroundActivity::new_with_update_admission(
        base_foreground,
        state
            .try_admit_update_sensitive_operation()
            .expect("admit base install update-sensitive operation"),
    );
    spawn_install_foreground_retention(
        state.clone(),
        base_install_id.to_string(),
        state
            .try_claim_producer()
            .expect("claim base foreground retention producer"),
        base_foreground.clone(),
    );
    drop(base_foreground);
    let loader_foreground = register_install_foreground(&state)
        .expect("register loader foreground")
        .wait_for_settlement()
        .await;
    let loader_foreground = InstallForegroundActivity::new_with_update_admission(
        loader_foreground,
        state
            .try_admit_update_sensitive_operation()
            .expect("admit loader install update-sensitive operation"),
    );
    let observed = observe_active_vanilla_base_install(state.installs(), "1.21.5")
        .await
        .expect("observe active base")
        .expect("active base");
    loader_foreground.release();
    assert!(!state.subscribe_integrity_idle().borrow().is_stably_idle());

    let (progress_tx, _progress_rx) = tokio_mpsc::unbounded_channel();
    let (wait_finished_tx, wait_finished_rx) = tokio::sync::oneshot::channel();
    let (reacquire_tx, reacquire_rx) = tokio::sync::oneshot::channel();
    let worker_state = state.clone();
    let worker_foreground = loader_foreground.clone();
    let worker_operation_id = operation_id.clone();
    let worker = tokio::spawn(async move {
        let base_install = wait_for_observed_vanilla_base_install(observed, &progress_tx).await;
        let _ = wait_finished_tx.send(());
        reacquire_rx.await.expect("start loader reacquisition");
        let _foreground = retain_install_foreground(&worker_state, &worker_foreground)
            .await
            .expect("reacquire loader foreground");
        let progress = base_install.expect_err("base install should fail");
        record_loader_base_install_dependency_guardian_failure_outcome(
            worker_state.journals(),
            &worker_operation_id,
            "loader_fabric_test",
            "1.21.5",
        )
        .await
        .expect("record dependency failure");
        worker_state
            .installs()
            .emit(loader_install_id, progress)
            .await;
    });
    drop(loader_foreground);

    state
        .installs()
        .emit(base_install_id, failed_progress())
        .await;
    wait_finished_rx.await.expect("base wait should finish");
    wait_for_integrity_idle(&state).await;
    let epoch = state.subscribe_integrity_idle().borrow().epoch();
    let reservation = state
        .try_reserve_idle_sweep(
            epoch,
            state.try_claim_producer().expect("claim sweep producer"),
        )
        .expect("reserve intervening sweep");
    let cancellation = reservation.cancellation();
    reacquire_tx.send(()).expect("release loader reacquisition");
    timeout(Duration::from_secs(1), async {
        while !cancellation.is_cancelled() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("loader reacquisition should cancel sweep");

    assert!(!worker.is_finished());
    assert!(
        state
            .journals()
            .get(&operation_id)
            .expect("loader journal")
            .completed_steps
            .is_empty()
    );
    assert!(
        !state
            .installs()
            .snapshot(loader_install_id)
            .await
            .unwrap()
            .done
    );

    drop(reservation);
    worker.await.expect("loader dependency failure worker");
    assert!(
        !state
            .journals()
            .get(&operation_id)
            .expect("loader journal")
            .completed_steps
            .is_empty()
    );
    assert!(
        state
            .installs()
            .snapshot(loader_install_id)
            .await
            .unwrap()
            .done
    );
    wait_for_integrity_idle(&state).await;
    state.installs().remove(base_install_id).await;
    state.installs().remove(loader_install_id).await;
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn loader_base_success_reacquires_after_sweep_before_loader_work() {
    let root = temp_root("loader-base-success-reacquire");
    let state = build_test_state(&root);
    let base_install_id = "loader-base-success-base";
    state
        .installs()
        .insert_or_existing_vanilla(base_install_id.to_string(), "1.21.5".to_string())
        .await;
    assert!(state.installs().mark_initialized(base_install_id).await);
    let base_foreground = register_install_foreground(&state)
        .expect("register base foreground")
        .wait_for_settlement()
        .await;
    let base_foreground = InstallForegroundActivity::new_with_update_admission(
        base_foreground,
        state
            .try_admit_update_sensitive_operation()
            .expect("admit base install update-sensitive operation"),
    );
    spawn_install_foreground_retention(
        state.clone(),
        base_install_id.to_string(),
        state
            .try_claim_producer()
            .expect("claim base foreground retention producer"),
        base_foreground.clone(),
    );
    drop(base_foreground);
    let loader_foreground = register_install_foreground(&state)
        .expect("register loader foreground")
        .wait_for_settlement()
        .await;
    let loader_foreground = InstallForegroundActivity::new_with_update_admission(
        loader_foreground,
        state
            .try_admit_update_sensitive_operation()
            .expect("admit loader install update-sensitive operation"),
    );
    let observed = observe_active_vanilla_base_install(state.installs(), "1.21.5")
        .await
        .expect("observe active base")
        .expect("active base");
    loader_foreground.release();
    assert!(!state.subscribe_integrity_idle().borrow().is_stably_idle());

    let loader_work = Arc::new(AtomicUsize::new(0));
    let worker_loader_work = Arc::clone(&loader_work);
    let (progress_tx, _progress_rx) = tokio_mpsc::unbounded_channel();
    let (wait_finished_tx, wait_finished_rx) = tokio::sync::oneshot::channel();
    let (reacquire_tx, reacquire_rx) = tokio::sync::oneshot::channel();
    let worker_state = state.clone();
    let worker_foreground = loader_foreground.clone();
    let worker = tokio::spawn(async move {
        wait_for_observed_vanilla_base_install(observed, &progress_tx)
            .await
            .expect("base install should succeed");
        let _ = wait_finished_tx.send(());
        reacquire_rx.await.expect("start loader reacquisition");
        let _foreground = retain_install_foreground(&worker_state, &worker_foreground)
            .await
            .expect("reacquire loader foreground");
        worker_loader_work.fetch_add(1, Ordering::SeqCst);
    });
    drop(loader_foreground);

    state
        .installs()
        .emit(base_install_id, done_progress())
        .await;
    wait_finished_rx.await.expect("base wait should finish");
    wait_for_integrity_idle(&state).await;
    let epoch = state.subscribe_integrity_idle().borrow().epoch();
    let reservation = state
        .try_reserve_idle_sweep(
            epoch,
            state.try_claim_producer().expect("claim sweep producer"),
        )
        .expect("reserve intervening sweep");
    let cancellation = reservation.cancellation();
    reacquire_tx.send(()).expect("release loader reacquisition");
    timeout(Duration::from_secs(1), async {
        while !cancellation.is_cancelled() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("loader reacquisition should cancel sweep");
    assert!(!worker.is_finished());
    assert_eq!(loader_work.load(Ordering::SeqCst), 0);

    drop(reservation);
    worker.await.expect("loader work owner");
    assert_eq!(loader_work.load(Ordering::SeqCst), 1);
    wait_for_integrity_idle(&state).await;
    state.installs().remove(base_install_id).await;
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn cancelled_initial_commit_releases_reservation_for_duplicate_retry() {
    let root = temp_root("cancelled-initial-journal");
    let state = build_test_state(&root);
    let (backend, journals) = install_journal_persistence_fixture(&root);
    let installs = Arc::new(InstallStore::new());
    let install_id = "cancelled-initial".to_string();
    let operation_id = install_operation_id(&install_id);
    assert!(
        installs
            .insert_or_existing_vanilla(install_id.clone(), "1.21.5".to_string())
            .await
            .1
    );

    let gate = backend.gate_attempt(1);
    let initialization = tokio::spawn(begin_install_journal_with_test_ownership(
        state.clone(),
        installs.clone(),
        journals.clone(),
        install_id.clone(),
        operation_id.clone(),
        "1.21.5".to_string(),
    ));
    backend.wait_for_attempt(1).await;
    assert!(!state.subscribe_integrity_idle().borrow().is_stably_idle());

    let duplicate_store = installs.clone();
    let duplicate_id = install_id.clone();
    let duplicate = tokio::spawn(async move {
        let (existing, inserted) = duplicate_store
            .insert_or_existing_vanilla("duplicate-waiter".to_string(), "1.21.5".to_string())
            .await;
        assert!(!inserted);
        assert_eq!(existing, duplicate_id);
        let status = duplicate_store.wait_for_initialization(&existing).await;
        assert!(matches!(
            status,
            InstallInitializationStatus::Reconciling | InstallInitializationStatus::Removed
        ));
        wait_for_install_removal(&duplicate_store, &existing).await;
        duplicate_store
            .insert_or_existing_vanilla("duplicate-retry".to_string(), "1.21.5".to_string())
            .await
    });

    initialization.abort();
    let cancellation = match initialization.await {
        Ok(_) => panic!("initialization caller was not cancelled"),
        Err(error) => error,
    };
    assert!(cancellation.is_cancelled());
    gate.release();
    let (retry_id, retry_inserted) = timeout(Duration::from_secs(1), duplicate)
        .await
        .expect("duplicate retry must not hang")
        .expect("duplicate retry task");
    assert!(retry_inserted);
    assert_eq!(retry_id, "duplicate-retry");
    assert_eq!(
        journals
            .get(&operation_id)
            .expect("committed initial journal")
            .status,
        OperationStatus::Failed
    );
    assert_eq!(
        journals
            .get(&operation_id)
            .expect("cancelled initial journal")
            .failure_point
            .as_deref(),
        Some("install_initialization_cancelled")
    );
    wait_for_integrity_idle(&state).await;
    installs.remove(&retry_id).await;
    journals.close().await.expect("close journals");
    fs::remove_dir_all(root).expect("cleanup");
}

#[tokio::test]
async fn cancelled_initialized_result_before_worker_handoff_releases_reservation() {
    let root = temp_root("cancelled-initialized-handoff");
    let state = build_test_state(&root);
    let journals = Arc::new(OperationJournalStore::new());
    let installs = Arc::new(InstallStore::new());
    let install_id = "cancelled-handoff".to_string();
    let operation_id = install_operation_id(&install_id);
    let assertion_operation_id = operation_id.clone();
    assert!(
        installs
            .insert_or_existing_vanilla(install_id.clone(), "1.21.5".to_string())
            .await
            .1
    );
    let (received_tx, received_rx) = tokio::sync::oneshot::channel();
    let (_release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
    let initialization = tokio::spawn({
        let initialization_state = state.clone();
        let installs = installs.clone();
        let journals = journals.clone();
        async move {
            let reservation = begin_install_journal_with_test_ownership(
                initialization_state,
                installs,
                journals,
                install_id,
                operation_id,
                "1.21.5".to_string(),
            )
            .await
            .expect("receive initialized reservation");
            let _ = received_tx.send(());
            let _ = release_rx.await;
            drop(reservation.hand_off());
        }
    });
    received_rx.await.expect("reservation received");
    assert!(!state.subscribe_integrity_idle().borrow().is_stably_idle());
    initialization.abort();
    assert!(
        initialization
            .await
            .expect_err("abort handoff")
            .is_cancelled()
    );

    let removed = timeout(
        Duration::from_secs(1),
        installs.wait_for_initialization("cancelled-handoff"),
    )
    .await
    .expect("reservation cleanup must not hang");
    assert_eq!(removed, InstallInitializationStatus::Removed);
    assert_eq!(
        journals
            .get(&assertion_operation_id)
            .expect("cancelled handoff journal")
            .status,
        OperationStatus::Failed
    );
    wait_for_integrity_idle(&state).await;
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn transient_initial_failure_reconciles_then_allows_retry() {
    let root = temp_root("transient-initial-journal");
    let state = build_test_state(&root);
    let (backend, journals) = install_journal_persistence_fixture(&root);
    let installs = Arc::new(InstallStore::new());
    let install_id = "transient-initial".to_string();
    let operation_id = install_operation_id(&install_id);
    assert!(
        installs
            .insert_or_existing_vanilla(install_id.clone(), "1.21.5".to_string())
            .await
            .1
    );
    backend.fail_next();
    let retry_gate = backend.gate_attempt(2);

    assert!(
        begin_install_journal_with_test_ownership(
            state.clone(),
            installs.clone(),
            journals.clone(),
            install_id.clone(),
            operation_id.clone(),
            "1.21.5".to_string(),
        )
        .await
        .is_err()
    );
    backend.wait_for_attempt(2).await;
    assert_eq!(
        installs.wait_for_initialization(&install_id).await,
        InstallInitializationStatus::Reconciling
    );
    let (existing, inserted) = installs
        .insert_or_existing_vanilla("bounded-duplicate".to_string(), "1.21.5".to_string())
        .await;
    assert!(!inserted);
    assert_eq!(existing, install_id);
    assert_eq!(
        timeout(
            Duration::from_millis(100),
            installs.wait_for_initialization(&existing),
        )
        .await
        .expect("reconciling duplicate response must be bounded"),
        InstallInitializationStatus::Reconciling
    );

    retry_gate.release();
    wait_for_install_removal(&installs, &install_id).await;
    wait_for_integrity_idle(&state).await;
    assert_eq!(
        journals.get(&operation_id).expect("retried journal").status,
        OperationStatus::Failed
    );
    assert!(
        installs
            .insert_or_existing_vanilla(
                "post-reconciliation-retry".to_string(),
                "1.21.5".to_string()
            )
            .await
            .1
    );
    installs.remove("post-reconciliation-retry").await;
    journals.close().await.expect("close journals");
    fs::remove_dir_all(root).expect("cleanup");
}

#[tokio::test]
async fn repeated_initial_failure_keeps_live_owner_and_bounds_duplicates() {
    let root = temp_root("persistent-initial-journal");
    let state = build_test_state(&root);
    let (backend, journals) = install_journal_persistence_fixture(&root);
    let installs = Arc::new(InstallStore::new());
    let install_id = "persistent-initial".to_string();
    let operation_id = install_operation_id(&install_id);
    assert!(
        installs
            .insert_or_existing_vanilla(install_id.clone(), "1.21.5".to_string())
            .await
            .1
    );
    backend.fail_attempts(2);
    let final_retry_gate = backend.gate_attempt(3);
    assert!(
        begin_install_journal_with_test_ownership(
            state.clone(),
            installs.clone(),
            journals.clone(),
            install_id.clone(),
            operation_id,
            "1.21.5".to_string(),
        )
        .await
        .is_err()
    );
    assert_eq!(
        timeout(
            Duration::from_millis(100),
            installs.wait_for_initialization(&install_id),
        )
        .await
        .expect("persistent reconciliation must not block duplicate response"),
        InstallInitializationStatus::Reconciling
    );
    timeout(Duration::from_millis(250), backend.wait_for_attempt(3))
        .await
        .expect("owned reconciliation must retry");
    assert!(!state.subscribe_integrity_idle().borrow().is_stably_idle());

    final_retry_gate.release();
    wait_for_install_removal(&installs, &install_id).await;
    wait_for_integrity_idle(&state).await;
    journals.close().await.expect("close journals");
    fs::remove_dir_all(root).expect("cleanup");
}

#[tokio::test]
async fn install_reconciliation_verifies_transition_after_candidate_is_cleared() {
    let root = temp_root("competing-journal-reconciliation");
    let (backend, journals) = install_journal_persistence_fixture(&root);
    let operation_id = install_operation_id("competing-reconciliation");
    backend.fail_next();
    let error = begin_install_operation_journal(&journals, &operation_id, "1.21.5")
        .await
        .expect_err("initial journal persistence fails");
    let expected = operation::planned_install_journal(&operation_id, "1.21.5");
    journals
        .retry()
        .await
        .expect("another reconciler commits candidate");
    let reconciliation =
        reconcile_install_journal_transition(&journals, &operation_id, error, |entry| {
            operation_journal_plan_is_visible(entry, &expected)
        })
        .await
        .expect("transition reconciler verifies cleared candidate");
    assert!(matches!(
        reconciliation,
        InstallJournalReconciliation::MutationCommitted
    ));
    assert_eq!(
        journals.get(&operation_id).expect("retried journal").status,
        OperationStatus::Planned
    );
    journals.close().await.expect("close journals");
    fs::remove_dir_all(root).expect("cleanup");
}

#[tokio::test]
async fn transient_content_initial_failure_reconciles_before_later_journal_mutation() {
    let root = temp_root("transient-content-initial-journal");
    let state = build_test_state(&root);
    let (backend, journals) = install_journal_persistence_fixture(&root);
    let producer = state
        .try_claim_producer()
        .expect("claim content journal reconciliation producer");
    let content_operation_id = install_operation_id("transient-content-initial");
    backend.fail_next();

    let reservation = begin_content_journal_with_owned_reconciliation(
        state.installs().clone(),
        journals.clone(),
        "transient-content-initial".to_string(),
        content_operation_id.clone(),
        "managed-instance".to_string(),
        &producer,
    )
    .await
    .expect("transient content journal failure reconciles");
    reservation.hand_off();

    assert!(!journals.has_retry_candidate());
    assert!(operation_journal_plan_is_visible(
        &journals
            .get(&content_operation_id)
            .expect("reconciled content journal"),
        &operation::planned_content_journal(&content_operation_id, "managed-instance"),
    ));

    let later_operation_id = install_operation_id("after-content-reconciliation");
    begin_install_operation_journal(&journals, &later_operation_id, "1.21.5")
        .await
        .expect("later journal mutation is not globally wedged");
    assert_eq!(
        journals
            .get(&later_operation_id)
            .expect("later journal")
            .status,
        OperationStatus::Planned
    );

    drop(producer);
    journals.close().await.expect("close journals");
    fs::remove_dir_all(root).expect("cleanup");
}

#[tokio::test]
async fn persistent_content_initial_failure_terminalizes_late_plan_without_an_orphan() {
    let root = temp_root("persistent-content-initial-journal");
    let state = build_test_state(&root);
    let (backend, journals) = install_journal_persistence_fixture(&root);
    let installs = Arc::new(InstallStore::new());
    let producer = state
        .try_claim_producer()
        .expect("claim content initialization producer");
    let install_id = "persistent-content-initial".to_string();
    let operation_id = install_operation_id(&install_id);
    backend.fail_attempts(64);

    assert!(
        begin_content_journal_with_owned_reconciliation(
            installs.clone(),
            journals.clone(),
            install_id.clone(),
            operation_id.clone(),
            "managed-instance".to_string(),
            &producer,
        )
        .await
        .is_err()
    );
    assert!(journals.has_retry_candidate());
    assert!(installs.snapshot(&install_id).await.is_none());

    backend.allow_writes();
    timeout(Duration::from_secs(2), async {
        loop {
            if journals.get(&operation_id).is_some_and(|entry| {
                entry.status == OperationStatus::Failed
                    && entry.failure_point.as_deref() == Some("content_initialization_cancelled")
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("late content plan is durably terminalized");

    assert!(!journals.has_retry_candidate());
    assert!(installs.snapshot(&install_id).await.is_none());
    drop(producer);
    journals.close().await.expect("close journals");
    fs::remove_dir_all(root).expect("cleanup");
}

#[tokio::test]
async fn transient_terminal_failure_retries_and_emits_exactly_once() {
    let root = temp_root("transient-terminal-journal");
    let (backend, journals) = install_journal_persistence_fixture(&root);
    let installs = Arc::new(InstallStore::new());
    let install_id = "transient-terminal";
    let operation_id = install_operation_id(install_id);
    begin_install_operation_journal(&journals, &operation_id, "1.21.5")
        .await
        .expect("initial journal");
    installs.insert(install_id.to_string()).await;
    let (_, mut receiver) = installs
        .subscribe_records(install_id)
        .await
        .expect("install subscription");
    let attempts_before = backend.attempts.load(Ordering::SeqCst);
    backend.fail_next();
    let mut last_phase = None;
    assert!(
        record_and_emit_install_progress(
            &installs,
            &journals,
            &operation_id,
            install_id,
            progress("error", true, Some("sanitized failure")),
            &mut last_phase,
        )
        .await
    );

    assert_eq!(backend.attempts.load(Ordering::SeqCst) - attempts_before, 2);
    assert_eq!(
        journals
            .get(&operation_id)
            .expect("terminal journal")
            .status,
        OperationStatus::Failed
    );
    let terminal = receiver.recv().await.expect("one terminal event");
    assert!(terminal.progress.done);
    assert!(matches!(
        receiver.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Empty)
    ));
    assert!(installs.snapshot(install_id).await.expect("snapshot").done);
    journals.close().await.expect("close journals");
    fs::remove_dir_all(root).expect("cleanup");
}

#[tokio::test]
async fn transient_interruption_failure_retries_before_one_terminal_handoff() {
    let root = temp_root("transient-interruption-journal");
    let (backend, journals) = install_journal_persistence_fixture(&root);
    let installs = Arc::new(InstallStore::new());
    let install_id = "transient-interruption";
    let operation_id = install_operation_id(install_id);
    begin_install_operation_journal(&journals, &operation_id, "1.21.5")
        .await
        .expect("initial journal");
    installs.insert(install_id.to_string()).await;
    let (_, mut receiver) = installs
        .subscribe_records(install_id)
        .await
        .expect("install subscription");
    let handoffs = Arc::new(AtomicUsize::new(0));
    let attempts_before = backend.attempts.load(Ordering::SeqCst);
    backend.fail_next();
    let interruption_operation_id = operation_id.clone();
    let worker = InstallStore::spawn_tracked_worker_with_interrupt_handler(
        installs.clone(),
        install_id.to_string(),
        interrupted_install_progress(),
        async {},
        {
            let journals = journals.clone();
            let handoffs = handoffs.clone();
            move |progress| async move {
                record_install_operation_interrupted(
                    &journals,
                    &interruption_operation_id,
                    &progress,
                )
                .await
                .expect("reconcile interrupted journal");
                handoffs.fetch_add(1, Ordering::SeqCst);
                true
            }
        },
    );
    timeout(Duration::from_secs(1), worker)
        .await
        .expect("interruption reconciliation must finish")
        .expect("tracked worker");

    assert_eq!(backend.attempts.load(Ordering::SeqCst) - attempts_before, 2);
    assert_eq!(handoffs.load(Ordering::SeqCst), 1);
    assert_eq!(
        journals
            .get(&operation_id)
            .expect("terminal journal")
            .status,
        OperationStatus::Failed
    );
    assert!(
        receiver
            .recv()
            .await
            .expect("one terminal event")
            .progress
            .done
    );
    assert!(matches!(
        receiver.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Empty)
    ));
    journals.close().await.expect("close journals");
    fs::remove_dir_all(root).expect("cleanup");
}

#[tokio::test]
async fn persistent_interruption_failure_keeps_tracked_owner_and_nonterminal_state() {
    let root = temp_root("persistent-interruption-journal");
    let (backend, journals) = install_journal_persistence_fixture(&root);
    let installs = Arc::new(InstallStore::new());
    let install_id = "persistent-interruption";
    let operation_id = install_operation_id(install_id);
    begin_install_operation_journal(&journals, &operation_id, "1.21.5")
        .await
        .expect("initial journal");
    installs.insert(install_id.to_string()).await;
    backend.fail_attempts(64);
    let worker = InstallStore::spawn_tracked_worker_with_interrupt_handler(
        installs.clone(),
        install_id.to_string(),
        interrupted_install_progress(),
        async {},
        {
            let journals = journals.clone();
            move |progress| async move {
                record_install_operation_interrupted(&journals, &operation_id, &progress)
                    .await
                    .expect("persistent reconciliation ends only at shutdown");
                true
            }
        },
    );

    timeout(Duration::from_millis(250), backend.wait_for_attempt(3))
        .await
        .expect("tracked interruption owner must keep retrying");
    assert!(!worker.is_finished());
    assert!(
        !installs
            .snapshot(install_id)
            .await
            .expect("active install")
            .done
    );
    assert_eq!(
        journals
            .get(&install_operation_id(install_id))
            .expect("nonterminal journal")
            .status,
        OperationStatus::Planned
    );
    assert!(journals.has_retry_candidate());
    drop(worker);
}

#[tokio::test]
async fn content_journal_uses_instance_command_and_exports_bounded_redacted_success_proof() {
    let journals = OperationJournalStore::new();
    let operation_id = install_operation_id("content-success");
    super::operation::begin_content_operation_journal(
        &journals,
        &operation_id,
        r"C:\Users\Alice\.minecraft\instances\secret",
    )
    .await
    .expect("create content journal");

    let mut download_facts = super::operation::ContentDownloadFactAccumulator::default();
    for _ in 0..500 {
        download_facts.record(ExecutionDownloadFact {
            kind: ExecutionDownloadFactKind::WrittenToTemp,
            target: "/Users/alice/.axial/mods/private.jar".to_string(),
            fields: vec![(
                "provider_payload".to_string(),
                "{\"token\":\"secret\"}".to_string(),
            )],
        });
        download_facts.record(ExecutionDownloadFact {
            kind: ExecutionDownloadFactKind::Promoted,
            target: r"C:\Users\Alice\.minecraft\mods\private.jar".to_string(),
            fields: Vec::new(),
        });
    }
    let journal_facts = download_facts.journal_facts();
    let mut last_phase = None;
    super::operation::record_content_operation_progress(
        &journals,
        &operation_id,
        &progress("planning", false, None),
        &[],
        &mut last_phase,
    )
    .await
    .expect("record planning");
    super::operation::record_content_operation_progress(
        &journals,
        &operation_id,
        &progress("done", true, None),
        &journal_facts,
        &mut last_phase,
    )
    .await
    .expect("record success");

    let entry = journals.get(&operation_id).expect("content journal");
    assert_eq!(entry.command, CommandKind::ModifyInstanceContent);
    assert_eq!(entry.status, OperationStatus::Succeeded);
    assert_eq!(entry.targets.len(), 1);
    assert_eq!(entry.targets[0].kind, TargetKind::Instance);
    assert_eq!(entry.targets[0].ownership, OwnershipClass::LauncherManaged);
    assert_eq!(entry.targets[0].id, "target");
    let terminal = entry.completed_steps.last().expect("terminal step");
    assert!(
        terminal
            .generated_facts
            .contains(&"execution_download_fact:written_to_temp:500".to_string())
    );
    assert!(
        terminal
            .generated_facts
            .contains(&"execution_download_fact:promoted:500".to_string())
    );
    assert!(terminal.generated_facts.len() <= crate::state::MAX_OPERATION_JOURNAL_STEP_FACTS);

    let proof = operation_journal_proof_record(&entry);
    let encoded = serde_json::to_string(&proof).expect("proof json");
    assert!(encoded.contains("ModifyInstanceContent"));
    assert_no_sensitive_fragments(&encoded);
}

#[tokio::test]
async fn content_journal_records_download_failure_guardian_outcome_and_proof() {
    let journals = OperationJournalStore::new();
    let failure_memory = GuardianFailureMemoryStore::new();
    let operation_id = install_operation_id("content-failure");
    super::operation::begin_content_operation_journal(&journals, &operation_id, "managed-instance")
        .await
        .expect("create content journal");
    let mut download_facts = super::operation::ContentDownloadFactAccumulator::default();
    download_facts.record(ExecutionDownloadFact {
        kind: ExecutionDownloadFactKind::MetadataInvalid,
        target: "/Users/alice/provider/private.jar".to_string(),
        fields: vec![
            (
                "url".to_string(),
                "https://example.invalid/?token=secret".to_string(),
            ),
            (
                "provider_payload".to_string(),
                "{\"token\":\"secret\"}".to_string(),
            ),
        ],
    });
    let journal_facts = download_facts.journal_facts();
    let facts = download_facts.facts();
    super::operation::record_content_failure_outcome(
        &journals,
        &failure_memory,
        &operation_id,
        &facts,
        None,
        OperationPhase::Downloading,
        "2026-07-16T10:00:00+00:00",
    )
    .await
    .expect("record content Guardian outcome");
    let mut last_phase = None;
    super::operation::record_content_operation_progress(
        &journals,
        &operation_id,
        &progress("error", true, Some("content failed")),
        &journal_facts,
        &mut last_phase,
    )
    .await
    .expect("record content failure");

    let entry = journals.get(&operation_id).expect("content journal");
    assert_eq!(entry.command, CommandKind::ModifyInstanceContent);
    assert_eq!(entry.status, OperationStatus::Failed);
    assert_eq!(entry.outcome, Some(OperationOutcome::Failed));
    assert_eq!(
        entry.guardian_diagnosis_ids,
        vec![DiagnosisId::InstallArtifactMetadataInvalid]
    );
    assert_eq!(
        entry.failure_point.as_deref(),
        Some("content_progress_error")
    );
    assert!(
        entry
            .completed_steps
            .last()
            .expect("terminal step")
            .generated_facts
            .contains(&"execution_download_fact:metadata_invalid:1".to_string())
    );
    assert!(
        install_guardian_outcome_summary_from_journal(&entry)
            .is_some_and(|outcome| outcome.decision() == "block")
    );
    let encoded =
        serde_json::to_string(&operation_journal_proof_record(&entry)).expect("proof json");
    assert_no_sensitive_fragments(&encoded);
}

#[tokio::test]
async fn content_journal_records_typed_metadata_failure_without_download_facts() {
    let journals = OperationJournalStore::new();
    let failure_memory = GuardianFailureMemoryStore::new();
    let operation_id = install_operation_id("content-provider-metadata-failure");
    super::operation::begin_content_operation_journal(&journals, &operation_id, "managed-instance")
        .await
        .expect("create content journal");
    let (evidence, phase) = content_execution_failure_evidence(
        &operation_id,
        crate::application::content::ContentExecutionFailureKind::MetadataInvalid,
    );

    super::operation::record_content_failure_outcome(
        &journals,
        &failure_memory,
        &operation_id,
        &[],
        Some(evidence),
        phase,
        "2026-07-16T10:00:00+00:00",
    )
    .await
    .expect("record content Guardian outcome");
    let mut last_phase = None;
    super::operation::record_content_operation_progress(
        &journals,
        &operation_id,
        &progress("error", true, Some("content failed")),
        &[],
        &mut last_phase,
    )
    .await
    .expect("record content failure");

    let entry = journals.get(&operation_id).expect("content journal");
    assert_eq!(
        entry.guardian_diagnosis_ids,
        vec![DiagnosisId::InstallArtifactMetadataInvalid]
    );
    assert!(
        install_guardian_outcome_summary_from_journal(&entry)
            .is_some_and(|outcome| outcome.decision() == "block")
    );
}

#[tokio::test]
async fn first_observable_content_terminal_already_contains_typed_guardian_outcome() {
    let journals = OperationJournalStore::new();
    let failure_memory = GuardianFailureMemoryStore::new();
    let installs = InstallStore::new();
    let install_id = "content-terminal-ordering";
    let operation_id = install_operation_id(install_id);
    super::operation::begin_content_operation_journal(&journals, &operation_id, "managed-instance")
        .await
        .expect("create content journal");
    installs.insert(install_id.to_string()).await;

    commit_and_emit_content_terminal_progress(
        &installs,
        &journals,
        &failure_memory,
        ContentTerminalProgress {
            operation_id: &operation_id,
            install_id,
            progress: progress("error", true, Some("content failed")),
            execution_facts: &[],
            journal_facts: &[],
            failure_kind: Some(
                crate::application::content::ContentExecutionFailureKind::MetadataInvalid,
            ),
        },
    )
    .await
    .expect("commit typed content terminal");

    assert!(
        installs
            .snapshot(install_id)
            .await
            .expect("content install")
            .done
    );
    let entry = journals.get(&operation_id).expect("content journal");
    assert_eq!(entry.status, OperationStatus::Failed);
    assert_eq!(
        entry.guardian_diagnosis_ids,
        vec![DiagnosisId::InstallArtifactMetadataInvalid]
    );
    assert!(
        install_guardian_outcome_summary_from_journal(&entry)
            .is_some_and(|outcome| outcome.decision() == "block")
    );
}

#[tokio::test]
async fn guardian_persistence_failure_cannot_publish_incomplete_content_terminal() {
    let root = temp_root("content-guardian-terminal-ordering");
    let (backend, journals) = install_journal_persistence_fixture(&root);
    let failure_memory = GuardianFailureMemoryStore::new();
    let installs = InstallStore::new();
    let install_id = "content-guardian-persistence";
    let operation_id = install_operation_id(install_id);
    super::operation::begin_content_operation_journal(&journals, &operation_id, "managed-instance")
        .await
        .expect("create content journal");
    installs.insert(install_id.to_string()).await;
    let terminal = progress("error", true, Some("content failed"));
    backend.fail_attempts(64);

    assert!(
        commit_and_emit_content_terminal_progress(
            &installs,
            &journals,
            &failure_memory,
            ContentTerminalProgress {
                operation_id: &operation_id,
                install_id,
                progress: terminal.clone(),
                execution_facts: &[],
                journal_facts: &[],
                failure_kind: Some(
                    crate::application::content::ContentExecutionFailureKind::MetadataInvalid,
                ),
            },
        )
        .await
        .is_err()
    );
    assert!(
        !installs
            .snapshot(install_id)
            .await
            .expect("content install remains active")
            .done
    );
    assert!(
        journals
            .get(&operation_id)
            .is_some_and(|entry| entry.status != OperationStatus::Failed)
    );
    assert!(journals.has_retry_candidate());

    backend.allow_writes();
    let settled = settle_content_worker_interruption(
        &journals,
        &failure_memory,
        &operation_id,
        content_interrupted_progress(false),
        &[],
        &[],
        Some((
            terminal,
            Some(crate::application::content::ContentExecutionFailureKind::MetadataInvalid),
        )),
    )
    .await
    .expect("interrupt owner settles a durable terminal");
    installs.emit(install_id, settled).await;

    assert!(
        installs
            .snapshot(install_id)
            .await
            .expect("content install")
            .done
    );
    let entry = journals.get(&operation_id).expect("content journal");
    assert_eq!(entry.status, OperationStatus::Failed);
    assert_eq!(
        entry.guardian_diagnosis_ids,
        vec![DiagnosisId::InstallArtifactMetadataInvalid]
    );
    assert!(
        install_guardian_outcome_summary_from_journal(&entry)
            .is_some_and(|outcome| outcome.decision() == "block")
    );
    assert!(!journals.has_retry_candidate());
    journals.close().await.expect("close journals");
    fs::remove_dir_all(root).expect("cleanup");
}

#[tokio::test]
async fn late_terminal_persistence_recovery_returns_original_content_terminal() {
    let root = temp_root("content-terminal-late-recovery");
    let (backend, journals) = install_journal_persistence_fixture(&root);
    let failure_memory = GuardianFailureMemoryStore::new();
    let installs = InstallStore::new();
    let install_id = "content-terminal-late-recovery";
    let operation_id = install_operation_id(install_id);
    super::operation::begin_content_operation_journal(&journals, &operation_id, "managed-instance")
        .await
        .expect("create content journal");
    installs.insert(install_id.to_string()).await;
    let terminal = progress("error", true, Some("content failed"));
    let (evidence, phase) = content_execution_failure_evidence(
        &operation_id,
        crate::application::content::ContentExecutionFailureKind::MetadataInvalid,
    );
    super::operation::record_content_failure_outcome(
        &journals,
        &failure_memory,
        &operation_id,
        &[],
        Some(evidence),
        phase,
        "2026-07-16T10:00:00+00:00",
    )
    .await
    .expect("record Guardian outcome before terminal");
    backend.fail_attempts(64);
    let mut last_phase = None;
    assert!(
        super::operation::record_content_operation_progress(
            &journals,
            &operation_id,
            &terminal,
            &[],
            &mut last_phase,
        )
        .await
        .is_err()
    );
    assert!(journals.has_retry_candidate());
    assert!(
        !installs
            .snapshot(install_id)
            .await
            .expect("content install remains active")
            .done
    );

    backend.allow_writes();
    let settled = settle_content_worker_interruption(
        &journals,
        &failure_memory,
        &operation_id,
        content_interrupted_progress(false),
        &[],
        &[],
        Some((
            terminal.clone(),
            Some(crate::application::content::ContentExecutionFailureKind::MetadataInvalid),
        )),
    )
    .await
    .expect("late terminal candidate remains authoritative");
    assert_eq!(
        settled.error.as_deref(),
        sanitize_install_progress(terminal).error.as_deref()
    );
    installs.emit(install_id, settled).await;

    let entry = journals.get(&operation_id).expect("content journal");
    assert_eq!(entry.status, OperationStatus::Failed);
    assert_eq!(
        entry.failure_point.as_deref(),
        Some("content_progress_error")
    );
    assert_eq!(
        entry.guardian_diagnosis_ids,
        vec![DiagnosisId::InstallArtifactMetadataInvalid]
    );
    assert!(
        install_guardian_outcome_summary_from_journal(&entry)
            .is_some_and(|outcome| outcome.decision() == "block")
    );
    assert!(
        installs
            .snapshot(install_id)
            .await
            .expect("content install")
            .done
    );
    assert!(!journals.has_retry_candidate());
    journals.close().await.expect("close journals");
    fs::remove_dir_all(root).expect("cleanup");
}

#[tokio::test]
async fn content_journal_records_interruption_without_crossing_install_command_identity() {
    let journals = OperationJournalStore::new();
    let operation_id = install_operation_id("content-interrupted");
    super::operation::begin_content_operation_journal(&journals, &operation_id, "managed-instance")
        .await
        .expect("create content journal");
    let install_collision =
        begin_install_operation_journal(&journals, &operation_id, "1.21.5").await;
    assert!(matches!(
        install_collision,
        Err(OperationJournalStoreError::AlreadyExists)
    ));

    super::operation::record_content_operation_interrupted(
        &journals,
        &operation_id,
        &content_interrupted_progress(false),
        &["execution_download_fact:promoted:2".to_string()],
        &[],
    )
    .await
    .expect("record content interruption");

    let entry = journals.get(&operation_id).expect("content journal");
    assert_eq!(entry.command, CommandKind::ModifyInstanceContent);
    assert_eq!(entry.status, OperationStatus::Failed);
    assert!(entry.guardian_diagnosis_ids.is_empty());
    assert!(install_guardian_outcome_summary_from_journal(&entry).is_none());
    assert_eq!(
        entry.failure_point.as_deref(),
        Some("content_worker_interrupted")
    );
    assert!(
        entry
            .completed_steps
            .last()
            .expect("terminal step")
            .generated_facts
            .contains(&"execution_download_fact:promoted:2".to_string())
    );
}

#[tokio::test]
async fn install_journal_records_progress_success_and_redacts_fields() {
    let journals = OperationJournalStore::new();
    let operation_id = install_operation_id(r"C:\Users\Alice\token-install");
    begin_install_operation_journal(
        &journals,
        &operation_id,
        r"C:\Users\Alice\.minecraft\versions\secret.jar",
    )
    .await
    .expect("record install journal");

    let mut last_phase = None;
    record_install_operation_progress(
        &journals,
        &operation_id,
        &progress("libraries", false, None),
        &mut last_phase,
    )
    .await
    .expect("record install journal");
    record_install_operation_progress(
        &journals,
        &operation_id,
        &progress("libraries", false, None),
        &mut last_phase,
    )
    .await
    .expect("record install journal");
    record_install_operation_progress(
        &journals,
        &operation_id,
        &progress("done", true, None),
        &mut last_phase,
    )
    .await
    .expect("record install journal");

    let entry = journals.get(&operation_id).expect("journal");
    assert_eq!(entry.status, OperationStatus::Succeeded);
    assert_eq!(entry.outcome, Some(OperationOutcome::Succeeded));
    assert_eq!(entry.completed_steps.len(), 2);
    assert!(entry.completed_steps.iter().any(|step| {
        step.result == OperationStepResult::Completed
            && step
                .generated_facts
                .contains(&"install_phase:libraries".to_string())
    }));
    let encoded = serde_json::to_string(&entry).expect("journal json");
    assert_no_sensitive_fragments(&encoded);
}

#[tokio::test]
async fn install_journal_records_failure_and_interruption() {
    let journals = OperationJournalStore::new();
    let failed_operation = install_operation_id("install-failed");
    begin_install_operation_journal(&journals, &failed_operation, "1.21.5")
        .await
        .expect("record install journal");
    let mut last_phase = None;
    record_install_operation_progress(
        &journals,
        &failed_operation,
        &progress(
            r"C:\Users\Alice\.minecraft -Xmx8192M --accessToken provider_payload",
            true,
            Some(
                "failed in /Users/alice/.axial with token secret provider_payload={\"token\":\"secret\"}",
            ),
        ),
        &mut last_phase,
    )
        .await
        .expect("record install journal");
    let failed = journals.get(&failed_operation).expect("failed journal");
    assert_eq!(failed.status, OperationStatus::Failed);
    assert_eq!(failed.outcome, Some(OperationOutcome::Failed));
    assert_no_sensitive_fragments(&serde_json::to_string(&failed).expect("journal json"));

    let interrupted_operation = install_operation_id("install-interrupted");
    begin_install_operation_journal(&journals, &interrupted_operation, "1.21.5")
        .await
        .expect("record install journal");
    record_install_operation_interrupted(
        &journals,
        &interrupted_operation,
        &progress("error", true, Some("worker interrupted")),
    )
    .await
    .expect("record install journal");
    let interrupted = journals
        .get(&interrupted_operation)
        .expect("interrupted journal");
    assert_eq!(interrupted.status, OperationStatus::Failed);
    assert_eq!(
        interrupted.failure_point.as_deref(),
        Some("install_worker_interrupted")
    );
}

#[tokio::test]
async fn install_journal_rejects_late_non_terminal_progress_after_terminal_state() {
    let journals = OperationJournalStore::new();
    let operation_id = install_operation_id("install-terminal-sticky");
    begin_install_operation_journal(&journals, &operation_id, "1.21.5")
        .await
        .expect("record install journal");
    let mut last_phase = None;
    record_install_operation_progress(
        &journals,
        &operation_id,
        &progress("error", true, Some("sanitized failure")),
        &mut last_phase,
    )
    .await
    .expect("record terminal install journal");
    record_install_operation_progress(
        &journals,
        &operation_id,
        &progress("libraries", false, None),
        &mut last_phase,
    )
    .await
    .expect_err("terminal install journal must reject late progress");

    let entry = journals.get(&operation_id).expect("journal");

    assert_eq!(entry.status, OperationStatus::Failed);
    assert_eq!(entry.outcome, Some(OperationOutcome::Failed));
    assert_eq!(entry.completed_steps.len(), 1);
    assert_eq!(entry.completed_steps[0].step_id, "install_progress_error");
}

#[tokio::test]
async fn install_journal_records_guardian_evidence_from_core_download_facts() {
    let journals = OperationJournalStore::new();
    let operation_id = install_operation_id("install-guardian-evidence");
    begin_install_operation_journal(&journals, &operation_id, "1.21.5")
        .await
        .expect("record install journal");
    let mut last_phase = None;
    record_install_operation_progress(
        &journals,
        &operation_id,
        &progress("error", true, Some("sanitized failure")),
        &mut last_phase,
    )
    .await
    .expect("record install journal");

    record_install_failure_outcome(
        &journals,
        &GuardianFailureMemoryStore::new(),
        &operation_id,
        &[
            ExecutionDownloadFact {
                kind: ExecutionDownloadFactKind::ChecksumMismatch,
                target: "minecraft_client_1.21.5".to_string(),
                fields: vec![
                    ("algorithm".to_string(), "sha1".to_string()),
                    (
                        "url".to_string(),
                        "https://example.invalid/artifact.jar?token=secret".to_string(),
                    ),
                ],
            },
            ExecutionDownloadFact {
                kind: ExecutionDownloadFactKind::Promoted,
                target: "minecraft_client_1.21.5".to_string(),
                fields: Vec::new(),
            },
        ],
        "2026-07-09T10:00:00+00:00",
    )
    .await;

    let entry = journals.get(&operation_id).expect("journal");
    assert_eq!(entry.status, OperationStatus::Failed);
    assert_eq!(
        entry.guardian_diagnosis_ids,
        vec![DiagnosisId::LauncherManagedArtifactCorrupt]
    );
    let terminal_step = entry.completed_steps.last().expect("terminal step");
    assert!(
        terminal_step
            .generated_facts
            .contains(&"guardian_fact:artifact_checksum_mismatch".to_string())
    );
    assert!(
        !terminal_step
            .generated_facts
            .iter()
            .any(|fact| fact.contains("Promoted"))
    );
    let guardian = install_guardian_outcome_summary_from_journal(&entry)
        .expect("persisted corruption outcome");
    let progress = vanilla_install_progress_view_model(&observed_install_failure_progress());
    let failure_view_model = install_failure_view_model(&progress, Some(&guardian))
        .expect("corruption failure view model");
    assert_eq!(failure_view_model.state_id, "failed_blocked");
    assert!(failure_view_model.retry_action.enabled);
    assert_eq!(failure_view_model.retry_action.disabled_reason, None);
    assert_no_sensitive_fragments(&serde_json::to_string(&entry).expect("journal json"));
}

#[tokio::test]
async fn install_journal_treats_temp_discard_as_non_terminal_evidence_only() {
    let journals = OperationJournalStore::new();
    let operation_id = install_operation_id("install-temp-discard-evidence");
    begin_install_operation_journal(&journals, &operation_id, "1.21.5")
        .await
        .expect("record install journal");
    let mut last_phase = None;
    record_install_operation_progress(
        &journals,
        &operation_id,
        &progress("error", true, Some("sanitized failure")),
        &mut last_phase,
    )
    .await
    .expect("record install journal");

    let facts = [ExecutionDownloadFact {
        kind: ExecutionDownloadFactKind::TempDiscarded,
        target: "minecraft_client_1.21.5".to_string(),
        fields: vec![(
            "path".to_string(),
            "/Users/alice/.axial/libraries/secret.jar".to_string(),
        )],
    }];
    record_install_failure_outcome(
        &journals,
        &GuardianFailureMemoryStore::new(),
        &operation_id,
        &facts,
        "2026-07-09T10:00:00+00:00",
    )
    .await;

    let entry = journals.get(&operation_id).expect("journal");
    assert!(install_guardian_outcome_summary_from_journal(&entry).is_none());
    assert!(entry.guardian_diagnosis_ids.is_empty());
    let terminal_step = entry.completed_steps.last().expect("terminal step");
    assert!(
        !terminal_step
            .generated_facts
            .iter()
            .any(|fact| fact.contains("guardian_fact:"))
    );
    assert_no_sensitive_fragments(&serde_json::to_string(&entry).expect("journal json"));
}

#[tokio::test]
async fn install_journal_records_guardian_download_failure_outcome_without_raw_details() {
    let journals = OperationJournalStore::new();
    let operation_id = install_operation_id("install-guardian-download-outcome");
    begin_install_operation_journal(&journals, &operation_id, "1.21.5")
        .await
        .expect("record install journal");
    let mut last_phase = None;
    record_install_operation_progress(
        &journals,
        &operation_id,
        &progress("error", true, Some("sanitized failure")),
        &mut last_phase,
    )
    .await
    .expect("record install journal");

    let facts = [ExecutionDownloadFact {
        kind: ExecutionDownloadFactKind::NetworkFailure,
        target: "minecraft_client_1.21.5".to_string(),
        fields: vec![
            (
                "url".to_string(),
                "https://example.invalid/client.jar?token=secret".to_string(),
            ),
            (
                "provider_payload".to_string(),
                "{\"token\":\"secret\"}".to_string(),
            ),
        ],
    }];
    record_install_failure_outcome(
        &journals,
        &GuardianFailureMemoryStore::new(),
        &operation_id,
        &facts,
        "2026-07-09T10:00:00+00:00",
    )
    .await;

    let entry = journals.get(&operation_id).expect("journal");
    let summary = install_guardian_outcome_summary_from_journal(&entry).expect("guardian outcome");
    assert_eq!(summary.diagnosis_id(), DiagnosisId::DownloadUnavailable);
    assert_eq!(summary.decision(), "retry");
    assert!(
        summary
            .label()
            .contains("install download failure as retryable")
    );
    assert!(
        summary
            .detail()
            .is_some_and(|detail| detail.contains("provider or network download"))
    );
    assert_no_sensitive_fragments(&serde_json::to_string(&entry).expect("journal json"));
    assert_no_sensitive_fragments(&serde_json::to_string(&summary).expect("summary json"));
}

fn persisted_download_outcome_entry() -> OperationJournalEntry {
    let operation_id = OperationId::new("install-persisted-guardian-outcome");
    let mut entry = OperationJournalEntry::new(
        JournalId::new("journal-install-persisted-guardian-outcome"),
        operation_id,
        CommandKind::InstallVersion,
        StabilizationSystem::Application,
        OwnershipClass::LauncherManaged,
        RollbackState::NotApplicable,
    );
    entry
        .guardian_diagnosis_ids
        .push(DiagnosisId::DownloadUnavailable);
    let mut step = OperationJournalStep::new("guardian-outcome", OperationPhase::Downloading);
    step.result = OperationStepResult::Failed;
    step.generated_facts = vec![
        "guardian_outcome_decision:retry".to_string(),
        "guardian_outcome_summary:Guardian classified the install download failure as retryable."
            .to_string(),
        "guardian_outcome_detail:The install stopped because a provider or network download was unavailable or interrupted."
            .to_string(),
    ];
    entry.completed_steps.push(step);
    entry
}

#[test]
fn install_journal_outcome_replay_does_not_borrow_facts_from_an_older_step() {
    let mut entry = persisted_download_outcome_entry();
    assert!(install_guardian_outcome_summary_from_journal(&entry).is_some());

    let mut partial = OperationJournalStep::new("partial-guardian-outcome", OperationPhase::Failed);
    partial.result = OperationStepResult::Failed;
    partial.generated_facts = vec!["guardian_outcome_decision:block".to_string()];
    entry.completed_steps.push(partial);

    assert!(install_guardian_outcome_summary_from_journal(&entry).is_none());
}

#[test]
fn install_journal_outcome_replay_rejects_duplicate_markers() {
    let entry = persisted_download_outcome_entry();
    for prefix in [
        "guardian_outcome_decision:",
        "guardian_outcome_summary:",
        "guardian_outcome_detail:",
    ] {
        let mut duplicated = entry.clone();
        let step = duplicated.completed_steps.last_mut().expect("outcome step");
        let fact = step
            .generated_facts
            .iter()
            .find(|fact| fact.starts_with(prefix))
            .expect("outcome marker")
            .clone();
        step.generated_facts.push(fact);

        assert!(
            install_guardian_outcome_summary_from_journal(&duplicated).is_none(),
            "duplicate marker was accepted: {prefix}"
        );
    }
}

#[tokio::test]
async fn provider_retry_memory_is_not_published_before_terminal_journal_reconciliation() {
    let root = temp_root("provider-memory-after-terminal-journal");
    let (backend, journals) = install_journal_persistence_fixture(&root);
    let failure_memory = Arc::new(GuardianFailureMemoryStore::new());
    let operation_id = install_operation_id("provider-memory-after-terminal-journal");
    begin_install_operation_journal(&journals, &operation_id, "1.21.5")
        .await
        .expect("record install journal");
    let facts = [download_fact(
        ExecutionDownloadFactKind::ProviderFailure,
        "minecraft_client_1.21.5",
    )];
    let evidence = install_failure_evidence_from_download_facts(&operation_id, &facts);
    record_install_operation_guardian_evidence(
        &journals,
        &operation_id,
        &evidence,
        OperationPhase::Downloading,
    )
    .await
    .expect("record Guardian evidence");

    let attempts_before = backend.attempts.load(Ordering::SeqCst);
    backend.fail_next();
    let retry_gate = backend.gate_attempt(attempts_before + 2);
    let terminal = tokio::spawn({
        let journals = journals.clone();
        let failure_memory = failure_memory.clone();
        let operation_id = operation_id.clone();
        let evidence = evidence.clone();
        async move {
            let assessment = assess_install_guardian_failure(
                Some(&failure_memory),
                &operation_id,
                &evidence,
                OperationPhase::Downloading,
                "2026-06-16T10:00:00+00:00",
            )
            .expect("provider failure assessment");
            record_install_guardian_terminal_outcome(
                &journals,
                Some(&failure_memory),
                &operation_id,
                &evidence,
                OperationPhase::Downloading,
                &assessment,
                "2026-06-16T10:00:00+00:00",
            )
            .await
        }
    });

    timeout(
        Duration::from_secs(1),
        backend.wait_for_attempt(attempts_before + 2),
    )
    .await
    .expect("terminal journal reconciliation retries");
    assert!(failure_memory.list().is_empty());
    assert!(
        journals
            .get(&operation_id)
            .and_then(|entry| install_guardian_outcome_summary_from_journal(&entry))
            .is_none()
    );

    retry_gate.release();
    timeout(Duration::from_secs(1), terminal)
        .await
        .expect("terminal reconciliation completes")
        .expect("terminal task")
        .expect("record terminal outcome");
    assert_eq!(failure_memory.list().len(), 1);
    let entry = journals.get(&operation_id).expect("reconciled journal");
    assert_eq!(
        install_guardian_outcome_summary_from_journal(&entry)
            .expect("reconciled Guardian outcome")
            .decision(),
        "retry"
    );

    journals.close().await.expect("close journals");
    fs::remove_dir_all(root).expect("cleanup");
}

#[tokio::test]
async fn vanilla_provider_failure_records_guardian_retry_then_suppression_without_raw_details() {
    let journals = OperationJournalStore::new();
    let failure_memory = GuardianFailureMemoryStore::new();
    let operation_id = install_operation_id("vanilla-provider-failure");
    begin_install_operation_journal(&journals, &operation_id, "1.21.5")
        .await
        .expect("record install journal");
    let mut last_phase = None;
    record_install_operation_progress(
        &journals,
        &operation_id,
        &progress("error", true, Some("sanitized failure")),
        &mut last_phase,
    )
    .await
    .expect("record install journal");
    let facts = [ExecutionDownloadFact {
        kind: ExecutionDownloadFactKind::ProviderFailure,
        target: "minecraft_client_1.21.5".to_string(),
        fields: vec![("status".to_string(), "503".to_string())],
    }];

    let (_, policy_evaluations) =
        crate::guardian::with_guardian_policy_evaluation_count(record_install_failure_outcome(
            &journals,
            &failure_memory,
            &operation_id,
            &facts,
            "2026-06-16T10:00:00+00:00",
        ))
        .await;
    assert_eq!(policy_evaluations, 1);

    let entry = journals.get(&operation_id).expect("journal");
    let summary = install_guardian_outcome_summary_from_journal(&entry).expect("guardian outcome");
    assert_eq!(summary.diagnosis_id(), DiagnosisId::DownloadUnavailable);
    assert_eq!(summary.decision(), "retry");
    assert!(
        summary
            .label()
            .contains("install download failure as retryable")
    );
    let retry_memory = failure_memory.list();
    assert_eq!(retry_memory.len(), 1);
    let retry_memory = retry_memory[0].clone();
    assert_eq!(retry_memory.occurrence_count, 1);
    assert_eq!(retry_memory.first_observed_at, "2026-06-16T10:00:00+00:00");
    assert_eq!(retry_memory.last_observed_at, "2026-06-16T10:00:00+00:00");
    assert_eq!(
        retry_memory.suppression_until.as_deref(),
        Some("2026-06-16T10:05:00+00:00")
    );
    assert_no_sensitive_fragments(&serde_json::to_string(&entry).expect("journal json"));

    let suppressed_operation_id = install_operation_id("vanilla-provider-failure-again");
    begin_install_operation_journal(&journals, &suppressed_operation_id, "1.21.5")
        .await
        .expect("record install journal");
    let mut suppressed_last_phase = None;
    record_install_operation_progress(
        &journals,
        &suppressed_operation_id,
        &progress("error", true, Some("sanitized failure")),
        &mut suppressed_last_phase,
    )
    .await
    .expect("record install journal");
    record_install_failure_outcome(
        &journals,
        &failure_memory,
        &suppressed_operation_id,
        &facts,
        "2026-06-16T10:01:00+00:00",
    )
    .await;

    let suppressed_entry = journals.get(&suppressed_operation_id).expect("journal");
    let suppressed =
        install_guardian_outcome_summary_from_journal(&suppressed_entry).expect("guardian outcome");
    assert_eq!(suppressed.diagnosis_id(), DiagnosisId::DownloadUnavailable);
    assert_eq!(suppressed.decision(), "block");
    assert!(
        suppressed
            .label()
            .contains("paused further install retries after repeated provider failure")
    );
    assert!(
        suppressed
            .guidance()
            .iter()
            .any(|guidance| guidance.contains("Wait a few minutes"))
    );
    assert_eq!(failure_memory.list(), vec![retry_memory]);
    assert_no_sensitive_fragments(&serde_json::to_string(&suppressed_entry).expect("journal json"));
    assert_no_sensitive_fragments(&serde_json::to_string(&suppressed).expect("summary json"));

    let boundary_operation_id = install_operation_id("vanilla-provider-failure-at-boundary");
    begin_install_operation_journal(&journals, &boundary_operation_id, "1.21.5")
        .await
        .expect("record boundary install journal");
    record_install_failure_outcome(
        &journals,
        &failure_memory,
        &boundary_operation_id,
        &facts,
        "2026-06-16T10:05:00+00:00",
    )
    .await;

    let boundary_entry = journals
        .get(&boundary_operation_id)
        .expect("boundary journal");
    let boundary = install_guardian_outcome_summary_from_journal(&boundary_entry)
        .expect("boundary Guardian outcome");
    assert_eq!(boundary.decision(), "retry");
    let renewed_memory = failure_memory.list();
    assert_eq!(renewed_memory.len(), 1);
    assert_eq!(renewed_memory[0].occurrence_count, 2);
    assert_eq!(
        renewed_memory[0].first_observed_at,
        "2026-06-16T10:00:00+00:00"
    );
    assert_eq!(
        renewed_memory[0].last_observed_at,
        "2026-06-16T10:05:00+00:00"
    );
    assert_eq!(
        renewed_memory[0].suppression_until.as_deref(),
        Some("2026-06-16T10:10:00+00:00")
    );
}

#[tokio::test]
async fn loader_provider_failure_records_guardian_retry_then_suppression_without_raw_details() {
    let journals = OperationJournalStore::new();
    let failure_memory = GuardianFailureMemoryStore::new();
    let operation_id = install_operation_id("loader-provider-failure");
    begin_install_operation_journal(&journals, &operation_id, "fabric-loader")
        .await
        .expect("record install journal");
    let mut last_phase = None;
    record_install_operation_progress(
        &journals,
        &operation_id,
        &progress("error", true, Some("sanitized failure")),
        &mut last_phase,
    )
    .await
    .expect("record install journal");
    let active_failure = || {
        let LoaderInstallError::Active(failure) =
            LoaderInstallError::from(LoaderError::ProviderUnavailable {
                kind: LoaderProviderFailureKind::HttpServer,
                status: Some(503),
            })
        else {
            panic!("provider failure must cross the active worker boundary")
        };
        failure
    };
    let error = active_failure();

    record_loader_install_operation_guardian_failure_outcome(
        &journals,
        &failure_memory,
        &operation_id,
        "loader_fabric_build_1_21_5",
        &error,
        "2026-06-16T10:00:00+00:00",
    )
    .await
    .expect("record loader failure outcome");

    let entry = journals.get(&operation_id).expect("journal");
    let summary = install_guardian_outcome_summary_from_journal(&entry).expect("guardian outcome");
    assert_eq!(summary.diagnosis_id(), DiagnosisId::DownloadUnavailable);
    assert_eq!(summary.decision(), "retry");
    assert!(
        summary
            .label()
            .contains("install download failure as retryable")
    );
    let retry_memory = failure_memory.list();
    assert_eq!(retry_memory.len(), 1);
    let retry_memory = retry_memory[0].clone();
    assert_no_sensitive_fragments(&serde_json::to_string(&entry).expect("journal json"));

    let suppressed_operation_id = install_operation_id("loader-provider-failure-again");
    begin_install_operation_journal(&journals, &suppressed_operation_id, "fabric-loader")
        .await
        .expect("record install journal");
    let mut suppressed_last_phase = None;
    record_install_operation_progress(
        &journals,
        &suppressed_operation_id,
        &progress("error", true, Some("sanitized failure")),
        &mut suppressed_last_phase,
    )
    .await
    .expect("record install journal");
    record_loader_install_operation_guardian_failure_outcome(
        &journals,
        &failure_memory,
        &suppressed_operation_id,
        "loader_fabric_build_1_21_5",
        &active_failure(),
        "2026-06-16T10:01:00+00:00",
    )
    .await
    .expect("record suppressed loader failure outcome");

    let suppressed_entry = journals.get(&suppressed_operation_id).expect("journal");
    let suppressed =
        install_guardian_outcome_summary_from_journal(&suppressed_entry).expect("guardian outcome");
    assert_eq!(suppressed.diagnosis_id(), DiagnosisId::DownloadUnavailable);
    assert_eq!(suppressed.decision(), "block");
    assert!(
        suppressed
            .label()
            .contains("paused further install retries after repeated provider failure")
    );
    assert!(
        suppressed
            .guidance()
            .iter()
            .any(|guidance| guidance.contains("Wait a few minutes"))
    );
    assert_eq!(failure_memory.list(), vec![retry_memory]);
    assert_no_sensitive_fragments(&serde_json::to_string(&suppressed_entry).expect("journal json"));
    assert_no_sensitive_fragments(&serde_json::to_string(&suppressed).expect("summary json"));
}

#[tokio::test]
async fn delegated_base_provider_fact_uses_download_pipeline_without_dependency_fallback() {
    let journals = OperationJournalStore::new();
    let failure_memory = GuardianFailureMemoryStore::new();
    let operation_id = install_operation_id("loader-base-provider-failure");
    begin_install_operation_journal(&journals, &operation_id, "fabric-loader")
        .await
        .expect("record install journal");
    let error = LoaderInstallError::from(LoaderError::BaseInstallFailed {
        error: Box::new(DownloadError::ResolveManifest(
            "provider unavailable".to_string(),
        )),
        facts: vec![download_fact(
            ExecutionDownloadFactKind::ProviderFailure,
            "minecraft_version_manifest",
        )],
    });

    dispatch_loader_install_failure(
        &journals,
        &failure_memory,
        &operation_id,
        "loader_fabric_build_1_21_5",
        "1.21.5",
        error,
        "2026-06-16T10:00:00+00:00",
    )
    .await;

    let entry = journals.get(&operation_id).expect("journal");
    let summary = install_guardian_outcome_summary_from_journal(&entry).expect("guardian outcome");
    assert_eq!(summary.diagnosis_id(), DiagnosisId::DownloadUnavailable);
    assert!(
        !entry
            .guardian_diagnosis_ids
            .contains(&DiagnosisId::InstallDependencyFailed)
    );
}

#[tokio::test]
async fn empty_base_install_payload_uses_only_dependency_fallback() {
    let journals = OperationJournalStore::new();
    let failure_memory = GuardianFailureMemoryStore::new();
    let operation_id = install_operation_id("loader-base-dependency-failure");
    begin_install_operation_journal(&journals, &operation_id, "fabric-loader")
        .await
        .expect("record install journal");
    let error = LoaderInstallError::from(LoaderError::BaseInstallFailed {
        error: Box::new(DownloadError::ResolveManifest(
            "private manifest".to_string(),
        )),
        facts: Vec::new(),
    });
    dispatch_loader_install_failure(
        &journals,
        &failure_memory,
        &operation_id,
        "loader_fabric_build_1_21_5",
        "1.21.5",
        error,
        "2026-06-16T10:00:00+00:00",
    )
    .await;

    let entry = journals.get(&operation_id).expect("journal");
    let summary = install_guardian_outcome_summary_from_journal(&entry).expect("guardian outcome");
    assert_eq!(summary.diagnosis_id(), DiagnosisId::InstallDependencyFailed);
    assert_eq!(summary.decision(), "block");
    assert!(
        summary.label().contains("required base install failed"),
        "{summary:?}"
    );
    assert!(
        summary
            .detail()
            .is_some_and(|detail| detail.contains("base Minecraft install failed")),
        "{summary:?}"
    );
    assert!(
        summary
            .guidance()
            .iter()
            .any(|guidance| guidance.contains("Retry the base version install")),
        "{summary:?}"
    );
    assert_no_sensitive_fragments(&serde_json::to_string(&entry).expect("journal json"));
    assert_no_sensitive_fragments(&serde_json::to_string(&summary).expect("summary json"));
}

fn assert_no_public_raw_fragments(message: &str) {
    for fragment in [
        "/home/zero",
        "/tmp/axial",
        "C:\\Users\\zero",
        "AppData\\Roaming",
        "https://",
        "piston-meta.mojang.com",
        "loader.example.invalid",
        "cdn.example.invalid",
        "request failed",
        "parse version json",
        "expected value",
        "line 1 column",
        "prepare java runtime",
        "mod-loader.jar",
        "/Users/alice",
        "C:\\Users\\Alice",
        "token secret",
        "provider_payload",
        "account_id",
        "account-secret",
        "username",
        "SecretPlayer",
        "raw-secret",
        "java.exe",
        "-Xmx8192M",
    ] {
        assert!(
            !message.contains(fragment),
            "message exposed raw fragment {fragment:?}: {message}"
        );
    }
}

fn build_test_state(root: &Path) -> AppState {
    let paths = test_app_paths(root);
    let config = Arc::new(ConfigStore::load_from(paths.clone()).expect("load config"));
    let instances = Arc::new(
        InstanceStore::from_snapshot(paths.clone(), InstanceRegistrySnapshot::default())
            .expect("load instances"),
    );
    AppState::new(AppStateInit {
        app_name: "Axial".to_string(),
        version: "test".to_string(),
        config,
        instances,
        installs: Arc::new(InstallStore::new()),
        sessions: Arc::new(SessionStore::new()),
        performance: Arc::new(
            PerformanceManager::load_for_startup(&paths.config_dir).expect("performance manager"),
        ),
        startup_warnings: Vec::new(),
        frontend_dir: root.join("frontend"),
    })
}

async fn begin_install_journal_with_test_ownership(
    state: AppState,
    store: Arc<InstallStore>,
    journals: Arc<OperationJournalStore>,
    install_id: String,
    operation_id: OperationId,
    version_id: String,
) -> Result<InstallInitializationReservation, ()> {
    let producer = state
        .try_claim_producer()
        .expect("claim test install reconciliation producer");
    let foreground = state
        .register_integrity_foreground()
        .expect("register test install foreground")
        .wait_for_settlement()
        .await;
    begin_install_journal_with_owned_reconciliation(
        store,
        journals,
        install_id,
        operation_id,
        version_id,
        &producer,
        foreground,
    )
    .await
}

async fn wait_for_integrity_idle(state: &AppState) {
    let mut idle = state.subscribe_integrity_idle();
    timeout(Duration::from_secs(1), async {
        loop {
            if idle.borrow_and_update().is_stably_idle() {
                return;
            }
            idle.changed()
                .await
                .expect("integrity activity remains open");
        }
    })
    .await
    .expect("install foreground should settle");
}

struct InstallJournalBackend {
    attempts: AtomicUsize,
    failures: AtomicUsize,
    started: Notify,
    gate: Mutex<Option<(usize, Arc<InstallJournalWriteGate>)>>,
}

struct InstallJournalWriteGate {
    released: Mutex<bool>,
    changed: Condvar,
}

struct InstallJournalWriteGateHandle(Arc<InstallJournalWriteGate>);

impl InstallJournalBackend {
    fn new() -> Self {
        Self {
            attempts: AtomicUsize::new(0),
            failures: AtomicUsize::new(0),
            started: Notify::new(),
            gate: Mutex::new(None),
        }
    }

    fn fail_next(&self) {
        self.fail_attempts(1);
    }

    fn fail_attempts(&self, attempts: usize) {
        self.failures.fetch_add(attempts, Ordering::SeqCst);
    }

    fn allow_writes(&self) {
        self.failures.store(0, Ordering::SeqCst);
    }

    fn gate_attempt(&self, attempt: usize) -> InstallJournalWriteGateHandle {
        let gate = Arc::new(InstallJournalWriteGate {
            released: Mutex::new(false),
            changed: Condvar::new(),
        });
        *self.gate.lock().expect("journal gate lock") = Some((attempt, gate.clone()));
        InstallJournalWriteGateHandle(gate)
    }

    async fn wait_for_attempt(&self, expected: usize) {
        loop {
            let started = self.started.notified();
            if self.attempts.load(Ordering::SeqCst) >= expected {
                return;
            }
            started.await;
        }
    }
}

impl InstallJournalWriteGate {
    fn release(&self) {
        *self.released.lock().expect("journal write gate lock") = true;
        self.changed.notify_all();
    }

    fn wait(&self) {
        let mut released = self.released.lock().expect("journal write gate lock");
        while !*released {
            released = self.changed.wait(released).expect("journal gate wait");
        }
    }
}

impl InstallJournalWriteGateHandle {
    fn release(&self) {
        self.0.release();
    }
}

impl Drop for InstallJournalWriteGateHandle {
    fn drop(&mut self) {
        self.0.release();
    }
}

impl AtomicWriteBackend for InstallJournalBackend {
    fn write(
        &self,
        target: &TargetDescriptor,
        destination: &Path,
        contents: &[u8],
    ) -> io::Result<()> {
        let attempt = self.attempts.fetch_add(1, Ordering::SeqCst) + 1;
        self.started.notify_one();
        let gate = {
            let mut gate = self.gate.lock().expect("journal gate lock");
            if gate
                .as_ref()
                .is_some_and(|(gated_attempt, _)| *gated_attempt == attempt)
            {
                gate.take().map(|(_, gate)| gate)
            } else {
                None
            }
        };
        if let Some(gate) = gate {
            gate.wait();
        }
        if self
            .failures
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |failures| {
                (failures > 0).then(|| failures - 1)
            })
            .is_ok()
        {
            return Err(io::Error::other("injected install journal failure"));
        }
        write_file_atomically(FileWriteRequest::new(target.clone(), destination, contents))
            .map(|_| ())
            .map_err(io::Error::from)
    }
}

fn install_journal_persistence_fixture(
    root: &Path,
) -> (Arc<InstallJournalBackend>, Arc<OperationJournalStore>) {
    let backend = Arc::new(InstallJournalBackend::new());
    let coordinator = PersistenceCoordinator::for_test(
        backend.clone(),
        Duration::from_millis(5),
        Duration::from_millis(20),
    );
    let journals = OperationJournalStore::try_load_from_paths_with_coordinator(
        &test_app_paths(root),
        coordinator,
    )
    .expect("load journal persistence fixture");
    (backend, Arc::new(journals))
}

async fn wait_for_install_removal(installs: &InstallStore, install_id: &str) {
    timeout(Duration::from_secs(1), async {
        while installs.snapshot(install_id).await.is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("install reservation removal");
}

fn configure_library_dir(state: &AppState, library_dir: &Path) {
    fs::create_dir_all(library_dir).expect("library dir");
    let mut config = state.config().current();
    config.library_dir = library_dir.to_string_lossy().to_string();
    state
        .config()
        .replace_for_test(config.clone())
        .expect("config update");
    state.set_library_dir_for_test(config.library_dir);
}

fn test_app_paths(root: &Path) -> AppPaths {
    let config_dir = root.join("config");
    AppPaths {
        config_file: config_dir.join("config.json"),
        instances_file: config_dir.join("instances.json"),
        instances_dir: root.join("instances"),
        music_dir: root.join("music"),
        library_dir: root.join("library"),
        config_dir,
    }
}

fn base_progress(phase: &str) -> DownloadProgress {
    DownloadProgress {
        phase: phase.to_string(),
        current: 1,
        total: 2,
        file: Some("base game".to_string()),
        error: None,
        done: false,
        bytes_done: None,
        bytes_total: None,
    }
}

fn done_progress() -> DownloadProgress {
    DownloadProgress {
        phase: "done".to_string(),
        current: 1,
        total: 1,
        file: None,
        error: None,
        done: true,
        bytes_done: None,
        bytes_total: None,
    }
}

fn failed_progress() -> DownloadProgress {
    DownloadProgress {
        phase: "error".to_string(),
        current: 0,
        total: 0,
        file: None,
        error: Some("failed".to_string()),
        done: true,
        bytes_done: None,
        bytes_total: None,
    }
}

fn assert_no_sensitive_fragments(encoded: &str) {
    for fragment in [
        "/Users/",
        r"C:\",
        "Alice",
        ".minecraft",
        "secret.jar",
        "https://",
        "-Xmx",
        "--accessToken",
        "provider_payload",
        "token",
        "secret",
    ] {
        assert!(
            !encoded.contains(fragment),
            "sensitive fragment survived: {fragment}"
        );
    }
}

fn progress(phase: &str, done: bool, error: Option<&str>) -> DownloadProgress {
    DownloadProgress {
        phase: phase.to_string(),
        current: 1,
        total: 2,
        file: Some("/Users/alice/.axial/libraries/secret.jar".to_string()),
        error: error.map(str::to_string),
        done,
        bytes_done: None,
        bytes_total: None,
    }
}

fn download_fact(kind: ExecutionDownloadFactKind, target: &str) -> ExecutionDownloadFact {
    ExecutionDownloadFact {
        kind,
        target: target.to_string(),
        fields: vec![("algorithm".to_string(), "sha1".to_string())],
    }
}

async fn wait_for_queue_empty(state: &AppState) {
    for _ in 0..40 {
        let snapshot = state.installs().queue_snapshot().await;
        if snapshot.active.is_none() && snapshot.pending.is_empty() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    panic!(
        "queue did not settle to empty: {:?}",
        state.installs().queue_snapshot().await
    );
}

fn temp_root(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "axial-api-install-application-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or_default()
    ));
    fs::create_dir_all(&path).expect("create temp root");
    path
}

fn launcher_managed_download_temp_path(destination: &Path) -> PathBuf {
    let mut name = destination
        .file_name()
        .expect("launcher managed artifact filename")
        .to_os_string();
    name.push(".axial-tmp");
    destination.with_file_name(name)
}
