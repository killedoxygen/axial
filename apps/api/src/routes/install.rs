use crate::application::{
    InstallGuardianRepairSummary, InstallVersionCommand, begin_install_operation_journal,
    install_guardian_repair_summary_from_journal, install_operation_id,
    record_install_operation_guardian_evidence, record_install_operation_guardian_repair_outcome,
    record_install_operation_interrupted, record_install_operation_progress,
    repair_install_artifact_corruption_with_guardian, stage_install_version_command,
};
use crate::observability::{RedactionAudience, sanitize_evidence_text, sanitize_evidence_token};
use crate::state::contracts::{OperationId, OperationStatus};
use crate::state::{AppState, DownloadProgress, InstallStore};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    response::sse::{Event, Sse},
    routing::{get, post},
};
use croopor_minecraft::Downloader;
use serde::{Deserialize, Serialize};
use std::{
    convert::Infallible,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::SystemTime,
};
use tokio::sync::mpsc;

const INSTALL_FAILURE_MESSAGE: &str =
    "Install failed. Check your connection and app data permissions, then try again.";

#[derive(Debug, Deserialize)]
struct InstallRequest {
    version_id: String,
    #[serde(default)]
    manifest_url: String,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/install", post(handle_install))
        .route("/api/v1/install/{id}/status", get(handle_install_status))
        .route("/api/v1/install/{id}/events", get(handle_install_events))
}

#[derive(Debug, Serialize)]
struct InstallStatusResponse {
    install_id: String,
    operation_id: OperationId,
    done: bool,
    progress: Vec<DownloadProgress>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    guardian_repair: Option<InstallGuardianRepairSummary>,
}

async fn handle_install(
    State(state): State<AppState>,
    Json(payload): Json<InstallRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let (version_id, manifest_url) = effective_install_fields(&payload);
    if version_id.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "version_id is required" })),
        ));
    }

    let mc_dir = state.library_dir().ok_or_else(|| {
        (
            StatusCode::PRECONDITION_FAILED,
            Json(serde_json::json!({ "error": "Croopor library is not configured" })),
        )
    })?;

    let install_id = generate_install_id();
    let (install_id, inserted) = state
        .installs()
        .insert_or_existing_active(install_id, version_id.clone(), manifest_url.clone())
        .await;
    let operation_id = install_operation_id(&install_id);
    let staging = stage_install_version_command(
        InstallVersionCommand {
            version_id: version_id.clone(),
            manifest_url: (!manifest_url.is_empty()).then_some(manifest_url.clone()),
        },
        install_id.clone(),
        operation_id.clone(),
    );
    if !inserted {
        return Ok(Json(serde_json::json!({
            "install_id": install_id,
            "operation_id": operation_id,
        })));
    }
    begin_install_operation_journal(state.journals(), &operation_id, &version_id);

    let store = state.installs().clone();
    let journals = state.journals().clone();
    let failure_memory = state.failure_memory().clone();
    let mc_dir = PathBuf::from(mc_dir);
    let install_id_task = install_id.clone();
    let operation_id_task = operation_id.clone();

    let worker_store = store.clone();
    let worker_install_id = install_id_task.clone();
    let worker_journals = journals.clone();
    let worker_failure_memory = failure_memory.clone();
    let worker_operation_id = operation_id_task.clone();
    InstallStore::spawn_tracked_worker_with_interrupt_handler(
        store,
        install_id_task,
        interrupted_install_progress(),
        async move {
            let (progress_tx, mut progress_rx) = mpsc::unbounded_channel::<DownloadProgress>();
            let terminal_progress_sent = Arc::new(AtomicBool::new(false));
            let store_task = {
                let store = worker_store.clone();
                let install_id = worker_install_id.clone();
                let journals = worker_journals.clone();
                let operation_id = worker_operation_id.clone();
                tokio::spawn(async move {
                    let mut last_journal_phase = None;
                    while let Some(progress) = progress_rx.recv().await {
                        let progress = sanitize_install_progress(progress);
                        record_install_operation_progress(
                            journals.as_ref(),
                            &operation_id,
                            &progress,
                            &mut last_journal_phase,
                        );
                        store.emit(&install_id, progress).await;
                    }
                })
            };

            let downloader = Downloader::new(mc_dir);
            let progress_tx_for_downloader = progress_tx.clone();
            let terminal_progress_sent_for_downloader = Arc::clone(&terminal_progress_sent);
            let mut install_facts = Vec::new();
            let mut install_descriptors = Vec::new();
            let install_result = downloader
                .install_version_with_facts_and_descriptors(
                    &version_id,
                    (!manifest_url.is_empty()).then_some(manifest_url.as_str()),
                    move |progress| {
                        if progress.done {
                            terminal_progress_sent_for_downloader.store(true, Ordering::SeqCst);
                        }
                        let _ = progress_tx_for_downloader.send(progress);
                    },
                    |fact| install_facts.push(fact),
                    |descriptor| install_descriptors.push(descriptor),
                )
                .await;
            if install_result.is_err() && !terminal_progress_sent.load(Ordering::SeqCst) {
                terminal_progress_sent.store(true, Ordering::SeqCst);
                let _ = progress_tx.send(observed_install_failure_progress());
            }
            drop(progress_tx);
            let _ = store_task.await;
            if install_result.is_err() {
                record_install_operation_guardian_evidence(
                    worker_journals.as_ref(),
                    &worker_operation_id,
                    &install_facts,
                );
                let observed_at = chrono::Utc::now().to_rfc3339();
                let repair_client = reqwest::Client::new();
                if let Some(repair_outcome) = repair_install_artifact_corruption_with_guardian(
                    worker_journals.as_ref(),
                    worker_failure_memory.as_ref(),
                    &repair_client,
                    &worker_operation_id,
                    &install_facts,
                    &install_descriptors,
                    &observed_at,
                )
                .await
                {
                    record_install_operation_guardian_repair_outcome(
                        worker_journals.as_ref(),
                        &worker_operation_id,
                        &repair_outcome,
                    );
                }
            }
        },
        move |progress| {
            record_install_operation_interrupted(journals.as_ref(), &operation_id_task, &progress);
        },
    );

    Ok(Json(serde_json::json!({
        "install_id": install_id,
        "operation_id": staging.result.operation_id,
    })))
}

