use super::*;
use crate::application::InstallVersionCommand;
use crate::execution::file::{FileWriteRequest, write_file_atomically};
use crate::execution::persistence::{AtomicWriteBackend, PersistenceCoordinator};
use crate::guardian::{
    DiagnosisId, GuardianActionKind, GuardianArtifactRepairOutcome, GuardianArtifactRepairStatus,
};
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
use axial_minecraft::download::{
    ExecutionDownloadFact, ExecutionDownloadFactKind, ExpectedIntegrity,
    SelectedDownloadArtifactDescriptor, SelectedDownloadArtifactKind,
    download_file_with_client_report,
};
use axial_minecraft::{
    DownloadError, DownloadProgress, LoaderComponentId, LoaderError, LoaderInstallFailureKind,
    LoaderProviderFailureKind,
};
use axial_performance::PerformanceManager;
use axum::{body::to_bytes, response::IntoResponse};
use serde_json::json;
use sha1::{Digest, Sha1};
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;
use std::{fs, sync::mpsc};
use tokio::sync::{Notify, mpsc as tokio_mpsc};
use tokio::time::timeout;

#[test]
fn install_staging_builds_command_operation_and_payload() {
    let operation_id = install_operation_id("install-1");
    let staging = stage_install_version_command(
        InstallVersionCommand {
            version_id: "1.21.5".to_string(),
            manifest_url: None,
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
fn effective_install_fields_trims_version_id_and_manifest_url() {
    let payload = InstallVersionStartRequest {
        version_id: " 1.21.5 ".to_string(),
        manifest_url: " https://example.invalid/manifest.json ".to_string(),
    };

    assert_eq!(
        effective_install_fields(&payload),
        (
            "1.21.5".to_string(),
            "https://example.invalid/manifest.json".to_string()
        )
    );
}

#[test]
fn effective_install_fields_preserves_explicit_manifest_url() {
    let normal = InstallVersionStartRequest {
        version_id: "1.21.5".to_string(),
        manifest_url: String::new(),
    };
    let explicit = InstallVersionStartRequest {
        version_id: "1.21.5".to_string(),
        manifest_url: "https://example.invalid/manifest.json".to_string(),
    };

    assert_ne!(
        effective_install_fields(&normal),
        effective_install_fields(&explicit)
    );
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
        .insert_or_existing_active(
            "existing-install".to_string(),
            "1.21.5".to_string(),
            String::new(),
        )
        .await;
    let operation_id = install_operation_id("existing-install");
    begin_install_operation_journal(state.journals(), &operation_id, "1.21.5")
        .await
        .expect("create existing install journal");
    assert!(state.installs().mark_initialized("existing-install").await);

    let response = start_install_version(
        &state,
        InstallVersionStartRequest {
            version_id: "1.21.5".to_string(),
            manifest_url: String::new(),
        },
    )
    .await
    .expect("existing active install should be returned");

    assert_eq!(response.install_id, "existing-install");
    assert_eq!(response.operation_id, operation_id);
    assert!(state.journals().get(&operation_id).is_some());

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
            InstallQueueSpec::vanilla("1.21.5".to_string(), String::new()),
            InstallQueuePlacement::Back,
        )
        .await;
    state
        .installs()
        .enqueue_queued_install(
            "queue-loader".to_string(),
            InstallQueueSpec::loader(
                LoaderComponentId::Fabric,
                "fabric:1.21.5:0.16.10".to_string(),
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

    let error = enqueue_install(
        &state,
        InstallQueueRequest {
            kind: "vanilla".to_string(),
            version_id: "1.21.5".to_string(),
            manifest_url: String::new(),
            component_id: String::new(),
            build_id: String::new(),
        },
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
            InstallQueueSpec::vanilla("1.21.5".to_string(), String::new()),
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
async fn queue_monitor_advances_only_after_terminal_progress_and_discards_start_failure() {
    let root = temp_root("install-queue-monitor-terminal");
    let state = build_test_state(&root);
    state
        .installs()
        .insert_or_existing_active(
            "active-install".to_string(),
            "1.21.5".to_string(),
            String::new(),
        )
        .await;
    state
        .installs()
        .enqueue_queued_install(
            "queue-active".to_string(),
            InstallQueueSpec::vanilla("1.21.5".to_string(), String::new()),
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
            InstallQueueSpec::vanilla("1.21.6".to_string(), String::new()),
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
        .insert_or_existing_active(
            "draining-active-install".to_string(),
            "1.21.5".to_string(),
            String::new(),
        )
        .await;
    state
        .installs()
        .enqueue_queued_install(
            "draining-active-queue".to_string(),
            InstallQueueSpec::vanilla("1.21.5".to_string(), String::new()),
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
            InstallQueueSpec::vanilla("1.21.6".to_string(), String::new()),
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
async fn install_status_exposes_backend_authored_guardian_repair_summary() {
    let root = temp_root("install-status-guardian-repair");
    let state = build_test_state(&root);
    let install_id = "repair-status-install";
    let operation_id = install_operation_id(install_id);
    state.installs().insert(install_id.to_string()).await;
    state
        .installs()
        .emit(install_id, observed_install_failure_progress())
        .await;
    begin_install_operation_journal(state.journals(), &operation_id, "1.21.5")
        .await
        .expect("begin install journal");
    let mut last_phase = None;
    record_install_operation_progress(
        state.journals(),
        &operation_id,
        &observed_install_failure_progress(),
        &mut last_phase,
    )
    .await
    .expect("record install progress");
    record_install_operation_guardian_repair_outcome(
        state.journals(),
        &operation_id,
        &GuardianArtifactRepairOutcome {
            operation_id: OperationId::new(
                "guardian-artifact-repair:123e4567-e89b-12d3-a456-426614174000",
            ),
            diagnosis_id: DiagnosisId::LauncherManagedArtifactCorrupt,
            action: GuardianActionKind::Repair,
            status: GuardianArtifactRepairStatus::Repaired,
            facts: vec!["https://example.invalid/client.jar?token=secret".to_string()],
            summary: "guardian_artifact_repaired".to_string(),
        },
    )
    .await
    .expect("record Guardian repair outcome");

    let response = install_status(&state, install_id)
        .await
        .expect("install status");

    assert_eq!(response.install_id, install_id);
    assert_eq!(response.operation_id, operation_id);
    assert!(response.done);
    assert_eq!(response.progress.len(), 1);
    let repair = response.guardian_repair.as_ref().expect("guardian repair");
    assert_eq!(repair.status, "repaired");
    assert_eq!(
        repair.repair_operation_id.as_str(),
        "guardian-artifact-repair:123e4567-e89b-12d3-a456-426614174000"
    );
    assert!(repair.label.contains("repaired"));
    assert_no_public_raw_fragments(&serde_json::to_string(&repair).expect("repair json"));
    let failure_view_model = response
        .failure_view_model
        .as_ref()
        .expect("failure view model");
    assert_eq!(failure_view_model.state_id, "failed_repair_applied");
    assert_eq!(failure_view_model.title, "Install failed");
    assert_eq!(failure_view_model.retry_action.action, "retry");
    assert!(failure_view_model.retry_action.enabled);
    assert_eq!(failure_view_model.repair_action.action, "repair");
    assert!(!failure_view_model.repair_action.enabled);
    assert!(
        failure_view_model
            .repair_action
            .label
            .contains("repair applied")
    );
    assert_no_public_raw_fragments(
        &serde_json::to_string(&failure_view_model).expect("failure view model json"),
    );

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
    assert!(response.guardian_repair.is_none());
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
async fn restart_interrupted_install_retry_discards_stale_temp_without_promoting_partial_bytes() {
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

    let fresh_body = b"fresh launcher managed artifact".to_vec();
    let server = TestByteServer::start(fresh_body.clone());
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .expect("download client");
    let expected = ExpectedIntegrity::from_mojang(fresh_body.len() as i64, &sha1_hex(&fresh_body));

    let report = download_file_with_client_report(&client, &server.url, &destination, &expected)
        .await
        .expect("retry download should clean stale temp and promote fresh bytes");

    assert!(
        report
            .facts
            .iter()
            .any(|fact| fact.kind == ExecutionDownloadFactKind::TempDiscarded)
    );
    assert!(
        report
            .facts
            .iter()
            .any(|fact| fact.kind == ExecutionDownloadFactKind::WrittenToTemp)
    );
    assert!(
        report
            .facts
            .iter()
            .any(|fact| fact.kind == ExecutionDownloadFactKind::Promoted)
    );
    assert_eq!(
        fs::read(&destination).expect("promoted artifact"),
        fresh_body
    );
    assert!(!temp_path.exists());
    assert_eq!(server.request_count(), 1);

    let status_json = serde_json::to_string(&response).expect("status json");
    let report_json = serde_json::to_string(&report).expect("report json");
    assert_no_public_raw_fragments(&status_json);
    assert_no_public_raw_fragments(&report_json);
    assert!(!report_json.contains(root.to_string_lossy().as_ref()));
    assert!(!report_json.contains("partial bytes"));
    server.stop();

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
async fn install_status_reconstructs_restart_loaded_journal_and_guardian_repair() {
    let root = temp_root("install-status-restart-journal-replay");
    let install_id = "restart-repair-status-install";
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
            &observed_install_failure_progress(),
            &mut last_phase,
        )
        .await
        .expect("record install journal");
        record_install_operation_guardian_repair_outcome(
            state.journals(),
            &operation_id,
            &GuardianArtifactRepairOutcome {
                operation_id: OperationId::new(
                    "guardian-artifact-repair:123e4567-e89b-12d3-a456-426614174001",
                ),
                diagnosis_id: DiagnosisId::LauncherManagedArtifactCorrupt,
                action: GuardianActionKind::Repair,
                status: GuardianArtifactRepairStatus::Suppressed,
                facts: vec!["https://example.invalid/client.jar?token=secret".to_string()],
                summary: "guardian_artifact_repair_suppressed".to_string(),
            },
        )
        .await
        .expect("record install journal");
    }

    let reloaded = build_test_state(&root);
    let response = install_status(&reloaded, install_id)
        .await
        .expect("restart-loaded journal status");

    assert_eq!(response.install_id, install_id);
    assert_eq!(response.operation_id, operation_id);
    assert!(response.done);
    assert_eq!(response.progress.len(), 1);
    assert_eq!(
        response.progress[0].error.as_deref(),
        Some(INSTALL_FAILURE_MESSAGE)
    );
    let repair = response.guardian_repair.as_ref().expect("guardian repair");
    assert_eq!(repair.status, "suppressed");
    assert_eq!(
        repair.repair_operation_id.as_str(),
        "guardian-artifact-repair:123e4567-e89b-12d3-a456-426614174001"
    );
    assert!(repair.label.contains("paused automatic install repair"));
    let proof = response.proof.as_ref().expect("operation proof");
    assert_eq!(proof.operation_id, operation_id);
    assert_eq!(proof.command, CommandKind::InstallVersion);
    assert_eq!(proof.status, OperationStatus::Failed);
    assert_eq!(proof.outcome, Some(OperationOutcome::Failed));
    assert_eq!(
        proof.failure_point.as_deref(),
        Some("install_progress_error")
    );
    assert!(
        proof
            .guardian_diagnosis_ids
            .contains(&DiagnosisId::LauncherManagedArtifactCorrupt)
    );
    assert!(proof.fields.iter().any(|field| {
        field.key == "generated_fact" && field.value == "guardian_repair_status:suppressed"
    }));
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
    record_install_operation_guardian_evidence(
        state.journals(),
        &operation_id,
        &[ExecutionDownloadFact {
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
        }],
    )
    .await
    .expect("record install journal");
    record_install_operation_guardian_failure_outcome(
        state.journals(),
        &operation_id,
        &[ExecutionDownloadFact {
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
        }],
    )
    .await
    .expect("record install journal");

    let response = install_status(&state, install_id)
        .await
        .expect("install status");

    assert!(response.done);
    assert!(response.guardian_repair.is_none());
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
    assert!(!failure_view_model.repair_action.enabled);
    assert!(
        failure_view_model
            .repair_action
            .disabled_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("No automatic repair"))
    );
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
    let facts = [
        ExecutionDownloadFact {
            kind: ExecutionDownloadFactKind::ArtifactMissing,
            target: "minecraft_runtime_manifest".to_string(),
            fields: Vec::new(),
        },
        ExecutionDownloadFact {
            kind: ExecutionDownloadFactKind::MetadataMissing,
            target: "minecraft_runtime_manifest".to_string(),
            fields: Vec::new(),
        },
    ];
    record_install_operation_guardian_evidence(state.journals(), &operation_id, &facts)
        .await
        .expect("record install journal");
    record_install_operation_guardian_failure_outcome_for_error_with_memory(
        state.journals(),
        &failure_memory,
        &operation_id,
        &error,
        &facts,
        "2026-07-09T10:00:00+00:00",
    )
    .await
    .expect("record install journal");

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
    let facts = [
        ExecutionDownloadFact {
            kind: ExecutionDownloadFactKind::ArtifactMissing,
            target: "minecraft_runtime_manifest".to_string(),
            fields: Vec::new(),
        },
        ExecutionDownloadFact {
            kind: ExecutionDownloadFactKind::MetadataMissing,
            target: "minecraft_runtime_manifest".to_string(),
            fields: Vec::new(),
        },
    ];
    record_install_operation_guardian_evidence(state.journals(), &operation_id, &facts)
        .await
        .expect("record install journal");
    record_install_operation_guardian_failure_outcome_for_error_with_memory(
        state.journals(),
        &failure_memory,
        &operation_id,
        &error,
        &facts,
        "2026-07-09T10:00:00+00:00",
    )
    .await
    .expect("record install journal");

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
            kind: ExecutionDownloadFactKind::ArtifactMissing,
            target: "minecraft_client_1.21.5".to_string(),
            fields: Vec::new(),
        },
        ExecutionDownloadFact {
            kind: ExecutionDownloadFactKind::MetadataMissing,
            target: "minecraft_asset_object_checksumless".to_string(),
            fields: Vec::new(),
        },
    ];
    record_install_operation_guardian_evidence(state.journals(), &operation_id, &facts)
        .await
        .expect("record install journal");
    record_install_operation_guardian_failure_outcome_for_error_with_memory(
        state.journals(),
        &failure_memory,
        &operation_id,
        &error,
        &facts,
        "2026-07-09T10:05:00+00:00",
    )
    .await
    .expect("record install journal");

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
            ExecutionDownloadFactKind::ArtifactMissing,
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

    record_install_operation_guardian_failure_outcome_for_error_with_memory(
        &journals,
        &failure_memory,
        &operation_id,
        &error,
        &facts,
        "2026-07-09T10:05:00+00:00",
    )
    .await
    .expect("record install journal");

    let entries = failure_memory.list();
    let entry = entries
        .iter()
        .find(|entry| entry.diagnosis_id.as_str() == "download_unavailable")
        .expect("provider failure memory");
    assert_eq!(entry.target.id, terminal_target);
    assert_ne!(entry.target.id, "minecraft_download");
}

#[tokio::test]
async fn error_based_install_repair_skips_stale_artifact_facts_for_runtime_failure() {
    let root = temp_root("install-runtime-stale-artifact-repair");
    let destination = root.join("client.jar");
    fs::write(&destination, b"already repaired client").expect("existing artifact");
    let replacement = b"unexpected guardian repair".to_vec();
    let server = TestByteServer::start(replacement.clone());
    let journals = OperationJournalStore::new();
    let failure_memory = GuardianFailureMemoryStore::new();
    let operation_id = install_operation_id("runtime-stale-artifact-repair");
    begin_install_operation_journal(&journals, &operation_id, "1.11.2")
        .await
        .expect("record install journal");
    let target_id = "minecraft_client_runtime_stale";
    let facts = vec![download_fact(
        ExecutionDownloadFactKind::ChecksumMismatch,
        target_id,
    )];
    let descriptors = vec![selected_descriptor(
        SelectedDownloadArtifactKind::ClientJar,
        target_id,
        &destination,
        &server.url,
        &replacement,
    )];
    let error = DownloadError::RuntimeRosettaRequired {
        component: "jre-legacy".to_string(),
    };

    let outcome = record_install_failure_outcome_and_repair_for_error(
        &journals,
        &failure_memory,
        &operation_id,
        &error,
        &facts,
        &descriptors,
        "2026-07-09T10:05:00+00:00",
    )
    .await;

    assert!(outcome.is_none());
    assert_eq!(
        fs::read(&destination).expect("artifact should be untouched"),
        b"already repaired client"
    );
    assert_eq!(server.request_count(), 0);
    let entry = journals.get(&operation_id).expect("operation journal");
    assert!(
        install_guardian_repair_summary_from_journal(&entry).is_none(),
        "runtime failure should not record stale artifact repair state"
    );

    server.stop();
    let _ = fs::remove_dir_all(root);
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
            DiagnosisId::TempFileLeftover,
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
        (
            "ownership-refused-status-install",
            ExecutionDownloadFactKind::OwnershipRefused,
            DiagnosisId::ArtifactOwnershipUnsafe,
            "block",
            "protect user-owned or unknown files",
            "ownership was unsafe",
            "launcher-managed library location",
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
        record_install_operation_guardian_evidence(state.journals(), &operation_id, &facts)
            .await
            .expect("record install journal");
        record_install_operation_guardian_failure_outcome(state.journals(), &operation_id, &facts)
            .await
            .expect("record install journal");

        let response = install_status(&state, install_id)
            .await
            .expect("install status");

        assert!(response.done);
        assert!(response.guardian_repair.is_none());
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
fn loader_error_response_keeps_status_and_failure_kind_without_raw_details() {
    let (status, Json(body)) = loader_error_response(LoaderError::CatalogUnavailable {
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

    let (status, Json(body)) = loader_error_response(LoaderError::ProviderUnavailable {
        kind: LoaderProviderFailureKind::HttpRateLimited,
        status: Some(429),
    });

    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(body["failure_kind"], json!("provider_rate_limited"));
    assert_eq!(
        body["error"],
        json!("Loader provider is unavailable. Check your connection and try again.")
    );
    assert_no_public_raw_fragments(body["error"].as_str().expect("error is a string"));

    let (status, Json(body)) = loader_error_response(LoaderError::ProviderUnavailable {
        kind: LoaderProviderFailureKind::HttpNotFound,
        status: Some(404),
    });

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["failure_kind"], json!("provider_http_failure"));
    assert_eq!(
        body["error"],
        json!("Loader provider is unavailable. Check your connection and try again.")
    );

    let (status, Json(body)) = loader_error_response(LoaderError::CatalogUnavailable {
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

    let (status, Json(body)) = loader_error_response(LoaderError::ProviderDataInvalid {
        kind: LoaderProviderFailureKind::ResponseTooLarge,
        status: None,
    });

    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(body["failure_kind"], json!("provider_response_too_large"));
    assert_eq!(
        body["error"],
        json!("Loader provider returned data Axial could not trust. Try again later.")
    );
    assert_no_public_raw_fragments(body["error"].as_str().expect("error is a string"));

    let (status, Json(body)) = loader_error_response(LoaderError::Io(std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        "permission denied: /home/zero/.axial/libraries/example.jar",
    )));

    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body["failure_kind"], json!("install_execution_failed"));
    assert_eq!(
        body["error"],
        json!("Could not write loader files. Check app data permissions and try again.")
    );
    assert_no_public_raw_fragments(body["error"].as_str().expect("error is a string"));

    let parse_error = serde_json::from_str::<serde_json::Value>("{\"loader\":")
        .expect_err("invalid json should fail");
    let (status, Json(body)) = loader_error_response(LoaderError::Parse(parse_error));

    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body["failure_kind"], json!("parse_failed"));
    assert_eq!(
        body["error"],
        json!("Loader install data could not be read. Try again.")
    );
    assert_no_public_raw_fragments(body["error"].as_str().expect("error is a string"));

    let (status, Json(body)) = loader_error_response(LoaderError::ArtifactMissing(
        "missing https://cdn.example.invalid/path/mod-loader.jar in /tmp/axial".to_string(),
    ));

    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(body["failure_kind"], json!("artifact_missing"));
    assert_eq!(
        body["error"],
        json!("Loader artifact is unavailable. Try another build or component.")
    );
    assert_no_public_raw_fragments(body["error"].as_str().expect("error is a string"));

    let (status, Json(body)) = loader_error_response(LoaderError::BaseInstallFailed {
        error: Box::new(DownloadError::ResolveManifest(
            "https://example.invalid/manifest.json?token=secret".to_string(),
        )),
        facts: vec![ExecutionDownloadFact {
            kind: ExecutionDownloadFactKind::ProviderFailure,
            target: "minecraft_client_1.21.5".to_string(),
            fields: vec![(
                "url".to_string(),
                "https://example.invalid/client.jar?token=secret".to_string(),
            )],
        }],
        descriptors: Vec::new(),
    });

    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(body["failure_kind"], json!("base_install_failed"));
    assert_eq!(
        body["error"],
        json!("Base game install failed. Retry the install from Downloads.")
    );
    assert_no_public_raw_fragments(body["error"].as_str().expect("error is a string"));
}

#[test]
fn loader_error_response_preserves_safe_explicit_messages() {
    let (status, Json(body)) = loader_error_response(LoaderError::InvalidMinecraftVersion);

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["failure_kind"], json!("invalid_minecraft_version"));
    assert_eq!(body["error"], json!("Invalid Minecraft version."));
}

#[test]
fn loader_failure_disposition_preserves_operation_boundaries() {
    assert!(matches!(
        loader_install_failure_disposition(LoaderInstallFailureKind::CatalogUnavailable),
        LoaderGuardianFailureDisposition::PreOperation
    ));
    assert!(matches!(
        loader_install_failure_disposition(LoaderInstallFailureKind::BaseInstallFailed),
        LoaderGuardianFailureDisposition::DelegatedBaseInstall
    ));
    assert!(matches!(
        loader_install_failure_disposition(LoaderInstallFailureKind::ArtifactDownloadFailed),
        LoaderGuardianFailureDisposition::DelegatedArtifactDownload
    ));
    assert!(matches!(
        loader_install_failure_disposition(LoaderInstallFailureKind::ProcessorFailed),
        LoaderGuardianFailureDisposition::Evidence {
            kind: crate::guardian::GuardianInstallArtifactFailureKind::ProcessorFailed,
            ownership: OwnershipClass::LauncherManaged,
            phase: OperationPhase::Installing,
        }
    ));
    assert!(matches!(
        loader_install_failure_disposition(LoaderInstallFailureKind::InstallExecutionFailed),
        LoaderGuardianFailureDisposition::Evidence {
            kind: crate::guardian::GuardianInstallArtifactFailureKind::ExecutionFailed,
            phase: OperationPhase::Installing,
            ..
        }
    ));
}

#[test]
fn loader_error_progress_hides_raw_details_and_keeps_terminal_shape() {
    let progress = loader_error_progress(&LoaderError::ArtifactMissing(
        "missing https://cdn.example.invalid/path/mod-loader.jar in /tmp/axial".to_string(),
    ));

    assert_eq!(progress.phase, "error");
    assert_eq!(progress.current, 0);
    assert_eq!(progress.total, 0);
    assert_eq!(progress.file, None);
    assert_eq!(
        progress.error.as_deref(),
        Some("Loader artifact is unavailable. Try another build or component.")
    );
    assert!(progress.done);
    assert_no_public_raw_fragments(progress.error.as_deref().expect("error is present"));
}

#[test]
fn loader_base_install_rosetta_failure_keeps_specific_terminal_message() {
    let progress = loader_error_progress(&LoaderError::BaseInstallFailed {
        error: Box::new(DownloadError::RuntimeRosettaRequired {
            component: "jre-legacy".to_string(),
        }),
        facts: Vec::new(),
        descriptors: Vec::new(),
    });

    let message = progress.error.clone().expect("error is present");
    assert!(message.contains("Rosetta 2"));
    assert!(message.contains("softwareupdate --install-rosetta --agree-to-license"));

    let sanitized = sanitize_install_progress(progress);
    assert_eq!(sanitized.error.as_deref(), Some(message.as_str()));
}

#[test]
fn loader_base_install_generic_failure_keeps_loader_message() {
    let progress = loader_error_progress(&LoaderError::BaseInstallFailed {
        error: Box::new(DownloadError::ResolveManifest(
            "https://example.invalid/manifest.json".to_string(),
        )),
        facts: Vec::new(),
        descriptors: Vec::new(),
    });

    assert_eq!(
        progress.error.as_deref(),
        Some("Base game install failed. Retry the install from Downloads.")
    );
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

#[test]
fn loader_install_key_fields_are_scoped_to_component_and_build() {
    let fabric_key = loader_install_key_fields(
        LoaderComponentId::Fabric,
        "fabric:1.21.5:0.16.14",
        "fabric-loader-0.16.14-1.21.5",
    );
    let quilt_key = loader_install_key_fields(
        LoaderComponentId::Quilt,
        "quilt:1.21.5:0.16.14",
        "fabric-loader-0.16.14-1.21.5",
    );
    let next_build_key = loader_install_key_fields(
        LoaderComponentId::Fabric,
        "fabric:1.21.5:0.16.15",
        "fabric-loader-0.16.14-1.21.5",
    );

    assert_ne!(fabric_key, quilt_key);
    assert_ne!(fabric_key, next_build_key);
    assert!(fabric_key.0.starts_with("loader:"));
    assert!(fabric_key.1.starts_with("loader:"));
}

#[test]
fn loader_install_key_fields_trim_resolved_fields() {
    assert_eq!(
        loader_install_key_fields(
            LoaderComponentId::Forge,
            " forge:1.20.1:47.4.0 ",
            " 1.20.1-forge-47.4.0 ",
        ),
        (
            "loader:net.minecraftforge:1.20.1-forge-47.4.0".to_string(),
            "loader:net.minecraftforge:forge:1.20.1:47.4.0".to_string()
        )
    );
}

#[tokio::test]
async fn wait_for_active_vanilla_base_install_waits_and_forwards_progress() {
    let store = Arc::new(InstallStore::new());
    store
        .insert_or_existing_active(
            "vanilla-install".to_string(),
            "1.21.5".to_string(),
            String::new(),
        )
        .await;
    let (progress_tx, mut progress_rx) = tokio_mpsc::unbounded_channel();

    let wait_store = store.clone();
    let waiter = tokio::spawn(async move {
        wait_for_active_vanilla_base_install(wait_store.as_ref(), "1.21.5", &progress_tx).await
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
async fn wait_for_active_vanilla_base_install_does_not_block_done_removed_or_failed_sessions() {
    let store = InstallStore::new();
    let (progress_tx, _progress_rx) = tokio_mpsc::unbounded_channel();

    store
        .insert_or_existing_active(
            "done-install".to_string(),
            "1.21.5".to_string(),
            String::new(),
        )
        .await;
    store.emit("done-install", done_progress()).await;
    timeout(
        Duration::from_secs(1),
        wait_for_active_vanilla_base_install(&store, "1.21.5", &progress_tx),
    )
    .await
    .expect("done session should not block")
    .expect("done session should not fail loader wait");

    store
        .insert_or_existing_active(
            "failed-install".to_string(),
            "1.21.5".to_string(),
            String::new(),
        )
        .await;
    store.emit("failed-install", failed_progress()).await;
    timeout(
        Duration::from_secs(1),
        wait_for_active_vanilla_base_install(&store, "1.21.5", &progress_tx),
    )
    .await
    .expect("failed session should not block")
    .expect("already failed session should not fail loader wait");

    store
        .insert_or_existing_active(
            "removed-install".to_string(),
            "1.21.5".to_string(),
            String::new(),
        )
        .await;
    store.remove("removed-install").await;
    timeout(
        Duration::from_secs(1),
        wait_for_active_vanilla_base_install(&store, "1.21.5", &progress_tx),
    )
    .await
    .expect("removed session should not block")
    .expect("removed session should not fail loader wait");
}

#[tokio::test]
async fn wait_for_active_vanilla_base_install_fails_loader_when_base_fails_while_waiting() {
    let store = Arc::new(InstallStore::new());
    store
        .insert_or_existing_active(
            "vanilla-install".to_string(),
            "1.21.5".to_string(),
            String::new(),
        )
        .await;
    let (progress_tx, mut progress_rx) = tokio_mpsc::unbounded_channel();

    let wait_store = store.clone();
    let waiter = tokio::spawn(async move {
        wait_for_active_vanilla_base_install(wait_store.as_ref(), "1.21.5", &progress_tx).await
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
async fn cancelled_initial_commit_releases_reservation_for_duplicate_retry() {
    let root = temp_root("cancelled-initial-journal");
    let (backend, journals) = install_journal_persistence_fixture(&root);
    let installs = Arc::new(InstallStore::new());
    let install_id = "cancelled-initial".to_string();
    let operation_id = install_operation_id(&install_id);
    assert!(
        installs
            .insert_or_existing_active(install_id.clone(), "1.21.5".to_string(), String::new(),)
            .await
            .1
    );

    let gate = backend.gate_attempt(1);
    let initialization = tokio::spawn(begin_install_journal_with_detached_reconciliation(
        installs.clone(),
        journals.clone(),
        install_id.clone(),
        operation_id.clone(),
        "1.21.5".to_string(),
    ));
    backend.wait_for_attempt(1).await;

    let duplicate_store = installs.clone();
    let duplicate_id = install_id.clone();
    let duplicate = tokio::spawn(async move {
        let (existing, inserted) = duplicate_store
            .insert_or_existing_active(
                "duplicate-waiter".to_string(),
                "1.21.5".to_string(),
                String::new(),
            )
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
            .insert_or_existing_active(
                "duplicate-retry".to_string(),
                "1.21.5".to_string(),
                String::new(),
            )
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
    installs.remove(&retry_id).await;
    journals.close().await.expect("close journals");
    fs::remove_dir_all(root).expect("cleanup");
}

#[tokio::test]
async fn cancelled_initialized_result_before_worker_handoff_releases_reservation() {
    let journals = Arc::new(OperationJournalStore::new());
    let installs = Arc::new(InstallStore::new());
    let install_id = "cancelled-handoff".to_string();
    let operation_id = install_operation_id(&install_id);
    let assertion_operation_id = operation_id.clone();
    assert!(
        installs
            .insert_or_existing_active(install_id.clone(), "1.21.5".to_string(), String::new(),)
            .await
            .1
    );
    let (received_tx, received_rx) = tokio::sync::oneshot::channel();
    let (_release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
    let initialization = tokio::spawn({
        let installs = installs.clone();
        let journals = journals.clone();
        async move {
            let reservation = begin_install_journal_with_detached_reconciliation(
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
            reservation.hand_off();
        }
    });
    received_rx.await.expect("reservation received");
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
}

#[tokio::test]
async fn transient_initial_failure_reconciles_then_allows_retry() {
    let root = temp_root("transient-initial-journal");
    let (backend, journals) = install_journal_persistence_fixture(&root);
    let installs = Arc::new(InstallStore::new());
    let install_id = "transient-initial".to_string();
    let operation_id = install_operation_id(&install_id);
    assert!(
        installs
            .insert_or_existing_active(install_id.clone(), "1.21.5".to_string(), String::new(),)
            .await
            .1
    );
    backend.fail_next();
    let retry_gate = backend.gate_attempt(2);

    assert!(
        begin_install_journal_with_detached_reconciliation(
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
        .insert_or_existing_active(
            "bounded-duplicate".to_string(),
            "1.21.5".to_string(),
            String::new(),
        )
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
    assert_eq!(
        journals.get(&operation_id).expect("retried journal").status,
        OperationStatus::Failed
    );
    assert!(
        installs
            .insert_or_existing_active(
                "post-reconciliation-retry".to_string(),
                "1.21.5".to_string(),
                String::new(),
            )
            .await
            .1
    );
    installs.remove("post-reconciliation-retry").await;
    journals.close().await.expect("close journals");
    fs::remove_dir_all(root).expect("cleanup");
}

#[tokio::test]
async fn persistent_initial_failure_keeps_live_owner_and_bounds_duplicates() {
    let root = temp_root("persistent-initial-journal");
    let (backend, journals) = install_journal_persistence_fixture(&root);
    let installs = Arc::new(InstallStore::new());
    let install_id = "persistent-initial".to_string();
    let operation_id = install_operation_id(&install_id);
    assert!(
        installs
            .insert_or_existing_active(install_id.clone(), "1.21.5".to_string(), String::new(),)
            .await
            .1
    );
    backend.fail_attempts(64);
    assert!(
        begin_install_journal_with_detached_reconciliation(
            installs.clone(),
            journals,
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
    timeout(Duration::from_millis(250), backend.wait_for_attempt(2))
        .await
        .expect("detached reconciliation owner must retry");
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

    record_install_operation_guardian_evidence(
        &journals,
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
    )
    .await
    .expect("record install journal");

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
    record_install_operation_guardian_evidence(&journals, &operation_id, &facts)
        .await
        .expect("record install journal");
    record_install_operation_guardian_failure_outcome(&journals, &operation_id, &facts)
        .await
        .expect("record install journal");

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
    record_install_operation_guardian_evidence(&journals, &operation_id, &facts)
        .await
        .expect("record install journal");
    record_install_operation_guardian_failure_outcome(&journals, &operation_id, &facts)
        .await
        .expect("record install journal");

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

    record_install_operation_guardian_failure_outcome_with_memory(
        &journals,
        &failure_memory,
        &operation_id,
        &facts,
        "2026-06-16T10:00:00+00:00",
    )
    .await
    .expect("record install journal");

    let entry = journals.get(&operation_id).expect("journal");
    let summary = install_guardian_outcome_summary_from_journal(&entry).expect("guardian outcome");
    assert_eq!(summary.diagnosis_id(), DiagnosisId::DownloadUnavailable);
    assert_eq!(summary.decision(), "retry");
    assert!(
        summary
            .label()
            .contains("install download failure as retryable")
    );
    assert_eq!(failure_memory.list().len(), 1);
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
    record_install_operation_guardian_failure_outcome_with_memory(
        &journals,
        &failure_memory,
        &suppressed_operation_id,
        &facts,
        "2026-06-16T10:01:00+00:00",
    )
    .await
    .expect("record install journal");

    let suppressed_entry = journals.get(&suppressed_operation_id).expect("journal");
    let suppressed =
        install_guardian_outcome_summary_from_journal(&suppressed_entry).expect("guardian outcome");
    assert_eq!(suppressed.diagnosis_id(), DiagnosisId::DownloadUnavailable);
    assert_eq!(suppressed.decision(), "block");
    assert!(
        suppressed
            .label()
            .contains("paused install retry after repeated provider failure")
    );
    assert!(
        suppressed
            .guidance()
            .iter()
            .any(|guidance| guidance.contains("Wait a few minutes"))
    );
    assert_eq!(failure_memory.list().len(), 1);
    assert_no_sensitive_fragments(&serde_json::to_string(&suppressed_entry).expect("journal json"));
    assert_no_sensitive_fragments(&serde_json::to_string(&suppressed).expect("summary json"));
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
    let error = LoaderError::ProviderUnavailable {
        kind: LoaderProviderFailureKind::HttpServer,
        status: Some(503),
    };

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
    assert_eq!(failure_memory.list().len(), 1);
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
        &error,
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
            .contains("paused install retry after repeated provider failure")
    );
    assert!(
        suppressed
            .guidance()
            .iter()
            .any(|guidance| guidance.contains("Wait a few minutes"))
    );
    assert_eq!(failure_memory.list().len(), 1);
    assert_no_sensitive_fragments(&serde_json::to_string(&suppressed_entry).expect("journal json"));
    assert_no_sensitive_fragments(&serde_json::to_string(&suppressed).expect("summary json"));
}

#[tokio::test]
async fn loader_base_install_dependency_failure_records_guardian_block_without_raw_details() {
    let journals = OperationJournalStore::new();
    let operation_id = install_operation_id("loader-base-dependency-failure");
    begin_install_operation_journal(&journals, &operation_id, "fabric-loader")
        .await
        .expect("record install journal");
    let mut last_phase = None;
    record_install_operation_progress(
        &journals,
        &operation_id,
        &base_install_failed_progress(),
        &mut last_phase,
    )
    .await
    .expect("record install journal");

    record_loader_base_install_dependency_guardian_failure_outcome(
        &journals,
        &operation_id,
        "loader_fabric_build_1_21_5",
        "1.21.5",
    )
    .await
    .expect("record loader dependency failure outcome");

    let entry = journals.get(&operation_id).expect("journal");
    let summary = install_guardian_outcome_summary_from_journal(&entry).expect("guardian outcome");
    assert_eq!(entry.status, OperationStatus::Failed);
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

#[tokio::test]
async fn install_journal_records_guardian_repair_summary_without_raw_details() {
    let journals = OperationJournalStore::new();
    let operation_id = install_operation_id("install-guardian-repair-summary");
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

    record_install_operation_guardian_repair_outcome(
        &journals,
        &operation_id,
        &GuardianArtifactRepairOutcome {
            operation_id: OperationId::new(
                "guardian-artifact-repair:123e4567-e89b-12d3-a456-426614174000",
            ),
            diagnosis_id: DiagnosisId::LauncherManagedArtifactCorrupt,
            action: GuardianActionKind::Repair,
            status: GuardianArtifactRepairStatus::Suppressed,
            facts: vec!["https://example.invalid/artifact.jar?token=secret".to_string()],
            summary: "guardian_artifact_repair_suppressed".to_string(),
        },
    )
    .await
    .expect("record install journal");

    let entry = journals.get(&operation_id).expect("journal");
    let summary = install_guardian_repair_summary_from_journal(&entry).expect("repair summary");
    assert_eq!(summary.status, "suppressed");
    assert_eq!(
        summary.repair_operation_id.as_str(),
        "guardian-artifact-repair:123e4567-e89b-12d3-a456-426614174000"
    );
    assert_eq!(
        summary.diagnosis_id,
        DiagnosisId::LauncherManagedArtifactCorrupt
    );
    assert!(summary.label.contains("paused automatic install repair"));
    assert_no_sensitive_fragments(&serde_json::to_string(&entry).expect("journal json"));
    assert_no_sensitive_fragments(&serde_json::to_string(&summary).expect("summary json"));
}

#[tokio::test]
async fn install_guardian_repair_repairs_matching_checksum_failure() {
    let root = temp_root("guardian-install-repair");
    let destination = root.join("client.jar");
    fs::write(&destination, b"corrupt client").expect("corrupt artifact");
    let replacement = b"fresh client".to_vec();
    let server = TestByteServer::start(replacement.clone());
    let journals = OperationJournalStore::new();
    let failure_memory = GuardianFailureMemoryStore::new();
    let operation_id = install_operation_id("install-repair");
    let target_id = "minecraft_client_1.21.5";
    let facts = vec![download_fact(
        ExecutionDownloadFactKind::ChecksumMismatch,
        target_id,
    )];
    let descriptors = vec![selected_descriptor(
        SelectedDownloadArtifactKind::ClientJar,
        target_id,
        &destination,
        &server.url,
        &replacement,
    )];

    let outcome = repair_install_artifact_corruption_with_guardian(
        &journals,
        &failure_memory,
        &reqwest::Client::new(),
        &operation_id,
        &facts,
        &descriptors,
        "2026-06-15T10:00:00+00:00",
    )
    .await
    .expect("repair journal")
    .expect("repair outcome");

    assert_eq!(outcome.status, GuardianArtifactRepairStatus::Repaired);
    assert_eq!(
        fs::read(&destination).expect("repaired artifact"),
        replacement
    );
    assert!(server.request_count() >= 1);
    let repair_journal = journals
        .get(&outcome.operation_id)
        .expect("repair journal should be recorded");
    assert_eq!(repair_journal.status, OperationStatus::Succeeded);
    assert_eq!(repair_journal.outcome, Some(OperationOutcome::Succeeded));
    assert_eq!(failure_memory.list().len(), 1);
    assert_no_sensitive_fragments(&serde_json::to_string(&repair_journal).expect("journal json"));

    server.stop();
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn install_worker_self_heals_corrupt_launcher_artifact_without_guardian_repair() {
    let root = temp_root("execution-install-self-heal");
    let state = build_test_state(&root);
    let library_dir = root.join("library");
    configure_library_dir(&state, &library_dir);
    write_bundled_runtime_fixture(&library_dir, "java-runtime-delta");

    let version_id = "repair-rerun";
    let replacement = b"fresh client from execution self-heal".to_vec();
    let client_server = TestByteServer::start(replacement.clone());
    let version_body = serde_json::json!({
        "id": version_id,
        "type": "release",
        "mainClass": "net.minecraft.client.main.Main",
        "downloads": {
            "client": {
                "url": client_server.url,
                "sha1": sha1_hex(&replacement),
                "size": replacement.len()
            }
        },
        "libraries": []
    })
    .to_string()
    .into_bytes();
    let version_server = TestByteServer::start(version_body);
    let client_jar = library_dir
        .join("versions")
        .join(version_id)
        .join(format!("{version_id}.jar"));
    fs::create_dir_all(client_jar.parent().expect("client jar parent")).expect("client jar parent");
    fs::write(&client_jar, vec![b'X'; replacement.len()]).expect("corrupt client jar");

    let response = start_install_version(
        &state,
        InstallVersionStartRequest {
            version_id: version_id.to_string(),
            manifest_url: version_server.url.clone(),
        },
    )
    .await
    .expect("start install");
    let status = wait_for_install_done(&state, &response.install_id).await;

    assert!(status.done);
    assert!(status.view_model.terminal);
    assert!(!status.view_model.failed);
    assert_eq!(status.view_model.phase_id, "done");
    assert!(
        status.guardian_repair.is_none(),
        "Execution self-heal should not produce a Guardian repair summary"
    );
    assert_eq!(
        fs::read(&client_jar).expect("self-healed client jar"),
        replacement
    );
    assert_eq!(
        client_server.request_count(),
        1,
        "client artifact should be downloaded once by the first install attempt"
    );
    assert_eq!(
        version_server.request_count(),
        1,
        "explicit version json should be fetched once because the install does not rerun"
    );

    client_server.stop();
    version_server.stop();
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn install_guardian_repair_restores_missing_matching_artifact() {
    let root = temp_root("guardian-install-missing-repair");
    let destination = root.join("missing-client.jar");
    let replacement = b"fresh missing client".to_vec();
    let server = TestByteServer::start(replacement.clone());
    let journals = OperationJournalStore::new();
    let failure_memory = GuardianFailureMemoryStore::new();
    let operation_id = install_operation_id("install-missing-repair");
    let target_id = "minecraft_client_1.21.5_missing";
    let facts = vec![download_fact(
        ExecutionDownloadFactKind::ArtifactMissing,
        target_id,
    )];
    let descriptors = vec![selected_descriptor(
        SelectedDownloadArtifactKind::ClientJar,
        target_id,
        &destination,
        &server.url,
        &replacement,
    )];

    let outcome = repair_install_artifact_corruption_with_guardian(
        &journals,
        &failure_memory,
        &reqwest::Client::new(),
        &operation_id,
        &facts,
        &descriptors,
        "2026-06-15T10:00:00+00:00",
    )
    .await
    .expect("repair journal")
    .expect("repair outcome");

    assert_eq!(outcome.status, GuardianArtifactRepairStatus::Repaired);
    assert_eq!(
        fs::read(&destination).expect("repaired artifact"),
        replacement
    );
    let journal = journals.get(&outcome.operation_id).expect("repair journal");
    assert!(
        !journal
            .completed_steps
            .iter()
            .any(|step| { step.step_id.contains("quarantine_launcher_managed_target") })
    );

    server.stop();
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn install_guardian_missing_artifact_repair_blocks_if_target_now_exists() {
    let root = temp_root("guardian-install-missing-now-exists");
    let destination = root.join("existing-client.jar");
    fs::write(&destination, b"existing client").expect("existing artifact");
    let replacement = b"fresh client".to_vec();
    let server = TestByteServer::start(replacement.clone());
    let journals = OperationJournalStore::new();
    let failure_memory = GuardianFailureMemoryStore::new();
    let operation_id = install_operation_id("install-missing-now-exists");
    let target_id = "minecraft_client_1.21.5_existing";
    let facts = vec![download_fact(
        ExecutionDownloadFactKind::ArtifactMissing,
        target_id,
    )];
    let descriptors = vec![selected_descriptor(
        SelectedDownloadArtifactKind::ClientJar,
        target_id,
        &destination,
        &server.url,
        &replacement,
    )];

    let outcome = repair_install_artifact_corruption_with_guardian(
        &journals,
        &failure_memory,
        &reqwest::Client::new(),
        &operation_id,
        &facts,
        &descriptors,
        "2026-06-15T10:00:00+00:00",
    )
    .await
    .expect("repair journal")
    .expect("blocked repair outcome");

    assert_eq!(outcome.status, GuardianArtifactRepairStatus::Blocked);
    assert_eq!(
        fs::read(&destination).expect("existing artifact is preserved"),
        b"existing client"
    );
    assert_eq!(server.request_count(), 0);
    let journal = journals.get(&outcome.operation_id).expect("repair journal");
    assert_eq!(journal.status, OperationStatus::Blocked);
    assert!(
        !journal
            .completed_steps
            .iter()
            .any(|step| { step.step_id.contains("quarantine_launcher_managed_target") })
    );

    server.stop();
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn install_guardian_repair_skips_artifact_missing_when_provider_failure_matches_target() {
    let root = temp_root("guardian-install-mixed-provider-failure");
    let destination = root.join("missing-client.jar");
    let replacement = b"fresh client".to_vec();
    let server = TestByteServer::start(replacement.clone());
    let journals = OperationJournalStore::new();
    let failure_memory = GuardianFailureMemoryStore::new();
    let operation_id = install_operation_id("install-mixed-provider-failure");
    let target_id = "minecraft_client_1.21.5_mixed";
    let facts = vec![
        download_fact(ExecutionDownloadFactKind::ArtifactMissing, target_id),
        ExecutionDownloadFact {
            kind: ExecutionDownloadFactKind::ProviderFailure,
            target: target_id.to_string(),
            fields: vec![("status".to_string(), "503".to_string())],
        },
    ];
    let descriptors = vec![selected_descriptor(
        SelectedDownloadArtifactKind::ClientJar,
        target_id,
        &destination,
        &server.url,
        &replacement,
    )];

    let outcome = repair_install_artifact_corruption_with_guardian(
        &journals,
        &failure_memory,
        &reqwest::Client::new(),
        &operation_id,
        &facts,
        &descriptors,
        "2026-06-15T10:00:00+00:00",
    )
    .await
    .expect("repair journal");

    assert!(outcome.is_none());
    assert!(!destination.exists());
    assert_eq!(server.request_count(), 0);
    assert!(failure_memory.list().is_empty());

    server.stop();
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn install_guardian_repair_skips_artifact_missing_when_later_terminal_failure_exists() {
    let root = temp_root("guardian-install-mixed-metadata-failure");
    let destination = root.join("asset-index.json");
    let replacement = b"fresh index".to_vec();
    let server = TestByteServer::start(replacement.clone());
    let journals = OperationJournalStore::new();
    let failure_memory = GuardianFailureMemoryStore::new();
    let operation_id = install_operation_id("install-mixed-metadata-failure");
    let facts = vec![
        download_fact(
            ExecutionDownloadFactKind::ArtifactMissing,
            "minecraft_asset_index_1.18",
        ),
        download_fact(
            ExecutionDownloadFactKind::MetadataInvalid,
            "minecraft_asset_object_bad",
        ),
    ];
    let descriptors = vec![selected_descriptor(
        SelectedDownloadArtifactKind::AssetIndex,
        "minecraft_asset_index_1.18",
        &destination,
        &server.url,
        &replacement,
    )];

    let outcome = repair_install_artifact_corruption_with_guardian(
        &journals,
        &failure_memory,
        &reqwest::Client::new(),
        &operation_id,
        &facts,
        &descriptors,
        "2026-06-15T10:00:00+00:00",
    )
    .await
    .expect("repair journal");

    assert!(outcome.is_none());
    assert!(!destination.exists());
    assert_eq!(server.request_count(), 0);

    server.stop();
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn install_guardian_repair_ignores_unrepairable_or_unmatched_facts() {
    let root = temp_root("guardian-install-repair-noop");
    let destination = root.join("client.jar");
    fs::write(&destination, b"corrupt client").expect("corrupt artifact");
    let journals = OperationJournalStore::new();
    let failure_memory = GuardianFailureMemoryStore::new();
    let operation_id = install_operation_id("install-no-repair");
    let descriptors = vec![selected_descriptor(
        SelectedDownloadArtifactKind::ClientJar,
        "client.jar",
        &destination,
        "https://example.invalid/client.jar",
        b"fresh client",
    )];

    let network_outcome = repair_install_artifact_corruption_with_guardian(
        &journals,
        &failure_memory,
        &reqwest::Client::new(),
        &operation_id,
        &[download_fact(
            ExecutionDownloadFactKind::NetworkFailure,
            "client.jar",
        )],
        &descriptors,
        "2026-06-15T10:00:00+00:00",
    )
    .await
    .expect("repair journal");
    let unmatched_outcome = repair_install_artifact_corruption_with_guardian(
        &journals,
        &failure_memory,
        &reqwest::Client::new(),
        &operation_id,
        &[download_fact(
            ExecutionDownloadFactKind::ChecksumMismatch,
            "other.jar",
        )],
        &descriptors,
        "2026-06-15T10:00:00+00:00",
    )
    .await
    .expect("repair journal");

    assert!(network_outcome.is_none());
    assert!(unmatched_outcome.is_none());
    assert_eq!(fs::read(&destination).expect("artifact"), b"corrupt client");
    assert!(failure_memory.list().is_empty());

    let _ = fs::remove_dir_all(root);
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

fn selected_descriptor(
    kind: SelectedDownloadArtifactKind,
    target: &str,
    destination: &Path,
    provider_url: &str,
    body: &[u8],
) -> SelectedDownloadArtifactDescriptor {
    SelectedDownloadArtifactDescriptor::new(
        kind,
        target,
        destination.to_path_buf(),
        provider_url,
        sha1_hex(body),
        Some(body.len() as u64),
        1024,
    )
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

async fn wait_for_install_done(state: &AppState, install_id: &str) -> InstallStatusResponse {
    for _ in 0..120 {
        let status = install_status(state, install_id)
            .await
            .expect("install status");
        if status.done {
            return status;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    panic!(
        "install did not finish: {:?}",
        install_status(state, install_id)
            .await
            .expect("install status")
    );
}

fn write_bundled_runtime_fixture(library_dir: &Path, component: &str) {
    let runtime_root = library_dir.join("runtime").join(component);
    let java = runtime_fixture_java_path(&runtime_root);
    fs::create_dir_all(java.parent().expect("java parent")).expect("java parent dir");
    fs::write(&java, b"java").expect("java executable");
    make_runtime_fixture_executable(&java);
    if cfg!(target_os = "windows") {
        let config = runtime_root.join("lib").join("jvm.cfg");
        fs::create_dir_all(config.parent().expect("runtime config parent"))
            .expect("runtime config parent");
        fs::write(config, b"jvm").expect("runtime config");
    }
}

fn runtime_fixture_java_path(runtime_root: &Path) -> PathBuf {
    if cfg!(target_os = "windows") {
        runtime_root.join("bin").join("javaw.exe")
    } else if cfg!(target_os = "macos") {
        runtime_root
            .join("jre.bundle")
            .join("Contents")
            .join("Home")
            .join("bin")
            .join("java")
    } else {
        runtime_root.join("bin").join("java")
    }
}

#[cfg(unix)]
fn make_runtime_fixture_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;

    let mut permissions = fs::metadata(path)
        .expect("runtime fixture metadata")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).expect("runtime fixture executable");
}

#[cfg(not(unix))]
fn make_runtime_fixture_executable(_path: &Path) {}

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

fn sha1_hex(bytes: impl AsRef<[u8]>) -> String {
    format!("{:x}", Sha1::digest(bytes.as_ref()))
}

fn launcher_managed_download_temp_path(destination: &Path) -> PathBuf {
    let mut name = destination
        .file_name()
        .expect("launcher managed artifact filename")
        .to_os_string();
    name.push(".axial-tmp");
    destination.with_file_name(name)
}

struct TestByteServer {
    url: String,
    request_count: Arc<AtomicUsize>,
    stop_server: mpsc::Sender<()>,
    server: thread::JoinHandle<()>,
}

impl TestByteServer {
    fn start(body: Vec<u8>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        listener
            .set_nonblocking(true)
            .expect("set test server nonblocking");
        let url = format!(
            "http://{}/artifact.jar",
            listener.local_addr().expect("server addr")
        );
        let request_count = Arc::new(AtomicUsize::new(0));
        let server_request_count = Arc::clone(&request_count);
        let (stop_server, server_stopped) = mpsc::channel();
        let server = thread::spawn(move || {
            loop {
                match listener.accept() {
                    Ok((stream, _)) => {
                        server_request_count.fetch_add(1, Ordering::SeqCst);
                        respond_ok(stream, &body);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        if server_stopped.try_recv().is_ok() {
                            break;
                        }
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("accept connection: {error}"),
                }
            }
        });

        Self {
            url,
            request_count,
            stop_server,
            server,
        }
    }

    fn request_count(&self) -> usize {
        self.request_count.load(Ordering::SeqCst)
    }

    fn stop(self) {
        self.stop_server.send(()).expect("stop test server");
        self.server.join().expect("server thread");
    }
}

fn respond_ok(mut stream: TcpStream, body: &[u8]) {
    let mut buffer = [0_u8; 1024];
    let _ = stream.read(&mut buffer);
    let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream
        .write_all(header.as_bytes())
        .expect("write response header");
    stream.write_all(body).expect("write response body");
}