fn effective_install_fields(payload: &InstallRequest) -> (String, String) {
    (
        payload.version_id.trim().to_string(),
        payload.manifest_url.trim().to_string(),
    )
}

pub(super) fn sanitize_install_progress(mut progress: DownloadProgress) -> DownloadProgress {
    progress.phase = sanitize_evidence_token(&progress.phase, RedactionAudience::UserVisible, 48)
        .unwrap_or_else(|| "install".to_string());
    progress.file = progress
        .file
        .take()
        .and_then(|file| sanitize_evidence_token(&file, RedactionAudience::UserVisible, 96));
    progress.error = progress.error.take().and_then(|error| {
        if progress.done {
            return Some(INSTALL_FAILURE_MESSAGE.to_string());
        }
        sanitize_evidence_text(&error, RedactionAudience::UserVisible, 160)
            .or_else(|| Some(INSTALL_FAILURE_MESSAGE.to_string()))
    });
    progress
}

fn public_install_id(id: &str) -> String {
    sanitize_evidence_token(id, RedactionAudience::UserVisible, 96)
        .unwrap_or_else(|| "install".to_string())
}

fn interrupted_install_progress() -> DownloadProgress {
    observed_install_failure_progress()
}

fn observed_install_failure_progress() -> DownloadProgress {
    DownloadProgress {
        phase: "error".to_string(),
        current: 0,
        total: 0,
        file: None,
        error: Some(INSTALL_FAILURE_MESSAGE.to_string()),
        done: true,
    }
}

async fn handle_install_status(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<InstallStatusResponse>, (StatusCode, Json<serde_json::Value>)> {
    let operation_id = install_operation_id(&id);
    let snapshot = state.installs().snapshot(&id).await;
    let journal = state.journals().get(&operation_id);
    if snapshot.is_none() && journal.is_none() {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "install session not found" })),
        ));
    }

    let done = snapshot.as_ref().is_some_and(|snapshot| snapshot.done)
        || journal
            .as_ref()
            .is_some_and(|journal| install_journal_is_terminal(journal.status));
    let progress = snapshot
        .map(|snapshot| snapshot.history)
        .unwrap_or_else(Vec::new)
        .into_iter()
        .map(sanitize_install_progress)
        .collect();
    let guardian_repair = journal
        .as_ref()
        .and_then(install_guardian_repair_summary_from_journal);

    Ok(Json(InstallStatusResponse {
        install_id: public_install_id(&id),
        operation_id,
        done,
        progress,
        guardian_repair,
    }))
}

async fn handle_install_events(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<
    Sse<impl futures_util::Stream<Item = Result<Event, Infallible>>>,
    (StatusCode, Json<serde_json::Value>),
> {
    let (history, mut receiver, done) = state.installs().subscribe(&id).await.ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "install session not found" })),
        )
    })?;

    let store = state.installs().clone();
    let install_id = id.clone();
    let stream = async_stream::stream! {
        for progress in history {
            let progress = sanitize_install_progress(progress);
            let terminal = progress.done;
            yield Ok(progress_event(&progress));
            if terminal {
                return;
            }
        }
        if done {
            return;
        }

        loop {
            match receiver.recv().await {
                Ok(progress) => {
                    let progress = sanitize_install_progress(progress);
                    let terminal = progress.done;
                    yield Ok(progress_event(&progress));
                    if terminal {
                        return;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    store.remove(&install_id).await;
                    return;
                }
            }
        }
    };

    Ok(Sse::new(stream))
}

fn progress_event(progress: &DownloadProgress) -> Event {
    Event::default()
        .event("progress")
        .data(serde_json::to_string(progress).unwrap_or_else(|_| "{}".to_string()))
}

fn install_journal_is_terminal(status: OperationStatus) -> bool {
    matches!(
        status,
        OperationStatus::Succeeded
            | OperationStatus::Failed
            | OperationStatus::Blocked
            | OperationStatus::Cancelled
    )
}

fn generate_install_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|value| value.as_nanos())
        .unwrap_or_default();
    format!("install-{:032x}", nanos)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guardian::{
        DiagnosisId, GuardianActionKind, GuardianArtifactRepairOutcome,
        GuardianArtifactRepairStatus,
    };
    use crate::state::{AppStateInit, SessionStore};
    use axum::{body::to_bytes, response::IntoResponse};
    use croopor_config::{AppPaths, ConfigStore, InstanceStore};
    use croopor_performance::PerformanceManager;
    use std::{fs, path::Path as FsPath, sync::Arc};

    #[test]
    fn effective_install_fields_trims_version_id_and_manifest_url() {
        let payload = InstallRequest {
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
        let normal = InstallRequest {
            version_id: "1.21.5".to_string(),
            manifest_url: String::new(),
        };
        let explicit = InstallRequest {
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
                 prepare java runtime: failed in /home/zero/.croopor/runtime/java \
                 and C:\\Users\\zero\\AppData\\Roaming\\Croopor\\runtime\\java"
                    .to_string(),
            ),
            done: true,
        };

        let sanitized = sanitize_install_progress(progress);
        let message = sanitized.error.as_deref().expect("error is present");

        assert_eq!(message, INSTALL_FAILURE_MESSAGE);
        assert_no_raw_fragments(message);
    }

    #[test]
    fn sanitize_install_progress_preserves_shape_and_only_changes_error_text() {
        let progress = DownloadProgress {
            phase: "error".to_string(),
            current: 13,
            total: 21,
            file: Some("1.20.1.json".to_string()),
            error: Some(
                "request failed for https://example.invalid/manifest.json in /tmp/croopor"
                    .to_string(),
            ),
            done: true,
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
            file: Some("/Users/alice/.croopor/libraries/secret.jar".to_string()),
            error: Some(
                "provider_payload={\"token\":\"secret\"} account_id=account-secret username=SecretPlayer"
                    .to_string(),
            ),
            done: false,
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

    #[tokio::test]
    async fn install_events_keep_terminal_installs_subscribable_after_stream_ends() {
        let root = test_root("install-events-terminal-retention");
        let state = build_test_state(&root);
        state.installs().insert("done-install".to_string()).await;
        state.installs().emit("done-install", done_progress()).await;

        let response =
            handle_install_events(State(state.clone()), Path("done-install".to_string()))
                .await
                .expect("terminal install events should be served")
                .into_response();
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("sse body should complete");
        let body = String::from_utf8(body.to_vec()).expect("sse body is utf8");

        assert!(body.contains("event: progress"));
        assert!(body.contains("\"phase\":\"done\""));
        let (history, _, done) = state
            .installs()
            .subscribe("done-install")
            .await
            .expect("terminal install remains subscribable after stream completion");
        assert!(done);
        assert_eq!(history.len(), 1);

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn install_existing_active_response_includes_backend_operation_id() {
        let root = test_root("install-existing-active-operation");
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

        let response = handle_install(
            State(state.clone()),
            Json(InstallRequest {
                version_id: "1.21.5".to_string(),
                manifest_url: String::new(),
            }),
        )
        .await
        .expect("existing active install should be returned");
        let operation_id = crate::application::install_operation_id("existing-install");

        assert_eq!(response.0["install_id"], "existing-install");
        assert_eq!(response.0["operation_id"], operation_id.as_str());
        assert!(state.journals().get(&operation_id).is_none());

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn install_status_exposes_backend_authored_guardian_repair_summary() {
        let root = test_root("install-status-guardian-repair");
        let state = build_test_state(&root);
        let install_id = "repair-status-install";
        let operation_id = install_operation_id(install_id);
        state.installs().insert(install_id.to_string()).await;
        state
            .installs()
            .emit(install_id, observed_install_failure_progress())
            .await;
        begin_install_operation_journal(state.journals(), &operation_id, "1.21.5");
        let mut last_phase = None;
        record_install_operation_progress(
            state.journals(),
            &operation_id,
            &observed_install_failure_progress(),
            &mut last_phase,
        );
        record_install_operation_guardian_repair_outcome(
            state.journals(),
            &operation_id,
            &GuardianArtifactRepairOutcome {
                operation_id: OperationId::new(
                    "guardian-artifact-repair:123e4567-e89b-12d3-a456-426614174000",
                ),
                diagnosis_id: DiagnosisId::new("launcher_managed_artifact_corrupt"),
                action: GuardianActionKind::Repair,
                status: GuardianArtifactRepairStatus::Repaired,
                facts: vec!["https://example.invalid/client.jar?token=secret".to_string()],
                summary: "guardian_artifact_repaired".to_string(),
            },
        );

        let response = handle_install_status(State(state), Path(install_id.to_string()))
            .await
            .expect("install status");

        assert_eq!(response.0.install_id, install_id);
        assert_eq!(response.0.operation_id, operation_id);
        assert!(response.0.done);
        assert_eq!(response.0.progress.len(), 1);
        let repair = response.0.guardian_repair.expect("guardian repair");
        assert_eq!(repair.status, "repaired");
        assert_eq!(
            repair.repair_operation_id.as_str(),
            "guardian-artifact-repair:123e4567-e89b-12d3-a456-426614174000"
        );
        assert!(repair.label.contains("repaired"));
        assert_no_raw_fragments(&serde_json::to_string(&repair).expect("repair json"));

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn install_status_exposes_interrupted_install_as_redacted_terminal_state() {
        let root = test_root("install-status-interrupted");
        let state = build_test_state(&root);
        let install_id = "interrupted-status-install";
        let operation_id = install_operation_id(install_id);
        state.installs().insert(install_id.to_string()).await;
        state
            .installs()
            .emit(install_id, interrupted_install_progress())
            .await;
        begin_install_operation_journal(state.journals(), &operation_id, "1.21.5");
        record_install_operation_interrupted(
            state.journals(),
            &operation_id,
            &DownloadProgress {
                phase: r"C:\Users\Alice\.minecraft --accessToken provider_payload".to_string(),
                current: 0,
                total: 0,
                file: Some("/Users/alice/.croopor/libraries/secret.jar".to_string()),
                error: Some(
                    "worker interrupted in /Users/alice/.croopor with token secret provider_payload={\"token\":\"secret\"}"
                        .to_string(),
                ),
                done: true,
            },
        );

        let response = handle_install_status(State(state.clone()), Path(install_id.to_string()))
            .await
            .expect("install status");

        assert_eq!(response.0.install_id, install_id);
        assert_eq!(response.0.operation_id, operation_id);
        assert!(response.0.done);
        assert_eq!(response.0.progress.len(), 1);
        assert_eq!(
            response.0.progress[0].error.as_deref(),
            Some(INSTALL_FAILURE_MESSAGE)
        );
        assert!(response.0.guardian_repair.is_none());
        let journal = state.journals().get(&operation_id).expect("journal");
        assert_eq!(journal.status, OperationStatus::Failed);
        assert_eq!(
            journal.failure_point.as_deref(),
            Some("install_worker_interrupted")
        );
        assert_no_raw_fragments(&serde_json::to_string(&response.0).expect("status json"));

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn install_status_redacts_raw_progress_history_and_install_id() {
        let root = test_root("install-status-raw-progress");
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
                    file: Some("/Users/alice/.croopor/libraries/secret.jar".to_string()),
                    error: Some(
                        "provider_payload={\"token\":\"secret\"} account_id=account-secret username=SecretPlayer"
                            .to_string(),
                    ),
                    done: false,
                },
            )
            .await;

        let response = handle_install_status(State(state), Path(install_id.to_string()))
            .await
            .expect("install status");

        assert_eq!(response.0.install_id, "install");
        assert_eq!(response.0.progress.len(), 1);
        assert_eq!(response.0.progress[0].phase, "install");
        assert_eq!(response.0.progress[0].file, None);
        assert_eq!(
            response.0.progress[0].error.as_deref(),
            Some(INSTALL_FAILURE_MESSAGE)
        );
        assert_no_raw_fragments(&serde_json::to_string(&response.0).expect("status json"));

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn install_status_returns_not_found_for_unknown_install() {
        let root = test_root("install-status-unknown");
        let state = build_test_state(&root);

        let error = handle_install_status(State(state), Path("missing-install".to_string()))
            .await
            .expect_err("missing install should be 404");

        assert_eq!(error.0, StatusCode::NOT_FOUND);
        assert_eq!(error.1.0["error"], "install session not found");

        let _ = fs::remove_dir_all(root);
    }

    fn assert_no_raw_fragments(message: &str) {
        for fragment in [
            "/home/zero",
            "/tmp/croopor",
            "C:\\Users\\zero",
            "AppData\\Roaming",
            "https://",
            "piston-meta.mojang.com",
            "request failed",
            "parse version json",
            "expected value",
            "line 1 column",
            "prepare java runtime",
            "/Users/alice",
            "C:\\Users\\Alice",
            "token secret",
            "provider_payload",
            "account_id",
            "account-secret",
            "username",
            "SecretPlayer",
            "raw-secret",
        ] {
            assert!(
                !message.contains(fragment),
                "message exposed raw fragment {fragment:?}: {message}"
            );
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
        }
    }

    fn build_test_state(root: &FsPath) -> AppState {
        let paths = test_paths(root);
        let config = Arc::new(ConfigStore::load_from(paths.clone()).expect("load config"));
        let instances = Arc::new(InstanceStore::load_from(paths.clone()).expect("load instances"));
        AppState::new(AppStateInit {
            app_name: "Croopor".to_string(),
            version: "test".to_string(),
            config,
            instances,
            installs: Arc::new(InstallStore::new()),
            sessions: Arc::new(SessionStore::new()),
            performance: Arc::new(PerformanceManager::new().expect("performance manager")),
            startup_warnings: Vec::new(),
            frontend_dir: root.join("frontend"),
        })
    }

    fn configure_library_dir(state: &AppState, library_dir: &FsPath) {
        fs::create_dir_all(library_dir).expect("library dir");
        let mut config = state.config().current();
        config.library_dir = library_dir.to_string_lossy().to_string();
        state
            .config()
            .replace_in_memory(config.clone())
            .expect("config update");
        state.set_library_dir(config.library_dir);
    }

    fn test_paths(root: &FsPath) -> AppPaths {
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

    fn test_root(name: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "croopor-api-install-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|value| value.as_nanos())
                .unwrap_or_default()
        ));
        fs::create_dir_all(&path).expect("create test root");
        path
    }
}
